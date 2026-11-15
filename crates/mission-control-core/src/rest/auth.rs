//! Who a REST request runs as: authentication by application password, after WordPress's
//! `wp_validate_application_password` and `rest_application_password_check_errors`.
//!
//! [`authenticate`] takes what it needs from the request ([`RequestCredentials`]) and returns
//! the signed-in user, no user (a visitor, WordPress's user 0), or the error the whole request
//! ends with. It holds no Rocket types: the `mission-control` crate's request guards call it once
//! per request.
//!
//! As WordPress does:
//!
//! - Application passwords are tried only on REST requests (WordPress's "API requests only",
//!   `application_password_is_api_request`).
//! - They are tried only when available ([`app_passwords_available`]): over HTTPS, or anywhere in
//!   a `local` environment. Otherwise the credentials are ignored and the request is anonymous,
//!   as `wp_validate_application_password` returns without trying them.
//! - A request with no `Authorization` header, or one that is not `Basic` credentials, is
//!   anonymous. A route that needs a user answers it with [`not_logged_in`].
//! - Credentials that are tried and fail end the request with status 401 and WordPress's code:
//!   `invalid_email`, `invalid_username` or `incorrect_password`, and no `WWW-Authenticate`
//!   header. That holds for every REST route, even one a visitor could call, as
//!   `rest_application_password_check_errors` runs before any route.
//! - The user's roles come from the app's [`UserRoles`] provider (Controller ruling R38).
//!
//! **Not yet: cookie and nonce authentication** (Controller ruling R46). WordPress's admin calls
//! the API with its logged-in cookie and an `X-WP-Nonce` header (`rest_cookie_check_errors`):
//! the cookie is tried before application passwords (`determine_current_user` runs
//! `wp_validate_auth_cookie` first), a cookie without a nonce is a visitor, and a bad nonce is
//! 403 `rest_cookie_invalid_nonce`. It rides on the logged-in session, which is Ignite's and not
//! usable on every database yet. It will be a second [`AuthMethod`] and a check before
//! application passwords in [`authenticate`].
//!
//! Differences from WordPress, on purpose:
//!
//! - WordPress ignores Basic credentials until the site has ever had an application password
//!   (`WP_Application_Passwords::is_in_use`). Mission Control always checks them, so a client
//!   with wrong credentials gets an error rather than silently running as a visitor.
//! - An `Authorization` header longer than [`MAX_AUTHORIZATION_LENGTH`] bytes is not tried (the
//!   request is anonymous). Rocket bounds headers only at the HTTP layer (hyper's 400 KiB
//!   HTTP/1 buffer, 16 MiB of HTTP/2 headers), far above any real credentials.

use std::fmt;
use std::net::IpAddr;

use orbit_orm::Database;

use crate::config::Environment;
use crate::rest::RestError;
use crate::users::app_password::{self, BasicAuthError, ByEmail};
use crate::users::{UserCaps, UserRoles};

/// The longest `Authorization` header tried, in bytes. Credentials are a user name of at most
/// 255 characters (the `users.email` column), a colon and a 24-character password, in base64:
/// well under this.
pub const MAX_AUTHORIZATION_LENGTH: usize = 2048;

/// WordPress's `invalid_email` message.
const INVALID_EMAIL: &str =
    "<strong>Error:</strong> Unknown email address. Check again or try your username.";
/// WordPress's `invalid_username` message, for application passwords.
const INVALID_USERNAME: &str =
    "<strong>Error:</strong> Unknown username. Check again or try your email address.";
/// WordPress's `incorrect_password` message, for application passwords.
const INCORRECT_PASSWORD: &str = "The provided password is an invalid application password.";

/// How the current user signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthMethod {
    /// By an application password, over HTTP Basic auth.
    ApplicationPassword {
        /// The public id of the password used, WordPress's
        /// `rest_get_authenticated_app_password`.
        uuid: String,
    },
}

/// The user a REST request runs as, with their capabilities.
#[derive(Clone, PartialEq, Eq)]
pub struct CurrentUser {
    caps: UserCaps,
    method: AuthMethod,
}

impl CurrentUser {
    /// The user `caps` describes, signed in by `method`.
    pub fn new(caps: UserCaps, method: AuthMethod) -> Self {
        Self { caps, method }
    }

    /// The user's id.
    pub fn id(&self) -> i64 {
        self.caps.id()
    }

    /// The user's roles and capabilities.
    pub fn caps(&self) -> &UserCaps {
        &self.caps
    }

    /// How the user signed in.
    pub fn method(&self) -> &AuthMethod {
        &self.method
    }

    /// The uuid of the application password the request signed in with, as WordPress's
    /// `rest_get_authenticated_app_password`; `None` for any other way in.
    pub fn app_password_uuid(&self) -> Option<&str> {
        match &self.method {
            AuthMethod::ApplicationPassword { uuid } => Some(uuid),
        }
    }
}

impl fmt::Debug for CurrentUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CurrentUser")
            .field("id", &self.caps.id())
            .field("roles", &self.caps.roles())
            .field("method", &self.method)
            .finish()
    }
}

/// What [`authenticate`] reads from a request.
#[derive(Clone, Copy)]
pub struct RequestCredentials<'a> {
    /// The `Authorization` header, if any.
    pub authorization: Option<&'a str>,
    /// Whether the request is for the REST API (`/wp-json/...` or `?rest_route=`).
    pub is_rest_request: bool,
    /// Whether the request reached the site over HTTPS, WordPress's `is_ssl`.
    pub is_ssl: bool,
    /// The site's environment.
    pub environment: Environment,
    /// The client's address, recorded with the password's usage (WordPress's `REMOTE_ADDR`).
    pub ip: Option<IpAddr>,
}

