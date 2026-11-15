//! The routes of the terms controller: `/wp-json/wp/v2/{rest_base}` and
//! `/wp-json/wp/v2/{rest_base}/{id}` for each taxonomy the [`Taxonomies`] registry shows in the
//! REST API, over the core's [`TermsController`].
//!
//! The paths are the posts routes' paths, so these routes rank after them: a `rest_base` that is
//! not a post type forwards from the posts routes to these, and one that is not a taxonomy
//! either, or an id that is not digits, forwards on to the REST catch-all routes, which answer
//! `rest_no_route`. Bodies are read as [`super::request`] reads them.

use std::sync::Arc;

use mission_control_core::rest::KsesLimiter;
use mission_control_core::rest::meta::MetaKeys;
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::terms::{Taxonomies, TaxonomySpec, TermsController};
use mission_control_core::users::UserCaps;
use orbit_orm::Database;
use rocket::data::Limits;
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::{Data, Request, Route};

use super::auth::RestAuth;
use super::request::{Id, Reply, params, query_of};

/// The taxonomy a route's `rest_base` names, when the REST API serves it. Any other forwards.
struct Served(TaxonomySpec);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Served {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let base = request.routed_segment(3);
        let taxonomies = request.rocket().state::<Arc<Taxonomies>>();
        match (base, taxonomies) {
            (Some(base), Some(taxonomies)) => match taxonomies.by_rest_base(base) {
                Some(taxonomy) => Outcome::Success(Self(taxonomy)),
                None => Outcome::Forward(Status::NotFound),
            },
            _ => Outcome::Forward(Status::NotFound),
        }
    }
}

/// What every terms route needs from the app.
struct App<'r> {
    db: &'r Database,
    kses: &'r KsesLimiter,
    post_types: &'r PostTypes,
    meta: &'r MetaKeys,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for App<'r> {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let rocket = request.rocket();
        match (
            rocket.state::<Arc<Database>>(),
            rocket.state::<Arc<KsesLimiter>>(),
            rocket.state::<Arc<PostTypes>>(),
            rocket.state::<Arc<MetaKeys>>(),
        ) {
            (Some(db), Some(kses), Some(post_types), Some(meta)) => Outcome::Success(Self {
                db,
                kses,
                post_types,
                meta,
            }),
            _ => {
                log::error!("The terms routes ran before the app was ready");
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

impl App<'_> {
    /// The controller for `taxonomy`, run as the user with `caps`.
    fn controller<'a>(
        &'a self,
        taxonomy: &'a TaxonomySpec,
        caps: &'a UserCaps,
    ) -> TermsController<'a> {
        TermsController {
            db: self.db,
            taxonomy,
            post_types: self.post_types,
            user: caps,
            kses: self.kses,
            meta: self.meta,
        }
    }
}

/// The routes, at `rank`: after the posts routes, whose paths they share, and before the REST
/// catch-alls.
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
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    let params = match params(origin, content_type, limits, data).await {
        Ok(params) => params,
        Err(e) => return Reply(Err(e)),
    };
    let caps = auth.caps();
    Reply(app.controller(&served.0, &caps).update(id.0, &params).await)
}

#[rocket::post("/wp-json/wp/v2/<_>/<id>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn update_post(
    served: Served,
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(served, id, origin, content_type, limits, auth, app, data).await
}

#[rocket::put("/wp-json/wp/v2/<_>/<id>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn update_put(
    served: Served,
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(served, id, origin, content_type, limits, auth, app, data).await
}

#[rocket::patch("/wp-json/wp/v2/<_>/<id>", data = "<data>")]
#[allow(clippy::too_many_arguments)]
async fn update_patch(
    served: Served,
    id: Id,
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    auth: RestAuth,
    app: App<'_>,
    data: Data<'_>,
) -> Reply {
    update(served, id, origin, content_type, limits, auth, app, data).await
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
