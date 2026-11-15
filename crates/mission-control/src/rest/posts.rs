//! The routes of the posts controller: `/wp-json/wp/v2/{rest_base}` and
//! `/wp-json/wp/v2/{rest_base}/{id}` for each post type the [`PostTypes`] registry shows in the
//! REST API, over the core's [`PostsController`].
//!
//! A `rest_base` that is not a served post type, or an id that is not digits, forwards: to the
//! terms routes ([`super::terms`]), then to the REST catch-all routes, which answer
//! `rest_no_route` as WordPress does. Bodies are read as [`super::request`] reads them.

use std::sync::Arc;

use mission_control_core::front::Frontend;
use mission_control_core::hooks::Hooks;
use mission_control_core::rest::meta::MetaKeys;
use mission_control_core::rest::posts::{PostType, PostTypes, PostsController};
use mission_control_core::rest::{KsesLimiter, RestResponse, discovery};
use mission_control_core::users::UserCaps;
use orbit_orm::Database;
use rocket::data::Limits;
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::{Data, Request, Route};

use super::auth::RestAuth;
use super::request::{Id, Reply, params, query_of};

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
    meta: &'r MetaKeys,
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
            rocket.state::<Arc<MetaKeys>>(),
        ) {
            (Some(db), Some(hooks), Some(kses), Some(meta)) => Outcome::Success(Self {
                db,
                hooks,
                kses,
                frontend: rocket.state::<Arc<Frontend>>().map(|f| &**f),
                meta,
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
            image_sizes: None,
            meta: self.meta,
        }
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
async fn get(served: Served, id: Id, origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
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
    id: Id,
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
    id: Id,
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
    id: Id,
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
    id: Id,
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
