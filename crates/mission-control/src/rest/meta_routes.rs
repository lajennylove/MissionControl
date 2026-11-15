//! The routes of the types, taxonomies, statuses and settings controllers:
//! `/mc-json/mc/v2/types`, `/types/{type}`, `/taxonomies`, `/taxonomies/{taxonomy}`, `/statuses`,
//! `/statuses/{status}` and `/settings`, over the core's [`MetaRoutesController`] and
//! [`SettingsController`].
//!
//! The paths are fixed, so these routes rank before the posts and terms routes, whose
//! `rest_base` segment would also match them. A name that is not WordPress's `[\w-]+` forwards,
//! and ends at the REST catch-all's `rest_no_route`; so does any method the controller does not
//! take. Settings bodies are read as [`super::request`] reads them.

use std::sync::Arc;

use mission_control_core::rest::KsesLimiter;
use mission_control_core::rest::meta_routes::MetaRoutesController;
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::settings::{Settings, SettingsController};
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::users::UserCaps;
use orbit_orm::Database;
use rocket::data::Limits;
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Status};
use rocket::request::{FromParam, FromRequest, Outcome};
use rocket::{Data, Request, Route};

use super::auth::RestAuth;
use super::request::{Reply, params, query_of};

/// A name in a path, as WordPress's `(?P<type>[\w-]+)`: ASCII letters, digits, `_` and `-`.
/// Anything else forwards.
pub(crate) struct Name<'a>(pub(crate) &'a str);

impl<'a> FromParam<'a> for Name<'a> {
    type Error = ();

    fn from_param(param: &'a str) -> Result<Self, ()> {
        if !param.is_empty()
            && param
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            Ok(Self(param))
        } else {
            Err(())
        }
    }
}

/// What every route here needs from the app.
struct App<'r> {
    db: &'r Database,
    post_types: &'r PostTypes,
    taxonomies: &'r Taxonomies,
    settings: &'r Settings,
    kses: &'r KsesLimiter,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for App<'r> {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let rocket = request.rocket();
        match (
            rocket.state::<Arc<Database>>(),
            rocket.state::<Arc<PostTypes>>(),
            rocket.state::<Arc<Taxonomies>>(),
            rocket.state::<Arc<Settings>>(),
            rocket.state::<Arc<KsesLimiter>>(),
        ) {
            (Some(db), Some(post_types), Some(taxonomies), Some(settings), Some(kses)) => {
                Outcome::Success(Self {
                    db,
                    post_types,
                    taxonomies,
                    settings,
                    kses,
                })
            }
            _ => {
                log::error!(
                    "The types, taxonomies, statuses and settings routes ran before the app was ready"
                );
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

impl App<'_> {
    /// The types, taxonomies and statuses controller, run as the user with `caps`.
    fn controller<'a>(&'a self, caps: &'a UserCaps) -> MetaRoutesController<'a> {
        MetaRoutesController {
            db: self.db,
            post_types: self.post_types,
            taxonomies: self.taxonomies,
            user: caps,
        }
    }

    /// The settings controller, run as the user with `caps`.
    fn settings<'a>(&'a self, caps: &'a UserCaps) -> SettingsController<'a> {
        SettingsController {
            db: self.db,
            settings: self.settings,
            user: caps,
            kses: self.kses,
        }
    }
}

/// The routes, at `rank`: before the posts and terms routes.
pub(crate) fn routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![
        types,
        post_type,
        taxonomies,
        taxonomy,
        statuses,
        status,
        settings_get,
        settings_post,
        settings_put,
        settings_patch
    ];
    for route in &mut routes {
        route.rank = rank;
    }
    routes
}

#[rocket::get("/mc-json/mc/v2/types")]
async fn types(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).types(query_of(origin)).await)
}

#[rocket::get("/mc-json/mc/v2/types/<name>")]
async fn post_type(name: Name<'_>, origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(
        app.controller(&caps)
            .post_type(name.0, query_of(origin))
            .await,
    )
}

#[rocket::get("/mc-json/mc/v2/taxonomies")]
async fn taxonomies(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).taxonomies(query_of(origin)).await)
}

#[rocket::get("/mc-json/mc/v2/taxonomies/<name>")]
async fn taxonomy(name: Name<'_>, origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(
        app.controller(&caps)
            .taxonomy(name.0, query_of(origin))
            .await,
    )
}

#[rocket::get("/mc-json/mc/v2/statuses")]
async fn statuses(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).statuses(query_of(origin)).await)
}

#[rocket::get("/mc-json/mc/v2/statuses/<name>")]
async fn status(name: Name<'_>, origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.controller(&caps).status(name.0, query_of(origin)).await)
}

#[rocket::get("/mc-json/mc/v2/settings")]
async fn settings_get(origin: &Origin<'_>, auth: RestAuth, app: App<'_>) -> Reply {
    let caps = auth.caps();
    Reply(app.settings(&caps).get(query_of(origin)).await)
}

/// A settings update by any of WordPress's editable methods.
async fn settings_update(
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
    Reply(app.settings(&caps).update(&params).await)
}

#[rocket::post("/mc-json/mc/v2/settings", data = "<data>")]
async fn settings_post(
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    settings_update(origin, content_type, limits, auth, app, data).await
}

#[rocket::put("/mc-json/mc/v2/settings", data = "<data>")]
async fn settings_put(
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    settings_update(origin, content_type, limits, auth, app, data).await
}

#[rocket::patch("/mc-json/mc/v2/settings", data = "<data>")]
async fn settings_patch(
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    settings_update(origin, content_type, limits, auth, app, data).await
}
