//! The media library in the admin (Task 6.4, Rulings R127 to R129): the screen of `attachment`,
//! the endpoints the editor's Add Media picker uses, the upload page, and the media library of
//! Ignite's media fields. The rules and queries are the core's
//! ([`mission_control_core::admin::media`]); this module is the Rocket side.
//!
//! # What is mounted
//!
//! - The screen: the `attachment` runtime resource at `/mc-admin/attachment` (list, edit form,
//!   Delete), behind the admin's authorization fairing as the post type screens are (Ruling
//!   R127).
//! - `GET /mc-admin/attachment/new` (WordPress's `media-new.php`, Ruling R129): a small page of
//!   its own with a file field, which posts (`POST /mc-admin/attachment/new`, the browser
//!   uploader) and then goes to the new attachment's edit form. It works without JavaScript.
//! - `POST /mc-admin/async-upload.php` (Ruling R128): one file in a `multipart/form-data` body
//!   (the part `async-upload`, and an optional `post_id` to attach it to), answered with JSON:
//!   `{"success": true, "data": <the attachment>}`, or `{"success": false, "data": {"message",
//!   "filename"}}` with the error's status.
//! - `GET /mc-admin/admin-ajax.php?action=query-attachments` (Ruling R128), with `search`,
//!   `page`, `per_page` (at most 100, 40 by default) and `type` (`image`, ...): `{"success":
//!   true, "data": [...]}`, with the totals in `X-MC-Total` and `X-MC-TotalPages` (WordPress's
//!   `X-WP-*` headers, in Mission Control's names). Another `action` answers 400 with `0`, as
//!   `admin-ajax.php` does.
//! - [`Attachments`] registered as Ignite's media library, so the Image, File and Gallery fields
//!   show the stored attachment and refuse an id that is not one, and link to the screen.
//!
//! # Who may use them
//!
//! Every endpoint needs a signed-in admin user (Ignite's session, [`AdminUser`]); a visitor gets
//! 401 (JSON for the two endpoints; the upload page goes to the sign-in page). Uploading and
//! querying need `upload_files`, and attaching to a post `edit_post` on it. A write is checked
//! against Ignite's CSRF token: the `X-CSRF-TOKEN` header, or the `_token` field of the body,
//! compared in constant time with the browser session's token (Ignite's [`CsrfToken`]); a
//! missing or wrong token answers 419, as Ignite's own forms do. Not the REST API: its
//! cookie-and-nonce sign-in stays deferred (Ruling R46).
//!
//! Bodies are read with a hard cap: the upload limit (`MC_UPLOAD_MAX_BYTES`) and
//! [`MULTIPART_OVERHEAD`] for the other parts; a body over it, or a `Content-Length` saying so,
//! answers 413 without reading further. A user who may not upload is refused before the body
//! is read.

use std::io::Cursor;
use std::sync::Arc;

use ignite_panel::{AdminUser, CsrfToken, ignite_core};
use mission_control_core::admin::media::{
    AttachmentQuery, Attachments, UploadRefusal, attachment_for_js, may_upload, query_attachments,
    register_attachment_resource, upload_to_library,
};
use mission_control_core::media::{ImageSizes, Storage, validate};
use mission_control_core::types::PostTypeResources;
use mission_control_core::users::{PostTypeCaps, UserCaps};
use orbit_orm::Database;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Header, Method, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::{self, Redirect, Responder, content::RawHtml};
use rocket::{Build, Data, Request, Response, Rocket, Route};
use serde_json::{Value, json};

use super::ADMIN_PATH;
use crate::editor::{editor_stylesheet_url, media_grid_script_url};
use crate::rest::auth::AuthSettings;
use crate::rest::media::{BodyError, MULTIPART_OVERHEAD, UploadSettings, read_body};

/// The slug of the media library screen.
pub(crate) const ATTACHMENT: &str = mission_control_core::admin::media::ATTACHMENT;

