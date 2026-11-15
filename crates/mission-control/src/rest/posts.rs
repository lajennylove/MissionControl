//! The routes of the posts controller: `/wp-json/wp/v2/{rest_base}` and
//! `/wp-json/wp/v2/{rest_base}/{id}` for each post type the [`PostTypes`] registry shows in the
//! REST API, over the core's [`PostsController`].
//!
//! A `rest_base` that is not served, or an id that is not digits, forwards to the REST catch-all
//! routes, which answer `rest_no_route` as WordPress does. Request bodies are JSON or
//! form-encoded, as WordPress reads them, up to Rocket's `json` data limit (for both); a larger
//! body is refused with 413 `rest_request_too_large` (Mission Control's code; PHP refuses such a
//! body before WordPress runs). Parameters in the body win over those in the query string.

use std::io::Cursor;
use std::sync::Arc;

use mission_control_core::front::Frontend;
use mission_control_core::hooks::Hooks;
use mission_control_core::rest::posts::{
    KsesLimiter, Params, PostType, PostTypes, PostsController, parse_form,
};
use mission_control_core::rest::{RestError, RestResponse, discovery};
use mission_control_core::users::UserCaps;
use orbit_orm::Database;
use rocket::data::{Limits, ToByteUnit};
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Header, Status};
use rocket::request::{FromParam, FromRequest, Outcome};
use rocket::response::{self, Responder};
use rocket::{Data, Request, Response, Route};
use serde_json::Value;

use super::auth::RestAuth;

/// The post type a route's `rest_base` names, when the REST API serves it. Any other forwards.
struct Served(PostType);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Served {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let base = request.routed_segment(3);
        let types = request.rocket().state::<Arc<PostTypes>>();
        match (base, types) {
            (Some(base), Some(types)) => match types.by_rest_base(base) {
                Some(post_type) => Outcome::Success(Self(post_type)),
                None => Outcome::Forward(Status::NotFound),
            },
            _ => Outcome::Forward(Status::NotFound),
        }
    }
}

/// A post id in a path: digits only, as WordPress's `(?P<id>[\d]+)`, that fit an `i64`.
struct PostId(i64);

impl<'a> FromParam<'a> for PostId {
    type Error = ();

    fn from_param(param: &'a str) -> Result<Self, ()> {
        if param.is_empty() || !param.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        param.parse().map(Self).map_err(|_| ())
    }
}

/// The `If-Unmodified-Since` header, if any.
struct IfUnmodifiedSince(Option<String>);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for IfUnmodifiedSince {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(Self(
            request
                .headers()
                .get_one("If-Unmodified-Since")
                .map(str::to_owned),
        ))
    }
}

/// What every posts route needs from the app.
struct App<'r> {
    db: &'r Database,
    hooks: &'r Hooks,
    kses: &'r KsesLimiter,
    frontend: Option<&'r Frontend>,
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
        ) {
            (Some(db), Some(hooks), Some(kses)) => Outcome::Success(Self {
                db,
                hooks,
                kses,
                frontend: rocket.state::<Arc<Frontend>>().map(|f| &**f),
            }),
            _ => {
                log::error!("The posts routes ran before the app was ready");
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

impl<'r> App<'r> {
    /// The controller for `post_type`, run as the user with `caps`.
    fn controller<'a>(
        &'a self,
        post_type: &'a PostType,
        caps: &'a UserCaps,
    ) -> PostsController<'a> {
        PostsController {
            db: self.db,
            hooks: self.hooks,
            post_type,
            user: caps,
            kses: self.kses,
            theme: self.frontend.map(Frontend::theme),
        }
    }
}

/// A REST answer: the response, or WordPress's error body with its status.
pub(crate) struct Reply(Result<RestResponse, RestError>);

impl<'r> Responder<'r, 'static> for Reply {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let (status, body, headers) = match self.0 {
            Ok(response) => (response.status, response.body, response.headers),
            Err(error) => (error.status, error.to_json(), Vec::new()),
        };
        let text = body.to_string();
        let mut builder = Response::build();
        builder
            .status(Status::from_code(status).unwrap_or(Status::InternalServerError))
            .header(ContentType::new("application", "json").with_params(("charset", "UTF-8")))
            .sized_body(text.len(), Cursor::new(text));
        for (name, value) in headers {
            builder.header_adjoin(Header::new(name, value));
        }
        Ok(builder.finalize())
    }
}

/// The routes, at their default ranks: below every route with a fixed path, above the REST
/// catch-alls.
pub(crate) fn routes() -> Vec<Route> {
    rocket::routes![
        list,
        create,
        get,
        update_post,
        update_put,
        update_patch,
        delete
    ]
}

/// The request's query string, as sent.
fn query_of<'a>(origin: &'a Origin<'_>) -> &'a str {
    origin.query().map_or("", |q| q.as_str())
}

