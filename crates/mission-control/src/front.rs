//! The public site's route: every GET the other routes leave, answered by the active theme.

use std::sync::Arc;

use mission_control_core::front::{Frontend, Response};
use mission_control_core::rest::discovery;
use orbit_orm::Database;
use rocket::http::uri::Origin;
use rocket::http::{Header, Status};
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
/// old slug's 301). `api` is the address of the REST index, advertised in a `Link` header on a
/// page as WordPress does.
struct Page {
    response: Response,
    api: Option<String>,
}

impl<'r> Responder<'r, 'static> for Page {
    fn respond_to(self, request: &'r Request<'_>) -> response::Result<'static> {
        match self.response {
            Response::Page { status, html } => {
                let mut response = (
                    Status::from_code(status).unwrap_or(Status::Ok),
                    RawHtml(html),
                )
                    .respond_to(request)?;
                if let Some(api) = self.api {
                    response.set_header(Header::new(
                        "Link",
                        format!("<{api}>; rel=\"https://api.w.org/\""),
                    ));
                }
                Ok(response)
            }
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
async fn front(
    origin: &Origin<'_>,
    frontend: &State<Arc<Frontend>>,
    db: &State<Arc<Database>>,
) -> Result<Page, Status> {
    let path = origin.path().as_str();
    let query = origin.query().map_or("", |query| query.as_str());
    match frontend.respond(path, query).await {
        Ok(response) => {
            // Not worth failing a page for: without the home option the header is left out.
            let api = match &response {
                Response::Page { .. } => discovery::home_address(db)
                    .await
                    .map(|home| discovery::index_url(&home))
                    .ok(),
                Response::Redirect { .. } => None,
            };
            Ok(Page { response, api })
        }
        Err(e) => {
            log::error!("Could not render `{path}`: {e}");
            Err(Status::InternalServerError)
        }
    }
}
