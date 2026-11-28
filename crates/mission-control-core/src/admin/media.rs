//! The admin's media library (Task 6.4, design 5.5, Rulings R127 to R129): the screen of the
//! `attachment` post type, what the editor's Add Media picker lists and uploads, and the media
//! library that Ignite's Image, File and Gallery fields pick from. Nothing here needs Rocket:
//! the `mission-control` crate mounts the screen and the upload and query endpoints and calls
//! these functions.
//!
//! # The screen (Ruling R127)
//!
//! [`attachment_resource`] is the Ignite runtime resource of `attachment`, at
//! `<mount>/attachment`, read and written through [`AttachmentData`]:
//!
//! - The list, WordPress's `upload.php` in its list mode: every attachment that is `inherit` or
//!   `private` (attachments have no trash, as WordPress without `MEDIA_TRASH`), newest first,
//!   with the title, the file name, the MIME type, the author and the upload date. The search
//!   matches the title, the caption, the description and the file name (`_wp_attached_file`),
//!   as `WP_Query::parse_search` and `wp_query_attachment_filenames` do. Ignite has no grid view;
//!   the grid is a follow-up.
//! - The form, WordPress's attachment details: the title, the alternative text
//!   (`_wp_attachment_image_alt`, its tags stripped as `edit_post` strips them), the caption (the
//!   excerpt) and the description (the content). Saved with
//!   [`update_attachment_details`].
//! - Delete removes the attachment for good, with its file and its sub-sizes
//!   ([`delete_attachment`], what the REST API's `force=true` delete runs).
//! - Ignite's create is refused: a file is added by uploading it (the `mission-control` crate
//!   answers `<mount>/attachment/new` with an upload page, Ruling R129).
//!
//! Who may use it, as WordPress decides: the list (`upload.php`) and the upload page
//! (`media-new.php`) need `upload_files`; editing or deleting an attachment needs `edit_post`
//! or `delete_post` on it (`map_meta_cap`, with the `post` capabilities: an author edits and
//! deletes their own files, not others'). [`AttachmentPolicy`] decides each action and each
//! row's Edit and Delete, and the admin's fairing checks the same in the URL. A save for a user
//! without `unfiltered_html` runs kses on the title, caption and description, as `edit_post`
//! does (`kses_init_filters`); the list leaves out private attachments for a user without
//! `read_private_posts` (`wp_edit_attachments_query_vars`).
//!
//! # The picker and uploads (Ruling R128)
//!
//! - [`query_attachments`] is WordPress's `wp_ajax_query_attachments`: a page of attachments
//!   the user may read (`inherit`, and `private` with `read_private_posts`), newest first,
//!   searched as the screen searches, each shaped as `wp_prepare_attachment_for_js` shapes it
//!   ([`prepare_attachment_for_js`]).
//! - [`upload_to_library`] is `async-upload.php` (and `media-new.php`'s browser uploader): it
//!   needs `upload_files`, and `edit_post` on the post the file is attached to, then stores the
//!   file with its sub-sizes ([`create_attachment`], titled from the client's file name as
//!   `media_handle_upload` titles it).
//!
//! # Ignite's media fields
//!
//! [`Attachments`] is the [`MediaLibrary`] the panel's Image, File and Gallery fields use: a
//! field shows the stored attachment's title and address, and refuses an id that is not an
//! attachment.
//!
//! Every query is bounded: a page holds at most [`MAX_PER_PAGE`] attachments, a search at most
//! [`MAX_SEARCH_BYTES`], a lookup of ids at most [`MAX_LOOKUP_IDS`]. Values are always bound,
//! never written into the SQL.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use chrono::{Duration, FixedOffset, NaiveDateTime};
use ignite_core::{
    ActingUser, ColumnDef, Direction, FieldDef as IgniteFieldDef, FieldKindDef, ListQuery,
    MediaItem, MediaLibrary, Page, Policy, Row, RuntimeData, RuntimeError, RuntimeFuture,
    RuntimeResource,
};
use orbit_orm::{Database, Error, Model, Value as Sql, raw};
use serde_json::{Map, Value, json};

use super::posts::{PostScreen, authorize_post_screen, site_pages, trash_status};
use crate::content::{MetaError, Post, PostFields, PostStatus, get_option};
use crate::front::{escape_like, search_terms, site_address};
use crate::media::{
    Attachment, AttachmentError, AttachmentMetadata, ImageSizes, Storage, UploadError,
    ValidatedUpload, create_attachment, delete_attachment, get_attachment,
    update_attachment_details,
};
use crate::rest::media::UPLOADS_PATH;
use crate::text::strip_all_tags;
use crate::types::{Access, PostTypeDef};
use crate::users::{Object, PostContext, PostTypeCaps, UserCaps, can, has_cap};

/// The post type of the media library, and the slug of its screen.
pub const ATTACHMENT: &str = "attachment";

/// The most attachments one page of the screen or of the picker holds.
pub const MAX_PER_PAGE: i64 = 100;

/// How many attachments a page of the picker holds when it does not ask (WordPress's media
/// modal asks for 40).
pub const DEFAULT_QUERY_PER_PAGE: i64 = 40;

/// The longest search read, in bytes; the rest is ignored.
pub const MAX_SEARCH_BYTES: usize = 1000;

/// The most ids a media field lookup ([`Attachments`]) reads; later ids are reported missing.
pub const MAX_LOOKUP_IDS: usize = 1000;

/// The capability that lets a user upload (WordPress's `upload_files`).
pub const UPLOAD_FILES: &str = "upload_files";

/// The post meta that holds an attachment's storage path.
const ATTACHED_FILE: &str = "_wp_attached_file";
/// The post meta that holds an image's metadata.
const METADATA: &str = "_wp_attachment_metadata";
/// The post meta that holds the alt text.
const ALT: &str = "_wp_attachment_image_alt";

/// The form's fields.
const TITLE: &str = "title";
const ALT_FIELD: &str = "alt";
const CAPTION: &str = "caption";
const DESCRIPTION: &str = "description";

/// The list's columns. `col_thumb` holds the address of the attachment's thumbnail (empty for a
/// file that is not an image); the `mission-control` crate shows it as the image.
pub const COL_THUMB: &str = "col_thumb";
const COL_TITLE: &str = "col_title";
const COL_FILE: &str = "col_file";
const COL_MIME: &str = "col_mime";
const COL_AUTHOR: &str = "col_author";
const COL_DATE: &str = "col_date";

/// The sizes the picker offers, in WordPress's order (`image_size_names_choose`); `full` is the
/// original.
pub const CHOOSABLE_SIZES: [&str; 4] = ["thumbnail", "medium", "large", "full"];

