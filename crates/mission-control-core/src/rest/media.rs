//! The media controller: `/mc/v2/media`, as WordPress's `WP_REST_Attachments_Controller` serves
//! it.
//!
//! Attachments are posts, so lists and single items run through the posts controller
//! ([`PostsController`]) with [`PostType::attachment`]: the same parameters, permissions, links,
//! `_fields` and `_embed`, plus what WordPress's attachments controller adds. [`MediaController`]
//! answers one request, with no Rocket types: the `mission-control` crate mounts it and reads the
//! request's body into a [`MediaUpload`].
//!
//! As WordPress:
//!
//! - An attachment's JSON has the post keys of its type (`id`, `date`, `date_gmt`, `guid`,
//!   `modified`, `modified_gmt`, `slug`, `status`, `type`, `link`, `title`, `author`,
//!   `comment_status`, `ping_status`, `template`, `meta`, `class_list`; no `password`), and
//!   `description`, `caption`, `alt_text`, `media_type`, `mime_type`, `media_details`, `post`,
//!   `source_url`, with `missing_image_sizes`, `permalink_template` and `generated_slug` in the
//!   edit context. The `guid` and `source_url` are the file's address under
//!   [`UPLOADS_PATH`]. An image's `media_details` holds its size, its sub-sizes (each with its
//!   `source_url`, and `full` for the original) and an empty `image_meta`.
//! - Lists take the posts' parameters with `parent`, `parent_exclude`, `media_type` and
//!   `mime_type`; `status` is `inherit` by default (`inherit`, `private` or `trash`). A search
//!   also matches the file's name.
//! - `POST /mc/v2/media` uploads a file: the raw body with `Content-Disposition: attachment;
//!   filename=...` and a `Content-Type` (`upload_from_data`), or a `multipart/form-data` body
//!   with a `file` part (`upload_from_file`); a `Content-MD5` header is checked when sent. It
//!   needs `upload_files`, and `edit_post` on the `post` it is attached to. The file is checked
//!   ([`validate`]), stored with its sub-sizes, and titled as WordPress's REST API titles it:
//!   `title`, or the stored file's name without its extension
//!   ([`AttachmentTitle::StoredName`]).
//! - `POST`, `PUT` or `PATCH /mc/v2/media/{id}` sets `title`, `caption`, `description`,
//!   `alt_text` and `post`; it needs `edit_post`. The title, caption and description pass
//!   through kses ([`KsesLimiter`]) for the acting user, as `wp_insert_post` filters them; the
//!   alt text through `sanitize_text_field`, its schema's sanitize callback.
//! - `DELETE /mc/v2/media/{id}` needs `force=true`: attachments have no trash unless WordPress's
//!   `MEDIA_TRASH` is on, which Mission Control does not offer, so without it the answer is 501
//!   `rest_trash_not_supported`. A delete removes the row and the files ([`delete_attachment`]).
//!
//! Mission Control's choices, where WordPress differs:
//!
//! - A refused file answers 400, not 500: `rest_upload_sideload_error` (raw body) or
//!   `rest_upload_unknown_error` (multipart), with WordPress's messages. WordPress returns them
//!   with status 500 from `wp_handle_sideload` and `wp_handle_upload`; a client error is the
//!   honest status, as Ruling R55 decided for terms. A file over the upload limit
//!   (`MC_UPLOAD_MAX_BYTES`) is `rest_upload_file_too_big` (400), the code and status WordPress
//!   gives on multisite, where PHP's own limit answers first on a single site.
//! - An unknown `post` on an update is refused (`rest_post_invalid_id`, 400); WordPress stores
//!   it.
//!
//! Left out, for now: the `/mc/v2/media/{id}/edit` and `post-process` routes, `generate_sub_sizes`,
//! reading a title and caption from the image's EXIF or IPTC data, the other writable post fields
//! (`slug`, `author`, `status`, `date`, `comment_status`, `meta`, ...), `featured_media` for audio
//! and video, `media_details` beyond an image's sizes (WordPress 6.0 stores every file's
//! `filesize`), and `the_content`'s attachment preview in a rendered `description`.

use std::sync::{Arc, LazyLock};

use md5::{Digest, Md5};
use orbit_orm::{Database, Model};
use serde_json::{Map, Value, json};

