//! Revisions and autosaves, after WordPress's `wp_save_post_revision` and
//! `wp_create_post_autosave`, with a conflict check so two editors never silently overwrite each
//! other.
//!
//! There is no table of its own: a revision is a row of `posts` with `post_type = revision`,
//! status `inherit` and `parent_id` set to the post, holding a copy of the post's title, content
//! and excerpt, as WordPress's `_wp_put_post_revision` stores it. A revision's slug is
//! `{post id}-revision-v1` and an autosave's is `{post id}-autosave-v1`, the names WordPress
//! gives them.
//!
//! ```ignore
//! use mission_control_core::content::{SaveError, save_revision};
//!
//! // `opened_at` is the post's `modified_at` when the editor opened it.
//! match save_revision(&db, &edited, opened_at).await {
//!     Ok(revision) => opened_at = revision.modified_at,
//!     Err(SaveError::Conflict { current_modified_at }) => { /* show the conflict */ }
//!     Err(e) => return Err(e.into()),
//! }
//! ```

use std::fmt;
use std::ops::Deref;

use chrono::NaiveDateTime;
use orbit_orm::{Database, Direction, Error, Model, Operator, Transaction};

use super::post::{Post, PostStatus, due_status, next_modified_at, now, write_settled_slug};
use super::slug::{record_old_slug, wanted_slug};
use super::taxonomy::recount_post_terms;

/// The `post_type` of revisions and autosaves.
const REVISION: &str = "revision";

/// A revision or an autosave of a post: a row of `posts` with `post_type = revision`. It reads
/// as the [`Post`] it wraps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision(Post);

impl Revision {
    /// Whether this is an autosave rather than a saved revision, as WordPress's
    /// `wp_is_post_autosave` tells by the slug.
    pub fn is_autosave(&self) -> bool {
        self.0
            .parent_id
            .is_some_and(|parent| self.0.slug == autosave_slug(parent))
    }

    /// The `posts` row.
    pub fn into_post(self) -> Post {
        self.0
    }
}

impl Deref for Revision {
    type Target = Post;

    fn deref(&self) -> &Post {
        &self.0
    }
}

/// Why a save or an autosave was not written. Nothing is written when it fails.
#[derive(Debug)]
pub enum SaveError {
    /// The post changed after `expected_modified_at`: another editor saved it first. The save
    /// is refused rather than overwrite that change. `current_modified_at` is the post's stored
    /// time, to save again from once the editor has seen the other change.
    Conflict {
        /// The post's `modified_at` as stored now.
        current_modified_at: NaiveDateTime,
    },
    /// No post has this id.
    NotFound {
        /// The id that was saved.
        id: i64,
    },
    /// The post is itself a revision or an autosave. WordPress does not revise revisions.
    IsRevision {
        /// The id that was saved.
        id: i64,
    },
    /// The database refused a query.
    Database(Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict {
                current_modified_at,
            } => write!(
                f,
                "the post was changed by another save at {current_modified_at}"
            ),
            Self::NotFound { id } => write!(f, "post {id} does not exist"),
            Self::IsRevision { id } => write!(f, "post {id} is a revision"),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            Self::Conflict { .. } | Self::NotFound { .. } | Self::IsRevision { .. } => None,
        }
    }
}

impl From<Error> for SaveError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