/// `attachment` as a post type definition, as WordPress registers it: the labels "Media", the
/// `post` capability type (so its screens need the posts' capabilities), title and author.
/// It is not in the [`Registry`](crate::types::Registry), which reserves its name.
pub fn attachment_post_type() -> PostTypeDef {
    PostTypeDef {
        show_ui: Some(true),
        show_in_rest: true,
        rest_base: Some("media".to_owned()),
        ..PostTypeDef::new(ATTACHMENT, "Media", "Media")
    }
}

// ---------------------------------------------------------------------------------------------
// The screen

/// The rows of the media library screen: the attachments in `posts` and their meta. See the
/// [module](self) for what it reads and writes, and why it must only be served behind the
/// admin's authorization.
pub struct AttachmentData {
    storage: Arc<dyn Storage>,
    access: Access,
}

impl fmt::Debug for AttachmentData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachmentData").finish_non_exhaustive()
    }
}

impl AttachmentData {
    /// The rows of the attachments whose files are in `storage`, which Delete removes them from,
    /// for users whose capabilities and kses `access` gives.
    pub fn new(storage: Arc<dyn Storage>, access: Access) -> Self {
        Self { storage, access }
    }
}

/// The resource of the media library screen at `<mount>/attachment`: the form's title,
/// alternative text, caption and description, and the list's title, file, type, author and
/// date; see the [module](self). `access` reads the acting user's capabilities and runs kses.
pub fn attachment_resource(storage: Arc<dyn Storage>, access: Access) -> RuntimeResource {
    let field = |name: &str, label: &str, kind: FieldKindDef| IgniteFieldDef {
        name: name.to_owned(),
        label: label.to_owned(),
        kind,
        required: false,
    };
    let column = |key: &str, label: &str, sortable: bool, searchable: bool| ColumnDef {
        key: key.to_owned(),
        label: label.to_owned(),
        sortable,
        searchable,
    };
    let mut resource = RuntimeResource::new(
        ATTACHMENT,
        "Media",
        "Media",
        AttachmentData::new(storage, access),
    );
    resource.fields = vec![
        field(TITLE, "Title", FieldKindDef::Text),
        field(ALT_FIELD, "Alternative Text", FieldKindDef::Text),
        field(CAPTION, "Caption", FieldKindDef::Textarea),
        field(DESCRIPTION, "Description", FieldKindDef::Textarea),
    ];
    resource.columns = vec![
        column(COL_THUMB, "Thumbnail", false, false),
        column(COL_TITLE, "Title", true, true),
        column(COL_FILE, "File", false, false),
        column(COL_MIME, "Type", false, false),
        column(COL_AUTHOR, "Author", false, false),
        column(COL_DATE, "Date", true, false),
    ];
    super::navigation::place(
        &mut resource,
        super::navigation::MEDIA,
        super::navigation::MEDIA_ICON,
    );
    // The upload page in the menu's group, as WordPress's Media > Add New Media File.
    resource.navigation_create_item = true;
    resource.navigation_create_label = Some(super::navigation::CREATE_LABEL.to_owned());
    // `upload.php` and `media-new.php` need `upload_files`; editing and deleting a file are the
    // post's `edit_post` and `delete_post` on it ([`AttachmentPolicy`]), which need at least
    // `edit_posts` and `delete_posts` (an author's own files).
    super::ScreenCaps {
        view: UPLOAD_FILES,
        create: UPLOAD_FILES,
        update: "edit_posts",
        delete: "delete_posts",
    }
    .apply(resource)
}

/// Registers [`attachment_resource`] on Ignite's runtime registry.
///
/// # Errors
///
/// Ignite's refusal when another resource or a panel page has the slug `attachment`.
pub fn register_attachment_resource(
    storage: Arc<dyn Storage>,
    access: Access,
) -> Result<(), RuntimeError> {
    ignite_core::register_runtime(attachment_resource(storage, access.clone()))?;
    ignite_core::register_policy(ATTACHMENT, AttachmentPolicy::new(access));
    Ok(())
}

/// The policy of the media library's screen, as WordPress decides for attachments: the list
/// and the upload page need `upload_files` (Ignite's abilities check it first, so they are
/// allowed here); seeing, editing and saving an attachment need `edit_post` on it, and Delete
/// `delete_post` (`map_meta_cap`, with the `post` capabilities).
pub struct AttachmentPolicy {
    access: Access,
}

impl AttachmentPolicy {
    /// The policy, reading users' capabilities through `access`.
    pub fn new(access: Access) -> Self {
        Self { access }
    }

    /// Whether `user` may make the request `screen` on the attachment `record`.
    async fn allows(
        &self,
        db: &Database,
        user: &ActingUser,
        record: &Row,
        screen: fn(i64) -> PostScreen,
    ) -> Result<bool, Error> {
        // A record without an id is no attachment: refused.
        let Some(id) = record.get("id").and_then(Value::as_i64) else {
            return Ok(false);
        };
        let caps = self.access.caps(db, user).await?;
        Ok(
            authorize_post_screen(db, &caps, &attachment_post_type(), screen(id))
                .await?
                .is_none(),
        )
    }
}

impl Policy for AttachmentPolicy {
    fn view<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(self.allows(db, user, record, PostScreen::Edit))
    }

    fn update<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(self.allows(db, user, record, PostScreen::Update))
    }

    fn delete<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(self.allows(db, user, record, PostScreen::Delete))
    }
}

/// Attachment `id`, when there is one with the status the screen lists.
async fn listed_attachment(db: &Database, id: i64) -> Result<Option<Attachment>, Error> {
    let Some(post) = Post::query().find(db, id).await? else {
        return Ok(None);
    };
    if post.post_type != ATTACHMENT
        || !matches!(post.status, PostStatus::Inherit | PostStatus::Private)
    {
        return Ok(None);
    }
    get_attachment(db, id).await.map_err(attachment_error)
}

/// The database error of an attachment call; the others cannot happen on a read or an update
/// of an existing attachment, and are reported as the database's.
fn attachment_error(e: AttachmentError) -> Error {
    match e {
        AttachmentError::Database(e) => e,
        AttachmentError::NotFound(_) => Error::NotFound { table: "posts" },
        other => Error::Database(sqlx::Error::Protocol(other.to_string())),
    }
}

