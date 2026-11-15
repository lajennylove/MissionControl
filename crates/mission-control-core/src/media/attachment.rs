//! Attachments: an uploaded file kept as a `posts` row of type `attachment`, as WordPress's
//! `media_handle_upload` and `wp_insert_attachment` keep it.
//!
//! The row has the status `inherit` and the post it was uploaded to as its parent. Its title
//! starts as the client's file name without the extension (see [`create_attachment`]); the
//! caption is the excerpt, the description is the content, and the MIME type is `mime_type`
//! (`post_mime_type`). Three post meta keys go with it, as in WordPress:
//!
//! - `_wp_attached_file`: the file's storage path, `YYYY/MM/name.ext`;
//! - `_wp_attachment_metadata`: an image's size and sub-sizes ([`AttachmentMetadata`]), for
//!   images only;
//! - `_wp_attachment_image_alt`: the alt text.
//!
//! [`Attachment`] reads the row and its meta as one value.
//!
//! ```ignore
//! use mission_control_core::media::{create_attachment, update_attachment_details, validate};
//!
//! let upload = validate("Harbour.jpg", &bytes, max_bytes)?;
//! let attachment = create_attachment(&db, &storage, &sizes, &upload, Some(post.id), author, None)
//!     .await?;
//! update_attachment_details(&db, attachment.id, "Harbour", "Boats at dawn", "", "").await?;
//! ```

use std::fmt;
use std::sync::Arc;

use chrono::NaiveDateTime;
use orbit_orm::{Database, Error, Model, Transaction};
use serde_json::{Value, json};

use super::sizes::{AttachmentMetadata, ImageSizes, create_image_subsizes};
use super::storage::{Storage, StorageError};
use super::upload::{UploadError, ValidatedUpload, store};
use crate::content::{
    MetaError, NewPost, Post, PostStatus, get_post_meta, next_modified_at, now, put_post_meta_in,
};
use crate::text::sanitize_text_field;

/// The post type of an attachment.
const ATTACHMENT: &str = "attachment";
/// The post meta that holds the file's storage path.
const ATTACHED_FILE_META: &str = "_wp_attached_file";
/// The post meta that holds an image's [`AttachmentMetadata`].
const METADATA_META: &str = "_wp_attachment_metadata";
/// The post meta that holds the alt text.
const ALT_META: &str = "_wp_attachment_image_alt";

/// An attachment: its row of `posts` and its meta, read together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The id, a `posts` id.
    pub id: i64,
    /// The user who uploaded it.
    pub author_id: i64,
    /// The post it was uploaded to (`post_parent`), if any.
    pub parent_id: Option<i64>,
    /// The title (`post_title`).
    pub title: String,
    /// The alt text (post meta `_wp_attachment_image_alt`); empty when there is none.
    pub alt: String,
    /// The caption (`post_excerpt`).
    pub caption: String,
    /// The description (`post_content`).
    pub description: String,
    /// The file's storage path, for example `2026/11/photo.jpg` (post meta `_wp_attached_file`).
    pub file: String,
    /// The MIME type (`post_mime_type`), for example `image/jpeg`.
    pub mime_type: String,
    /// An image's size and sub-sizes (post meta `_wp_attachment_metadata`); `None` for a file
    /// that is not an image Mission Control resizes.
    pub metadata: Option<AttachmentMetadata>,
}

/// Why an attachment was not created, read or updated.
#[derive(Debug)]
pub enum AttachmentError {
    /// The parent is not a post an attachment can belong to: it does not exist, or it is an
    /// attachment or a revision (as WordPress's REST API refuses them).
    InvalidParent(i64),
    /// No attachment has this id.
    NotFound(i64),
    /// No free file name, or the file could not be written.
    Upload(UploadError),
    /// Storage failed while making the sub-sizes.
    Storage(StorageError),
    /// A meta value is not valid JSON.
    Meta(MetaError),
    /// The database refused a query, for example an author that does not exist.
    Database(Error),
}

