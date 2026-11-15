//! What the REST controllers' routes share: the id in a path, the request's parameters (query
//! string and body), and the [`Reply`] that sends a controller's answer.
//!
//! Request bodies are JSON or form-encoded, as WordPress reads them, up to the `rest` data limit
//! (for both; [`REST_BODY_LIMIT`], 8 MiB, by default); a larger body is refused with 413
//! `rest_request_too_large` (Mission Control's code; PHP refuses such a body before WordPress
//! runs). Parameters in the body win over those in the query string.

use std::io::Cursor;

use mission_control_core::rest::{Params, RestError, RestResponse, parse_form};
use rocket::data::{Limits, ToByteUnit};
use rocket::http::uri::Origin;
use rocket::http::{ContentType, Header, Status};
use rocket::request::FromParam;
use rocket::response::{self, Responder};
use rocket::{Data, Request, Response};
use serde_json::Value;

/// An id in a path: digits only, as WordPress's `(?P<id>[\d]+)`, that fit an `i64`. Anything
/// else forwards.
pub(crate) struct Id(pub(crate) i64);

impl<'a> FromParam<'a> for Id {
    type Error = ();

    fn from_param(param: &'a str) -> Result<Self, ()> {
        if param.is_empty() || !param.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        param.parse().map(Self).map_err(|_| ())
    }
}

/// A REST answer: the response, or WordPress's error body with its status.
pub(crate) struct Reply(pub(crate) Result<RestResponse, RestError>);

impl<'r> Responder<'r, 'static> for Reply {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let (status, body, headers) = match self.0 {
            Ok(response) => (response.status, response.body, response.headers),
            Err(error) => (error.status, error.to_json(), Vec::new()),
        };
        let text = body.to_string();
        let mut builder = Response::build();
        builder
            .status(Status::from_code(status).unwrap_or(Status::InternalServerError))
            .header(ContentType::new("application", "json").with_params(("charset", "UTF-8")))
            .sized_body(text.len(), Cursor::new(text));
        for (name, value) in headers {
            builder.header_adjoin(Header::new(name, value));
        }
        Ok(builder.finalize())
    }
}

/// The request's query string, as sent.
pub(crate) fn query_of<'a>(origin: &'a Origin<'_>) -> &'a str {
    origin.query().map_or("", |q| q.as_str())
}

/// The largest REST request body (JSON or form) read by default: 8 MiB, PHP's default
/// `post_max_size`, which caps what WordPress accepts (Ruling R58). A site changes it with
/// Rocket's `rest` limit, as any Rocket data limit: `limits.rest = "16 MiB"` under a profile in
/// `Rocket.toml`, or `ROCKET_LIMITS={rest="16 MiB"}`.
pub(crate) const REST_BODY_LIMIT: u64 = 8 * 1024 * 1024;

/// The request's parameters: the query string's, then the body's over them.
pub(crate) async fn params(
    origin: &Origin<'_>,
    content_type: Option<&ContentType>,
    limits: &Limits,
    data: Data<'_>,
) -> Result<Params, RestError> {
    let mut params = parse_form(query_of(origin));
    let json = content_type.is_some_and(|c| c.is_json());
    let form = content_type.is_some_and(|c| c.is_form());
    if !json && !form {
        return Ok(params);
    }
    // One limit for both kinds: a post sent as a form is as large as one sent as JSON, and
    // Rocket's own `json` (1 MiB) and `form` (32 KiB) limits are sized for smaller bodies.
    let limit = limits.get("rest").unwrap_or(REST_BODY_LIMIT.bytes());
    let body = data.open(limit).into_string().await.map_err(|e| {
        log::warn!("Could not read a REST request body: {e}");
        RestError::new(
            "rest_invalid_body",
            "The request body could not be read.",
            400,
        )
    })?;
    if !body.is_complete() {
        return Err(RestError::new(
            "rest_request_too_large",
            "The request body is too large.",
            413,
        ));
    }
    let body = body.into_inner();
    let from_body = if json {
        if body.trim().is_empty() {
            Params::new()
        } else {
            match serde_json::from_str::<Value>(&body) {
                Ok(Value::Object(object)) => object,
                Ok(_) => Params::new(),
                Err(e) => {
                    return Err(RestError::new(
                        "rest_invalid_json",
                        "Invalid JSON body passed.",
                        400,
                    )
                    .with_data("json_error_message", Value::String(e.to_string())));
                }
            }
        }
    } else {
        parse_form(&body)
    };
    params.extend(from_body);
    Ok(params)
}
