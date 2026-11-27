//! A refused save's own message on its form field, for the settings screens and the Menus
//! screen: Ignite answers their data layer's refusal with the form (422) and "<label> is already
//! taken." under the field, and this fairing puts the refusal's message there instead (see
//! `mission_control_core::admin::field_errors`).
//
// TODO(Ignite field errors): remove with `field_errors` once a runtime resource's data layer can
// return field errors.

use std::io::Cursor;

use mission_control_core::admin::{NAV_MENUS, SettingsScreen, with_field_messages};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Method, Status};
use rocket::{Request, Response};

use super::ADMIN_SEGMENT;

pub(crate) struct FieldMessages;

#[rocket::async_trait]
impl Fairing for FieldMessages {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control admin field messages",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if request.method() != Method::Post
            || response.status() != Status::UnprocessableEntity
            || response.content_type() != Some(ContentType::HTML)
        {
            return;
        }
        let mut segments = request.uri().path().segments();
        let (Some(ADMIN_SEGMENT), Some(slug)) = (segments.next(), segments.next()) else {
            return;
        };
        if slug != NAV_MENUS && SettingsScreen::from_slug(slug).is_none() {
            return;
        }
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        let html = with_field_messages(slug, &html).unwrap_or(html);
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}