/// A UTC time as the site's local time.
fn local_text(utc: NaiveDateTime, offset: FixedOffset) -> String {
    (utc + Duration::seconds(i64::from(offset.local_minus_utc())))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// The file name of a storage path: `photo.jpg` for `2026/11/photo.jpg`.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The title of an upload with no title: its file name without the extension, as
/// `media_handle_upload` makes it (`photo` for `2026/11/photo.jpg`, `archive.tar` for
/// `archive.tar.gz`). A name that is only an extension (`.htaccess`) is kept whole.
fn file_title(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// Question marks for `n` binds.
fn marks(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// The text of an optional row value: a string, a number written out, empty for null or a key
/// the form did not send.
fn row_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// What the list and the picker read of a page of attachments, with as few queries as the page
/// needs: the rows, their meta, their authors' names.
struct Loaded {
    posts: Vec<Post>,
    meta: HashMap<(i64, &'static str), Value>,
    authors: HashMap<i64, String>,
}

impl Loaded {
    /// The attachments `ids`, in that order (missing ones left out).
    async fn load(db: &Database, ids: &[i64]) -> Result<Self, Error> {
        if ids.is_empty() {
            return Ok(Self {
                posts: Vec::new(),
                meta: HashMap::new(),
                authors: HashMap::new(),
            });
        }
        let mut posts = Post::query()
            .where_in(Post::PRIMARY_KEY, ids.to_vec())
            .get(db)
            .await?;
        posts.retain(|post| post.post_type == ATTACHMENT);
        posts.sort_by_key(|post| ids.iter().position(|id| *id == post.id));
        let mut meta = HashMap::new();
        let mut authors = HashMap::new();
        for chunk in ids.chunks(500) {
            let mut binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
            binds.extend([ATTACHED_FILE, METADATA, ALT].map(Sql::from));
            let rows: Vec<(i64, String, String)> = raw::fetch_all(
                db,
                &format!(
                    "SELECT post_id, meta_key, meta_value FROM post_meta WHERE post_id IN ({}) \
                     AND meta_key IN (?, ?, ?) ORDER BY id",
                    marks(chunk.len())
                ),
                &binds,
            )
            .await?;
            for (post_id, key, value) in rows {
                // Exact comparison: MySQL compares text without case. The first row of a key
                // wins, as `get_post_meta` reads it.
                let Some(key) = [ATTACHED_FILE, METADATA, ALT]
                    .into_iter()
                    .find(|k| *k == key)
                else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&value) else {
                    continue;
                };
                meta.entry((post_id, key)).or_insert(value);
            }
        }
        let mut author_ids: Vec<i64> = posts.iter().map(|p| p.author_id).collect();
        author_ids.sort_unstable();
        author_ids.dedup();
        for chunk in author_ids.chunks(500) {
            let rows: Vec<(i64, String)> = raw::fetch_all(
                db,
                &format!(
                    "SELECT id, name FROM users WHERE id IN ({})",
                    marks(chunk.len())
                ),
                &chunk.iter().map(|id| Sql::from(*id)).collect::<Vec<_>>(),
            )
            .await?;
            authors.extend(rows);
        }
        Ok(Self {
            posts,
            meta,
            authors,
        })
    }

    /// A meta value of `id` as text: a string as it is, empty otherwise.
    fn text(&self, id: i64, key: &'static str) -> String {
        match self.meta.get(&(id, key)) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        }
    }

    /// The image metadata of `id`, when it reads.
    fn metadata(&self, id: i64) -> Option<AttachmentMetadata> {
        serde_json::from_value(self.meta.get(&(id, METADATA))?.clone()).ok()
    }

    /// `post` as an [`Attachment`].
    fn attachment(&self, post: &Post) -> Attachment {
        Attachment {
            id: post.id,
            author_id: post.author_id,
            parent_id: post.parent_id,
            title: post.title.clone(),
            alt: self.text(post.id, ALT),
            caption: post.excerpt.clone(),
            description: post.content.clone(),
            file: self.text(post.id, ATTACHED_FILE),
            mime_type: post.mime_type.clone(),
            metadata: self.metadata(post.id),
        }
    }
}

/// The `FROM ... WHERE` of the attachments with `statuses` matching `search`, as
/// `WP_Query::parse_search` splits it, on the title, caption, description and file name; and an
/// optional MIME type family (`image`, `audio`, ...).
fn attachment_filter(
    statuses: &[PostStatus],
    search: Option<&str>,
    media_type: Option<&str>,
    uploaded_to: Option<i64>,
) -> (String, Vec<Sql>) {
    let mut filter = format!(
        "FROM posts p WHERE p.post_type = ? AND p.status IN ({})",
        marks(statuses.len())
    );
    let mut binds: Vec<Sql> = vec![ATTACHMENT.into()];
    binds.extend(statuses.iter().map(|s| Sql::from(*s)));
    if let Some(family) = media_type {
        filter.push_str(" AND p.mime_type LIKE ? ESCAPE '!'");
        binds.push(format!("{}/%", escape_like(family)).into());
    }
    if let Some(parent) = uploaded_to {
        filter.push_str(" AND p.parent_id = ?");
        binds.push(parent.into());
    }
    if let Some(search) = search {
        let mut cut = search.len().min(MAX_SEARCH_BYTES);
        while !search.is_char_boundary(cut) {
            cut -= 1;
        }
        for term in search_terms(&search[..cut]) {
            let like = format!("%{}%", escape_like(&term.text.to_lowercase()));
            filter.push_str(if term.exclude { " AND NOT " } else { " AND " });
            filter.push_str(
                "(LOWER(p.title) LIKE ? ESCAPE '!' OR LOWER(p.excerpt) LIKE ? ESCAPE '!' \
                 OR LOWER(p.content) LIKE ? ESCAPE '!' OR EXISTS (SELECT 1 FROM post_meta m \
                 WHERE m.post_id = p.id AND m.meta_key = ? AND LOWER(m.meta_value) LIKE ? \
                 ESCAPE '!'))",
            );
            binds.extend(std::iter::repeat_n(Sql::from(like.clone()), 3));
            binds.push(ATTACHED_FILE.into());
            binds.push(like.into());
        }
    }
    (filter, binds)
}

/// The ids of one page of attachments, and how many match.
async fn page_ids(
    db: &Database,
    filter: &str,
    binds: Vec<Sql>,
    order: &str,
    page: i64,
    per_page: i64,
) -> Result<(Vec<i64>, i64), Error> {
    let per_page = per_page.clamp(1, MAX_PER_PAGE);
    let offset = page.max(1).saturating_sub(1).saturating_mul(per_page);
    let total: i64 = raw::fetch_scalar(db, &format!("SELECT COUNT(*) {filter}"), &binds).await?;
    let mut page_binds = binds;
    page_binds.push(per_page.into());
    page_binds.push(offset.into());
    let ids: Vec<(i64,)> = raw::fetch_all(
        db,
        &format!("SELECT p.id {filter} ORDER BY {order} LIMIT ? OFFSET ?"),
        &page_binds,
    )
    .await?;
    Ok((ids.into_iter().map(|(id,)| id).collect(), total))
}

/// The upload date of an attachment: its `published_at` (set at upload), or its last change.
fn upload_date(post: &Post) -> NaiveDateTime {
    post.published_at.unwrap_or(post.modified_at)
}

impl AttachmentData {
    /// The list's column values of `post`; `base` is the uploads' address.
    fn columns(row: &mut Row, post: &Post, loaded: &Loaded, offset: FixedOffset, base: &str) {
        row.insert(
            COL_THUMB.into(),
            Value::from(thumbnail_url(base, &loaded.attachment(post))),
        );
        let title = if post.title.is_empty() {
            "(no title)".to_owned()
        } else {
            post.title.clone()
        };
        row.insert(COL_TITLE.into(), Value::from(title));
        let file = loaded.text(post.id, ATTACHED_FILE);
        row.insert(COL_FILE.into(), Value::from(file_name(&file)));
        row.insert(COL_MIME.into(), Value::from(post.mime_type.clone()));
        row.insert(
            COL_AUTHOR.into(),
            Value::from(
                loaded
                    .authors
                    .get(&post.author_id)
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        row.insert(
            COL_DATE.into(),
            Value::from(local_text(upload_date(post), offset)),
        );
    }

    /// One page of the screen's list.
    async fn list_page(
        &self,
        db: &Database,
        query: &ListQuery,
        statuses: &[PostStatus],
    ) -> Result<Page<Row>, Error> {
        let (filter, binds) = attachment_filter(statuses, query.search.as_deref(), None, None);
        let date = "COALESCE(p.published_at, p.modified_at)";
        let order = match &query.sort {
            Some((key, direction)) => {
                let dir = match direction {
                    Direction::Asc => "ASC",
                    Direction::Desc => "DESC",
                };
                match key.as_str() {
                    COL_TITLE => format!("p.title {dir}, p.id {dir}"),
                    _ => format!("{date} {dir}, p.id {dir}"),
                }
            }
            None => format!("{date} DESC, p.id DESC"),
        };
        let (ids, total) = page_ids(db, &filter, binds, &order, query.page, query.per_page).await?;
        let loaded = Loaded::load(db, &ids).await?;
        let offset = crate::content::gmt_offset(db).await?;
        let base = uploads_url(db).await?;
        let rows = loaded
            .posts
            .iter()
            .map(|post| {
                let mut row = Row::new();
                row.insert("id".into(), Value::from(post.id));
                Self::columns(&mut row, post, &loaded, offset, &base);
                row
            })
            .collect();
        Ok(Page { rows, total })
    }

    /// The form's row of attachment `id`, with its columns.
    async fn row(&self, db: &Database, id: i64) -> Result<Option<Row>, Error> {
        if listed_attachment(db, id).await?.is_none() {
            return Ok(None);
        }
        let loaded = Loaded::load(db, &[id]).await?;
        let Some(post) = loaded.posts.first() else {
            return Ok(None);
        };
        let attachment = loaded.attachment(post);
        let offset = crate::content::gmt_offset(db).await?;
        let base = uploads_url(db).await?;
        let mut row = Row::new();
        row.insert("id".into(), Value::from(id));
        row.insert(TITLE.into(), Value::from(attachment.title));
        row.insert(ALT_FIELD.into(), Value::from(attachment.alt));
        row.insert(CAPTION.into(), Value::from(attachment.caption));
        row.insert(DESCRIPTION.into(), Value::from(attachment.description));
        Self::columns(&mut row, post, &loaded, offset, &base);
        Ok(Some(row))
    }

    /// Saves the form: each field sent replaces the stored value (a cleared one is empty), each
    /// one not sent keeps it. A title left empty is the file's name without its extension, as
    /// `media_handle_upload` names an upload (`photo` for `2026/11/photo.jpg`). The alternative text is stored without tags, as `edit_post`
    /// strips them (`wp_strip_all_tags`); the other fields as sent, or through kses for `user`
    /// when they lack `unfiltered_html`.
    async fn update_row(
        &self,
        db: &Database,
        id: i64,
        input: Row,
        user: Option<&UserCaps>,
    ) -> Result<Row, Error> {
        let Some(current) = listed_attachment(db, id).await? else {
            return Err(Error::NotFound { table: "posts" });
        };
        let value = |key: &str, stored: String| input.get(key).map_or(stored, row_text);
        let mut title = value(TITLE, current.title);
        if title.trim().is_empty() {
            title = file_title(&current.file).to_owned();
        }
        let alt = strip_all_tags(&value(ALT_FIELD, current.alt))
            .trim()
            .to_owned();
        let mut caption = value(CAPTION, current.caption);
        let mut description = value(DESCRIPTION, current.description);
        if let Some(user) = user.filter(|user| !has_cap(user, "unfiltered_html")) {
            let fields = self
                .access
                .kses
                .filter(
                    user,
                    PostFields {
                        title,
                        content: description,
                        excerpt: caption,
                    },
                )
                .await
                .map_err(|e| {
                    Error::Database(sqlx::Error::Protocol(format!(
                        "the admin's kses filter refused the save: {e}"
                    )))
                })?;
            title = fields.title;
            description = fields.content;
            caption = fields.excerpt;
        }
        update_attachment_details(db, id, &title, &alt, &caption, &description)
            .await
            .map_err(attachment_error)?;
        self.row(db, id)
            .await?
            .ok_or(Error::NotFound { table: "posts" })
    }

    /// Deletes attachment `id` for good, with its files. 0 when there is none.
    async fn delete_row(&self, db: &Database, id: i64) -> Result<u64, Error> {
        if listed_attachment(db, id).await?.is_none() {
            return Ok(0);
        }
        let deleted = delete_attachment(db, &self.storage, id)
            .await
            .map_err(attachment_error)?;
        Ok(u64::from(deleted))
    }
}

impl RuntimeData for AttachmentData {
    fn list<'a>(&'a self, db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move {
            self.list_page(db, query, &[PostStatus::Inherit, PostStatus::Private])
                .await
        })
    }

    /// The list for `user`: private attachments only with `read_private_posts`, as
    /// `wp_edit_attachments_query_vars` lists them.
    fn list_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        query: &'a ListQuery,
    ) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move {
            let caps = self.access.caps(db, user).await?;
            let statuses: &[PostStatus] = if has_cap(&caps, "read_private_posts") {
                &[PostStatus::Inherit, PostStatus::Private]
            } else {
                &[PostStatus::Inherit]
            };
            self.list_page(db, query, statuses).await
        })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move { self.row(db, id).await })
    }

    /// Refuses: a file is added by uploading it (`<mount>/attachment/new`, Ruling R129). The
    /// panel shows the refusal as a server error.
    fn create<'a>(&'a self, _db: &'a Database, _input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "creating an attachment from the form (files are uploaded)",
                driver: "Mission Control's media library",
            })
        })
    }

    fn update<'a>(&'a self, db: &'a Database, id: i64, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move { self.update_row(db, id, input, None).await })
    }

    /// The save for `user` (the policy has checked `edit_post`): kses on the title, caption and
    /// description for a user without `unfiltered_html`, as `edit_post` filters them.
    fn update_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
        input: Row,
    ) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            let caps = self.access.caps(db, user).await?;
            self.update_row(db, id, input, Some(&caps)).await
        })
    }

    fn delete<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, u64> {
        Box::pin(async move { self.delete_row(db, id).await })
    }
}