impl fmt::Debug for RequestCredentials<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestCredentials")
            .field("authorization", &self.authorization.map(|_| "[hidden]"))
            .field("is_rest_request", &self.is_rest_request)
            .field("is_ssl", &self.is_ssl)
            .field("environment", &self.environment)
            .field("ip", &self.ip)
            .finish()
    }
}

/// Whether application passwords can be used, as WordPress's `wp_is_application_passwords_supported`:
/// over HTTPS, or in a `local` environment.
pub fn app_passwords_available(is_ssl: bool, environment: Environment) -> bool {
    is_ssl || environment == Environment::Local
}

/// The user `request` runs as: `Ok(Some(user))` when its application password is right,
/// `Ok(None)` for a visitor, or the error the request ends with.
///
/// # Errors
///
/// A 401 [`RestError`] with WordPress's code when the credentials are tried and refused
/// (`invalid_email`, `invalid_username`, `incorrect_password`), or a 500 one when the database
/// or the roles provider fails: the request is refused rather than run as a visitor.
pub async fn authenticate(
    db: &Database,
    roles: &dyn UserRoles,
    request: &RequestCredentials<'_>,
) -> Result<Option<CurrentUser>, RestError> {
    if !request.is_rest_request || !app_passwords_available(request.is_ssl, request.environment) {
        return Ok(None);
    }
    let Some(authorization) = request.authorization else {
        return Ok(None);
    };
    if authorization.len() > MAX_AUTHORIZATION_LENGTH {
        return Ok(None);
    }
    let ip = request.ip.map(|ip| ip.to_string());
    let item =
        match app_password::authenticate_basic(db, authorization, &ByEmail, ip.as_deref()).await {
            Ok(item) => item,
            Err(BasicAuthError::Malformed) => return Ok(None),
            Err(e) => return Err(basic_auth_error(&e)),
        };
    let assignment = roles.roles(db, item.user_id).await.map_err(|e| {
        log::error!("Could not read the roles of user {}: {e}", item.user_id);
        RestError::internal()
    })?;
    Ok(Some(CurrentUser::new(
        assignment.user_caps(item.user_id),
        AuthMethod::ApplicationPassword { uuid: item.uuid },
    )))
}

/// The REST error for refused Basic credentials, as `rest_application_password_check_errors`
/// gives it: WordPress's code and message with status 401. A database error is a 500, logged.
///
/// [`BasicAuthError::Malformed`] is not a refusal: credentials that are not Basic are no
/// credentials, and the caller treats the request as a visitor's without calling this (as
/// [`authenticate`] does). Passed anyway, it fails closed with a 500.
pub fn basic_auth_error(error: &BasicAuthError) -> RestError {
    let message = match error {
        BasicAuthError::InvalidEmail => INVALID_EMAIL,
        BasicAuthError::InvalidUsername => INVALID_USERNAME,
        BasicAuthError::IncorrectPassword => INCORRECT_PASSWORD,
        // The caller's job, see above: reaching here is a bug, refused rather than let through.
        BasicAuthError::Malformed => {
            log::error!("Malformed Basic credentials were passed as a refusal");
            return RestError::internal();
        }
        BasicAuthError::Database(e) => {
            log::error!("Could not check an application password: {e}");
            return RestError::internal();
        }
    };
    let code = error.code().expect("every refusal has a WordPress code");
    RestError::new(code, message, 401)
}

/// The error of a route that needs a signed-in user, for a visitor: WordPress's
/// `rest_not_logged_in`, status 401.
pub fn not_logged_in() -> RestError {
    RestError::new(
        "rest_not_logged_in",
        "You are not currently logged in.",
        401,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_passwords_need_https_outside_a_local_environment() {
        assert!(app_passwords_available(true, Environment::Production));
        assert!(app_passwords_available(false, Environment::Local));
        for environment in [
            Environment::Development,
            Environment::Staging,
            Environment::Production,
        ] {
            assert!(!app_passwords_available(false, environment));
        }
    }

    #[test]
    fn refusals_are_401_with_wordpresss_codes() {
        for (error, code) in [
            (BasicAuthError::InvalidEmail, "invalid_email"),
            (BasicAuthError::InvalidUsername, "invalid_username"),
            (BasicAuthError::IncorrectPassword, "incorrect_password"),
        ] {
            let rest = basic_auth_error(&error);
            assert_eq!(rest.code, code);
            assert_eq!(rest.status, 401);
        }
        assert_eq!(basic_auth_error(&BasicAuthError::Malformed).status, 500);
    }

    #[test]
    fn the_error_body_is_wordpresss_shape() {
        assert_eq!(
            not_logged_in().to_json(),
            serde_json::json!({
                "code": "rest_not_logged_in",
                "message": "You are not currently logged in.",
                "data": {"status": 401},
            })
        );
    }

    #[test]
    fn debug_hides_the_credentials() {
        let request = RequestCredentials {
            authorization: Some("Basic c2VjcmV0OnNlY3JldA=="),
            is_rest_request: true,
            is_ssl: true,
            environment: Environment::Production,
            ip: None,
        };
        assert!(!format!("{request:?}").contains("c2Vj"));
    }
}
