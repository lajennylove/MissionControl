//! The routes of the users controller: `GET /mc-json/mc/v2/users`, `/users/{id}` and
//! `/users/me`, over the core's [`UsersController`].
//!
//! The paths are fixed, so these routes rank before the posts and terms routes, whose
//! `rest_base` segment would also match `users`. An id that is not digits forwards, and ends at
//! the REST catch-all's `rest_no_route`; so does any method but `GET`, as Mission Control serves
//! no user writes.

use std::sync::Arc;

use mission_control_core::rest::meta::MetaKeys;
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::users::UsersController;
use mission_control_core::users::UserCaps;
use orbit_orm::Database;
use rocket::Route;
use rocket::http::Status;
use rocket::http::uri::Origin;
use rocket::request::{FromRequest, Outcome, Request};

use super::auth::{AuthSettings, RestAuth};
use super::request::{Id, Reply, query_of};

/// What every users route needs from the app.
struct App<'r> {
    db: &'r Database,
    post_types: &'r PostTypes,
    settings: &'r AuthSettings,
    meta: &'r MetaKeys,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for App<'r> {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let rocket = request.rocket();
        match (
            rocket.state::<Arc<Database>>(),
            rocket.state::<Arc<PostTypes>>(),
            rocket.state::<AuthSettings>(),
            rocket.state::<Arc<MetaKeys>>(),
        ) {
            (Some(db), Some(post_types), Some(settings), Some(meta)) => Outcome::Success(Self {
                db,
                post_types,
                settings,
                meta,
            }),
            _ => {
                log::error!("The users routes ran before the app was ready");
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

impl App<'_> {
    /// The controller, run as the user with `caps`.
    fn controller<'a>(&'a self, caps: &'a UserCaps) -> UsersController<'a> {
        UsersController {
            db: self.db,
            user: caps,
            roles: self.settings.user_roles.as_ref(),
            post_types: self.post_types,
            meta: self.meta,
        }
    }
}

/// The routes, at `rank`: before the posts and terms routes. `/users/me` ranks just before
/// `/users/{id}`, whose path would match it.
pub(crate) fn routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![list, get];
    for route in &mut routes {
        route.rank = rank;
    }
    let mut me_route = rocket::routes![me];
    for route in &mut me_route {
        route.rank = rank - 1;
    }
    routes.extend(me_route);
    routes
}

#[rocket::get("/mc-json/mc/v2/users")]
async fn list(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).list(query_of(origin)).await)
}

#[rocket::get("/mc-json/mc/v2/users/me")]
async fn me(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).me(query_of(origin)).await)
}

#[rocket::get("/mc-json/mc/v2/users/<id>")]
async fn get(id: Id, origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).get(id.0, query_of(origin)).await)
}
