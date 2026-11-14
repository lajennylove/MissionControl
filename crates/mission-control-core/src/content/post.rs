//! The `Post` model and its status.

use std::fmt;
use std::str::FromStr;

use chrono::{Duration, NaiveDateTime, SubsecRound, Utc};
use orbit_orm::{Database, Error, Executor, Model, OrbitModel, Query, Transaction, Value};
use serde_json::json;
use sqlx::error::BoxDynError;

use super::meta::{delete_post_meta_in, put_post_meta_in};
use super::taxonomy::recount_post_terms;

/// The post meta that holds a trashed post's previous status, as in WordPress.
const TRASH_META_STATUS: &str = "_wp_trash_meta_status";
/// The post meta that holds when a post was trashed, as a unix time, as in WordPress.
const TRASH_META_TIME: &str = "_wp_trash_meta_time";

/// A row of `posts`: a post, a page, a custom type, an attachment, a revision or a menu item, as
/// in WordPress's `wp_posts`. `post_type` tells them apart.
#[derive(Debug, Clone, PartialEq, Eq, OrbitModel)]
#[orbit(table = "posts")]
pub struct Post {
    /// The id, shared by every post type.
    pub id: i64,
    /// The user who wrote it (`post_author`), a row of Ignite's `users`.
    #[orbit(fillable)]
    pub author_id: i64,
    /// `post`, `page`, `attachment`, `revision`, `nav_menu_item` or a custom type's name.
    #[orbit(fillable)]
    pub post_type: String,
    /// The status (`post_status`).
    #[orbit(fillable)]
    pub status: PostStatus,
    /// The title (`post_title`).
    #[orbit(fillable)]
    pub title: String,
    /// The slug (`post_name`). A draft may have none yet.
    #[orbit(fillable)]
    pub slug: String,
    /// The body (`post_content`).
    #[orbit(fillable)]
    pub content: String,
    /// The excerpt (`post_excerpt`).
    #[orbit(fillable)]
    pub excerpt: String,
    /// The parent post (`post_parent`): a page's parent page, an attachment's post, a
    /// revision's post.
    #[orbit(fillable)]
    pub parent_id: Option<i64>,
    /// The order among siblings (`menu_order`), used by pages and menu items.
    #[orbit(fillable)]
    pub menu_order: i32,
    /// When it was, or will be, published (`post_date_gmt`), in UTC.
    #[orbit(fillable)]
    pub published_at: Option<NaiveDateTime>,
    /// When it last changed (`post_modified_gmt`), in UTC. Set on create and on every save.
    #[orbit(fillable)]
    pub modified_at: NaiveDateTime,
    /// The password that protects it (`post_password`), if any.
    #[orbit(fillable)]
    pub password: Option<String>,
}

/// The columns of a post to create: every column of [`Post`] but the id and `modified_at`,
/// which are set on insert. Start from [`NewPost::new`] and set the rest with struct update
/// syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPost {
    /// The user who writes it.
    pub author_id: i64,
    /// The post type, such as `post` or `page`.
    pub post_type: String,
    /// The status.
    pub status: PostStatus,
    /// The title.
    pub title: String,
    /// The slug.
    pub slug: String,
    /// The body.
    pub content: String,
    /// The excerpt.
    pub excerpt: String,
    /// The parent post.
    pub parent_id: Option<i64>,
    /// The order among siblings.
    pub menu_order: i32,
    /// When it was, or will be, published, in UTC.
    pub published_at: Option<NaiveDateTime>,
    /// The password that protects it.
    pub password: Option<String>,
}

impl NewPost {
    /// An empty draft of `post_type` by `author_id`, with no parent, order 0, no publish date and
    /// no password.
    pub fn new(author_id: i64, post_type: &str) -> Self {
        Self {
            author_id,
            post_type: post_type.to_owned(),
            status: PostStatus::Draft,
            title: String::new(),
            slug: String::new(),
            content: String::new(),
            excerpt: String::new(),
            parent_id: None,
            menu_order: 0,
            published_at: None,
            password: None,
        }
    }
}