/// The request's parameters: the query string's, then the body's over them.
async fn params(
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    data: Data<'_>,
) -> Result<Params, RestError> {
    let mut params = parse_form(query_of(origin));
    let json = content_type.is_some_and(|c| c.is_json());
    let form = content_type.is_some_and(|c| c.is_form());
    if !json && !form {
        return Ok(params);
    }
    // One limit for both kinds, Rocket's `json` limit (1 MiB by default): a post sent as a form
    // is as large as one sent as JSON, and Rocket's `form` limit (32 KiB) is sized for small
    // forms.
    let limit = limits.get("json").unwrap_or(1.mebibytes());
    let body = data.open(limit).into_string().await.map_err(|e| {
        log::warn!("Could not read a REST request body: {e}");
        RestError::new(
            "rest_invalid_body",
            "The request body could not be read.",
            400,
        )
    })?;
    if !body.is_complete() {
        return Err(RestError::new(
            "rest_request_too_large",
            "The request body is too large.",
            413,
        ));
    }
    let body = body.into_inner();
    let from_body = if json {
        if body.trim().is_empty() {
            Params::new()
        } else {
            match serde_json::from_str::<Value>(&body) {
                Ok(Value::Object(object)) => object,
                Ok(_) => Params::new(),
                Err(e) => {
                    return Err(RestError::new(
                        "rest_invalid_json",
                        "Invalid JSON body passed.",
                        400,
                    )
                    .with_data("json_error_message", Value::String(e.to_string())));
                }
            }
        }
    } else {
        parse_form(&body)
    };
    params.extend(from_body);
    Ok(params)
}

#[rocket::get("/wp-json/wp/v2/<_>")]
async fn list(served: Served, origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(
        app.controller(&served.0, &caps)
            .list(query_of(origin))
            .await,
    )
}

#[rocket::get("/wp-json/wp/v2/<_>/<id>")]
async fn get(
    served: Served,
    id: PostId,
    origin: &Origin<'_>,
    auth: RestAuth,
    app: App<'_>,
) -> Reply {
    let caps = auth.caps();
    Reply(
        app.controller(&served.0, &caps)
            .get(id.0, query_of(origin))
            .await,
    )
}

#[rocket::post("/wp-json/wp/v2/<_>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn create(
    served: Served,
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
    Reply(app.controller(&served.0, &caps).create(&params).await)
}

/// An update by any of WordPress's editable methods.
#[allow(clippy::too_many_arguments)]
async fn update(
    served: Served,
    id: PostId,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    since: IfUnmodifiedSince,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    let params = match params(origin, content_type, limits, data).await {
        Ok(params) => params,
        Err(e) => return Reply(Err(e)),
    };
    let caps = auth.caps();
    let controller = app.controller(&served.0, &caps);
    Reply(controller.update(id.0, &params, since.0.as_deref()).await)
}

#[rocket::post("/wp-json/wp/v2/<_>/<id>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn update_post(
    served: Served,
    id: PostId,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    since: IfUnmodifiedSince,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(
        served,
        id,
        origin,
        content_type,
        limits,
        since,
        auth,
        app,
        data,
    )
    .await
}

#[rocket::put("/wp-json/wp/v2/<_>/<id>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn update_put(
    served: Served,
    id: PostId,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    since: IfUnmodifiedSince,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(
        served,
        id,
        origin,
        content_type,
        limits,
        since,
        auth,
        app,
        data,
    )
    .await
}

#[rocket::patch("/wp-json/wp/v2/<_>/<id>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn update_patch(
    served: Served,
    id: PostId,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    since: IfUnmodifiedSince,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(
        served,
        id,
        origin,
        content_type,
        limits,
        since,
        auth,
        app,
        data,
    )
    .await
}

#[rocket::delete("/wp-json/wp/v2/<_>/<id>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn delete(
    served: Served,
    id: PostId,
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
    Reply(app.controller(&served.0, &caps).delete(id.0, &params).await)
}

/// The REST answer for a path or method no route takes: `rest_no_route`, 404. For the methods
/// other than `GET`, whose catch-all is the discovery route.
pub(crate) fn no_route_routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![no_route_post, no_route_put, no_route_patch, no_route_delete];
    for route in &mut routes {
        route.rank = rank;
    }
    routes
}

fn no_route() -> Reply {
    let body = discovery::no_route();
    Reply(Ok(RestResponse {
        status: 404,
        body,
        headers: Vec::new(),
    }))
}

#[rocket::post("/wp-json/<_..>")]
fn no_route_post(_auth: RestAuth) -> Reply {
    no_route()
}

#[rocket::put("/wp-json/<_..>")]
fn no_route_put(_auth: RestAuth) -> Reply {
    no_route()
}

#[rocket::patch("/wp-json/<_..>")]
fn no_route_patch(_auth: RestAuth) -> Reply {
    no_route()
}

#[rocket::delete("/wp-json/<_..>")]
fn no_route_delete(_auth: RestAuth) -> Reply {
    no_route()
}