use super::meta::{MetaKeys, MetaObject, MetaUpdate};
use super::params::{Params, Reader, as_integer};
use super::posts::prepare::{ATTACHED_FILE, ATTACHMENT_ALT, ATTACHMENT_METADATA, Batch};
use super::posts::{PostType, PostTypes, PostsController, invalid_id};
use super::shared::{Context, Fields, Site, database_error};
use super::{Endpoint, KsesLimiter, RestError, RestResponse, RestRoutes, RouteSpec};
use crate::content::{Post, PostFields};
use crate::front::trim_excerpt;
use crate::hooks::{Hooks, THE_CONTENT};
use crate::media::{
    AttachmentError, AttachmentMetadata, AttachmentTitle, ImageSizes, Storage, UploadError,
    create_attachment_titled, delete_attachment, get_attachment, image_resize_dimensions,
    update_attachment_details, validate,
};
use crate::text::{escape_html, sanitize_text_field};
use crate::users::UserCaps;

/// Where uploads are served: `/mc-content/uploads` (WordPress's `wp-content/uploads`), under
/// the site's address. A file stored at `2026/11/photo.jpg` is at `{home}/mc-content/uploads/2026/11/photo.jpg`.
///
/// WordPress puts uploads under `siteurl`, where its own files live. Mission Control has no
/// separate place for its files: the app that serves `home` serves the uploads, so `home` is
/// their address.
pub const UPLOADS_PATH: &str = "/mc-content/uploads";

/// The attachment post type the controller serves.
static ATTACHMENT: LazyLock<PostType> = LazyLock::new(PostType::attachment);

/// WordPress's message for a file type it refuses (`wp_check_filetype_and_ext`).
const TYPE_NOT_ALLOWED: &str = "Sorry, you are not allowed to upload this file type.";

/// WordPress's message for an empty file (`_wp_handle_upload`).
const EMPTY_FILE: &str = "File is empty. Please upload something more substantial.";

/// Registers the media routes on `routes`, so discovery lists them: the collection (`GET`,
/// `POST`) and the item (`GET`, `POST`/`PUT`/`PATCH`, `DELETE`).
pub fn register_routes(routes: &RestRoutes) {
    let post_type = PostType::attachment();
    let collection = post_type.route();
    let item = format!("{collection}/(?P<id>[\\d]+)");
    let id = json!({"description": "Unique identifier for the post.", "type": "integer", "required": false});
    let context = json!({
        "description": "Scope under which the request is made; determines fields present in response.",
        "type": "string",
        "enum": ["view", "embed", "edit"],
        "default": "view",
        "required": false,
    });
    let mut write = writable_args();
    write.insert("id".into(), id.clone());
    routes.register(
        RouteSpec::new(
            collection,
            vec![
                Endpoint::new(
                    ["GET"],
                    Value::Object(super::posts::collection_args(&post_type)),
                ),
                Endpoint::new(["POST"], Value::Object(writable_args())),
            ],
        )
        .with_meta(MetaObject::Post, &post_type.name),
    );
    routes.register(
        RouteSpec::new(
            item,
            vec![
                Endpoint::new(["GET"], json!({"id": id, "context": context})),
                Endpoint::new(["POST", "PUT", "PATCH"], Value::Object(write)),
                Endpoint::new(
                    ["DELETE"],
                    json!({
                        "id": id,
                        "force": {
                            "type": "boolean",
                            "default": false,
                            "description": "Whether to bypass Trash and force deletion.",
                            "required": false,
                        },
                    }),
                ),
            ],
        )
        .with_meta(MetaObject::Post, &post_type.name),
    );
}

/// The arguments of an upload or an update, as discovery lists the writable fields.
fn writable_args() -> Map<String, Value> {
    let raw_object = |description: &str| json!({"description": description, "type": "object", "properties": {"raw": {"type": "string"}}, "required": false});
    let mut args = Map::new();
    args.insert("title".into(), raw_object("The title for the post."));
    args.insert("caption".into(), raw_object("The attachment caption."));
    args.insert(
        "description".into(),
        raw_object("The attachment description."),
    );
    args.insert(
        "alt_text".into(),
        json!({"description": "Alternative text to display when attachment is not displayed.", "type": "string", "required": false}),
    );
    args.insert(
        "post".into(),
        json!({"description": "The ID for the associated post of the attachment.", "type": "integer", "required": false}),
    );
    args.insert(
        "meta".into(),
        json!({"description": "Meta fields.", "type": "object", "properties": [], "required": false}),
    );
    args
}