impl Post {
    /// Inserts `new` and returns the stored post, with `modified_at` set to now.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::ForeignKeyViolation`] when the author does not exist, or with the
    /// database's error.
    pub async fn create<'a>(db: impl Executor<'a>, new: NewPost) -> Result<Self, Error> {
        let mut post = Self {
            id: 0,
            author_id: new.author_id,
            post_type: new.post_type,
            status: new.status,
            title: new.title,
            slug: new.slug,
            content: new.content,
            excerpt: new.excerpt,
            parent_id: new.parent_id,
            menu_order: new.menu_order,
            published_at: new.published_at,
            modified_at: now(),
            password: new.password,
        };
        let id = Self::query().insert(db, &post.columns()).await?;
        post.id = i64::try_from(id).expect("an id from a BIGINT column fits in i64");
        Ok(post)
    }

    /// Writes every column of this post to its row, as Laravel's `save` does, with a new
    /// `modified_at` (see [`next_modified_at`]). In the same transaction, recounts the post's
    /// terms, as WordPress does when a post's status changes, so publishing or unpublishing a
    /// post keeps its terms' `count` in step. A `future` post whose date has passed is saved as
    /// `publish`, as `wp_insert_post` does (see [`due_status`]).
    ///
    /// This is the unchecked path: the last write wins, whatever was saved since this post was
    /// read. Editor saves must not use it; they go through
    /// [`save_revision`](super::save_revision), which refuses a save that would overwrite
    /// another. The new `modified_at` is computed from the stored one, and the update is
    /// conditional on it, retried when another write lands in between, so the stamp never
    /// moves backwards and never returns to a value an editor may still hold.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NotFound`] when the row is gone, or with the database's error; then
    /// nothing is written and the post is unchanged.
    pub async fn save(&mut self, db: &Database) -> Result<(), Error> {
        let id = self.id;
        let mut saved = self.clone();
        saved.status = due_status(saved.status, saved.published_at);
        loop {
            let stored = Self::query().find_or_fail(db, id).await?;
            saved.modified_at = next_modified_at(stored.modified_at);
            let columns = saved.columns();
            let written = db
                .transaction(async |tx| {
                    // The stamp always changes, so a matched row is a changed row and MySQL's
                    // count of changed rows is the count of matched rows.
                    let matched = Self::query()
                        .where_eq(Self::PRIMARY_KEY, id)
                        .where_eq("modified_at", stored.modified_at)
                        .update(&mut *tx, &columns)
                        .await?;
                    if matched == 0 {
                        return Ok(false);
                    }
                    recount_post_terms(tx, id).await?;
                    Ok::<_, Error>(true)
                })
                .await?;
            if written {
                *self = saved;
                return Ok(());
            }
        }
    }

    /// Moves this post to the trash, as WordPress's `wp_trash_post` does: records its previous
    /// status in the post meta `_wp_trash_meta_status` and the time in `_wp_trash_meta_time` (a
    /// unix time), sets its status to `trash` with a new `modified_at`, and recounts its terms,
    /// in one transaction. Only the status and `modified_at` are written, read from and
    /// conditional on the stored row, so a stale copy of the post does not overwrite another
    /// save. A trashed post stays in the table until it is deleted. A post already in the trash
    /// is left as it is, so the first status is kept. On success this post becomes the stored
    /// row.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NotFound`] when the row is gone, or with the database's error; then
    /// nothing is written and the post is unchanged.
    pub async fn trash(&mut self, db: &Database) -> Result<(), Error> {
        let id = self.id;
        let stored = loop {
            let stored = Self::query().find_or_fail(db, id).await?;
            if stored.status == PostStatus::Trash {
                *self = stored;
                return Ok(());
            }
            let previous = stored.status;
            let next = Self {
                status: PostStatus::Trash,
                modified_at: next_modified_at(stored.modified_at),
                ..stored.clone()
            };
            let written = db
                .transaction(async |tx| {
                    if !set_status(tx, &stored, &next).await? {
                        return Ok(false);
                    }
                    put_post_meta_in(tx, id, TRASH_META_STATUS, &json!(previous.as_str())).await?;
                    put_post_meta_in(tx, id, TRASH_META_TIME, &json!(Utc::now().timestamp()))
                        .await?;
                    Ok::<_, Error>(true)
                })
                .await?;
            if written {
                break next;
            }
        };
        *self = stored;
        Ok(())
    }

    /// Takes this post out of the trash, as WordPress 5.6 and later do in `wp_untrash_post`: the
    /// status becomes `draft`, whatever it was before, except that an attachment goes back to
    /// `inherit`. Deletes the two trash meta keys. Like [`Post::trash`], only the status and
    /// `modified_at` are written, conditional on the stored row. A post that is not in the
    /// trash is left as it is. On success this post becomes the stored row.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NotFound`] when the row is gone, or with the database's error; then
    /// nothing is written and the post is unchanged.
    pub async fn untrash(&mut self, db: &Database) -> Result<(), Error> {
        let id = self.id;
        let stored = loop {
            let stored = Self::query().find_or_fail(db, id).await?;
            if stored.status != PostStatus::Trash {
                *self = stored;
                return Ok(());
            }
            let next = Self {
                status: if stored.post_type == "attachment" {
                    PostStatus::Inherit
                } else {
                    PostStatus::Draft
                },
                modified_at: next_modified_at(stored.modified_at),
                ..stored.clone()
            };
            let written = db
                .transaction(async |tx| {
                    if !set_status(tx, &stored, &next).await? {
                        return Ok(false);
                    }
                    delete_post_meta_in(tx, id, TRASH_META_STATUS).await?;
                    delete_post_meta_in(tx, id, TRASH_META_TIME).await?;
                    Ok::<_, Error>(true)
                })
                .await?;
            if written {
                break next;
            }
        };
        *self = stored;
        Ok(())
    }

    /// The posts of one type, such as `post` or `page`, like a Laravel query scope. Attachments,
    /// revisions and menu items share the table, and this leaves them out unless asked for.
    pub fn of_type(post_type: &str) -> Query<Self> {
        Self::query().where_eq("post_type", post_type)
    }

    /// Every column but the id, with its value, for insert and update.
    pub(super) fn columns(&self) -> [(&'static str, Value); 12] {
        [
            ("author_id", self.author_id.into()),
            ("post_type", self.post_type.clone().into()),
            ("status", self.status.into()),
            ("title", self.title.clone().into()),
            ("slug", self.slug.clone().into()),
            ("content", self.content.clone().into()),
            ("excerpt", self.excerpt.clone().into()),
            ("parent_id", self.parent_id.into()),
            ("menu_order", self.menu_order.into()),
            ("published_at", self.published_at.into()),
            ("modified_at", self.modified_at.into()),
            ("password", self.password.clone().into()),
        ]
    }
}

