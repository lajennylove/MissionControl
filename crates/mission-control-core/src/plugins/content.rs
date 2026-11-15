//! The content API behind the WIT interfaces `content-read` and `content-write`: posts and post
//! meta, read and written with the acting user's capabilities (design section 4.2), through the
//! same functions and checks the REST API uses, never raw SQL.
//!
//! Each call checks what WordPress's `map_meta_cap` asks for the object, with the post's status
//! before trashing and the site's special pages, as the REST controllers check it:
//!
//! - Reading a post: a published post of a public type, or one `read_post` allows; an
//!   attachment (`inherit`) is readable when its parent is, up a chain of at most 16 posts, or
//!   when it has no parent, as the REST API's `check_read_permission`. A revision (an autosave
//!   too) needs `edit_post` on its parent, as the REST API's revisions controller. A post with a
//!   password, or an `inherit` post whose parent has one, is read without its content and
//!   excerpt unless the user may edit the post that has the password.
//! - Reading post meta: the post must be readable; a protected key (one starting with `_`) also
//!   needs `edit_post_meta`, granted by name (WordPress's PHP plugins read any key; a sandboxed
//!   plugin may hold secrets of other plugins there, so the key is kept as the REST API keeps
//!   it, hidden).
//! - Writing post meta: `add_post_meta` when the post has no value for the key, `edit_post_meta`
//!   otherwise, and `delete_post_meta` to delete, each with the key (a protected key needs the
//!   capability by name).
//! - Changing a post: `edit_post`; the title, content and excerpt sent go through
//!   [`kses_post_fields`] for the user, and the save records a revision
//!   ([`save_revision`]).
//!
//! A post type's own capability names are not looked up in the type registry here: a check
//! falls back to [`PostTypeCaps::for_post_type`](crate::users::PostTypeCaps::for_post_type),
//! which gives `post` and `page` their names and asks `edit_others_posts` for any other type,
//! the stricter outcome.

use std::fmt;

use chrono::NaiveDateTime;
use orbit_orm::{Database, Model};
use serde_json::Value;

use crate::content::{
    MetaError, Post, PostFields, PostStatus, SaveError, delete_post_meta, get_post_meta,
    kses_post_fields, save_revision, set_post_meta,
};
use crate::rest::id_of;
use crate::rest::meta::MAX_META_VALUE_BYTES;
use crate::users::{Object, PostContext, SitePages, UserCaps, can};

/// The longest meta key a plugin may name, in bytes: the size of WordPress's `meta_key` column.
pub const MAX_META_KEY_BYTES: usize = 255;

/// The longest title, content or excerpt a plugin may send, in bytes (4 MiB). A user without
/// `unfiltered_html` is held to the smaller bound of kses
/// ([`MAX_FILTERED_BYTES`](crate::content::MAX_FILTERED_BYTES)).
pub const MAX_POST_FIELD_BYTES: usize = 4 * 1024 * 1024;

/// How far up a chain of `inherit` posts reading follows parents.
const MAX_INHERIT_DEPTH: usize = 16;

/// The post meta that holds a trashed post's status before trashing.
const TRASH_META_STATUS: &str = "_wp_trash_meta_status";

/// Why a content call was refused: the cases of the WIT `error` variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContentError {
    /// The acting user may not do this.
    Forbidden(String),
    /// There is no such post.
    NotFound(String),
    /// An argument is not valid.
    Invalid(String),
    /// The site failed; the details are logged.
    Failed(String),
}

impl fmt::Display for ContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forbidden(m) | Self::NotFound(m) | Self::Invalid(m) | Self::Failed(m) => {
                f.write_str(m)
            }
        }
    }
}

/// A database failure: logged, and given to the plugin without its details.
fn failed(error: &dyn fmt::Display) -> ContentError {
    log::error!("a plugin's content call failed: {error}");
    ContentError::Failed("the site could not complete the call".to_owned())
}

impl From<orbit_orm::Error> for ContentError {
    fn from(error: orbit_orm::Error) -> Self {
        failed(&error)
    }
}

impl From<MetaError> for ContentError {
    fn from(error: MetaError) -> Self {
        failed(&error)
    }
}

