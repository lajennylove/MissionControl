//! The `Post` model and its status.

use std::fmt;
use std::str::FromStr;

use chrono::{Duration, NaiveDateTime, SubsecRound, Utc};
use orbit_orm::{Database, Error, Model, OrbitModel, Query, Transaction, Value};
use serde_json::json;
use sqlx::error::BoxDynError;

use super::meta::{
    MetaError, delete_post_meta_in, get_option, get_post_meta_in, put_post_meta_in, set_option,
};
use super::slug::{
    DESIRED_SLUG_META, TRASHED_SUFFIX, is_hierarchical, record_old_slug, settle_slug, trashed_slug,
    unique_slug_on, wanted_slug,
};
use super::taxonomy::{post_term_ids_in, recount_in, recount_post_terms};

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
    /// The MIME type (`post_mime_type`), such as `image/jpeg`. Set for attachments; empty for
    /// every other post type.
    #[orbit(fillable)]
    pub mime_type: String,
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
    /// The MIME type, for an attachment; empty otherwise.
    pub mime_type: String,
}

impl NewPost {
    /// An empty draft of `post_type` by `author_id`, with no parent, order 0, no publish date, no
    /// password and no MIME type.
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
            mime_type: String::new(),
        }
    }
}

impl Post {
    /// Inserts `new` and returns the stored post, with `modified_at` set to now.
    ///
    /// The slug is settled as `wp_insert_post` does: the given slug is sanitized with
    /// [`make_slug`](super::make_slug). Unless the status is `draft`, `pending` or `auto-draft`,
    /// an empty slug is made from the title (or, when the title gives none, the new post's id)
    /// and the slug is made unique with [`unique_slug`](super::unique_slug). Drafts and pending
    /// posts keep their slug as given, even empty or taken, until they are published. The insert
    /// and the slug are one transaction.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::ForeignKeyViolation`] when the author does not exist, or with the
    /// database's error.
    pub async fn create(db: &Database, new: NewPost) -> Result<Self, Error> {
        db.transaction(async |tx| {
            let post = Self::create_in(tx, new).await?;
            // `_wp_auto_add_pages_to_menu`, on the new post's status.
            crate::theme::auto_add_page_in(tx, &post, None).await?;
            Ok(post)
        })
        .await
    }