/// Sets `stored`'s status and `modified_at` to `next`'s, if the row still has `stored`'s
/// status and `modified_at`, and recounts its terms, on the connection of a transaction. Returns
/// whether the row was written; `false` means another write landed since `stored` was read.
async fn set_status(tx: &mut Transaction, stored: &Post, next: &Post) -> Result<bool, Error> {
    // The stamp always changes, so a matched row is a changed row on MySQL too.
    let matched = Post::query()
        .where_eq(Post::PRIMARY_KEY, stored.id)
        .where_eq("status", stored.status)
        .where_eq("modified_at", stored.modified_at)
        .update(
            &mut *tx,
            &[
                ("status", next.status.into()),
                ("modified_at", next.modified_at.into()),
            ],
        )
        .await?;
    if matched == 0 {
        return Ok(false);
    }
    recount_post_terms(tx, stored.id).await?;
    Ok(true)
}

/// The `modified_at` for a write to a row whose stored `modified_at` is `stored`: now, or one
/// second after `stored` when now is not later. Every write to a post row uses it, so the stamp
/// only moves forward. The column has one-second resolution, so without the extra second two
/// writes in the same second would leave the same stamp, and an editor holding it as the
/// expected value of [`save_revision`](super::save_revision) would overwrite the second write
/// unnoticed. Under rapid writes the stamp can run a few seconds ahead of the clock.
pub(super) fn next_modified_at(stored: NaiveDateTime) -> NaiveDateTime {
    now().max(stored + Duration::seconds(1))
}