/// The file an upload request carries, as the route read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaUpload {
    /// The body is the file, as `upload_from_data` reads it: its name comes from the
    /// `Content-Disposition` header.
    Raw {
        /// The `Content-Type` header, which must be sent (its value is not trusted: the type
        /// comes from the file).
        content_type: Option<String>,
        /// The `Content-Disposition` header.
        content_disposition: Option<String>,
        /// The body.
        bytes: Vec<u8>,
    },
    /// A `multipart/form-data` body, as `upload_from_file` reads `$_FILES['file']`.
    Multipart {
        /// The `file` part, if the body has one.
        file: Option<UploadedFile>,
    },
}

/// A file part of a multipart body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    /// The part's file name, as the client sent it.
    pub file_name: String,
    /// The part's bytes.
    pub bytes: Vec<u8>,
}

/// `rest_upload_file_too_big` (400): the file is over `limit` bytes, the upload limit
/// (`MC_UPLOAD_MAX_BYTES`). The route answers with it when the body is larger than the limit
/// allows, before reading past it.
pub fn upload_too_big(limit: u64) -> RestError {
    RestError::new(
        "rest_upload_file_too_big",
        format!(
            "This file is too big. Files must be less than {} KB in size.",
            limit / 1024
        ),
        400,
    )
}

/// The file name in a `Content-Disposition` header, as `get_filename_from_disposition` reads
/// it: the `filename` attribute after the type (`attachment; filename="photo.jpg"`), unquoted.
/// `None` when there is none.
pub fn filename_from_disposition(header: &str) -> Option<String> {
    let (_, attributes) = header.trim().split_once(';')?;
    let mut filename = None;
    for part in attributes.split(';') {
        if let Some((key, value)) = part.split_once('=')
            && key.trim() == "filename"
        {
            filename = Some(value.trim().to_owned());
        }
    }
    let filename = filename.filter(|f| !f.is_empty())?;
    let unquoted = filename
        .strip_prefix('"')
        .and_then(|f| f.strip_suffix('"'))
        .map_or(filename.as_str(), |f| f);
    Some(unquoted.to_owned())
}

/// One request to the media controller.
///
/// ```ignore
/// let controller = MediaController {
///     db: &db,
///     hooks: &hooks,
///     user: &caps,
///     kses: &kses,
///     storage: &storage,
///     sizes: &sizes,
///     upload_max_bytes: config.upload_max_bytes,
///     meta: &meta_keys,
///     post_types: &post_types,
/// };
/// let response = controller.list("media_type=image").await?;
/// ```
pub struct MediaController<'a> {
    /// The database.
    pub db: &'a Database,
    /// The hooks, for `the_content` in a rendered description.
    pub hooks: &'a Hooks,
    /// The current user's capabilities: a visitor's (id 0, no role) when nobody signed in.
    pub user: &'a UserCaps,
    /// Where writes filter the title, caption and description.
    pub kses: &'a KsesLimiter,
    /// Where the files are stored.
    pub storage: &'a Arc<dyn Storage>,
    /// The registered image sizes, made for each uploaded image.
    pub sizes: &'a Arc<ImageSizes>,
    /// The largest file accepted, in bytes (`MC_UPLOAD_MAX_BYTES`).
    pub upload_max_bytes: u64,
    /// The registered meta keys, shown and written under `meta`.
    pub meta: &'a MetaKeys,
    /// The registered post types, for the capability names of the post media is attached to.
    pub post_types: &'a PostTypes,
}

/// The fields an upload or an update writes. `None` leaves a field as it is.
#[derive(Debug, Default)]
struct Changes {
    title: Option<String>,
    caption: Option<String>,
    description: Option<String>,
    alt_text: Option<String>,
    /// `Some(None)` detaches the attachment (`post` 0).
    post: Option<Option<i64>>,
}