// ---------------------------------------------------------------------------------------------
// Addresses

/// The uploads' base address: `{home}/mc-content/uploads`, as the REST API gives it.
async fn uploads_url(db: &Database) -> Result<String, Error> {
    let home = match get_option(db, "home").await {
        Ok(Some(Value::String(home))) => site_address(&home),
        Ok(_) | Err(MetaError::InvalidJson(_)) => String::new(),
        Err(MetaError::Database(e)) => return Err(e),
    };
    Ok(format!("{home}{UPLOADS_PATH}"))
}

/// The address of a file stored at `path` under the uploads' base `base`.
fn file_url(base: &str, path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{base}/{path}")
    }
}

/// The address of `attachment`'s thumbnail, as the list's File column shows it
/// (`WP_Media_List_Table::column_title`, a 60 by 60 `wp_get_attachment_image`): its `thumbnail`
/// sub-size, or the image itself when it has none; empty for a file that is not an image.
pub fn thumbnail_url(base: &str, attachment: &Attachment) -> String {
    if !attachment.mime_type.starts_with("image/") {
        return String::new();
    }
    if let Some(size) = attachment
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.sizes.get("thumbnail"))
    {
        let folder = attachment
            .file
            .rsplit_once('/')
            .map_or(String::new(), |(folder, _)| format!("{folder}/"));
        return file_url(base, &format!("{folder}{}", size.file));
    }
    file_url(base, &attachment.file)
}