    /// [`Post::create`] on the connection of a transaction the caller holds, so other writes
    /// (an attachment's meta) commit or roll back with the post.
    pub(crate) async fn create_in(tx: &mut Transaction, new: NewPost) -> Result<Self, Error> {
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
            mime_type: new.mime_type,
        };
        post.slug = wanted_slug(&post, None);
        // The insert comes first, so on SQLite the transaction holds the write lock before it
        // reads.
        let id = Self::query().insert(&mut *tx, &post.columns()).await?;
        post.id = i64::try_from(id).expect("an id from a BIGINT column fits in i64");
        write_settled_slug(tx, &mut post).await?;
        Ok(post)
    }

    /// Writes every column of this post to its row, as Laravel's `save` does, with a new
    /// `modified_at`: now, or one second after the stored one when now is not later. In the same
    /// transaction, recounts the post's terms, as WordPress does when a post's status changes,
    /// so publishing or unpublishing a post keeps its terms' `count` in step. A `future` post
    /// whose date is less than a minute ahead, or past, is saved as `publish`, as `wp_insert_post`
    /// does; attachments are left as they are. The slug is settled as in [`Post::create`]
    /// (sanitized, made from the title or the id when empty, made unique unless the post is a
    /// draft, pending or an auto-draft). A published post whose slug changes keeps the old slug
    /// in the post meta `_wp_old_slug`, so links to it can still find it (see
    /// [`find_by_old_slug`](super::find_by_old_slug)).
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
        saved.status = due_status(&saved.post_type, saved.status, saved.published_at);
        saved.slug = wanted_slug(&saved, Some(id));
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
                    crate::theme::auto_add_page_in(tx, &saved, Some(stored.status)).await?;
                    write_settled_slug(tx, &mut saved).await?;
                    record_old_slug(tx, &stored, &saved).await?;
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
    /// in one transaction. The slug gets the suffix `__trashed`, so another post can take it,
    /// and the slug is kept in the post meta `_wp_desired_post_slug` for [`Post::untrash`], as
    /// `wp_add_trashed_suffix_to_post_name_for_post` does. Only the status, slug and
    /// `modified_at` are written, read from and conditional on the stored row, so a stale copy of
    /// the post does not overwrite another save. A trashed post stays in the table until it is
    /// deleted. A post already in the trash is left as it is, so the first status is kept. On
    /// success this post becomes the stored row.
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
                slug: trashed_slug(&stored.slug),
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
                    if next.slug != stored.slug {
                        put_post_meta_in(tx, id, DESIRED_SLUG_META, &json!(stored.slug)).await?;
                    }
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
    /// `inherit`. Deletes the two trash meta keys. The slug the post had before the trash
    /// (`_wp_desired_post_slug`) comes back, as `wp_insert_post` restores it, made unique with
    /// `wp_unique_post_slug`'s rules when another post took it meanwhile (`-2`, `-3`, ...), so
    /// two posts never share a slug after a restore. (WordPress itself leaves a restored draft's
    /// slug as it is until the post is published; the owner's rule is that a restore never
    /// gives two posts the same slug.) Like [`Post::trash`], only the
    /// status, slug and `modified_at` are written, conditional on the stored row. A post that is
    /// not in the trash is left as it is. On success this post becomes the stored row.
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
            let mut next = Self {
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
                    // The write comes first, so on SQLite the transaction takes the write lock
                    // (waiting for it) before it reads; the slug is set once it is known.
                    if !set_status(tx, &stored, &next).await? {
                        return Ok(false);
                    }
                    let desired = match get_post_meta_in(tx, id, DESIRED_SLUG_META).await {
                        Ok(Some(serde_json::Value::String(slug))) => slug,
                        // No desired slug (trashed before slugs were kept, or a value that is
                        // not a string): the trashed slug without its suffix.
                        Ok(_) | Err(MetaError::InvalidJson(_)) => stored
                            .slug
                            .strip_suffix(TRASHED_SUFFIX)
                            .unwrap_or(&stored.slug)
                            .to_owned(),
                        Err(MetaError::Database(e)) => return Err(e),
                    };
                    next.slug = unique_slug_on(
                        tx,
                        &stored.post_type,
                        is_hierarchical(&stored.post_type),
                        stored.parent_id,
                        &desired,
                        Some(id),
                    )
                    .await?;
                    Self::query()
                        .where_eq(Self::PRIMARY_KEY, id)
                        .update(&mut *tx, &[("slug", next.slug.clone().into())])
                        .await?;
                    delete_post_meta_in(tx, id, TRASH_META_STATUS).await?;
                    delete_post_meta_in(tx, id, TRASH_META_TIME).await?;
                    delete_post_meta_in(tx, id, DESIRED_SLUG_META).await?;
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

    /// Deletes this post for good, as WordPress's `wp_delete_post` with `$force_delete` does, and
    /// returns whether a row was deleted (`false` when it was gone already).
    ///
    /// In one transaction: the post's children of the same type and its attachments move up to
    /// the post's own parent, its revisions and autosaves are deleted, the post row is deleted
    /// (its meta and term relationships go with it, by foreign key), and the terms it had are
    /// recounted, so a category's `count` stays right. Then, as WordPress does: a page that was
    /// the static front page (`page_on_front`) sets `show_on_front` back to `posts` and
    /// `page_on_front` to 0, a posts page (`page_for_posts`) sets that option to 0, the post
    /// leaves `sticky_posts`, and its scheduled publication is removed. Comments do not exist
    /// yet, so none are deleted.
    ///
    /// # Errors
    ///
    /// Fails with the database's error; the transaction then leaves the post as it was.
    pub async fn delete(&self, db: &Database) -> Result<bool, Error> {
        let id = self.id;
        let post_type = self.post_type.clone();
        let parent = self.parent_id;
        let deleted = db
            .transaction(async |tx| {
                let terms = post_term_ids_in(tx, id).await?;
                Self::query()
                    .where_eq("parent_id", id)
                    .where_in("post_type", [post_type.clone(), "attachment".to_owned()])
                    .update(&mut *tx, &[("parent_id", parent.into())])
                    .await?;
                Self::query()
                    .where_eq("post_type", "revision")
                    .where_eq("parent_id", id)
                    .delete(&mut *tx)
                    .await?;
                let deleted = Self::query()
                    .where_eq(Self::PRIMARY_KEY, id)
                    .delete(&mut *tx)
                    .await?;
                for (taxonomy, ids) in terms {
                    recount_in(tx, &taxonomy, &ids).await?;
                }
                Ok::<_, Error>(deleted > 0)
            })
            .await?;
        if !deleted {
            return Ok(false);
        }
        forget_deleted_post(db, self).await?;
        Ok(true)
    }

    /// The posts of one type, such as `post` or `page`, like a Laravel query scope. Attachments,
    /// revisions and menu items share the table, and this leaves them out unless asked for.
    pub fn of_type(post_type: &str) -> Query<Self> {
        Self::query().where_eq("post_type", post_type)
    }

    /// Every column but the id, with its value, for insert and update.
    pub(super) fn columns(&self) -> [(&'static str, Value); 13] {
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
            ("mime_type", self.mime_type.clone().into()),
        ]
    }
}