/// Saves `post` and records a revision of it, as WordPress's `wp_update_post` followed by
/// `wp_save_post_revision` does, in one transaction. Returns the revision; its `modified_at` is
/// the post's new `modified_at`, the value to pass as `expected_modified_at` on the next save.
///
/// `expected_modified_at` is the post's `modified_at` when the editor opened it. The post row is
/// written only if it still holds that time: the update is conditional on the id and the
/// stored `modified_at`, so of two saves from the same starting point the database lets exactly
/// one through, and the other gets [`SaveError::Conflict`] with the stored time. Nothing is
/// written then, neither the post nor a revision. WordPress itself has no such check (it warns
/// with a post lock instead); Mission Control refuses the second save so it is never lost
/// silently.
///
/// The new `modified_at` is strictly later than `expected_modified_at`: now, or one second after
/// the expected time when now is not later, the rule every write to a post row follows. The
/// column has one-second resolution, so two saves in the same second from the same start would
/// otherwise both stamp, and find, the same time. Under rapid saves the stamp can run a few
/// seconds ahead of the clock.
///
/// As `wp_insert_post` does, a `future` post whose date is less than a minute ahead, or past, is
/// saved as `publish` (attachments are left as they are), so an editor who opened the post
/// before the scheduler published it does not put it back to `future`, where it would never be
/// published.
///
/// The slug is settled as [`Post::save`] settles it: sanitized, made from the title when empty,
/// and made unique unless the post is a draft, pending or an auto-draft. The returned revision
/// does not carry the post's slug; read the post again for it. A published post whose slug
/// changes keeps the old slug in the post meta `_wp_old_slug`, as [`Post::save`] does.
///
/// Like WordPress, the revision's author is the post's author, and its `published_at` and
/// `modified_at` are the post's new modification time. Every save records a revision, even one
/// that changed nothing, and there is no cap on how many are kept. No hooks are fired.
///
/// # Errors
///
/// [`SaveError::Conflict`] when the post was saved after `expected_modified_at`,
/// [`SaveError::NotFound`] when there is no post with `post.id`, [`SaveError::IsRevision`] when
/// the post is a revision, and [`SaveError::Database`] when the database fails.
pub async fn save_revision(
    db: &Database,
    post: &Post,
    expected_modified_at: NaiveDateTime,
) -> Result<Revision, SaveError> {
    let id = post.id;
    if post.post_type == REVISION {
        return Err(SaveError::IsRevision { id });
    }
    let mut saved = post.clone();
    saved.status = due_status(&saved.post_type, saved.status, saved.published_at);
    saved.slug = wanted_slug(&saved, Some(id));
    saved.modified_at = next_modified_at(expected_modified_at);
    let columns = saved.columns();
    let revision = revision_of(
        &saved,
        saved.author_id,
        revision_slug(id),
        saved.modified_at,
    );
    // The row as it is before the save, for the old slug. Read outside the transaction, so on
    // SQLite the transaction starts with its write and waits for the lock rather than failing to
    // upgrade a read. Stamps only move forward, so when the update below matches
    // `expected_modified_at`, no write landed since this read and it is the row the save replaces.
    let before = match Post::query().find(db, id).await? {
        None => return Err(SaveError::NotFound { id }),
        Some(row) if row.post_type == REVISION => return Err(SaveError::IsRevision { id }),
        Some(row) if row.modified_at != expected_modified_at => {
            return Err(SaveError::Conflict {
                current_modified_at: row.modified_at,
            });
        }
        Some(row) => row,
    };
    db.transaction(async |tx| {
        let matched = Post::query()
            .where_eq(Post::PRIMARY_KEY, id)
            .where_eq("modified_at", expected_modified_at)
            .where_op("post_type", Operator::Ne, REVISION)
            .update(&mut *tx, &columns)
            .await?;
        // MySQL reports changed rows, not matched rows. The new stamp is always later than the
        // one matched, so a matched row is always a changed row, and 0 means no row matched.
        if matched == 0 {
            return Err(match Post::query().find(&mut *tx, id).await? {
                Some(current) if current.post_type == REVISION => SaveError::IsRevision { id },
                Some(current) => SaveError::Conflict {
                    current_modified_at: current.modified_at,
                },
                None => SaveError::NotFound { id },
            });
        }
        recount_post_terms(tx, id).await?;
        crate::theme::auto_add_page_in(tx, &saved, Some(before.status)).await?;
        write_settled_slug(tx, &mut saved).await?;
        record_old_slug(tx, &before, &saved).await?;
        Ok(insert(tx, revision).await?)
    })
    .await
}

