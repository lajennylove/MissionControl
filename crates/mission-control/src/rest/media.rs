//! The routes of the media controller: `/wp-json/wp/v2/media` and `/wp-json/wp/v2/media/{id}`,
//! over the core's [`MediaController`].
//!
//! The paths are fixed, so these routes rank before the posts and terms routes, whose
//! `rest_base` segment would also match `media`. An id that is not digits forwards, and ends at
//! the REST catch-all's `rest_no_route`.
//!
//! An upload's body is read here, with a hard cap: never more than the upload limit
//! (`MC_UPLOAD_MAX_BYTES`) for a raw body, or that limit and [`MULTIPART_OVERHEAD`] for a
//! multipart one; a body over it is refused with `rest_upload_file_too_big` without reading
//! further, and a `Content-Length` over it before reading at all. A user who may not upload is
//! refused before the body is read. Updates and deletes read their parameters as
//! [`super::request`] reads them.

use std::io::Cursor;
use std::sync::Arc;

use mission_control_core::hooks::Hooks;
use mission_control_core::media::{ImageSizes, Storage};
use mission_control_core::rest::media::{
    MediaController, MediaUpload, UploadedFile, upload_too_big,
};
use mission_control_core::rest::{KsesLimiter, MAX_INPUT_VARS, Params, RestError, parse_form};
use mission_control_core::users::UserCaps;
use orbit_orm::Database;
use rocket::data::{Limits, ToByteUnit};
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::{Data, Request, Route};
use serde_json::Value;

use super::auth::RestAuth;
use super::request::{Id, Reply, params, query_of};

/// What a multipart body may hold beside the file: its other fields and the part headers.
pub(crate) const MULTIPART_OVERHEAD: u64 = 1024 * 1024;

/// The upload limit, managed by the app.
pub(crate) struct UploadSettings {
    /// The largest file accepted, in bytes (`MC_UPLOAD_MAX_BYTES`).
    pub(crate) max_bytes: u64,
}

/// What every media route needs from the app.
struct App<'r> {
    db: &'r Database,
    hooks: &'r Hooks,
    kses: &'r KsesLimiter,
    storage: &'r Arc<dyn Storage>,
    sizes: &'r Arc<ImageSizes>,
    settings: &'r UploadSettings,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for App<'r> {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let rocket = request.rocket();
        match (
            rocket.state::<Arc<Database>>(),
            rocket.state::<Arc<Hooks>>(),
            rocket.state::<Arc<KsesLimiter>>(),
            rocket.state::<Arc<dyn Storage>>(),
            rocket.state::<Arc<ImageSizes>>(),
            rocket.state::<UploadSettings>(),
        ) {
            (Some(db), Some(hooks), Some(kses), Some(storage), Some(sizes), Some(settings)) => {
                Outcome::Success(Self {
                    db,
                    hooks,
                    kses,
                    storage,
                    sizes,
                    settings,
                })
            }
            _ => {
                log::error!("The media routes ran before the app was ready");
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

impl App<'_> {
    /// The controller, run as the user with `caps`.
    fn controller<'a>(&'a self, caps: &'a UserCaps) -> MediaController<'a> {
        MediaController {
            db: self.db,
            hooks: self.hooks,
            user: caps,
            kses: self.kses,
            storage: self.storage,
            sizes: self.sizes,
            upload_max_bytes: self.settings.max_bytes,
        }
    }
}

/// The headers an upload reads.
struct UploadHeaders {
    content_type: Option<String>,
    content_disposition: Option<String>,
    content_md5: Option<String>,
    content_length: Option<u64>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for UploadHeaders {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let headers = request.headers();
        let get = |name: &str| headers.get_one(name).map(str::to_owned);
        Outcome::Success(Self {
            content_type: get("Content-Type"),
            content_disposition: get("Content-Disposition"),
            content_md5: get("Content-MD5"),
            content_length: headers
                .get_one("Content-Length")
                .and_then(|l| l.trim().parse().ok()),
        })
    }
}

/// The routes, at `rank`: before the posts and terms routes.
pub(crate) fn routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![
        list,
        create,
        get,
        update_post,
        update_put,
        update_patch,
        delete
    ];
    for route in &mut routes {
        route.rank = rank;
    }
    routes
}

#[rocket::get("/wp-json/wp/v2/media")]
async fn list(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).list(query_of(origin)).await)
}

#[rocket::get("/wp-json/wp/v2/media/<id>")]
async fn get(id: Id, origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).get(id.0, query_of(origin)).await)
}