impl fmt::Display for AttachmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidParent(id) => write!(f, "post {id} cannot have attachments"),
            Self::NotFound(id) => write!(f, "no attachment with id {id}"),
            Self::Upload(e) => e.fmt(f),
            Self::Storage(e) => e.fmt(f),
            Self::Meta(e) => e.fmt(f),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for AttachmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidParent(_) | Self::NotFound(_) => None,
            Self::Upload(e) => Some(e),
            Self::Storage(e) => Some(e),
            Self::Meta(e) => Some(e),
            Self::Database(e) => Some(e),
        }
    }
}

impl From<Error> for AttachmentError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

impl From<MetaError> for AttachmentError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Database(e) => Self::Database(e),
            e => Self::Meta(e),
        }
    }
}

impl From<UploadError> for AttachmentError {
    fn from(e: UploadError) -> Self {
        Self::Upload(e)
    }
}

impl From<StorageError> for AttachmentError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// Where a new attachment's title comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentTitle<'a> {
    /// The admin's default, as `media_handle_upload` makes it: the client's file name
    /// ([`ValidatedUpload::client_name`]) without its extension, cleaned as `sanitize_text_field`
    /// does (tags stripped, whitespace collapsed and trimmed). `Sunset Beach.jpg` stored as
    /// `Sunset-Beach-1.jpg` is titled `Sunset Beach`.
    ClientName,
    /// The REST API's default, as `WP_REST_Attachments_Controller::insert_attachment` makes it:
    /// the stored file's name without its extension. `Sunset Beach.jpg` stored as
    /// `Sunset-Beach-1.jpg` is titled `Sunset-Beach-1`.
    StoredName,
    /// This title, as it is.
    Given(&'a str),
}

/// Stores a validated upload and creates its attachment, as WordPress's `media_handle_upload`
/// does, and returns it.
///
/// The title is `title` when given (see [`AttachmentTitle::Given`]); otherwise it is the
/// admin's default, from the client's file name ([`AttachmentTitle::ClientName`]).
/// [`create_attachment_titled`] takes the REST API's rule instead.
///
/// # Errors
///
/// As [`create_attachment_titled`].
pub async fn create_attachment(
    db: &Database,
    storage: &Arc<dyn Storage>,
    sizes: &Arc<ImageSizes>,
    upload: &ValidatedUpload<'_>,
    parent: Option<i64>,
    author: i64,
    title: Option<&str>,
) -> Result<Attachment, AttachmentError> {
    let title = match title {
        Some(title) => AttachmentTitle::Given(title),
        None => AttachmentTitle::ClientName,
    };
    create_attachment_titled(db, storage, sizes, upload, parent, author, title).await
}