impl Changes {
    /// Reads the writable fields from `params`, as the schema validates them.
    fn read(params: &Params) -> Result<Self, RestError> {
        let mut r = Reader::new(params);
        let changes = Self {
            title: raw_text(&mut r, "title"),
            caption: raw_text(&mut r, "caption"),
            description: raw_text(&mut r, "description"),
            alt_text: r.string("alt_text").map(|alt| sanitize_text_field(&alt)),
            post: r
                .integer("post", Some(0), None)
                .map(|p| (p > 0).then_some(p)),
        };
        if let Some(meta) = r.raw("meta")
            && !meta.is_object()
            && !meta.as_array().is_some_and(Vec::is_empty)
        {
            r.invalid.wrong_type("meta", "object");
        }
        r.invalid.finish()?;
        Ok(changes)
    }
}

/// A `title`, `caption` or `description` parameter: a string, or an object with `raw`.
fn raw_text(r: &mut Reader<'_>, name: &str) -> Option<String> {
    match r.raw(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Object(object) => match object.get("raw") {
            Some(Value::String(s)) => Some(s.clone()),
            None => None,
            Some(_) => {
                r.invalid.wrong_type(&format!("{name}[raw]"), "string");
                None
            }
        },
        _ => {
            r.invalid.wrong_type(name, "object");
            None
        }
    }
}

/// `rest_invalid_param` for a parent that cannot have attachments: a revision or an attachment.
fn invalid_parent_type() -> RestError {
    RestError::new("rest_invalid_param", "Invalid parent type.", 400)
}