/// The rank of the media routes: before Ignite's `/<slug>` and `/<slug>/new` routes (ranks -12
/// to -1) and the post types' New route (-20), whose `/<_>/new` would also match
/// `attachment/new`.
const MEDIA_RANK: isize = -21;

/// The multipart part that holds the file, as WordPress's uploaders name it.
const FILE_PART: &str = "async-upload";

/// Whether this app registered the media library screen. Managed when the panel mounts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MediaScreen {
    /// True when the `attachment` resource on Ignite is this app's.
    pub(crate) registered: bool,
}

/// Registers the media library screen and Ignite's media library, and mounts the media routes
/// on `rocket`; see the [module](self). The screen is not registered when Ignite already has a
/// resource at `attachment` (another app in the process): the fairing then refuses it (fail
/// closed), logged.
pub(crate) fn mount_media(rocket: Rocket<Build>) -> (Rocket<Build>, MediaScreen) {
    let Some(storage) = rocket.state::<Arc<dyn Storage>>().cloned() else {
        log::error!("The media library is not mounted: the uploads' storage is missing");
        return (rocket, MediaScreen { registered: false });
    };
    let registered = match register_attachment_resource(storage) {
        Ok(()) => true,
        Err(e) => {
            log::error!(
                "The media library screen is off: Ignite refused the `{ATTACHMENT}` resource \
                 ({e}). Its screens answer 503 until the app starts alone."
            );
            false
        }
    };
    ignite_core::register_media_library(Attachments::new(Some(format!(
        "{ADMIN_PATH}/{ATTACHMENT}"
    ))));
    let screen = MediaScreen { registered };
    (
        rocket
            .mount(ADMIN_PATH, routes())
            .manage(screen)
            .attach(MediaViews),
        screen,
    )
}

/// The media routes, at [`MEDIA_RANK`].
fn routes() -> Vec<Route> {
    let mut routes = rocket::routes![async_upload, admin_ajax, upload_page_route, browser_upload];
    for route in &mut routes {
        route.rank = MEDIA_RANK;
    }
    routes
}

// ---------------------------------------------------------------------------------------------
// Guards

/// Who sends the request: a visitor, a signed-in admin user with their capabilities, or
/// nobody known because the session or the roles could not be read (the database is down).
enum Session {
    /// Nobody is signed in.
    Visitor,
    /// The signed-in admin user's capabilities.
    User(UserCaps),
    /// The session or the roles could not be read (logged): answered with 500, not as a
    /// signed-out request.
    Failed,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Session {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let user = match request.guard::<AdminUser>().await {
            Outcome::Success(user) => user,
            // Ignite's guard fails with 500 when the session cannot be read.
            Outcome::Error((status, _)) if status.code >= 500 => {
                return Outcome::Success(Self::Failed);
            }
            _ => return Outcome::Success(Self::Visitor),
        };
        let rocket = request.rocket();
        let (Some(db), Some(settings)) = (
            rocket.state::<Arc<Database>>(),
            rocket.state::<AuthSettings>(),
        ) else {
            log::error!("The media routes ran before the app was ready");
            return Outcome::Success(Self::Failed);
        };
        Outcome::Success(match settings.user_roles.roles(db, user.user_id()).await {
            Ok(roles) => Self::User(roles.user_caps(user.user_id())),
            Err(e) => {
                log::error!("Could not read the roles of user {}: {e}", user.user_id());
                Self::Failed
            }
        })
    }
}