/// Stores a validated upload and creates its attachment titled by `title`, as WordPress's
/// `media_handle_upload` does, and returns it.
///
/// 1. The file is written under a free name ([`store`]) in the `YYYY/MM` folder of now, or, when
///    `parent` is given and is not a page, of the parent's publish date, as WordPress files an
///    upload under its post's month.
/// 2. An image gets its sub-sizes, one per size in `sizes` ([`create_image_subsizes`]).
/// 3. The attachment row (status `inherit`, parent `parent`, the upload's MIME type) and its
///    meta `_wp_attached_file` and, for an image, `_wp_attachment_metadata` are inserted in one
///    transaction, as `wp_insert_attachment` and `wp_update_attachment_metadata` write them.
///
/// Steps 1 and 2 are disk and CPU work, run on a blocking thread (`spawn_blocking`); the bytes
/// are copied for it. When step 3 returns an error, the original and its sub-sizes are deleted
/// (on a blocking thread too), and when step 2 returns an error, the original is; both are best
/// effort, with failures logged. Files can still be left behind when step 2 fails after writing
/// some sub-sizes, or when the caller drops this future part way (a closed request).
///
/// # Errors
///
/// Fails with [`AttachmentError::InvalidParent`] when `parent` does not exist or is an
/// attachment or a revision (nothing is written); with [`AttachmentError::Upload`] or
/// [`AttachmentError::Storage`] when a file cannot be written; or with
/// [`AttachmentError::Database`], for example when the author does not exist.
pub async fn create_attachment_titled(
    db: &Database,
    storage: &Arc<dyn Storage>,
    sizes: &Arc<ImageSizes>,
    upload: &ValidatedUpload<'_>,
    parent: Option<i64>,
    author: i64,
    title: AttachmentTitle<'_>,
) -> Result<Attachment, AttachmentError> {
    let date = match parent {
        Some(id) => upload_date(db, id).await?,
        None => now(),
    };

    let (path, metadata) = {
        let storage = Arc::clone(storage);
        let sizes = Arc::clone(sizes);
        let client_name = upload.client_name.clone();
        let file_name = upload.file_name.clone();
        let extension = upload.extension.clone();
        let mime_type = upload.mime_type;
        let bytes = upload.bytes.to_vec();
        blocking(move || {
            let upload = ValidatedUpload {
                client_name,
                file_name,
                extension,
                mime_type,
                bytes: &bytes,
            };
            write_files(&*storage, &sizes, &upload, date)
        })
        .await??
    };

    let title = match title {
        AttachmentTitle::Given(title) => title.to_owned(),
        AttachmentTitle::ClientName => default_title(&upload.client_name),
        AttachmentTitle::StoredName => stored_title(&path),
    };
    let new = NewPost {
        status: PostStatus::Inherit,
        title,
        parent_id: parent,
        published_at: Some(now()),
        mime_type: upload.mime_type.to_owned(),
        ..NewPost::new(author, ATTACHMENT)
    };
    let inserted = db
        .transaction(async |tx| insert_attachment(tx, new, &path, metadata.as_ref()).await)
        .await;
    match inserted {
        Ok(post) => Ok(Attachment {
            id: post.id,
            author_id: post.author_id,
            parent_id: post.parent_id,
            title: post.title,
            alt: String::new(),
            caption: post.excerpt,
            description: post.content,
            file: path,
            mime_type: post.mime_type,
            metadata,
        }),
        Err(e) => {
            let storage = Arc::clone(storage);
            let cleanup = blocking(move || delete_files(&*storage, &path, metadata.as_ref()));
            if let Err(join) = cleanup.await {
                log::warn!("could not delete the files of a failed upload: {join}");
            }
            Err(e)
        }
    }
}

/// Runs `f` on a blocking thread and waits for it. A panic in `f` is resumed here; a task the
/// runtime cancelled (it is shutting down) is an I/O error.
async fn blocking<T, F>(f: F) -> Result<T, AttachmentError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Ok(value),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(StorageError::Io(std::io::Error::other(e)).into()),
    }
}

/// The admin's default title for an upload sent as `client_name`, as `media_handle_upload`
/// makes it: the name without its last extension (`wp_basename($name, ".$ext")`), through
/// [`sanitize_text_field`].
fn default_title(client_name: &str) -> String {
    let stem = client_name
        .rsplit_once('.')
        .map_or(client_name, |(stem, _)| stem);
    sanitize_text_field(stem)
}

/// The REST API's default title for a file stored at `path`: its name without the last
/// extension, trimmed, as `WP_REST_Attachments_Controller::insert_attachment` cuts it from
/// `wp_basename( $file )`.
fn stored_title(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .trim()
        .to_owned()
}

