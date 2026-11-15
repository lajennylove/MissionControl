//! The REST API's routes: discovery (`GET /wp-json/`, the namespace indexes and WordPress's
//! `?rest_route=` form on the site root), the posts controller ([`posts`]), the terms
//! controller ([`terms`]), the users controller ([`users`]) and the media controller
//! ([`media`]). Every route takes a guard from [`auth`], so refused credentials end any REST
//! request with WordPress's error.

pub mod auth;
pub(crate) mod media;
pub(crate) mod posts;
mod request;
pub(crate) mod terms;
pub(crate) mod users;

/// The rank of the routes with a fixed `rest_base` (`users`, `media`): before the posts and
/// terms routes, whose `/wp-json/wp/v2/<_>/<id>` paths they share at the default rank.
const FIXED_BASE_RANK: isize = -20;

use std::sync::Arc;

use mission_control_core::rest::{RestRoutes, discovery};
use orbit_orm::Database;
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::{Request, Route, State};

/// A JSON body with WordPress's content type, `application/json; charset=UTF-8`.
pub(crate) type Json = (Status, (ContentType, String));

/// The REST routes, at `rank`: before the public site's catch-all, after every route with a
/// path of its own (those rank below zero). The terms routes share the posts routes' paths and
/// rank just above `rank`, after the posts routes and before the catch-alls. The users and media
/// routes rank first ([`FIXED_BASE_RANK`]).
pub(crate) fn routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![wp_json, rest_route];
    for route in &mut routes {
        route.rank = rank;
    }
    routes.extend(posts::no_route_routes(rank));
    routes.extend(posts::routes());
    routes.extend(terms::routes(rank - 1));
    routes.extend(users::routes(FIXED_BASE_RANK));
    routes.extend(media::routes(FIXED_BASE_RANK));
    routes
}

pub(crate) fn json(status: Status, body: &serde_json::Value) -> Json {
    let content_type = ContentType::new("application", "json").with_params(("charset", "UTF-8"));
    (status, (content_type, body.to_string()))
}

/// The document at REST route `route` (`/`, `/wp/v2`): the index, a namespace index or
/// `rest_no_route`.
async fn answer(db: &Database, routes: &RestRoutes, route: &str) -> Json {
    let route = route.trim_matches('/');
    let result = if route.is_empty() {
        discovery::index(db, routes).await.map(Some)
    } else {
        discovery::namespace_index(db, routes, route).await
    };
    match result {
        Ok(Some(document)) => json(Status::Ok, &document),
        Ok(None) => json(Status::NotFound, &discovery::no_route()),
        Err(e) => {
            log::error!("Could not build the REST index for `/{route}`: {e}");
            json(
                Status::InternalServerError,
                &serde_json::json!({
                    "code": "internal_server_error",
                    "message": "The site is experiencing technical difficulties.",
                    "data": {"status": 500},
                }),
            )
        }
    }
}

/// `/wp-json`, with or without a trailing slash, and any path under it that no other REST route
/// answers. The path is read from the request, not from a path guard, so a segment such as
/// `.hidden` or `a:b` gets `rest_no_route` like any other unknown route.
#[rocket::get("/wp-json/<_..>")]
async fn wp_json(
    origin: &Origin<'_>,
    _auth: auth::RestAuth,
    db: &State<Arc<Database>>,
    routes: &State<Arc<RestRoutes>>,
) -> Json {
    let path = origin.path().as_str();
    let route = path.strip_prefix("/wp-json").unwrap_or(path);
    answer(db, routes, route).await
}

/// The `rest_route` query of a request to the site root, as WordPress's `rest_api_loaded` reads
/// it: a route with a leading slash (`/`, `/wp/v2`). An empty or relative value is no REST
/// request, and the request goes on to the public site.
struct RestRouteQuery(String);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for RestRouteQuery {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        match request.query_value::<&str>("rest_route") {
            Some(Ok(route)) if route.starts_with('/') => Outcome::Success(Self(route.to_owned())),
            _ => Outcome::Forward(Status::NotFound),
        }
    }
}

/// WordPress's `/?rest_route=/wp/v2` form, which works where the server cannot rewrite paths.
#[rocket::get("/")]
async fn rest_route(
    route: RestRouteQuery,
    _auth: auth::RestAuth,
    db: &State<Arc<Database>>,
    routes: &State<Arc<RestRoutes>>,
) -> Json {
    answer(db, routes, &route.0).await
}