/// The status to save a post with, as `wp_insert_post` decides it: a `future` post whose date
/// is now or past, or that has no date, is saved as `publish`. So a save of a stale copy does
/// not put back `future` on a post the scheduler has already published, which would never be
/// published again. Any other status is kept.
pub(super) fn due_status(status: PostStatus, published_at: Option<NaiveDateTime>) -> PostStatus {
    match (status, published_at) {
        (PostStatus::Future, Some(date)) if date > now() => PostStatus::Future,
        (PostStatus::Future, _) => PostStatus::Publish,
        (other, _) => other,
    }
}

/// The current time in UTC, to the second: what MySQL's `DATETIME` and PostgreSQL's
/// `TIMESTAMP(0)` store, so the value read back equals the one written.
pub(super) fn now() -> NaiveDateTime {
    Utc::now().naive_utc().trunc_subsecs(0)
}

/// A post's status, stored as WordPress's `post_status` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PostStatus {
    /// `draft`: saved, not published.
    Draft,
    /// `auto-draft`: created when the editor opens, before anything is saved.
    AutoDraft,
    /// `pending`: awaiting review.
    Pending,
    /// `private`: published, visible only to users who may read private posts.
    Private,
    /// `publish`: published.
    Publish,
    /// `future`: scheduled for its publish date.
    Future,
    /// `trash`: in the trash.
    Trash,
    /// `inherit`: takes its parent's status, as WordPress gives revisions and attachments.
    Inherit,
}

impl PostStatus {
    /// The string WordPress stores, such as `auto-draft`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::AutoDraft => "auto-draft",
            Self::Pending => "pending",
            Self::Private => "private",
            Self::Publish => "publish",
            Self::Future => "future",
            Self::Trash => "trash",
            Self::Inherit => "inherit",
        }
    }
}

impl fmt::Display for PostStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not one of WordPress's post statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPostStatus(pub String);

impl fmt::Display for UnknownPostStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown post status {:?}", self.0)
    }
}

impl std::error::Error for UnknownPostStatus {}

impl FromStr for PostStatus {
    type Err = UnknownPostStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "draft" => Self::Draft,
            "auto-draft" => Self::AutoDraft,
            "pending" => Self::Pending,
            "private" => Self::Private,
            "publish" => Self::Publish,
            "future" => Self::Future,
            "trash" => Self::Trash,
            "inherit" => Self::Inherit,
            other => return Err(UnknownPostStatus(other.to_owned())),
        })
    }
}

impl From<PostStatus> for Value {
    fn from(status: PostStatus) -> Self {
        Self::from(status.as_str())
    }
}

/// Reads as a string column on every database.
impl<DB: sqlx::Database> sqlx::Type<DB> for PostStatus
where
    String: sqlx::Type<DB>,
{
    fn type_info() -> DB::TypeInfo {
        <String as sqlx::Type<DB>>::type_info()
    }

    fn compatible(ty: &DB::TypeInfo) -> bool {
        <String as sqlx::Type<DB>>::compatible(ty)
    }
}

/// Decodes the stored string; a string that is not a status is an error.
impl<'r, DB: sqlx::Database> sqlx::Decode<'r, DB> for PostStatus
where
    String: sqlx::Decode<'r, DB>,
{
    fn decode(value: DB::ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(<String as sqlx::Decode<'r, DB>>::decode(value)?.parse()?)
    }
}