#[rocket::post("/wp-json/wp/v2/media", data = "<data>")]
async fn create(
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    headers: UploadHeaders,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    let caps = auth.caps();
    let controller = app.controller(&caps);
    if let Err(e) = controller.check_upload_permission() {
        return Reply(Err(e));
    }
    let mut params = parse_form(query_of(origin));
    let boundary = content_type
        .filter(|c| c.top() == "multipart" && c.sub() == "form-data")
        .and_then(|c| c.param("boundary"))
        .map(str::to_owned);
    let max = app.settings.max_bytes;
    let cap = match boundary {
        Some(_) => max.saturating_add(MULTIPART_OVERHEAD),
        None => max,
    };
    let body = match read_capped(data, headers.content_length, cap, max).await {
        Ok(body) => body,
        Err(e) => return Reply(Err(e)),
    };
    let upload = match boundary {
        Some(boundary) => match read_multipart(body, &boundary, &mut params).await {
            Ok(file) => MediaUpload::Multipart { file },
            Err(e) => return Reply(Err(e)),
        },
        None => MediaUpload::Raw {
            content_type: headers.content_type,
            content_disposition: headers.content_disposition,
            bytes: body,
        },
    };
    Reply(
        controller
            .create(&params, upload, headers.content_md5.as_deref())
            .await,
    )
}

/// The body, read up to `cap` bytes; `rest_upload_file_too_big` (naming the upload limit
/// `max`) when it is longer, or says it is.
async fn read_capped(
    data: Data<'_>,
    content_length: Option<u64>,
    cap: u64,
    max: u64,
) -> Result<Vec<u8>, RestError> {
    if content_length.is_some_and(|length| length > cap) {
        return Err(upload_too_big(max));
    }
    let body = data.open(cap.bytes()).into_bytes().await.map_err(|e| {
        log::warn!("Could not read an upload: {e}");
        unreadable()
    })?;
    if !body.is_complete() {
        return Err(upload_too_big(max));
    }
    Ok(body.into_inner())
}

/// `rest_invalid_body`, 400: a body that cannot be read.
fn unreadable() -> RestError {
    RestError::new(
        "rest_invalid_body",
        "The request body could not be read.",
        400,
    )
}

/// The `file` part of a multipart `body`, as PHP's `$_FILES['file']`: the first part named
/// `file` that has a file name. Its other fields (and a `file` part with no file name) go into
/// `params`, over the query string's, as `$_POST`; as PHP's `max_input_vars` bounds `$_POST`,
/// at most [`MAX_INPUT_VARS`] of the body's fields are read, and later ones are ignored.
async fn read_multipart(
    body: Vec<u8>,
    boundary: &str,
    params: &mut Params,
) -> Result<Option<UploadedFile>, RestError> {
    let mut multipart = multer::Multipart::with_reader(Cursor::new(body), boundary);
    let mut file = None;
    let mut fields = 0;
    let failed = |e: multer::Error| {
        log::warn!("Could not read a multipart upload: {e}");
        unreadable()
    };
    while let Some(field) = multipart.next_field().await.map_err(failed)? {
        let Some(name) = field.name().map(str::to_owned) else {
            continue;
        };
        if let Some(file_name) = field.file_name().map(str::to_owned) {
            if name == "file" && file.is_none() {
                let bytes = field.bytes().await.map_err(failed)?;
                file = Some(UploadedFile {
                    file_name,
                    bytes: Vec::from(bytes),
                });
            }
            continue;
        }
        if fields >= MAX_INPUT_VARS {
            continue;
        }
        let text = field.text().await.map_err(failed)?;
        params.insert(name, Value::String(text));
        fields += 1;
    }
    Ok(file)
}

/// An update by any of WordPress's editable methods.
async fn update(
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    let params = match params(origin, content_type, limits, data).await {
        Ok(params) => params,
        Err(e) => return Reply(Err(e)),
    };
    let caps = auth.caps();
    Reply(app.controller(&caps).update(id.0, &params).await)
}

#[rocket::post("/wp-json/wp/v2/media/<id>", data = "<data>")]
async fn update_post(
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(id, origin, content_type, limits, auth, app, data).await
}

#[rocket::put("/wp-json/wp/v2/media/<id>", data = "<data>")]
async fn update_put(
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(id, origin, content_type, limits, auth, app, data).await
}

#[rocket::patch("/wp-json/wp/v2/media/<id>", data = "<data>")]
async fn update_patch(
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(id, origin, content_type, limits, auth, app, data).await
}

#[rocket::delete("/wp-json/wp/v2/media/<id>", data = "<data>")]
async fn delete(
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    let params = match params(origin, content_type, limits, data).await {
        Ok(params) => params,
        Err(e) => return Reply(Err(e)),
    };
    let caps = auth.caps();
    Reply(app.controller(&caps).delete(id.0, &params).await)
}
