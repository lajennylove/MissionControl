//! The request guards for the current REST user, over the core's
//! [`authenticate`](mission_control_core::rest::auth::authenticate).
//!
//! As WordPress checks authentication before any route runs (`rest_authentication_errors`), the
//! [`RestAuthFairing`] authenticates every REST request when it arrives, whichever route answers
//! it, even an unknown path or one whose route takes no guard. Refused credentials end the
//! request there: it is sent to an internal route that answers with WordPress's error, so no REST
//! route runs. Routes then take a guard for the user:
//!
//! - [`RestAuth`]: the signed-in user or a visitor.
//! - [`RestUser`]: a signed-in user only. A visitor gets 401 `rest_not_logged_in`.
//!
//! The user is worked out once per request and cached. A guard that ends the request leaves its
//! [`RestError`] for the catchers ([`catchers`]), which answer with WordPress's JSON body and the
//! error's status; any other error keeps Rocket's own answer.
//!
//! ```ignore
//! #[rocket::get("/mc-json/acme/v1/secret")]
//! fn secret(user: RestUser) -> String {
//!     format!("hello, user {}", user.id())
//! }
//! ```

use std::io::Cursor;
use std::ops::Deref;
use std::sync::{Arc, OnceLock};

use mission_control_core::config::{Environment, TrustedProxies};
use mission_control_core::rest::RestError;
use mission_control_core::rest::auth::{
    CurrentUser, RequestCredentials, authenticate, not_logged_in,
};
use mission_control_core::users::{UserCaps, UserRoles};
use orbit_orm::Database;
use rocket::catcher::{BoxFuture, Catcher};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Method, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::Responder;
use rocket::{Data, Request, Response};

use super::{Json, json};

/// The internal path a request whose credentials are refused is sent to. Only answers a request
/// the fairing sent; anything else goes on to the public site.
const AUTH_ERROR_PATH: &str = "/__mission-control/rest-auth-error";

/// What authentication needs from the app's configuration, managed by the app.
pub(crate) struct AuthSettings {
    /// The site's environment (`MC_ENV`).
    pub(crate) environment: Environment,
    /// The proxies whose `X-Forwarded-Proto` is believed (`MC_TRUSTED_PROXIES`).
    pub(crate) trusted_proxies: TrustedProxies,
    /// Where signed-in users' roles come from.
    pub(crate) user_roles: Arc<dyn UserRoles>,
}

/// The current REST user, or a visitor. A request whose credentials are refused never reaches
/// the route: it ends with WordPress's error (401 `incorrect_password`, ...).
#[derive(Debug, Clone)]
pub struct RestAuth {
    user: Option<CurrentUser>,
}

impl RestAuth {
    /// The signed-in user, or `None` for a visitor.
    pub fn user(&self) -> Option<&CurrentUser> {
        self.user.as_ref()
    }

    /// The capabilities the request runs with: the user's, or a visitor's (id 0, no role), as
    /// WordPress's `wp_get_current_user` gives user 0.
    pub fn caps(&self) -> UserCaps {
        match &self.user {
            Some(user) => user.caps().clone(),
            None => UserCaps::with_default_roles(0, &[], &Default::default()),
        }
    }
}

/// The current REST user, who must be signed in. A visitor gets 401 `rest_not_logged_in`, as
/// WordPress's `/wp/v2/users/me` answers one.
#[derive(Debug, Clone)]
pub struct RestUser(CurrentUser);

impl RestUser {
    /// The user.
    pub fn into_inner(self) -> CurrentUser {
        self.0
    }
}

impl Deref for RestUser {
    type Target = CurrentUser;