impl MediaController<'_> {
    /// The posts controller for attachments, run as the same user.
    fn posts(&self) -> PostsController<'_> {
        PostsController {
            db: self.db,
            hooks: self.hooks,
            post_type: &ATTACHMENT,
            user: self.user,
            kses: self.kses,
            theme: None,
            image_sizes: Some(self.sizes),
            meta: self.meta,
            post_types: self.post_types,
        }
    }

    /// The request's `meta`, checked against the keys registered for attachments.
    fn read_meta(&self, params: &Params) -> Result<MetaUpdate, RestError> {
        MetaUpdate::read(self.meta, MetaObject::Post, &ATTACHMENT.name, params)
    }

    /// `GET /mc/v2/media`: page `page` of the attachments the query asks for.
    ///
    /// # Errors
    ///
    /// As [`PostsController::list`].
    pub async fn list(&self, query: &str) -> Result<RestResponse, RestError> {
        self.posts().list(query).await
    }

    /// `GET /mc/v2/media/{id}`.
    ///
    /// # Errors
    ///
    /// As [`PostsController::get`]: `rest_post_invalid_id` (404) when there is no such
    /// attachment.
    pub async fn get(&self, id: i64, query: &str) -> Result<RestResponse, RestError> {
        self.posts().get(id, query).await
    }

    /// Whether the user may upload at all: the attachment type's `create_posts`, which is
    /// `upload_files`. The route asks before it reads a body, so a user who may not upload never
    /// sends a file into memory.
    ///
    /// # Errors
    ///
    /// `rest_cannot_create` with `rest_authorization_required_code`'s status, as
    /// `WP_REST_Posts_Controller::create_item_permissions_check` refuses it.
    pub fn check_upload_permission(&self) -> Result<(), RestError> {
        let posts = self.posts();
        if posts.has("upload_files") {
            Ok(())
        } else {
            Err(posts.forbidden(
                "rest_cannot_create",
                "Sorry, you are not allowed to create posts as this user.",
            ))
        }
    }

    /// `POST /mc/v2/media`: stores `upload` and creates its attachment from `params` (the
    /// query string, and a multipart body's other fields over it). `content_md5` is the
    /// `Content-MD5` header, checked against the file when sent.
    ///
    /// # Errors
    ///
    /// WordPress's errors for a bad parameter, a missing capability or parent, a missing or
    /// refused file (`rest_upload_*`), and `rest_upload_hash_mismatch` (412).
    pub async fn create(
        &self,
        params: &Params,
        upload: MediaUpload,
        content_md5: Option<&str>,
    ) -> Result<RestResponse, RestError> {
        let changes = Changes::read(params)?;
        let meta = self.read_meta(params)?;
        if params
            .get("id")
            .is_some_and(|id| as_integer(id).is_some_and(|id| id != 0))
        {
            return Err(RestError::new(
                "rest_post_exists",
                "Cannot create existing post.",
                400,
            ));
        }
        self.check_upload_permission()?;
        let posts = self.posts();
        let site = Site::load(self.db).await?;
        let parent = changes.post.flatten();
        if let Some(parent) = parent {
            // Attaching media to a post requires the ability to edit that post; a post that
            // does not exist cannot be edited.
            let found = Post::query()
                .find(self.db, parent)
                .await
                .map_err(|e| database_error(&e))?;
            let editable = match &found {
                Some(post) => {
                    let trash = posts.trash_status(post).await?;
                    posts.can_on("edit_post", post, trash, &site)
                }
                None => false,
            };
            if !editable {
                return Err(posts.forbidden(
                    "rest_cannot_edit",
                    "Sorry, you are not allowed to upload media to this post.",
                ));
            }
            if found.is_some_and(|p| p.post_type == "revision" || p.post_type == "attachment") {
                return Err(invalid_parent_type());
            }
        }
        let (client_name, bytes, multipart) = self.file_of(upload)?;
        if let Some(expected) = content_md5 {
            let actual = Md5::digest(&bytes);
            let actual: String = actual.iter().map(|b| format!("{b:02x}")).collect();
            if !actual.eq_ignore_ascii_case(expected.trim()) {
                return Err(RestError::new(
                    "rest_upload_hash_mismatch",
                    "Content hash did not match expected.",
                    412,
                ));
            }
        }
        let validated = validate(&client_name, &bytes, self.upload_max_bytes)
            .map_err(|e| self.upload_error(&e, multipart))?;
        let fields = self
            .kses
            .filter(
                self.user,
                PostFields {
                    title: changes.title.clone().unwrap_or_default(),
                    content: changes.description.clone().unwrap_or_default(),
                    excerpt: changes.caption.clone().unwrap_or_default(),
                },
            )
            .await?;
        let title = if fields.title.is_empty() {
            AttachmentTitle::StoredName
        } else {
            AttachmentTitle::Given(&fields.title)
        };
        let attachment = create_attachment_titled(
            self.db,
            self.storage,
            self.sizes,
            &validated,
            parent,
            self.user.id(),
            title,
        )
        .await
        .map_err(|e| attachment_error(&e))?;
        let alt = changes.alt_text.unwrap_or_default();
        if !fields.content.is_empty() || !fields.excerpt.is_empty() || !alt.is_empty() {
            update_attachment_details(
                self.db,
                attachment.id,
                &attachment.title,
                &alt,
                &fields.excerpt,
                &fields.content,
            )
            .await
            .map_err(|e| attachment_error(&e))?;
        }
        // As `create_item`, the meta is checked on the new attachment, so a refused key leaves it
        // saved.
        let saved = posts.find(attachment.id).await?;
        let can_edit = posts.can_on("edit_post", &saved, None, &site);
        meta.authorize(self.db, self.user, attachment.id, can_edit)
            .await?
            .apply(self.db)
            .await?;
        let response = posts.edit_response(attachment.id, 201).await?;
        let location = site.rest_url(&format!("mc/v2/media/{}", attachment.id));
        Ok(response.with_header("Location", location))
    }

    /// The client's file name and the bytes of `upload`, and whether it came as multipart, or
    /// WordPress's error for a request with no usable file.
    fn file_of(&self, upload: MediaUpload) -> Result<(String, Vec<u8>, bool), RestError> {
        let no_data = || RestError::new("rest_upload_no_data", "No data supplied.", 400);
        match upload {
            MediaUpload::Multipart { file: None } => Err(no_data()),
            MediaUpload::Multipart { file: Some(file) } => Ok((file.file_name, file.bytes, true)),
            MediaUpload::Raw {
                content_type,
                content_disposition,
                bytes,
            } => {
                if bytes.is_empty() {
                    return Err(no_data());
                }
                if content_type.as_deref().is_none_or(|t| t.trim().is_empty()) {
                    return Err(RestError::new(
                        "rest_upload_no_content_type",
                        "No Content-Type supplied.",
                        400,
                    ));
                }
                let Some(disposition) = content_disposition else {
                    return Err(RestError::new(
                        "rest_upload_no_content_disposition",
                        "No Content-Disposition supplied.",
                        400,
                    ));
                };
                let Some(file_name) = filename_from_disposition(&disposition) else {
                    return Err(RestError::new(
                        "rest_upload_invalid_disposition",
                        "Invalid Content-Disposition supplied. Content-Disposition needs to be \
                         formatted as `attachment; filename=\"image.png\"` or similar.",
                        400,
                    ));
                };
                Ok((file_name, bytes, false))
            }
        }
    }

    /// WordPress's error for a refused file: `rest_upload_file_too_big` over the limit,
    /// otherwise `rest_upload_unknown_error` (multipart) or `rest_upload_sideload_error` (raw
    /// body) with WordPress's message, status 400.
    fn upload_error(&self, error: &UploadError, multipart: bool) -> RestError {
        let code = if multipart {
            "rest_upload_unknown_error"
        } else {
            "rest_upload_sideload_error"
        };
        match error {
            UploadError::TooLarge { limit, .. } => upload_too_big(*limit),
            UploadError::Empty => RestError::new(code, EMPTY_FILE, 400),
            UploadError::TypeNotAllowed { .. } | UploadError::ContentMismatch { .. } => {
                RestError::new(code, TYPE_NOT_ALLOWED, 400)
            }
            UploadError::NoUniqueName(_) | UploadError::Storage(_) => database_error(error),
        }
    }

    /// `POST`, `PUT` or `PATCH /mc/v2/media/{id}`: sets the attachment's title, caption,
    /// description, alt text and post from `params`.
    ///
    /// # Errors
    ///
    /// `rest_post_invalid_id` (404) when there is no such attachment, a bad parameter,
    /// `rest_cannot_edit`, and `rest_invalid_param` for a parent that is a revision or an
    /// attachment.
    pub async fn update(&self, id: i64, params: &Params) -> Result<RestResponse, RestError> {
        // WordPress validates the parameters before the route looks the attachment up.
        let changes = Changes::read(params)?;
        let posts = self.posts();
        let stored = posts.find(id).await?;
        let site = Site::load(self.db).await?;
        let trash = posts.trash_status(&stored).await?;
        if !posts.can_on("edit_post", &stored, trash, &site) {
            return Err(posts.forbidden(
                "rest_cannot_edit",
                "Sorry, you are not allowed to edit this post.",
            ));
        }
        // As `update_item`, the meta's values are checked once the user may edit the attachment.
        let meta = self.read_meta(params)?;
        if let Some(Some(parent)) = changes.post {
            let found = Post::query()
                .find(self.db, parent)
                .await
                .map_err(|e| database_error(&e))?;
            match found {
                None => {
                    return Err(RestError::new(
                        "rest_post_invalid_id",
                        "Invalid post parent ID.",
                        400,
                    ));
                }
                Some(p) if p.post_type == "revision" || p.post_type == "attachment" => {
                    return Err(invalid_parent_type());
                }
                Some(_) => {}
            }
        }
        // Checked before anything is written, so a refused key leaves the attachment as it was.
        let meta = meta.authorize(self.db, self.user, id, true).await?;
        let current = get_attachment(self.db, id)
            .await
            .map_err(|e| attachment_error(&e))?
            .ok_or_else(invalid_id)?;
        // kses runs on the whole attachment, as `wp_update_post` filters the merged fields.
        let fields = self
            .kses
            .filter(
                self.user,
                PostFields {
                    title: changes.title.unwrap_or(current.title),
                    content: changes.description.unwrap_or(current.description),
                    excerpt: changes.caption.unwrap_or(current.caption),
                },
            )
            .await?;
        let alt = changes.alt_text.unwrap_or(current.alt);
        update_attachment_details(
            self.db,
            id,
            &fields.title,
            &alt,
            &fields.excerpt,
            &fields.content,
        )
        .await
        .map_err(|e| attachment_error(&e))?;
        if let Some(parent) = changes.post
            && parent != current.parent_id
        {
            Post::query()
                .where_eq(Post::PRIMARY_KEY, id)
                .update(self.db, &[("parent_id", parent.into())])
                .await
                .map_err(|e| database_error(&e))?;
        }
        meta.apply(self.db).await?;
        posts.edit_response(id, 200).await
    }

    /// `DELETE /mc/v2/media/{id}`: deletes the attachment and its files, which needs `force`.
    ///
    /// # Errors
    ///
    /// `rest_post_invalid_id`, `rest_cannot_delete`, and `rest_trash_not_supported` (501)
    /// without `force`.
    pub async fn delete(&self, id: i64, params: &Params) -> Result<RestResponse, RestError> {
        let mut r = Reader::new(params);
        let force = r.boolean("force").unwrap_or(false);
        r.invalid.finish()?;
        let posts = self.posts();
        let post = posts.find(id).await?;
        let site = Site::load(self.db).await?;
        let trash = posts.trash_status(&post).await?;
        if !posts.can_on("delete_post", &post, trash, &site) {
            return Err(posts.forbidden(
                "rest_cannot_delete",
                "Sorry, you are not allowed to delete this post.",
            ));
        }
        if !force {
            return Err(RestError::new(
                "rest_trash_not_supported",
                "The post does not support trashing. Set 'force=true' to delete.",
                501,
            ));
        }
        let previous = posts.edit_response(id, 200).await?.body;
        let deleted = delete_attachment(self.db, self.storage, id)
            .await
            .map_err(|e| attachment_error(&e))?;
        if !deleted {
            return Err(RestError::new(
                "rest_cannot_delete",
                "The post cannot be deleted.",
                500,
            ));
        }
        Ok(RestResponse::ok(
            json!({"deleted": true, "previous": previous}),
        ))
    }
}