/// WordPress's `size_format`: `bytes` in the largest unit (B, KB, MB, GB, TB, of 1024) it
/// reaches, with `decimals` decimals, as `number_format_i18n` writes them in English (`1,024`).
pub fn size_format(bytes: u64, decimals: usize) -> String {
    const UNITS: [(&str, u64); 5] = [
        ("TB", 1 << 40),
        ("GB", 1 << 30),
        ("MB", 1 << 20),
        ("KB", 1 << 10),
        ("B", 1),
    ];
    let (unit, magnitude) = UNITS
        .into_iter()
        .find(|(_, magnitude)| bytes >= *magnitude)
        .unwrap_or(("B", 1));
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / magnitude as f64;
    format!("{} {unit}", number_format(value, decimals))
}

/// WordPress's `number_format_i18n` in English: rounded half away from zero to `decimals`, the
/// thousands separated by commas.
fn number_format(value: f64, decimals: usize) -> String {
    let scale = 10f64.powi(i32::try_from(decimals).unwrap_or(0));
    let rounded = (value * scale).round() / scale;
    let text = format!("{rounded:.decimals$}");
    let (whole, fraction) = text
        .split_once('.')
        .map_or((text.as_str(), None), |(w, f)| (w, Some(f)));
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match fraction {
        Some(fraction) => format!("{grouped}.{fraction}"),
        None => grouped,
    }
}

// ---------------------------------------------------------------------------------------------
// The picker

/// `landscape` or `portrait`, as WordPress names an image's orientation.
fn orientation(width: u32, height: u32) -> &'static str {
    if height > width {
        "portrait"
    } else {
        "landscape"
    }
}

/// `attachment` shaped as WordPress's `wp_prepare_attachment_for_js` shapes it for the media
/// modal (the keys the picker reads): `id`, `title`, `filename`, `url`, `alt`, `author`,
/// `authorName`, `description`, `caption`, `name`, `status`, `uploadedTo`, `date` and `modified`
/// (milliseconds since the epoch, UTC), `dateFormatted`, `mime`, `type`, `subtype`, and for an
/// image `width`, `height`, `orientation` and `sizes`: the stored sub-sizes the media modal
/// offers ([`CHOOSABLE_SIZES`], WordPress's `image_size_names_choose`) and `full`, each with its
/// `url`, `width`, `height` and `orientation`. `base` is the uploads' address, `post`
/// the attachment's row and `author_name` its author's name.
pub fn prepare_attachment_for_js(
    base: &str,
    post: &Post,
    attachment: &Attachment,
    author_name: &str,
) -> Value {
    let (kind, subtype) = attachment
        .mime_type
        .split_once('/')
        .unwrap_or((attachment.mime_type.as_str(), ""));
    let uploaded = upload_date(post);
    let mut item = json!({
        "id": attachment.id,
        "title": attachment.title,
        "filename": file_name(&attachment.file),
        "url": file_url(base, &attachment.file),
        "alt": attachment.alt,
        "author": attachment.author_id.to_string(),
        "authorName": author_name,
        "description": attachment.description,
        "caption": attachment.caption,
        "name": post.slug,
        "status": post.status.as_str(),
        "uploadedTo": attachment.parent_id.unwrap_or(0),
        "date": uploaded.and_utc().timestamp_millis(),
        "modified": post.modified_at.and_utc().timestamp_millis(),
        "dateFormatted": uploaded.format("%B %-d, %Y").to_string(),
        "mime": attachment.mime_type,
        "type": kind,
        "subtype": subtype,
    });
    if let Some(metadata) = &attachment.metadata {
        let folder = attachment
            .file
            .rsplit_once('/')
            .map_or(String::new(), |(folder, _)| format!("{folder}/"));
        let mut sizes = Map::new();
        // As `image_size_names_choose`: the sizes the media modal offers, not every sub-size.
        for (name, size) in metadata
            .sizes
            .iter()
            .filter(|(name, _)| CHOOSABLE_SIZES.contains(&name.as_str()))
        {
            sizes.insert(
                name.clone(),
                json!({
                    "url": file_url(base, &format!("{folder}{}", size.file)),
                    "width": size.width,
                    "height": size.height,
                    "orientation": orientation(size.width, size.height),
                }),
            );
        }
        sizes.insert(
            "full".to_owned(),
            json!({
                "url": file_url(base, &attachment.file),
                "width": metadata.width,
                "height": metadata.height,
                "orientation": orientation(metadata.width, metadata.height),
            }),
        );
        let object = item.as_object_mut().expect("an object");
        if metadata.filesize > 0 {
            object.insert("filesizeInBytes".into(), json!(metadata.filesize));
            object.insert(
                "filesizeHumanReadable".into(),
                json!(size_format(metadata.filesize, 0)),
            );
        }
        object.insert("width".into(), json!(metadata.width));
        object.insert("height".into(), json!(metadata.height));
        object.insert(
            "orientation".into(),
            json!(orientation(metadata.width, metadata.height)),
        );
        object.insert("sizes".into(), Value::Object(sizes));
    }
    item
}