/// A post as a plugin reads it.
#[derive(Debug, Clone)]
pub(crate) struct PostView {
    /// The post, with its content and excerpt emptied when `password_protected`.
    pub(crate) post: Post,
    /// Whether the post has a password the user may not bypass.
    pub(crate) password_protected: bool,
}

/// The changes a plugin asks for in `update-post`.
#[derive(Debug, Clone, Default)]
pub(crate) struct PostChanges {
    pub(crate) title: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) excerpt: Option<String>,
}

/// What a capability check reads beside a post's row.
struct Facts {
    trash: Option<PostStatus>,
    pages: SitePages,
}

impl Facts {
    async fn load(db: &Database, post: &Post) -> Result<Self, ContentError> {
        Ok(Self {
            trash: trash_status(db, post).await?,
            pages: site_pages(db).await?,
        })
    }

    /// Whether `user` may do the meta capability `cap` on `post`, for meta `key` when given.
    fn can(&self, user: &UserCaps, cap: &str, post: &Post, key: Option<&str>) -> bool {
        let context = PostContext {
            post,
            trash_meta_status: self.trash,
            parent: None,
            site_pages: Some(self.pages),
            post_type_caps: None,
        };
        let object = match key {
            Some(key) => Object::PostMeta { post: context, key },
            None => Object::PostWithContext(context),
        };
        can(user, cap, Some(&object))
    }
}

