//! The route of the search controller: `GET /wp-json/wp/v2/search`, over the core's
//! [`SearchController`].
//!
//! The path is fixed, so the route ranks before the posts and terms routes, whose `rest_base`
//! segment would also match `search`. Any method but `GET` ends at the REST catch-all's
//! `rest_no_route`.

use std::sync::Arc;

use mission_control_core::front::Frontend;
use mission_control_core::hooks::Hooks;
use mission_control_core::rest::KsesLimiter;
use mission_control_core::rest::meta::MetaKeys;
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::search::SearchController;
use mission_control_core::rest::terms::Taxonomies;
use orbit_orm::Database;
use rocket::Route;
use rocket::http::Status;
use rocket::http::uri::Origin;
use rocket::request::{FromRequest, Outcome, Request};

use super::auth::RestAuth;
use super::request::{Reply, query_of};

/// What the search route needs from the app.
struct App<'r> {
    db: &'r Database,
    hooks: &'r Hooks,
    post_types: &'r PostTypes,
    taxonomies: &'r Taxonomies,
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
            rocket.state::<Arc<PostTypes>>(),
            rocket.state::<Arc<Taxonomies>>(),
            rocket.state::<Arc<KsesLimiter>>(),
            rocket.state::<Arc<MetaKeys>>(),
        ) {
            (Some(db), Some(hooks), Some(post_types), Some(taxonomies), Some(kses), Some(meta)) => {
                Outcome::Success(Self {
                    db,
                    hooks,
                    post_types,
                    taxonomies,
                    kses,
                    frontend: rocket.state::<Arc<Frontend>>().map(|f| &**f),
                    meta,
                })
            }
            _ => {
                log::error!("The search route ran before the app was ready");
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

/// The route, at `rank`: before the posts and terms routes.
pub(crate) fn routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![search];
    for route in &mut routes {
        route.rank = rank;
    }
    routes
}

#[rocket::get("/wp-json/wp/v2/search")]
async fn search(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    let controller = SearchController {
        db: app.db,
        hooks: app.hooks,
        post_types: app.post_types,
        taxonomies: app.taxonomies,
        user: &caps,
        kses: app.kses,
        theme: app.frontend.map(Frontend::theme),
        meta: app.meta,
    };
    Reply(controller.search(query_of(origin)).await)
}