/// What the picker asks for: `query-attachments`'s `s`, `paged`, `posts_per_page`,
/// `post_mime_type` and `post_parent` (the media modal's `uploadedTo`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentQuery {
    /// The search, if any; cut to [`MAX_SEARCH_BYTES`].
    pub search: Option<String>,
    /// The page, 1-based; below 1 is 1.
    pub page: i64,
    /// The page size; clamped to 1..=[`MAX_PER_PAGE`], [`DEFAULT_QUERY_PER_PAGE`] when 0.
    pub per_page: i64,
    /// A MIME type family (`image`, `audio`, `video`, `application`, `text`), if any. Anything
    /// that is not lowercase letters is ignored.
    pub media_type: Option<String>,
    /// Only the attachments uploaded to this post (`post_parent`, the media modal's "Uploaded to
    /// this post"), if any.
    pub uploaded_to: Option<i64>,
}

/// One page of the picker's attachments.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentPage {
    /// The attachments, each as [`prepare_attachment_for_js`] shapes it.
    pub items: Vec<Value>,
    /// How many attachments match, on every page (`X-MC-Total`, WordPress's `X-WP-Total`).
    pub total: i64,
    /// The number of pages (`X-MC-TotalPages`).
    pub total_pages: i64,
}

/// WordPress's `wp_ajax_query_attachments`: one page of the attachments `user` may read, newest
/// first, matching `query`. A user without `upload_files` gets nothing (the caller refuses them
/// first); `private` attachments are listed for users with `read_private_posts`.
///
/// # Errors
///
/// The database's error.
pub async fn query_attachments(
    db: &Database,
    user: &UserCaps,
    query: &AttachmentQuery,
) -> Result<AttachmentPage, Error> {
    let per_page = match query.per_page {
        0 => DEFAULT_QUERY_PER_PAGE,
        n => n.clamp(1, MAX_PER_PAGE),
    };
    if !has_cap(user, UPLOAD_FILES) {
        return Ok(AttachmentPage {
            items: Vec::new(),
            total: 0,
            total_pages: 0,
        });
    }
    let mut statuses = vec![PostStatus::Inherit];
    if has_cap(user, &attachment_post_type().caps().read_private_posts) {
        statuses.push(PostStatus::Private);
    }
    let media_type = query
        .media_type
        .as_deref()
        .filter(|t| !t.is_empty() && t.len() <= 32 && t.bytes().all(|b| b.is_ascii_lowercase()));
    let (filter, binds) = attachment_filter(
        &statuses,
        query.search.as_deref(),
        media_type,
        query.uploaded_to,
    );
    let order = "COALESCE(p.published_at, p.modified_at) DESC, p.id DESC";
    let (ids, total) = page_ids(db, &filter, binds, order, query.page, per_page).await?;
    let loaded = Loaded::load(db, &ids).await?;
    let base = uploads_url(db).await?;
    let items = loaded
        .posts
        .iter()
        .map(|post| {
            let author = loaded
                .authors
                .get(&post.author_id)
                .map_or("", String::as_str);
            prepare_attachment_for_js(&base, post, &loaded.attachment(post), author)
        })
        .collect();
    Ok(AttachmentPage {
        items,
        total,
        total_pages: (total + per_page - 1) / per_page,
    })
}

/// Attachment `id` as [`prepare_attachment_for_js`] shapes it, or `None` when there is none.
///
/// # Errors
///
/// The database's error.
pub async fn attachment_for_js(db: &Database, id: i64) -> Result<Option<Value>, Error> {
    let loaded = Loaded::load(db, &[id]).await?;
    let Some(post) = loaded.posts.first() else {
        return Ok(None);
    };
    let base = uploads_url(db).await?;
    let author = loaded
        .authors
        .get(&post.author_id)
        .map_or("", String::as_str);
    Ok(Some(prepare_attachment_for_js(
        &base,
        post,
        &loaded.attachment(post),
        author,
    )))
}

/// WordPress's `wp_ajax_get_attachment`: attachment `id` as [`prepare_attachment_for_js`] shapes
/// it, when `user` may read it: `upload_files`, and an attachment that is `inherit`, or `private`
/// with `read_private_posts` (what [`query_attachments`] lists). `None` otherwise.
///
/// # Errors
///
/// The database's error.
pub async fn attachment_for_user(
    db: &Database,
    user: &UserCaps,
    id: i64,
) -> Result<Option<Value>, Error> {
    if !has_cap(user, UPLOAD_FILES) || id <= 0 {
        return Ok(None);
    }
    let Some(post) = Post::query().find(db, id).await? else {
        return Ok(None);
    };
    let readable = match post.status {
        PostStatus::Inherit => true,
        PostStatus::Private => has_cap(user, &attachment_post_type().caps().read_private_posts),
        _ => false,
    };
    if post.post_type != ATTACHMENT || !readable {
        return Ok(None);
    }
    attachment_for_js(db, id).await
}

