//! The public site's route: every GET the other routes leave, answered by the active theme.

use std::sync::Arc;

use mission_control_core::front::{Frontend, Response};
use rocket::http::Status;
use rocket::http::uri::Origin;
use rocket::response::content::RawHtml;
use rocket::response::{self, Redirect, Responder};
use rocket::{Request, Route, State};

/// The public site's routes, at `rank`.
pub(crate) fn routes(rank: isize) -> Vec<Route> {
    let mut routes = rocket::routes![front];
    for route in &mut routes {
        route.rank = rank;
    }
    routes
}

/// What the public site sends back: a rendered template with its status, or a redirect (an
/// old slug's 301).
struct Page(Response);

impl<'r> Responder<'r, 'static> for Page {
    fn respond_to(self, request: &'r Request<'_>) -> response::Result<'static> {
        match self.0 {
            Response::Page { status, html } => (
                Status::from_code(status).unwrap_or(Status::Ok),
                RawHtml(html),
            )
                .respond_to(request),
            Response::Redirect { location, status } => match status {
                302 => Redirect::found(location),
                _ => Redirect::moved(location),
            }
            .respond_to(request),
        }
    }
}

/// Any path: resolved as WordPress resolves a URL, and rendered with the theme's template.
/// HEAD requests are answered by this route too, without the body.
#[rocket::get("/<_..>")]
async fn front(origin: &Origin<'_>, frontend: &State<Arc<Frontend>>) -> Result<Page, Status> {
    let path = origin.path().as_str();
    let query = origin.query().map_or("", |query| query.as_str());
    match frontend.respond(path, query).await {
        Ok(response) => Ok(Page(response)),
        Err(e) => {
            log::error!("Could not render `{path}`: {e}");
            Err(Status::InternalServerError)
        }
    }
}