/// What `wp_delete_post` does outside the post's own rows once `post` is gone: the site's special
/// pages and sticky posts forget it, and its scheduled publication is removed.
async fn forget_deleted_post(db: &Database, post: &Post) -> Result<(), Error> {
    let option_id = async |name: &str| -> Result<Option<i64>, Error> {
        Ok(match get_option(db, name).await {
            Ok(Some(serde_json::Value::Number(n))) => n.as_i64(),
            Ok(Some(serde_json::Value::String(s))) => s.trim().parse().ok(),
            Ok(_) | Err(MetaError::InvalidJson(_)) => None,
            Err(MetaError::Database(e)) => return Err(e),
        })
    };
    let set = async |name: &str, value: serde_json::Value| -> Result<(), Error> {
        match set_option(db, name, &value).await {
            Ok(()) => Ok(()),
            Err(MetaError::Database(e)) => Err(e),
            // The value is JSON already; only the database can refuse it.
            Err(e @ MetaError::InvalidJson(_)) => {
                log::error!("Could not set the {name} option: {e}");
                Ok(())
            }
        }
    };
    if post.post_type == "page" {
        if option_id("page_on_front").await? == Some(post.id) {
            set("show_on_front", json!("posts")).await?;
            set("page_on_front", json!(0)).await?;
        }
        if option_id("page_for_posts").await? == Some(post.id) {
            set("page_for_posts", json!(0)).await?;
        }
    }
    let sticky = match get_option(db, "sticky_posts").await {
        Ok(Some(serde_json::Value::Array(sticky))) => Some(sticky),
        // Unset, or not a list (nor JSON): nothing to unstick.
        Ok(_) | Err(MetaError::InvalidJson(_)) => None,
        Err(MetaError::Database(e)) => return Err(e),
    };
    if let Some(sticky) = sticky {
        let is_post = |v: &serde_json::Value| match v {
            serde_json::Value::Number(n) => n.as_i64() == Some(post.id),
            serde_json::Value::String(s) => s.trim().parse::<i64>().ok() == Some(post.id),
            _ => false,
        };
        let kept: Vec<serde_json::Value> = sticky.iter().filter(|v| !is_post(v)).cloned().collect();
        if kept.len() != sticky.len() {
            set("sticky_posts", serde_json::Value::Array(kept)).await?;
        }
    }
    match crate::scheduler::unschedule(db, super::PUBLISH_FUTURE_POST, &post.id).await {
        Ok(_) => Ok(()),
        Err(crate::scheduler::Error::Database(e)) => Err(e),
        // An id always serialises; nothing else can fail.
        Err(e) => {
            log::error!("Could not unschedule the deleted post {}: {e}", post.id);
            Ok(())
        }
    }
}

/// Sets `stored`'s status, slug and `modified_at` to `next`'s, if the row still has `stored`'s
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
                ("slug", next.slug.clone().into()),
                ("modified_at", next.modified_at.into()),
            ],
        )
        .await?;
    if matched == 0 {
        return Ok(false);
    }
    recount_post_terms(tx, stored.id).await?;
    crate::theme::auto_add_page_in(tx, next, Some(stored.status)).await?;
    Ok(true)
}

/// Settles `post`'s slug with [`settle_slug`] once its row is written, on the save's
/// transaction, and writes it when it differs from the one written.
pub(super) async fn write_settled_slug(tx: &mut Transaction, post: &mut Post) -> Result<(), Error> {
    let slug = settle_slug(tx, post, post.id).await?;
    if slug != post.slug {
        Post::query()
            .where_eq(Post::PRIMARY_KEY, post.id)
            .update(&mut *tx, &[("slug", slug.clone().into())])
            .await?;
        post.slug = slug;
    }
    Ok(())
}

/// The `modified_at` for a write to a row whose stored `modified_at` is `stored`: now, or one
/// second after `stored` when now is not later. Every write to a post row uses it, so the stamp
/// only moves forward. The column has one-second resolution, so without the extra second two
/// writes in the same second would leave the same stamp, and an editor holding it as the
/// expected value of [`save_revision`](super::save_revision) would overwrite the second write
/// unnoticed. Under rapid writes the stamp can run a few seconds ahead of the clock.
pub(crate) fn next_modified_at(stored: NaiveDateTime) -> NaiveDateTime {
    now().max(stored + Duration::seconds(1))
}

/// How far ahead a `future` post's date must be to stay `future`: WordPress's
/// `MINUTE_IN_SECONDS`, so a post scheduled less than a minute ahead is published at once.
const FUTURE_GRACE: Duration = Duration::seconds(60);

/// The status to save a post of `post_type` with, as `wp_insert_post` decides it: a `future`
/// post whose date is less than a minute ahead, or past, or that has no date, is saved as
/// `publish`. So a save of a stale copy does not put back `future` on a post the scheduler has
/// already published, which would never be published again. Attachments are left as they are,
/// as WordPress never applies the rule to them. Any other status is kept.
///
/// WordPress also saves a `publish` post dated a minute or more ahead as `future`; that half is
/// not applied here, since nothing would then schedule its publication.
pub(super) fn due_status(
    post_type: &str,
    status: PostStatus,
    published_at: Option<NaiveDateTime>,
) -> PostStatus {
    if post_type == "attachment" {
        return status;
    }
    match (status, published_at) {
        (PostStatus::Future, Some(date)) if date - now() >= FUTURE_GRACE => PostStatus::Future,
        (PostStatus::Future, _) => PostStatus::Publish,
        (other, _) => other,
    }
}

/// The current time in UTC, to the second: what MySQL's `DATETIME` and PostgreSQL's
/// `TIMESTAMP(0)` store, so the value read back equals the one written.
pub(crate) fn now() -> NaiveDateTime {
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