/// What the edit form of an attachment shows above its fields, as WordPress's attachment edit
/// screen shows the file (`wp_get_attachment_image` at 900 by 450) and its facts
/// (`attachment_submitbox_metadata`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDetails {
    /// The attachment's id.
    pub id: i64,
    /// The file's address.
    pub url: String,
    /// The address of the preview: the `large` sub-size, or the image itself; empty for a file
    /// that is not an image.
    pub preview_url: String,
    /// The alternative text, for the preview.
    pub alt: String,
    /// The title, for the link to a file that is not an image.
    pub title: String,
    /// The file name: `photo.jpg`.
    pub file_name: String,
    /// The file type as WordPress shows it: the extension in capitals (`JPG`), or the MIME
    /// subtype in capitals when the file has no extension.
    pub file_type: String,
    /// The file's size in bytes, when it is known.
    pub file_size: Option<u64>,
    /// The image's width and height, for an image.
    pub dimensions: Option<(u32, u32)>,
    /// The upload date in the site's time zone, as WordPress writes it (`Nov 27, 2026 at 09:30`).
    pub uploaded_on: String,
    /// The name of the user who uploaded it.
    pub uploaded_by: String,
}

/// The file type `attachment_submitbox_metadata` shows: the extension of `file` in capitals,
/// or the MIME type's subtype in capitals (`image/` removed) when it has none.
fn file_type(file: &str, mime: &str) -> String {
    let name = file_name(file);
    match name.rsplit_once('.') {
        Some((_, extension))
            if !extension.is_empty()
                && extension.chars().all(|c| c.is_alphanumeric() || c == '_') =>
        {
            extension.to_uppercase()
        }
        _ => mime.trim_start_matches("image/").to_uppercase(),
    }
}