/// The status `post` had before it was trashed, for a trashed post.
async fn trash_status(db: &Database, post: &Post) -> Result<Option<PostStatus>, ContentError> {
    if post.status != PostStatus::Trash {
        return Ok(None);
    }
    match get_post_meta(db, post.id, TRASH_META_STATUS).await {
        Ok(Some(Value::String(s))) => Ok(s.parse().ok()),
        Ok(_) | Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The site's special pages, from their options. A value that is not an id counts as unset.
async fn site_pages(db: &Database) -> Result<SitePages, ContentError> {
    async fn page(db: &Database, name: &str) -> Result<Option<i64>, ContentError> {
        match get_option_lenient(db, name).await? {
            Some(value) => Ok(id_of(&value)),
            None => Ok(None),
        }
    }
    Ok(SitePages {
        page_on_front: page(db, "page_on_front").await?,
        page_for_posts: page(db, "page_for_posts").await?,
        privacy_policy: page(db, "wp_page_for_privacy_policy").await?,
    })
}

/// An option, or `None` when it is unset or not valid JSON.
async fn get_option_lenient(db: &Database, name: &str) -> Result<Option<Value>, ContentError> {
    match crate::content::get_option(db, name).await {
        Ok(value) => Ok(value),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Post `id`, or [`ContentError::NotFound`].
async fn find(db: &Database, id: i64) -> Result<Post, ContentError> {
    Post::query()
        .find(db, id)
        .await?
        .ok_or_else(|| ContentError::NotFound(format!("there is no post with the id {id}")))
}

/// Post types whose posts are not public content, whatever their status: menu items, templates,
/// global styles, navigation, changesets, the oEmbed cache, personal data requests and fonts. A
/// published one is read only with `read_post` (which [`PostTypeCaps::for_post_type`] maps to
/// `edit_theme_options`, `customize` or `edit_others_posts`), as WordPress keeps them out of the
/// REST API's public routes. Revisions have their own rule ([`readable`]).
///
/// [`PostTypeCaps::for_post_type`]: crate::users::PostTypeCaps::for_post_type
const INTERNAL_TYPES: [&str; 10] = [
    "nav_menu_item",
    "wp_template",
    "wp_template_part",
    "wp_global_styles",
    "wp_navigation",
    "customize_changeset",
    "oembed_cache",
    "user_request",
    "wp_font_family",
    "wp_font_face",
];

/// The parent of `post`, when it has one that exists and is not itself.
async fn parent_of(db: &Database, post: &Post) -> Result<Option<Post>, ContentError> {
    match post.parent_id.filter(|p| *p > 0 && *p != post.id) {
        Some(parent_id) => Ok(Post::query().find(db, parent_id).await?),
        None => Ok(None),
    }
}

/// Whether `user` may read `post`.
///
/// - A revision (an autosave too) needs `edit_post` on its parent, as
///   `WP_REST_Revisions_Controller::get_item_permissions_check`; a revision without a parent is
///   not readable.
/// - Any other post follows the REST API's `check_read_permission`: a published post of a type
///   that is not internal ([`INTERNAL_TYPES`]), or one `read_post` allows; an `inherit` post (an
///   attachment) is readable when its parent is, up a chain of at most 16 posts, or when it has
///   no parent (or the parent is gone).
async fn readable(
    db: &Database,
    user: &UserCaps,
    post: &Post,
    facts: &Facts,
) -> Result<bool, ContentError> {
    if post.post_type == "revision" {
        let Some(parent) = parent_of(db, post).await? else {
            return Ok(false);
        };
        let parent_facts = Facts::load(db, &parent).await?;
        return Ok(parent_facts.can(user, "edit_post", &parent, None));
    }
    let mut current = post.clone();
    let mut trash = facts.trash;
    for _ in 0..MAX_INHERIT_DEPTH {
        let current_facts = Facts {
            trash,
            pages: facts.pages,
        };
        let public = current.status == PostStatus::Publish
            && !INTERNAL_TYPES.contains(&current.post_type.as_str());
        if public || current_facts.can(user, "read_post", &current, None) {
            return Ok(true);
        }
        if current.status != PostStatus::Inherit {
            return Ok(false);
        }
        match parent_of(db, &current).await? {
            Some(parent) => {
                trash = trash_status(db, &parent).await?;
                current = parent;
            }
            None => return Ok(true),
        }
    }
    Ok(false)
}

/// Whether `post` is protected by a password `user` may not bypass: its own, or, for an
/// `inherit` post (a revision, an attachment), the first password up its chain of parents.
/// `user` bypasses a password on a post they may edit. A chain longer than 16 posts counts as
/// protected.
async fn password_protected(
    db: &Database,
    user: &UserCaps,
    post: &Post,
    facts: &Facts,
) -> Result<bool, ContentError> {
    let mut current = post.clone();
    let mut trash = facts.trash;
    for _ in 0..MAX_INHERIT_DEPTH {
        if current.password.as_deref().is_some_and(|p| !p.is_empty()) {
            let current_facts = Facts {
                trash,
                pages: facts.pages,
            };
            return Ok(!current_facts.can(user, "edit_post", &current, None));
        }
        if current.status != PostStatus::Inherit && current.post_type != "revision" {
            return Ok(false);
        }
        match parent_of(db, &current).await? {
            Some(parent) => {
                trash = trash_status(db, &parent).await?;
                current = parent;
            }
            None => return Ok(false),
        }
    }
    Ok(true)
}

fn forbidden(what: &str) -> ContentError {
    ContentError::Forbidden(format!("the acting user may not {what}"))
}

/// Refuses a meta key that is empty or too long.
fn check_key(key: &str) -> Result<(), ContentError> {
    if key.is_empty() {
        return Err(ContentError::Invalid("the meta key is empty".to_owned()));
    }
    if key.len() > MAX_META_KEY_BYTES {
        return Err(ContentError::Invalid(format!(
            "the meta key is {} bytes; at most {MAX_META_KEY_BYTES} are allowed",
            key.len()
        )));
    }
    Ok(())
}

/// Post `id` as `user` may read it (`get-post`).
pub(crate) async fn read_post(
    db: &Database,
    user: &UserCaps,
    id: i64,
) -> Result<PostView, ContentError> {
    let mut post = find(db, id).await?;
    let facts = Facts::load(db, &post).await?;
    if !readable(db, user, &post, &facts).await? {
        return Err(forbidden("read this post"));
    }
    let password_protected = password_protected(db, user, &post, &facts).await?;
    if password_protected {
        post.content.clear();
        post.excerpt.clear();
    }
    Ok(PostView {
        post,
        password_protected,
    })
}

/// The value of post meta `key` as JSON text (`get-post-meta`).
pub(crate) async fn read_post_meta(
    db: &Database,
    user: &UserCaps,
    post_id: i64,
    key: &str,
) -> Result<Option<String>, ContentError> {
    check_key(key)?;
    let post = find(db, post_id).await?;
    let facts = Facts::load(db, &post).await?;
    if !readable(db, user, &post, &facts).await? {
        return Err(forbidden("read this post"));
    }
    if crate::users::meta_cap::is_protected_meta(key)
        && !facts.can(user, "edit_post_meta", &post, Some(key))
    {
        return Err(forbidden("read this protected meta key"));
    }
    Ok(get_post_meta(db, post_id, key)
        .await?
        .map(|value| value.to_string()))
}

/// Sets post meta `key` to `value_json` (`set-post-meta`).
pub(crate) async fn write_post_meta(
    db: &Database,
    user: &UserCaps,
    post_id: i64,
    key: &str,
    value_json: &str,
) -> Result<(), ContentError> {
    check_key(key)?;
    if value_json.len() > MAX_META_VALUE_BYTES {
        return Err(ContentError::Invalid(format!(
            "the meta value is {} bytes; at most {MAX_META_VALUE_BYTES} are allowed",
            value_json.len()
        )));
    }
    let value: Value = serde_json::from_str(value_json)
        .map_err(|e| ContentError::Invalid(format!("the meta value is not valid JSON: {e}")))?;
    let post = find(db, post_id).await?;
    let facts = Facts::load(db, &post).await?;
    let exists = get_post_meta(db, post_id, key).await?.is_some();
    let cap = if exists {
        "edit_post_meta"
    } else {
        "add_post_meta"
    };
    if !facts.can(user, cap, &post, Some(key)) {
        return Err(forbidden(&format!("set this meta key ({cap})")));
    }
    set_post_meta(db, post_id, key, &value).await?;
    Ok(())
}

/// Deletes post meta `key` (`delete-post-meta`); true when the post had it.
pub(crate) async fn remove_post_meta(
    db: &Database,
    user: &UserCaps,
    post_id: i64,
    key: &str,
) -> Result<bool, ContentError> {
    check_key(key)?;
    let post = find(db, post_id).await?;
    let facts = Facts::load(db, &post).await?;
    if !facts.can(user, "delete_post_meta", &post, Some(key)) {
        return Err(forbidden("delete this meta key (delete_post_meta)"));
    }
    Ok(delete_post_meta(db, post_id, key).await?)
}

/// Changes post `id` as an editor's save does (`update-post`).
pub(crate) async fn update_post(
    db: &Database,
    user: &UserCaps,
    id: i64,
    changes: PostChanges,
) -> Result<(), ContentError> {
    for (field, value) in [
        ("title", &changes.title),
        ("content", &changes.content),
        ("excerpt", &changes.excerpt),
    ] {
        if let Some(value) = value
            && value.len() > MAX_POST_FIELD_BYTES
        {
            return Err(ContentError::Invalid(format!(
                "the {field} is {} bytes; at most {MAX_POST_FIELD_BYTES} are allowed",
                value.len()
            )));
        }
    }
    let mut post = find(db, id).await?;
    if post.post_type == "revision" {
        return Err(ContentError::Invalid(
            "a revision cannot be changed".to_owned(),
        ));
    }
    let facts = Facts::load(db, &post).await?;
    if !facts.can(user, "edit_post", &post, None) {
        return Err(forbidden("edit this post (edit_post)"));
    }
    // Only the fields sent are filtered; the others keep what is stored.
    let filtered = kses_post_fields(
        user,
        PostFields {
            title: changes.title.clone().unwrap_or_default(),
            content: changes.content.clone().unwrap_or_default(),
            excerpt: changes.excerpt.clone().unwrap_or_default(),
        },
    )
    .map_err(|e| ContentError::Invalid(e.to_string()))?;
    if changes.title.is_some() {
        post.title = filtered.title;
    }
    if changes.content.is_some() {
        post.content = filtered.content;
    }
    if changes.excerpt.is_some() {
        post.excerpt = filtered.excerpt;
    }
    let expected: NaiveDateTime = post.modified_at;
    match save_revision(db, &post, expected).await {
        Ok(_) => Ok(()),
        Err(SaveError::Conflict { .. }) => Err(ContentError::Failed(
            "the post was saved by someone else meanwhile; read it again and retry".to_owned(),
        )),
        Err(SaveError::NotFound { id }) => Err(ContentError::NotFound(format!(
            "there is no post with the id {id}"
        ))),
        Err(SaveError::IsRevision { .. }) => Err(ContentError::Invalid(
            "a revision cannot be changed".to_owned(),
        )),
        Err(e @ SaveError::Database(_)) => Err(failed(&e)),
    }
}