/// The date whose `YYYY/MM` folder an upload to `parent` goes in, as `media_handle_upload`
/// picks it: the parent's publish date, unless the parent is a page or has none; then now.
/// Refuses a parent that does not exist or cannot have attachments.
async fn upload_date(db: &Database, parent: i64) -> Result<NaiveDateTime, AttachmentError> {
    let Some(post) = Post::query().find(db, parent).await? else {
        return Err(AttachmentError::InvalidParent(parent));
    };
    if post.post_type == ATTACHMENT || post.post_type == "revision" {
        return Err(AttachmentError::InvalidParent(parent));
    }
    Ok(match post.published_at {
        Some(date) if post.post_type != "page" => date,
        _ => now(),
    })
}

/// Stores the upload and makes its sub-sizes; on a sub-size failure, deletes the original.
fn write_files(
    storage: &dyn Storage,
    sizes: &ImageSizes,
    upload: &ValidatedUpload<'_>,
    date: NaiveDateTime,
) -> Result<(String, Option<AttachmentMetadata>), AttachmentError> {
    let path = store(storage, upload, date)?;
    match create_image_subsizes(storage, &path, sizes) {
        Ok(metadata) => Ok((path, metadata)),
        Err(e) => {
            delete_files(storage, &path, None);
            Err(e.into())
        }
    }
}

/// Deletes `path` and the sub-sizes in `metadata`, logging what cannot be deleted.
fn delete_files(storage: &dyn Storage, path: &str, metadata: Option<&AttachmentMetadata>) {
    let folder = path.rsplit_once('/').map(|(folder, _)| folder);
    let sub_sizes = metadata
        .into_iter()
        .flat_map(|metadata| metadata.sizes.values())
        .map(|size| match folder {
            Some(folder) => format!("{folder}/{}", size.file),
            None => size.file.clone(),
        });
    for file in std::iter::once(path.to_owned()).chain(sub_sizes) {
        if let Err(e) = storage.delete(&file) {
            log::warn!("could not delete {file:?}: {e}");
        }
    }
}

/// Inserts the attachment row and its meta on the connection of a transaction.
async fn insert_attachment(
    tx: &mut Transaction,
    new: NewPost,
    path: &str,
    metadata: Option<&AttachmentMetadata>,
) -> Result<Post, AttachmentError> {
    let post = Post::create_in(tx, new).await?;
    put_post_meta_in(tx, post.id, ATTACHED_FILE_META, &json!(path)).await?;
    if let Some(metadata) = metadata {
        let value = serde_json::to_value(metadata).expect("the metadata serialises to JSON");
        put_post_meta_in(tx, post.id, METADATA_META, &value).await?;
    }
    Ok(post)
}

/// The attachment with this id, with its meta, or `None` when there is no attachment with it
/// (no post, or a post of another type).
///
/// Stored `_wp_attachment_metadata` that is not shaped like [`AttachmentMetadata`] (written by
/// something else) reads as `None`, with a warning logged.
///
/// # Errors
///
/// Fails with [`AttachmentError::Meta`] when a meta value is not JSON, or with the database's
/// error.
pub async fn get_attachment(db: &Database, id: i64) -> Result<Option<Attachment>, AttachmentError> {
    let Some(post) = Post::query().find(db, id).await? else {
        return Ok(None);
    };
    if post.post_type != ATTACHMENT {
        return Ok(None);
    }
    let alt = string_meta(get_post_meta(db, id, ALT_META).await?);
    let file = string_meta(get_post_meta(db, id, ATTACHED_FILE_META).await?);
    let metadata = match get_post_meta(db, id, METADATA_META).await? {
        Some(value) => match serde_json::from_value(value) {
            Ok(metadata) => Some(metadata),
            Err(e) => {
                log::warn!("attachment {id} has unreadable {METADATA_META}: {e}");
                None
            }
        },
        None => None,
    };
    Ok(Some(Attachment {
        id: post.id,
        author_id: post.author_id,
        parent_id: post.parent_id,
        title: post.title,
        alt,
        caption: post.excerpt,
        description: post.content,
        file,
        mime_type: post.mime_type,
        metadata,
    }))
}