/// The facts of attachment `id` for its edit form ([`AttachmentDetails`]); `storage` gives the
/// size of a file that has no image metadata. `None` when the screen does not list it.
///
/// # Errors
///
/// The database's error.
pub async fn attachment_details(
    db: &Database,
    storage: &Arc<dyn Storage>,
    id: i64,
) -> Result<Option<AttachmentDetails>, Error> {
    if listed_attachment(db, id).await?.is_none() {
        return Ok(None);
    }
    let loaded = Loaded::load(db, &[id]).await?;
    let Some(post) = loaded.posts.first() else {
        return Ok(None);
    };
    let attachment = loaded.attachment(post);
    let base = uploads_url(db).await?;
    let offset = crate::content::gmt_offset(db).await?;
    let uploaded = upload_date(post) + Duration::seconds(i64::from(offset.local_minus_utc()));
    let image = attachment.mime_type.starts_with("image/");
    let preview_url = if !image {
        String::new()
    } else if let Some(large) = attachment
        .metadata
        .as_ref()
        .and_then(|m| m.sizes.get("large"))
    {
        let folder = attachment
            .file
            .rsplit_once('/')
            .map_or(String::new(), |(folder, _)| format!("{folder}/"));
        file_url(&base, &format!("{folder}{}", large.file))
    } else {
        file_url(&base, &attachment.file)
    };
    let file_size = match &attachment.metadata {
        Some(metadata) if metadata.filesize > 0 => Some(metadata.filesize),
        _ if attachment.file.is_empty() => None,
        _ => match storage.open(&attachment.file) {
            Ok(crate::media::OpenedFile::File(_, length)) => Some(length),
            Ok(crate::media::OpenedFile::Bytes(bytes)) => Some(bytes.len() as u64),
            Err(e) => {
                log::warn!("Could not read the size of attachment {id}'s file: {e}");
                None
            }
        },
    };
    Ok(Some(AttachmentDetails {
        id,
        url: file_url(&base, &attachment.file),
        preview_url,
        alt: attachment.alt.clone(),
        title: attachment.title.clone(),
        file_name: file_name(&attachment.file).to_owned(),
        file_type: file_type(&attachment.file, &attachment.mime_type),
        file_size,
        dimensions: attachment.metadata.as_ref().map(|m| (m.width, m.height)),
        uploaded_on: uploaded.format("%b %-d, %Y at %H:%M").to_string(),
        uploaded_by: loaded
            .authors
            .get(&post.author_id)
            .cloned()
            .unwrap_or_default(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Uploads

/// Why an upload was refused, with WordPress's message for it.
#[derive(Debug)]
pub enum UploadRefusal {
    /// The user lacks `upload_files`.
    NotAllowed,
    /// The user may not edit the post the file would be attached to, or it does not exist.
    CannotAttach,
    /// The post the file would be attached to is an attachment or a revision.
    InvalidParent,
    /// The request held no file.
    NoFile,
    /// The file is over the upload limit, in bytes.
    TooLarge(u64),
    /// The file is empty, or of a type that is not allowed, or not the type its name says.
    File(UploadError),
    /// Storing the file or its attachment failed (logged).
    Failed,
}

impl UploadRefusal {
    /// The HTTP status: 403 for a permission, 400 for a bad file, 413 for one too large, 500
    /// when storing failed.
    pub fn status(&self) -> u16 {
        match self {
            Self::NotAllowed | Self::CannotAttach => 403,
            Self::TooLarge(_) | Self::File(UploadError::TooLarge { .. }) => 413,
            Self::InvalidParent | Self::NoFile | Self::File(_) => 400,
            Self::Failed => 500,
        }
    }

    /// WordPress's message.
    pub fn message(&self) -> String {
        match self {
            Self::NotAllowed => "Sorry, you are not allowed to upload files.".to_owned(),
            Self::CannotAttach => {
                "Sorry, you are not allowed to attach files to this post.".to_owned()
            }
            Self::InvalidParent => "Invalid parent type.".to_owned(),
            Self::NoFile => "No file was uploaded.".to_owned(),
            Self::TooLarge(limit) => format!(
                "This file exceeds the maximum upload size for this site ({} KB).",
                limit / 1024
            ),
            Self::File(UploadError::Empty) => {
                "File is empty. Please upload something more substantial.".to_owned()
            }
            Self::File(UploadError::TooLarge { limit, .. }) => Self::TooLarge(*limit).message(),
            Self::File(_) => "Sorry, you are not allowed to upload this file type.".to_owned(),
            Self::Failed => "The file could not be stored. Try again in a moment.".to_owned(),
        }
    }
}

impl fmt::Display for UploadRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for UploadRefusal {}

/// Whether `user` may upload at all (`upload_files`). Asked before a request body is read.
pub fn may_upload(user: &UserCaps) -> bool {
    has_cap(user, UPLOAD_FILES)
}

/// WordPress's `async-upload.php`: checks that `user` may upload (`upload_files`) and, when
/// `parent` is given, edit that post (`edit_post`, with the capabilities `caps_of` gives its
/// post type); then stores `upload` with its sub-sizes and creates its attachment, owned by
/// `user`, attached to `parent`, titled from the client's file name ([`create_attachment`]).
///
/// # Errors
///
/// An [`UploadRefusal`]; a database or storage failure is logged and is
/// [`UploadRefusal::Failed`].
pub async fn upload_to_library(
    db: &Database,
    storage: &Arc<dyn Storage>,
    sizes: &Arc<ImageSizes>,
    user: &UserCaps,
    upload: &ValidatedUpload<'_>,
    parent: Option<i64>,
    caps_of: &(dyn Fn(&str) -> PostTypeCaps + Sync),
) -> Result<Attachment, UploadRefusal> {
    if !may_upload(user) {
        return Err(UploadRefusal::NotAllowed);
    }
    if let Some(parent) = parent {
        let failed = |e: Error| {
            log::error!("Could not check the post an upload is attached to: {e}");
            UploadRefusal::Failed
        };
        let Some(post) = Post::query().find(db, parent).await.map_err(failed)? else {
            return Err(UploadRefusal::CannotAttach);
        };
        let caps = caps_of(&post.post_type);
        let context = PostContext {
            post: &post,
            trash_meta_status: trash_status(db, &post).await.map_err(failed)?,
            parent: None,
            site_pages: Some(site_pages(db).await.map_err(failed)?),
            post_type_caps: Some(&caps),
        };
        if !can(user, "edit_post", Some(&Object::PostWithContext(context))) {
            return Err(UploadRefusal::CannotAttach);
        }
        if post.post_type == ATTACHMENT || post.post_type == "revision" {
            return Err(UploadRefusal::InvalidParent);
        }
    }
    create_attachment(db, storage, sizes, upload, parent, user.id(), None)
        .await
        .map_err(|e| match e {
            AttachmentError::InvalidParent(_) => UploadRefusal::InvalidParent,
            AttachmentError::Upload(
                e @ (UploadError::NoUniqueName(_) | UploadError::Storage(_)),
            ) => {
                log::error!("Could not store an upload: {e}");
                UploadRefusal::Failed
            }
            AttachmentError::Upload(e) => UploadRefusal::File(e),
            other => {
                log::error!("Could not store an upload: {other}");
                UploadRefusal::Failed
            }
        })
}

// ---------------------------------------------------------------------------------------------
// Ignite's media fields

/// The media library of Ignite's Image, File and Gallery fields: the attachments that are
/// `inherit` or `private`, each with its title and its file's address. Register it with
/// [`ignite_core::register_media_library`].
#[derive(Debug, Clone, Default)]
pub struct Attachments {
    browse_url: Option<String>,
}

impl Attachments {
    /// The library, whose fields link to `browse_url` (the media library screen) when given.
    pub fn new(browse_url: Option<String>) -> Self {
        Self { browse_url }
    }

    /// The attachments among `ids`, at most [`MAX_LOOKUP_IDS`] of them.
    async fn items(db: &Database, ids: &[i64]) -> Result<Vec<MediaItem>, Error> {
        let mut ids: Vec<i64> = ids.iter().copied().filter(|id| *id > 0).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.truncate(MAX_LOOKUP_IDS);
        let mut listed = Vec::new();
        for chunk in ids.chunks(500) {
            let mut binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
            binds.push(ATTACHMENT.into());
            binds.push(PostStatus::Inherit.into());
            binds.push(PostStatus::Private.into());
            let rows: Vec<(i64,)> = raw::fetch_all(
                db,
                &format!(
                    "SELECT id FROM posts WHERE id IN ({}) AND post_type = ? AND status IN (?, ?)",
                    marks(chunk.len())
                ),
                &binds,
            )
            .await?;
            listed.extend(rows.into_iter().map(|(id,)| id));
        }
        let loaded = Loaded::load(db, &listed).await?;
        let base = uploads_url(db).await?;
        Ok(loaded
            .posts
            .iter()
            .map(|post| MediaItem {
                id: post.id,
                title: if post.title.is_empty() {
                    file_name(&loaded.text(post.id, ATTACHED_FILE)).to_owned()
                } else {
                    post.title.clone()
                },
                url: file_url(&base, &loaded.text(post.id, ATTACHED_FILE)),
            })
            .collect())
    }
}

impl MediaLibrary for Attachments {
    fn find<'a>(&'a self, db: &'a Database, ids: &'a [i64]) -> RuntimeFuture<'a, Vec<MediaItem>> {
        Box::pin(async move { Self::items(db, ids).await })
    }

    fn browse_url(&self) -> Option<String> {
        self.browse_url.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attachment_type_needs_the_posts_capabilities() {
        let caps = attachment_post_type().caps();
        assert_eq!(caps.edit_posts, "edit_posts");
        assert_eq!(caps.delete_others_posts, "delete_others_posts");
    }

    #[test]
    fn refusals_have_wordpress_messages() {
        assert_eq!(UploadRefusal::NotAllowed.status(), 403);
        assert_eq!(
            UploadRefusal::NotAllowed.message(),
            "Sorry, you are not allowed to upload files."
        );
        assert_eq!(UploadRefusal::TooLarge(2048).status(), 413);
        assert!(UploadRefusal::TooLarge(2048).message().contains("2 KB"));
        assert_eq!(
            UploadRefusal::File(UploadError::TypeNotAllowed { extension: None }).message(),
            "Sorry, you are not allowed to upload this file type."
        );
    }

    #[test]
    fn sizes_are_formatted_as_wordpress_does() {
        assert_eq!(size_format(0, 0), "0 B");
        assert_eq!(size_format(512 * 1024, 0), "512 KB");
        assert_eq!(size_format(2 * 1024 * 1024, 0), "2 MB");
        assert_eq!(size_format(59_610, 2), "58.21 KB");
        assert_eq!(size_format(1023, 0), "1,023 B");
        assert_eq!(size_format(1536, 0), "2 KB");
    }

    #[test]
    fn file_types_are_the_extension_in_capitals() {
        assert_eq!(file_type("2026/11/photo.jpg", "image/jpeg"), "JPG");
        assert_eq!(file_type("2026/11/report.pdf", "application/pdf"), "PDF");
        assert_eq!(file_type("2026/11/noext", "image/png"), "PNG");
    }

    #[test]
    fn file_names_and_orientation() {
        assert_eq!(file_name("2026/11/photo.jpg"), "photo.jpg");
        assert_eq!(file_name("photo.jpg"), "photo.jpg");
        assert_eq!(file_title("2026/11/photo.jpg"), "photo");
        assert_eq!(file_title("archive.tar.gz"), "archive.tar");
        assert_eq!(file_title(".htaccess"), ".htaccess");
        assert_eq!(orientation(300, 200), "landscape");
        assert_eq!(orientation(200, 300), "portrait");
        assert_eq!(file_url("http://x/mc-content/uploads", ""), "");
    }
}