/// An attachment error as a REST error: a parent that cannot have attachments is
/// `rest_invalid_param`, a missing attachment `rest_post_invalid_id`, anything else a 500,
/// logged.
fn attachment_error(error: &AttachmentError) -> RestError {
    match error {
        AttachmentError::InvalidParent(_) => invalid_parent_type(),
        AttachmentError::NotFound(_) => invalid_id(),
        other => database_error(other),
    }
}

/// The uploads' base address, `wp_upload_dir`'s `baseurl`: `{home}/mc-content/uploads` (see
/// [`UPLOADS_PATH`]).
fn uploads_url(site: &Site) -> String {
    format!("{}{UPLOADS_PATH}", site.home)
}

/// A meta value read as text: a JSON string as it is, anything else empty.
fn text_meta(batch: &Batch, id: i64, key: &str) -> String {
    match batch.meta.get(&(id, key.to_owned())) {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// The address of attachment `id`'s file, as `wp_get_attachment_url` gives it; empty when it
/// has no file.
pub(crate) fn source_url(site: &Site, batch: &Batch, id: i64) -> String {
    let file = text_meta(batch, id, ATTACHED_FILE);
    if file.is_empty() {
        String::new()
    } else {
        format!("{}/{file}", uploads_url(site))
    }
}

/// An image's `_wp_attachment_metadata`, when it is readable.
fn metadata(batch: &Batch, id: i64) -> Option<AttachmentMetadata> {
    let value = batch.meta.get(&(id, ATTACHMENT_METADATA.to_owned()))?;
    serde_json::from_value(value.clone()).ok()
}

/// The `image_meta` of an image whose EXIF and IPTC data were not read: WordPress's empty
/// values (`wp_read_image_metadata`'s defaults).
fn empty_image_meta() -> Value {
    json!({
        "aperture": "0",
        "credit": "",
        "camera": "",
        "caption": "",
        "created_timestamp": "0",
        "copyright": "",
        "focal_length": "0",
        "iso": "0",
        "shutter_speed": "0",
        "title": "",
        "orientation": "0",
        "keywords": [],
    })
}

/// `media_details`: an image's size, file, sub-sizes (each with its `source_url`, and `full`
/// for the original) and `image_meta`; `{}` for any other file.
fn media_details(site: &Site, metadata: Option<&AttachmentMetadata>, mime_type: &str) -> Value {
    let Some(metadata) = metadata else {
        return json!({});
    };
    let base = uploads_url(site);
    let folder = metadata
        .file
        .rsplit_once('/')
        .map_or(String::new(), |(folder, _)| format!("{folder}/"));
    let mut sizes = Map::new();
    for (name, size) in &metadata.sizes {
        sizes.insert(
            name.clone(),
            json!({
                "file": size.file,
                "width": size.width,
                "height": size.height,
                "filesize": size.filesize,
                "mime_type": size.mime_type,
                "source_url": format!("{base}/{folder}{}", size.file),
            }),
        );
    }
    if !sizes.is_empty() {
        let file = metadata.file.rsplit('/').next().unwrap_or(&metadata.file);
        sizes.insert(
            "full".to_owned(),
            json!({
                "file": file,
                "width": metadata.width,
                "height": metadata.height,
                "mime_type": mime_type,
                "source_url": format!("{base}/{}", metadata.file),
            }),
        );
    }
    json!({
        "width": metadata.width,
        "height": metadata.height,
        "file": metadata.file,
        "filesize": metadata.filesize,
        "sizes": sizes,
        "image_meta": empty_image_meta(),
    })
}

/// The registered sizes an image should have and does not, as
/// `wp_get_missing_image_subsizes` finds them: a size the image is larger than, with no sub-size
/// stored under its name.
fn missing_image_sizes(
    sizes: Option<&ImageSizes>,
    metadata: Option<&AttachmentMetadata>,
) -> Vec<String> {
    let (Some(sizes), Some(metadata)) = (sizes, metadata) else {
        return Vec::new();
    };
    sizes
        .registered()
        .into_iter()
        .filter(|(name, size)| {
            !metadata.sizes.contains_key(name)
                && image_resize_dimensions(
                    metadata.width,
                    metadata.height,
                    size.width,
                    size.height,
                    size.crop,
                )
                .is_some()
        })
        .map(|(name, _)| name)
        .collect()
}

/// Adds what `WP_REST_Attachments_Controller::prepare_item_for_response` adds to a post's JSON:
/// `description`, `caption`, `alt_text`, `media_type`, `mime_type`, `media_details`, `post`,
/// `source_url` and, in the edit context, `missing_image_sizes`. The context filter runs after.
pub(crate) fn add_fields(
    item: &mut Map<String, Value>,
    ctl: &PostsController<'_>,
    site: &Site,
    post: &Post,
    batch: &Batch,
    context: Context,
    fields: &Fields,
) {
    let mut put = |key: &str, value: Value| {
        item.insert(key.to_owned(), value);
    };
    // Rendered only when it is shown: `the_content` runs the shortcodes.
    let needs_rendered = fields.includes("description.rendered")
        || (post.excerpt.trim().is_empty() && fields.includes("caption.rendered"));
    let rendered_description = if needs_rendered {
        ctl.hooks.apply_filters(THE_CONTENT, post.content.clone())
    } else {
        String::new()
    };
    put(
        "description",
        json!({"raw": post.content, "rendered": rendered_description}),
    );
    // `the_excerpt` of `get_the_excerpt`: the caption, or, with none, the trimmed description.
    // Ruling R34, as the front end: a caption stays text.
    let caption_text = if post.excerpt.trim().is_empty() {
        trim_excerpt(&rendered_description)
    } else {
        escape_html(&post.excerpt)
    };
    let caption = if caption_text.is_empty() {
        String::new()
    } else {
        format!("<p>{caption_text}</p>\n")
    };
    put("caption", json!({"raw": post.excerpt, "rendered": caption}));
    put("alt_text", json!(text_meta(batch, post.id, ATTACHMENT_ALT)));
    let is_image = post.mime_type.starts_with("image/");
    put("media_type", json!(if is_image { "image" } else { "file" }));
    put("mime_type", json!(post.mime_type));
    let metadata = metadata(batch, post.id);
    put(
        "media_details",
        media_details(site, metadata.as_ref(), &post.mime_type),
    );
    put("post", json!(post.parent_id.filter(|p| *p > 0)));
    put("source_url", json!(source_url(site, batch, post.id)));
    if context == Context::Edit {
        let missing = if is_image {
            missing_image_sizes(ctl.image_sizes, metadata.as_ref())
        } else {
            Vec::new()
        };
        put("missing_image_sizes", json!(missing));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_name_comes_from_the_disposition() {
        for (header, name) in [
            ("attachment; filename=\"photo.jpg\"", Some("photo.jpg")),
            ("attachment; filename=photo.jpg", Some("photo.jpg")),
            ("attachment;filename=\"a b.png\" ", Some("a b.png")),
            ("form-data; name=\"x\"; filename=\"c.pdf\"", Some("c.pdf")),
            ("attachment", None),
            ("filename=photo.jpg", None),
            ("attachment; filename=", None),
            ("attachment; name=photo.jpg", None),
        ] {
            assert_eq!(
                filename_from_disposition(header).as_deref(),
                name,
                "{header}"
            );
        }
    }

    #[test]
    fn too_big_names_the_limit_in_kb() {
        let error = upload_too_big(2048);
        assert_eq!(
            (error.code.as_ref(), error.status),
            ("rest_upload_file_too_big", 400)
        );
        assert_eq!(
            error.message,
            "This file is too big. Files must be less than 2 KB in size."
        );
    }
}