/// What the upload routes need from the app.
struct App<'r> {
    db: &'r Arc<Database>,
    storage: &'r Arc<dyn Storage>,
    sizes: &'r Arc<ImageSizes>,
    max_bytes: u64,
    resources: Option<&'r Arc<PostTypeResources>>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for App<'r> {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let rocket = request.rocket();
        match (
            rocket.state::<Arc<Database>>(),
            rocket.state::<Arc<dyn Storage>>(),
            rocket.state::<Arc<ImageSizes>>(),
            rocket.state::<UploadSettings>(),
        ) {
            (Some(db), Some(storage), Some(sizes), Some(settings)) => Outcome::Success(Self {
                db,
                storage,
                sizes,
                max_bytes: settings.max_bytes,
                resources: rocket.state::<Arc<PostTypeResources>>(),
            }),
            _ => {
                log::error!("The media routes ran before the app was ready");
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

impl App<'_> {
    /// The capabilities of a post type, for `edit_post` on the post an upload is attached to:
    /// the definition's when the admin knows the type, WordPress's defaults otherwise.
    fn caps_of(&self, post_type: &str) -> PostTypeCaps {
        self.resources
            .and_then(|resources| {
                resources
                    .post_types()
                    .into_iter()
                    .find(|def| def.slug == post_type)
            })
            .map_or_else(|| PostTypeCaps::for_post_type(post_type), |def| def.caps())
    }

    /// The largest body read: the upload limit and the other parts.
    fn cap(&self) -> u64 {
        self.max_bytes.saturating_add(MULTIPART_OVERHEAD)
    }
}

/// The headers an upload reads.
struct UploadHeaders {
    csrf: Option<String>,
    content_length: Option<u64>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for UploadHeaders {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let headers = request.headers();
        Outcome::Success(Self {
            csrf: headers.get_one("X-CSRF-TOKEN").map(str::to_owned),
            content_length: headers
                .get_one("Content-Length")
                .and_then(|l| l.trim().parse().ok()),
        })
    }
}

/// Whether `submitted` is the session's CSRF token, compared without stopping at the first
/// difference.
pub(super) fn token_matches(expected: &CsrfToken, submitted: Option<&str>) -> bool {
    let Some(submitted) = submitted else {
        return false;
    };
    let (a, b) = (expected.value().as_bytes(), submitted.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------------------------
// Reading an upload

/// Why an upload request was refused before the file was checked.
enum RequestError {
    /// The CSRF token is missing or wrong (419).
    Token,
    /// The body is over the cap (413).
    TooLarge(u64),
    /// The body is not a readable multipart form (400).
    Unreadable,
}

/// One upload request's body, read: the file part and the other fields.
struct UploadForm {
    file: Option<(String, Vec<u8>)>,
    token: Option<String>,
    post_id: Option<String>,
}

/// Reads an upload body of at most `cap` bytes (`max` is the upload limit, for the message).
async fn read_upload(
    data: Data<'_>,
    content_type: Option<&ContentType>,
    content_length: Option<u64>,
    cap: u64,
    max: u64,
) -> Result<UploadForm, RequestError> {
    let Some(boundary) = content_type
        .filter(|c| c.top() == "multipart" && c.sub() == "form-data")
        .and_then(|c| c.param("boundary"))
        .map(str::to_owned)
    else {
        return Err(RequestError::Unreadable);
    };
    let body = read_body(data, content_length, cap)
        .await
        .map_err(|e| match e {
            BodyError::TooLarge => RequestError::TooLarge(max),
            BodyError::Unreadable => RequestError::Unreadable,
        })?;
    let mut multipart = multer::Multipart::with_reader(Cursor::new(body), boundary);
    let mut form = UploadForm {
        file: None,
        token: None,
        post_id: None,
    };
    let failed = |e: multer::Error| {
        log::warn!("Could not read a multipart upload: {e}");
        RequestError::Unreadable
    };
    // The body is capped, so the parts are too; only the fields used are kept.
    while let Some(field) = multipart.next_field().await.map_err(failed)? {
        let name = field.name().unwrap_or_default().to_owned();
        if let Some(file_name) = field.file_name().map(str::to_owned) {
            if name == FILE_PART && form.file.is_none() {
                let bytes = field.bytes().await.map_err(failed)?;
                form.file = Some((file_name, bytes.to_vec()));
            }
            continue;
        }
        match name.as_str() {
            "_token" if form.token.is_none() => {
                form.token = Some(field.text().await.map_err(failed)?)
            }
            "post_id" if form.post_id.is_none() => {
                form.post_id = Some(field.text().await.map_err(failed)?);
            }
            _ => {}
        }
    }
    Ok(form)
}

/// The post an upload is attached to: a positive `post_id`, or none (empty, `0` or not a
/// number).
fn parent_of(post_id: Option<&str>) -> Option<i64> {
    post_id
        .and_then(|id| id.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
}

/// Stores the upload in `form` for `caps`, and returns the new attachment's id.
async fn store_upload(
    app: &App<'_>,
    caps: &UserCaps,
    form: UploadForm,
) -> Result<(i64, String), (UploadRefusal, String)> {
    let Some((client_name, bytes)) = form.file else {
        return Err((UploadRefusal::NoFile, String::new()));
    };
    let validated = validate(&client_name, &bytes, app.max_bytes)
        .map_err(|e| (UploadRefusal::File(e), client_name.clone()))?;
    let caps_of = |post_type: &str| app.caps_of(post_type);
    let attachment = upload_to_library(
        app.db,
        app.storage,
        app.sizes,
        caps,
        &validated,
        parent_of(form.post_id.as_deref()),
        &caps_of,
    )
    .await
    .map_err(|e| (e, client_name.clone()))?;
    Ok((attachment.id, client_name))
}

// ---------------------------------------------------------------------------------------------
// JSON answers

/// A JSON answer of the admin's endpoints, never cached, with optional extra headers.
struct Json {
    status: Status,
    body: Value,
    headers: Vec<Header<'static>>,
}

impl Json {
    fn new(status: Status, body: Value) -> Self {
        Self {
            status,
            body,
            headers: Vec::new(),
        }
    }

    /// WordPress's `wp_send_json_error` shape: `{"success": false, "data": {"message", ...}}`.
    fn error(status: u16, message: &str, filename: &str) -> Self {
        let mut data = json!({ "message": message });
        if !filename.is_empty() {
            data["filename"] = json!(filename);
        }
        Self::new(
            Status::new(status),
            json!({ "success": false, "data": data }),
        )
    }
}

impl<'r> Responder<'r, 'static> for Json {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let body = self.body.to_string();
        let mut response = Response::build();
        response
            .status(self.status)
            .header(ContentType::JSON)
            .header(Header::new("Cache-Control", "no-store"))
            .sized_body(body.len(), Cursor::new(body));
        for header in self.headers {
            response.header(header);
        }
        response.ok()
    }
}

/// The JSON of a signed-out request.
fn signed_out() -> Json {
    Json::error(401, "Your session has expired. Please sign in again.", "")
}

/// What a request whose session or roles could not be read is told.
const FAILED: &str = "Something went wrong. Try again in a moment.";

/// The JSON of a request whose session or roles could not be read.
fn server_error() -> Json {
    Json::error(500, FAILED, "")
}

/// The JSON of a refused CSRF token.
fn expired() -> Json {
    Json::error(419, "The link you followed has expired.", "")
}

/// The JSON of a request refused before its file was checked.
fn request_error(error: RequestError) -> Json {
    match error {
        RequestError::Token => expired(),
        RequestError::TooLarge(max) => {
            let refusal = UploadRefusal::TooLarge(max);
            Json::error(refusal.status(), &refusal.message(), "")
        }
        RequestError::Unreadable => Json::error(400, &UploadRefusal::NoFile.message(), ""),
    }
}

/// Attachment `id` for the picker, with its `editLink`.
async fn attachment_json(db: &Database, id: i64) -> Option<Value> {
    match attachment_for_js(db, id).await {
        Ok(Some(mut item)) => {
            item["editLink"] = json!(edit_link(id));
            Some(item)
        }
        Ok(None) => None,
        Err(e) => {
            log::error!("Could not read attachment {id}: {e}");
            None
        }
    }
}

/// The edit form of attachment `id`.
fn edit_link(id: i64) -> String {
    format!("{ADMIN_PATH}/{ATTACHMENT}/{id}/edit")
}

/// `POST /mc-admin/async-upload.php`: one file, stored as an attachment, answered as JSON; see
/// the [module](self).
#[rocket::post("/async-upload.php", data = "<data>")]
async fn async_upload(
    session: Session,
    token: CsrfToken,
    headers: UploadHeaders,
    content_type: Option<&ContentType>,
    app: App<'_>,
    data: Data<'_>,
) -> Json {
    let caps = match session {
        Session::User(caps) => caps,
        Session::Visitor => return signed_out(),
        Session::Failed => return server_error(),
    };
    if !may_upload(&caps) {
        let refusal = UploadRefusal::NotAllowed;
        return Json::error(refusal.status(), &refusal.message(), "");
    }
    // A token in the header is checked before the body is read.
    if headers.csrf.is_some() && !token_matches(&token, headers.csrf.as_deref()) {
        return expired();
    }
    let form = match read_upload(
        data,
        content_type,
        headers.content_length,
        app.cap(),
        app.max_bytes,
    )
    .await
    {
        Ok(form) => form,
        Err(e) => return request_error(e),
    };
    if headers.csrf.is_none() && !token_matches(&token, form.token.as_deref()) {
        return request_error(RequestError::Token);
    }
    match store_upload(&app, &caps, form).await {
        Ok((id, _)) => match attachment_json(app.db, id).await {
            Some(item) => Json::new(Status::Ok, json!({ "success": true, "data": item })),
            None => Json::error(500, &UploadRefusal::Failed.message(), ""),
        },
        Err((refusal, filename)) => Json::error(refusal.status(), &refusal.message(), &filename),
    }
}

/// `GET /mc-admin/admin-ajax.php`: `action=query-attachments`, a page of the attachments the
/// user may read, as JSON; see the [module](self).
#[rocket::get("/admin-ajax.php?<action>&<search>&<page>&<per_page>&<type>")]
async fn admin_ajax(
    session: Session,
    db: &rocket::State<Arc<Database>>,
    action: Option<&str>,
    search: Option<&str>,
    page: Option<&str>,
    per_page: Option<&str>,
    r#type: Option<&str>,
) -> Result<Json, (Status, &'static str)> {
    if action != Some("query-attachments") {
        return Err((Status::BadRequest, "0"));
    }
    let caps = match session {
        Session::User(caps) => caps,
        Session::Visitor => return Ok(signed_out()),
        Session::Failed => return Ok(server_error()),
    };
    if !may_upload(&caps) {
        let refusal = UploadRefusal::NotAllowed;
        return Ok(Json::error(refusal.status(), &refusal.message(), ""));
    }
    let number = |text: Option<&str>| text.and_then(|t| t.trim().parse::<i64>().ok());
    let query = AttachmentQuery {
        search: search.map(str::to_owned).filter(|s| !s.trim().is_empty()),
        page: number(page).unwrap_or(1),
        per_page: number(per_page).unwrap_or(0),
        media_type: r#type.map(str::to_owned),
    };
    match query_attachments(db, &caps, &query).await {
        Ok(found) => {
            let items: Vec<Value> = found
                .items
                .into_iter()
                .map(|mut item| {
                    if let Some(id) = item["id"].as_i64() {
                        item["editLink"] = json!(edit_link(id));
                    }
                    item
                })
                .collect();
            let mut answer = Json::new(Status::Ok, json!({ "success": true, "data": items }));
            answer.headers = vec![
                Header::new("X-MC-Total", found.total.to_string()),
                Header::new("X-MC-TotalPages", found.total_pages.to_string()),
            ];
            Ok(answer)
        }
        Err(e) => {
            log::error!("Could not query the attachments: {e}");
            Ok(Json::error(500, "The media library could not be read.", ""))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The upload page

/// `text` with `&`, `<`, `>`, `"` and `'` escaped.
pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#039;"),
            c => out.push(c),
        }
    }
    out
}

/// WordPress's `media-new.php`: a file field that posts to the browser uploader. `error` is
/// shown above the form; every value is escaped.
fn upload_page(token: &str, max_bytes: u64, error: Option<&str>) -> RawHtml<String> {
    let notice = error.map_or_else(String::new, |message| {
        format!(
            "<div class=\"mc-upload-error\" role=\"alert\"><p>{}</p></div>\n",
            escape(message)
        )
    });
    RawHtml(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>Upload New Media</title>
<link rel="icon" href="data:,">
<link rel="stylesheet" href="{stylesheet}">
</head>
<body class="mc-upload-page">
<main>
<p><a href="{admin}/{slug}">Media Library</a></p>
<h1>Upload New Media</h1>
{notice}<form method="post" action="{admin}/{slug}/new" enctype="multipart/form-data">
<input type="hidden" name="_token" value="{token}">
<p><label for="async-upload">Select a file to upload</label><br>
<input type="file" id="async-upload" name="async-upload" required aria-describedby="mc-upload-limit"></p>
<p><button type="submit">Upload</button></p>
</form>
<p id="mc-upload-limit">Maximum upload file size: {kb} KB.</p>
</main>
</body>
</html>"#,
        stylesheet = escape(&editor_stylesheet_url()),
        admin = ADMIN_PATH,
        slug = ATTACHMENT,
        notice = notice,
        token = escape(token),
        kb = max_bytes / 1024,
    ))
}

/// `GET /mc-admin/attachment/new` (Ruling R129): the upload page. The fairing has checked the
/// screen's capabilities; a visitor fails [`AdminUser`] with 401, which Ignite sends to its
/// sign-in page.
#[rocket::get("/attachment/new")]
fn upload_page_route(
    _user: AdminUser,
    session: Session,
    token: CsrfToken,
    app: App<'_>,
) -> (Status, RawHtml<String>) {
    match session {
        Session::Failed => (
            Status::InternalServerError,
            upload_page(token.value(), app.max_bytes, Some(FAILED)),
        ),
        Session::User(caps) if may_upload(&caps) => {
            (Status::Ok, upload_page(token.value(), app.max_bytes, None))
        }
        _ => (
            Status::Forbidden,
            upload_page(
                token.value(),
                app.max_bytes,
                Some(&UploadRefusal::NotAllowed.message()),
            ),
        ),
    }
}

/// The answer of the browser uploader: the new attachment's edit form, or the page again with
/// the error.
type BrowserUpload = Result<Redirect, (Status, RawHtml<String>)>;

/// `POST /mc-admin/attachment/new`: `media-new.php`'s browser uploader. Stores the file in the
/// `async-upload` part (with the form's `_token`) and goes to its edit form (303), or shows the
/// page again with WordPress's message.
#[rocket::post("/attachment/new", data = "<data>")]
async fn browser_upload(
    _user: AdminUser,
    session: Session,
    token: CsrfToken,
    headers: UploadHeaders,
    content_type: Option<&ContentType>,
    app: App<'_>,
    data: Data<'_>,
) -> BrowserUpload {
    let refused = |status: u16, message: &str| {
        Err((
            Status::new(status),
            upload_page(token.value(), app.max_bytes, Some(message)),
        ))
    };
    let caps = match session {
        Session::User(caps) => caps,
        Session::Visitor => {
            return refused(401, "Your session has expired. Please sign in again.");
        }
        Session::Failed => return refused(500, FAILED),
    };
    if !may_upload(&caps) {
        let refusal = UploadRefusal::NotAllowed;
        return refused(refusal.status(), &refusal.message());
    }
    let form = match read_upload(
        data,
        content_type,
        headers.content_length,
        app.cap(),
        app.max_bytes,
    )
    .await
    {
        Ok(form) => form,
        Err(RequestError::Token) => return refused(419, "The link you followed has expired."),
        Err(RequestError::TooLarge(max)) => {
            let refusal = UploadRefusal::TooLarge(max);
            return refused(refusal.status(), &refusal.message());
        }
        Err(RequestError::Unreadable) => {
            return refused(400, &UploadRefusal::NoFile.message());
        }
    };
    let submitted = form.token.clone().or(headers.csrf);
    if !token_matches(&token, submitted.as_deref()) {
        return refused(419, "The link you followed has expired.");
    }
    match store_upload(&app, &caps, form).await {
        Ok((id, _)) => Ok(Redirect::to(edit_link(id))),
        Err((refusal, filename)) => {
            let message = if filename.is_empty() {
                refusal.message()
            } else {
                format!("“{filename}” has failed to upload. {}", refusal.message())
            };
            refused(refusal.status(), &message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_escapes_what_it_shows() {
        let page = upload_page("tok\"<en>", 2048, Some("<script>x</script> & “it”")).0;
        assert!(page.contains(r#"value="tok&quot;&lt;en&gt;""#), "{page}");
        assert!(
            page.contains("&lt;script&gt;x&lt;/script&gt; &amp; “it”"),
            "{page}"
        );
        assert!(!page.contains("<script>"), "{page}");
        assert!(page.contains("Maximum upload file size: 2 KB."), "{page}");
    }

    #[test]
    fn a_parent_is_a_positive_id() {
        assert_eq!(parent_of(Some("12")), Some(12));
        assert_eq!(parent_of(Some(" 7 ")), Some(7));
        for text in [None, Some(""), Some("0"), Some("-3"), Some("x")] {
            assert_eq!(parent_of(text), None, "{text:?}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The grid and the list (Task A8)

/// The grid, the list and the switch between them, inside the admin layout.
///
/// Both views are Ignite's own screen at `/mc-admin/attachment`, so both render in the active
/// admin theme's layout (sidebar, top bar, content wrapper). This fairing changes the answer
/// of that one `GET`: it adds the grid/list switch under the heading, and in the grid mode,
/// the default, it puts the grid (`assets/editor/grid.js`) where Ignite's table is. WordPress
/// keeps the mode in `media_library_mode`; here it is in the address, `?mode=grid` or
/// `?mode=list`, so the switch is two links and works without JavaScript. Without `mode`, the
/// list's own search, sort and page mean the list, as Ignite's links to them carry no `mode`;
/// the plain address is the grid.
struct MediaViews;

#[rocket::async_trait]
impl Fairing for MediaViews {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control media library views",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let screen = format!("{ADMIN_PATH}/{ATTACHMENT}");
        if request.method() != Method::Get
            || request.uri().path().as_str().trim_end_matches('/') != screen
            || response.status() != Status::Ok
            || response.content_type() != Some(ContentType::HTML)
            || !request
                .rocket()
                .state::<MediaScreen>()
                .is_some_and(|screen| screen.registered)
        {
            return;
        }
        let Some(token) = request.guard::<CsrfToken>().await.succeeded() else {
            return;
        };
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        let html = with_views(&html, wants_grid(request), token.value());
        response.set_header(Header::new("Cache-Control", "no-store"));
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}

/// Whether the request asks for the grid: `mode=grid`, or no `mode` and none of the list's own
/// search, sort or page.
fn wants_grid(request: &Request<'_>) -> bool {
    match request.query_value::<&str>("mode").and_then(Result::ok) {
        Some("grid") => true,
        Some("list") => false,
        _ => !["q", "sort", "dir", "page"]
            .iter()
            .any(|name| request.query_value::<&str>(name).is_some()),
    }
}

/// The marker of the content that Ignite's list view starts with after its heading.
const LIST_START: &str = r#"<section class="ig-card p-0">"#;

/// The grid/list switch: the current view is text, the other a link.
fn view_switch(grid: bool) -> String {
    let (admin, slug) = (ADMIN_PATH, ATTACHMENT);
    let (grid_view, list_view) = if grid {
        (
            "<span aria-current=\"page\">Grid view</span>".to_owned(),
            format!("<a href=\"{admin}/{slug}?mode=list\">List view</a>"),
        )
    } else {
        (
            format!("<a href=\"{admin}/{slug}?mode=grid\">Grid view</a>"),
            "<span aria-current=\"page\">List view</span>".to_owned(),
        )
    };
    format!(
        "<p class=\"mc-view-switch\" role=\"navigation\" aria-label=\"Media view\">{grid_view} | {list_view}</p>\n"
    )
}

/// Ignite's list page `html` with the switch added and, for the `grid`, the grid in place of
/// the table and its pager. A page without the list's markup is returned as it is.
fn with_views(html: &str, grid: bool, token: &str) -> String {
    let Some(start) = html.find(LIST_START) else {
        return html.to_owned();
    };
    let mut out = String::with_capacity(html.len() + 2048);
    out.push_str(&html[..start]);
    out.push_str(&view_switch(grid));
    if grid {
        let after = &html[start..];
        let Some(table_end) = after.find("</section>").map(|i| i + "</section>".len()) else {
            return html.to_owned();
        };
        // The pager, a `div` without nested ones, follows the table.
        let rest = &after[table_end..];
        let rest = match rest.find("<div class=\"mt-5") {
            Some(at) => rest[at..]
                .find("</div>")
                .map_or(rest, |end| &rest[at + end + "</div>".len()..]),
            None => rest,
        };
        out.push_str(&grid_markup(token));
        out.push_str(rest);
    } else {
        out.push_str(&html[start..]);
    }
    let head = format!(
        "<link rel=\"stylesheet\" href=\"{}\">\n</head>",
        escape(&editor_stylesheet_url())
    );
    let script = format!(
        "<script src=\"{}\"></script>\n</body>",
        escape(&media_grid_script_url())
    );
    let mut out = out.replacen("</head>", &head, 1);
    if grid {
        out = out.replacen("</body>", &script, 1);
    }
    out
}

/// The grid's markup, which `assets/editor/grid.js` fills.
fn grid_markup(token: &str) -> String {
    format!(
        r#"<div id="mc-media-grid" data-ajax="{admin}/admin-ajax.php" data-admin="{admin}" data-token="{token}">
<div class="mc-grid-toolbar">
<label for="mc-grid-search">Search media</label>
<input type="search" id="mc-grid-search">
</div>
<p class="mc-grid-status" role="status" aria-live="polite"></p>
<div class="mc-grid-body">
<ul class="mc-grid-items"></ul>
<aside class="mc-grid-details" hidden aria-label="Attachment details"></aside>
</div>
<p><button type="button" class="mc-grid-more" hidden>Load more</button></p>
</div>
"#,
        admin = ADMIN_PATH,
        token = escape(token),
    )
}

#[cfg(test)]
mod view_tests {
    use super::*;

    const PAGE: &str = "<html><head></head><body><main><h1>Media</h1>\
        <section class=\"ig-card p-0\"><table></table></section>\
        <div class=\"mt-5 flex\"><span>Page 1</span></div><footer></footer></main></body></html>";

    #[test]
    fn the_grid_replaces_the_table_and_the_pager() {
        let page = with_views(PAGE, true, "t\"k");
        assert!(page.contains("id=\"mc-media-grid\""), "{page}");
        assert!(page.contains("data-token=\"t&quot;k\""), "{page}");
        assert!(page.contains("?mode=list\">List view</a>"), "{page}");
        assert!(
            !page.contains("<table") && !page.contains("Page 1"),
            "{page}"
        );
        assert!(page.contains("<footer></footer>"), "{page}");
        assert!(page.contains("media-grid.js"), "{page}");
    }

    #[test]
    fn the_list_keeps_its_table_and_gets_the_switch() {
        let page = with_views(PAGE, false, "t");
        assert!(page.contains("<table") && page.contains("Page 1"), "{page}");
        assert!(page.contains("?mode=grid\">Grid view</a>"), "{page}");
        assert!(!page.contains("mc-media-grid"), "{page}");
    }

    #[test]
    fn another_page_is_left_alone() {
        assert_eq!(with_views("<html></html>", true, "t"), "<html></html>");
    }
}