/// Deletes an attachment for good, as WordPress's `wp_delete_attachment` with `force_delete`:
/// the row (its meta and term relationships go with it, as [`Post::delete`] removes them), every
/// `_thumbnail_id` that points at it, then its file and its sub-sizes from `storage`. Returns
/// `false` when there is no attachment with this id (nothing is deleted).
///
/// The files are deleted after the row, on a blocking thread, as WordPress deletes them after
/// the post: a file that cannot be deleted is logged and left, and the attachment is gone.
///
/// # Errors
///
/// Fails with the database's error, or [`AttachmentError::Meta`] when the attachment's meta is
/// not JSON (nothing is deleted then).
pub async fn delete_attachment(
    db: &Database,
    storage: &Arc<dyn Storage>,
    id: i64,
) -> Result<bool, AttachmentError> {
    let Some(attachment) = get_attachment(db, id).await? else {
        return Ok(false);
    };
    let Some(post) = Post::query().find(db, id).await? else {
        return Ok(false);
    };
    if !post.delete(db).await? {
        return Ok(false);
    }
    // `delete_metadata( 'post', null, '_thumbnail_id', $post_id, true )`: the id is stored as a
    // JSON number, or as a string by code that wrote it so.
    orbit_orm::raw::execute(
        db,
        "DELETE FROM post_meta WHERE meta_key = ? AND meta_value IN (?, ?)",
        &[
            "_thumbnail_id".into(),
            id.to_string().into(),
            format!("\"{id}\"").into(),
        ],
    )
    .await?;
    if !attachment.file.is_empty() {
        let storage = Arc::clone(storage);
        let deleted = blocking(move || {
            delete_files(&*storage, &attachment.file, attachment.metadata.as_ref());
        })
        .await;
        if let Err(e) = deleted {
            log::warn!("could not delete the files of attachment {id}: {e}");
        }
    }
    Ok(true)
}

/// A meta value read as text: a JSON string as it is, nothing as empty, anything else as its
/// JSON text.
fn string_meta(value: Option<Value>) -> String {
    match value {
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Sets an attachment's title, alt text, caption and description, as WordPress's media screen
/// and REST API save them: the title, the excerpt and the content of the row, and the post meta
/// `_wp_attachment_image_alt`, in one transaction. The slug stays as it is. Like
/// [`Post::save`], the row gets a new `modified_at` and the update is conditional on the stored
/// one, retried when another write lands in between. Returns the updated attachment.
///
/// # Errors
///
/// Fails with [`AttachmentError::NotFound`] when there is no attachment with this id (nothing is
/// written), or with the database's error.
pub async fn update_attachment_details(
    db: &Database,
    id: i64,
    title: &str,
    alt: &str,
    caption: &str,
    description: &str,
) -> Result<Attachment, AttachmentError> {
    loop {
        let stored = match Post::query().find(db, id).await? {
            Some(post) if post.post_type == ATTACHMENT => post,
            _ => return Err(AttachmentError::NotFound(id)),
        };
        let modified_at = next_modified_at(stored.modified_at);
        let written = db
            .transaction(async |tx| {
                // The stamp always changes, so a matched row is a changed row on MySQL too.
                let matched = Post::query()
                    .where_eq(Post::PRIMARY_KEY, id)
                    .where_eq("modified_at", stored.modified_at)
                    .update(
                        &mut *tx,
                        &[
                            ("title", title.into()),
                            ("excerpt", caption.into()),
                            ("content", description.into()),
                            ("modified_at", modified_at.into()),
                        ],
                    )
                    .await?;
                if matched == 0 {
                    return Ok(false);
                }
                put_post_meta_in(tx, id, ALT_META, &json!(alt)).await?;
                Ok::<_, Error>(true)
            })
            .await?;
        if written {
            return get_attachment(db, id)
                .await?
                .ok_or(AttachmentError::NotFound(id));
        }
    }
}