    fn deref(&self) -> &CurrentUser {
        &self.0
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for RestAuth {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        match current_user(request).await {
            Ok(user) => Outcome::Success(Self { user: user.clone() }),
            Err(e) => fail(request, e.clone()),
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for RestUser {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        match current_user(request).await {
            Ok(Some(user)) => Outcome::Success(Self(user.clone())),
            Ok(None) => fail(request, not_logged_in()),
            Err(e) => fail(request, e.clone()),
        }
    }
}

/// Authenticates every REST request on arrival, as WordPress's `rest_authentication_errors` runs
/// before any route. Refused credentials send the request to the internal error route, so the
/// route it asked for never runs, whatever its method or path.
pub(crate) struct RestAuthFairing;

#[rocket::async_trait]
impl Fairing for RestAuthFairing {
    fn info(&self) -> Info {
        Info {
            name: "REST authentication",
            kind: Kind::Request,
        }
    }

    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        if !is_rest_request(request) {
            return;
        }
        let Some(error) = current_user(request).await.as_ref().err().cloned() else {
            return;
        };
        let _ = request.local_cache(PendingError::default).0.set(error);
        // HEAD stays HEAD: Rocket answers it with the GET route and drops the body.
        if request.method() != Method::Head {
            request.set_method(Method::Get);
        }
        request.set_uri(Origin::parse(AUTH_ERROR_PATH).expect("a valid path"));
    }
}

/// The error the fairing ended a request with.
pub(crate) struct RefusedRequest(RestError);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for RefusedRequest {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        match request.local_cache(PendingError::default).0.get() {
            Some(error) => Outcome::Success(Self(error.clone())),
            None => Outcome::Forward(Status::NotFound),
        }
    }
}

/// The route that answers a request the fairing refused, at the lowest rank Rocket has, so no
/// other route, however general (`/<a>/<b>`, `/<path..>`), answers it first.
pub(crate) fn refused_routes() -> Vec<rocket::Route> {
    let mut routes = rocket::routes![refused];
    for route in &mut routes {
        route.rank = isize::MIN;
    }
    routes
}

/// Answers a request the fairing refused with its error.
#[rocket::get("/__mission-control/rest-auth-error")]
pub(crate) fn refused(refused: RefusedRequest) -> Json {
    let status = Status::from_code(refused.0.status).unwrap_or(Status::InternalServerError);
    json(status, &refused.0.to_json())
}

/// The request's authentication, worked out once.
struct CachedAuth(Result<Option<CurrentUser>, RestError>);

/// The REST error a guard ended the request with, for the catchers.
#[derive(Default)]
struct PendingError(OnceLock<RestError>);

/// Ends the request with `error`: its status, and the error left for the catchers.
fn fail<T>(request: &Request<'_>, error: RestError) -> Outcome<T, ()> {
    let status = Status::from_code(error.status).unwrap_or(Status::InternalServerError);
    // The first error stays: a later guard on the same request does not replace it.
    let _ = request.local_cache(PendingError::default).0.set(error);
    Outcome::Error((status, ()))
}

async fn current_user<'r>(request: &'r Request<'_>) -> &'r Result<Option<CurrentUser>, RestError> {
    &request
        .local_cache_async(async { CachedAuth(authenticate_request(request).await) })
        .await
        .0
}

async fn authenticate_request(request: &Request<'_>) -> Result<Option<CurrentUser>, RestError> {
    let rocket = request.rocket();
    let (Some(db), Some(settings)) = (
        rocket.state::<Arc<Database>>(),
        rocket.state::<AuthSettings>(),
    ) else {
        log::error!("REST authentication ran before the app was ready");
        return Err(RestError::internal());
    };
    let peer = request.remote().map(|remote| remote.ip().to_canonical());
    let credentials = RequestCredentials {
        authorization: request.headers().get_one("Authorization"),
        is_rest_request: is_rest_request(request),
        is_ssl: rocket.config().tls_enabled()
            || (settings.trusted_proxies.trusts(peer) && forwarded_https(request)),
        environment: settings.environment,
        ip: peer,
    };
    authenticate(db, settings.user_roles.as_ref(), &credentials).await
}

/// Whether the request is for the REST API: a path whose first segment, percent-decoded, is
/// `mc-json`, or the site root with a `rest_route` query starting with `/`.
fn is_rest_request(request: &Request<'_>) -> bool {
    match request.uri().path().segments().next() {
        Some(first) => first == "mc-json",
        None => matches!(
            request.query_value::<&str>("rest_route"),
            Some(Ok(route)) if route.starts_with('/')
        ),
    }
}

/// Whether every `X-Forwarded-Proto` value is `https`. A proxy chain that went over plain HTTP
/// anywhere, or a request with no such header, is not HTTPS.
fn forwarded_https(request: &Request<'_>) -> bool {
    let mut values = request
        .headers()
        .get("X-Forwarded-Proto")
        .flat_map(|header| header.split(','))
        .map(str::trim)
        .peekable();
    values.peek().is_some() && values.all(|proto| proto.eq_ignore_ascii_case("https"))
}

/// The catchers that answer a REST guard's error with WordPress's JSON body: 401, 403 and 500.
/// Another error with one of these statuses gets Rocket's own default answer.
pub(crate) fn catchers() -> Vec<Catcher> {
    [401, 403, 500]
        .into_iter()
        .map(|code| Catcher::new(code, rest_catcher))
        .collect()
}

fn rest_catcher<'r>(status: Status, request: &'r Request<'_>) -> BoxFuture<'r> {
    Box::pin(async move {
        match request.local_cache(PendingError::default).0.get() {
            Some(error) => json(status, &error.to_json()).respond_to(request),
            None => Ok(rocket_default(status, request)),
        }
    })
}

/// Rocket 0.5's default catcher's answer for 401, 403 and 500 (`rocket::catcher::default_handler`
/// is not public): HTML, or JSON when the client prefers it.
fn rocket_default<'r>(status: Status, request: &Request<'_>) -> Response<'r> {
    let (reason, description) = match status.code {
        401 => ("Unauthorized", "The request requires user authentication."),
        403 => ("Forbidden", "The server refused to authorize the request."),
        500 => (
            "Internal Server Error",
            "The server encountered an internal error while processing this request.",
        ),
        _ => ("Unknown Error", "An unknown error has occurred."),
    };
    let code = status.code;
    let prefers_json = request
        .accept()
        .is_some_and(|accept| accept.preferred().is_json());
    let (content_type, text) = if prefers_json {
        (
            ContentType::JSON,
            format!(
                "{{\n  \"error\": {{\n    \"code\": {code},\n    \"reason\": \"{reason}\",\n    \"description\": \"{description}\"\n  }}\n}}"
            ),
        )
    } else {
        (
            ContentType::HTML,
            format!(
                r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="color-scheme" content="light dark">
    <title>{code} {reason}</title>
</head>
<body align="center">
    <div role="main" align="center">
        <h1>{code}: {reason}</h1>
        <p>{description}</p>
        <hr />
    </div>
    <div role="contentinfo" align="center">
        <small>Rocket</small>
    </div>
</body>
</html>"#
            ),
        )
    };
    Response::build()
        .status(status)
        .header(content_type)
        .sized_body(text.len(), Cursor::new(text))
        .finalize()
}
