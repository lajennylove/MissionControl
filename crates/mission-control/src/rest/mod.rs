//! The REST API's routes. Discovery for now: `GET /wp-json/`, the namespace indexes and
//! WordPress's `?rest_route=` form on the site root.

use std::sync::Arc;

use mission_control_core::rest::{RestRoutes, discovery};
use orbit_orm::Database;
use rocket::http::{ContentType, Status};
use rocket::{Route, State};

/// A JSON body with WordPress's content type, `application/json; charset=UTF-8`.
type Json = (Status, (ContentType, String));

/// The REST routes, at `rank`: before the public site's catch-all, after every route with a
/// path of its own (those rank below zero).
pub(crate) fn routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![wp_json, rest_route];
    for route in &mut routes {
        route.rank = rank;
    }
    routes
}

fn json(status: Status, body: &serde_json::Value) -> Json {
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
/// answers.
#[rocket::get("/wp-json/<path..>")]
async fn wp_json(
    path: std::path::PathBuf,
    db: &State<Arc<Database>>,
    routes: &State<Arc<RestRoutes>>,
) -> Json {
    answer(db, routes, &path.to_string_lossy()).await
}

/// WordPress's `/?rest_route=/wp/v2` form, which works where the server cannot rewrite paths.
#[rocket::get("/?<rest_route>")]
async fn rest_route(
    rest_route: &str,
    db: &State<Arc<Database>>,
    routes: &State<Arc<RestRoutes>>,
) -> Json {
    answer(db, routes, rest_route).await
}