/// Stores `post` as `user_id`'s autosave of it, as WordPress's `wp_create_post_autosave` does,
/// and returns the autosave. A user has at most one autosave per post: an earlier one is
/// replaced in place, keeping its id. The post row is not changed, nor its status, nor its
/// `modified_at`, so an autosave never causes a [`SaveError::Conflict`] for a later save.
///
/// The post's row is locked for the transaction (MySQL, MariaDB and PostgreSQL), so two
/// autosaves of one post do not both insert. SQLite has no row locks; there the second writer
/// waits for the first, or fails with a busy error.
///
/// A replaced autosave's `modified_at` follows the rule of every post write: strictly later
/// than its stored one.
///
/// # Errors
///
/// [`SaveError::NotFound`] when there is no post with `post.id`, [`SaveError::IsRevision`] when
/// the post is a revision, [`SaveError::Database`] when the database fails.
pub async fn create_post_autosave(
    db: &Database,
    post: &Post,
    user_id: i64,
) -> Result<Revision, SaveError> {
    let id = post.id;
    if post.post_type == REVISION {
        return Err(SaveError::IsRevision { id });
    }
    let mut autosave = revision_of(post, user_id, autosave_slug(id), now());
    db.transaction(async |tx| {
        let parent = Post::query()
            .where_eq(Post::PRIMARY_KEY, id)
            .for_update()
            .first(&mut *tx)
            .await?
            .ok_or(SaveError::NotFound { id })?;
        if parent.post_type == REVISION {
            return Err(SaveError::IsRevision { id });
        }
        match autosave_query(id, user_id)
            .for_update()
            .first(&mut *tx)
            .await?
        {
            Some(existing) => {
                autosave.id = existing.id;
                autosave.modified_at = next_modified_at(existing.modified_at);
                autosave.published_at = Some(autosave.modified_at);
                Post::query()
                    .where_eq(Post::PRIMARY_KEY, existing.id)
                    .update(&mut *tx, &autosave.columns())
                    .await?;
                Ok(Revision(autosave))
            }
            None => Ok(insert(tx, autosave).await?),
        }
    })
    .await
}

/// The revisions of post `post_id`, autosaves included, newest first, as WordPress's
/// `wp_get_post_revisions` lists them (by date, then id).
///
/// # Errors
///
/// Fails with the database's error.
pub async fn get_post_revisions(db: &Database, post_id: i64) -> Result<Vec<Revision>, Error> {
    let rows = Post::query()
        .where_eq("post_type", REVISION)
        .where_eq("parent_id", post_id)
        .order_by("published_at", Direction::Desc)
        .order_by(Post::PRIMARY_KEY, Direction::Desc)
        .get(db)
        .await?;
    Ok(rows.into_iter().map(Revision).collect())
}

/// `user_id`'s autosave of post `post_id`, if any, as WordPress's `wp_get_post_autosave`.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn get_post_autosave(
    db: &Database,
    post_id: i64,
    user_id: i64,
) -> Result<Option<Revision>, Error> {
    Ok(autosave_query(post_id, user_id)
        .first(db)
        .await?
        .map(Revision))
}

/// The query for `user_id`'s autosave of post `post_id`.
fn autosave_query(post_id: i64, user_id: i64) -> orbit_orm::Query<Post> {
    Post::query()
        .where_eq("post_type", REVISION)
        .where_eq("parent_id", post_id)
        .where_eq("author_id", user_id)
        .where_eq("slug", autosave_slug(post_id))
}

/// A revision row of `post` by `author_id`, dated `at`, not yet stored: the fields WordPress's
/// `_wp_post_revision_data` copies (title, content, excerpt) and the parent.
fn revision_of(post: &Post, author_id: i64, slug: String, at: NaiveDateTime) -> Post {
    Post {
        id: 0,
        author_id,
        post_type: REVISION.to_owned(),
        status: PostStatus::Inherit,
        title: post.title.clone(),
        slug,
        content: post.content.clone(),
        excerpt: post.excerpt.clone(),
        parent_id: Some(post.id),
        menu_order: 0,
        published_at: Some(at),
        modified_at: at,
        password: None,
        mime_type: String::new(),
    }
}

/// Inserts `revision` and returns it with its id.
async fn insert(tx: &mut Transaction, mut revision: Post) -> Result<Revision, Error> {
    let id = Post::query().insert(&mut *tx, &revision.columns()).await?;
    revision.id = i64::try_from(id).expect("an id from a BIGINT column fits in i64");
    Ok(Revision(revision))
}

/// The slug WordPress gives a revision of post `post_id`.
fn revision_slug(post_id: i64) -> String {
    format!("{post_id}-revision-v1")
}

/// The slug WordPress gives an autosave of post `post_id`.
fn autosave_slug(post_id: i64) -> String {
    format!("{post_id}-autosave-v1")
}
