//! What the admin's pages show around their content, where Ignite 0.6.7 renders otherwise:
//!
//! - **The user's name** ([`ChromeViews`]): the top bar ("Howdy, ..." and the user menu in the
//!   WordPress admin theme, the name beside the picture in the Ignite theme) and the Ignite
//!   theme's dashboard greeting show the user's display name ("Display name publicly as", see
//!   `mission_control_core::users::profile::display_names`), as WordPress shows `display_name`
//!   in its admin bar. Ignite's pages show the username (`users.name`), which is the login here.
//! - **A refused profile picture** ([`ChromeViews`]): Ignite's message for a picture it does not
//!   keep ([`PICTURE_REFUSED`]) is flashed as any other message, in the success notice; it is
//!   shown as an error instead (WordPress's `notice-error` in the WordPress theme, Ignite's error
//!   style in the Ignite theme), with the same words.
//! - **Page not found** ([`not_found`]): an address under the admin that no screen answers, or a
//!   record that does not exist, answers 404 with a page in the admin's look (the brand, the
//!   current admin theme's colours) with a short message and a link to the dashboard, in place
//!   of Rocket's bare page; a GET under the admin that no route takes ([`routes`]) is answered
//!   so, not by the public site. Ignite 0.6.7 has no way for an app to render a page in its layout
//!   (its menu and top bar), so the page is the admin's own, as the refusals' pages are.

use std::io::Cursor;

use ignite_panel::AdminUser;
use mission_control_core::users::profile::display_names;
use orbit_orm::Database;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Method, Status};
use rocket::request::Outcome;
use rocket::response::content::RawHtml;
use rocket::{Request, Response};

use super::wordpress_theme::NAME as WORDPRESS;
use super::{ADMIN_PATH, ADMIN_SEGMENT, BRAND_LOGO, BRAND_NAME, FAVICON, PICTURE_REFUSED};

/// Text escaped as the admin's views escape it (Propel's `{{ }}`).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// The display name and the refused picture's notice on the admin's pages. See the
/// [module](self).
pub(crate) struct ChromeViews;

#[rocket::async_trait]
impl Fairing for ChromeViews {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control admin chrome",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if !matches!(request.method(), Method::Get | Method::Post)
            || response.content_type() != Some(ContentType::HTML)
            || request.uri().path().segments().next() != Some(ADMIN_SEGMENT)
        {
            return;
        }
        let Outcome::Success(user) = request.guard::<AdminUser>().await else {
            return;
        };
        let Some(db) = request.rocket().state::<std::sync::Arc<Database>>() else {
            return;
        };
        let shown = match display_names(db, &[user.user_id()]).await {
            Ok(mut names) => names.remove(&user.user_id()),
            Err(e) => {
                log::error!(
                    "Could not read the display name of user {}: {e}",
                    user.user_id()
                );
                None
            }
        };
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        let mut html = with_picture_error(&html);
        if let Some(shown) = shown.filter(|shown| shown != user.name()) {
            html = with_display_name(&html, user.name(), &shown);
        }
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}

/// `html` with the name `username` shown as `shown` where the top bar and the dashboard's
/// greeting show the user's name: the WordPress theme's `.display-name` spans, the Ignite
/// theme's top bar link to the profile, and its "Welcome back, ...".
fn with_display_name(html: &str, username: &str, shown: &str) -> String {
    let (from, to) = (escape(username), escape(shown));
    let mut html = html.replace(
        &format!("<span class=\"display-name\">{from}</span>"),
        &format!("<span class=\"display-name\">{to}</span>"),
    );
    html = html.replace(
        &format!("Welcome back, {from}</h1>"),
        &format!("Welcome back, {to}</h1>"),
    );
    // The Ignite theme's top bar: the link to the profile holds the picture, then the name.
    let link = format!("href=\"{ADMIN_PATH}/profile\">");
    if let Some(start) = html.find(&link) {
        let name = format!("{from}</a>");
        if let Some(at) = html[start..].find("</a>").map(|end| start + end)
            && html[..at + "</a>".len()].ends_with(&name)
        {
            let at = at + "</a>".len() - name.len();
            html.replace_range(at..at + from.len(), &to);
        }
    }
    html
}

/// The WordPress theme's flash notice, as its partial renders it.
const WORDPRESS_NOTICE: (&str, &str) = (
    "<div class=\"notice notice-success\" role=\"status\"><p>",
    "<div class=\"notice notice-error\" role=\"alert\"><p>",
);

/// The Ignite theme's flash message, as its partial renders it.
const IGNITE_NOTICE: (&str, &str) = (
    "<p class=\"mb-5 rounded-md border border-primary/30 bg-lightprimary px-4 py-3 \
     text-heading\" role=\"status\">",
    "<p class=\"mb-5 rounded-md border border-error bg-lighterror px-4 py-3 \
     text-error-emphasis\" role=\"alert\">",
);

/// `html` with Ignite's refusal of a profile picture ([`PICTURE_REFUSED`]) in the error notice
/// of the theme instead of its success notice.
fn with_picture_error(html: &str) -> String {
    let message = escape(PICTURE_REFUSED);
    let mut html = html.to_owned();
    for (success, error) in [WORDPRESS_NOTICE, IGNITE_NOTICE] {
        html = html.replace(&format!("{success}{message}"), &format!("{error}{message}"));
    }
    html
}

/// The heading of the admin's page for an address nothing answers.
pub(crate) const NOT_FOUND_TITLE: &str = "Page not found";

/// What the page says.
pub(crate) const NOT_FOUND_MESSAGE: &str =
    "Sorry, the page you are looking for does not exist or is no longer here.";

/// The link back.
pub(crate) const NOT_FOUND_BACK: &str = "Go to the Dashboard";

/// The rank of [`routes`]' catch-all: after every admin route (Ignite's and Mission Control's
/// have negative or small ranks), before the public site's catch-all (`FRONT_RANK`, 20), so the
/// public site never answers an address under the admin.
const NOT_FOUND_RANK: isize = 19;

/// The admin's catch-all: any GET under the admin path that no screen answers is not found, and
/// [`not_found`] answers it.
pub(crate) fn routes() -> Vec<rocket::Route> {
    let mut routes = rocket::routes![unknown_admin_path];
    for route in &mut routes {
        route.rank = NOT_FOUND_RANK;
    }
    routes
}

#[rocket::get("/<_..>")]
fn unknown_admin_path() -> Status {
    Status::NotFound
}

/// Answers 404 under the admin path with the admin's page; see the [module](self).
#[rocket::catch(404)]
pub(crate) async fn not_found(request: &Request<'_>) -> (Status, RawHtml<String>) {
    let theme = match request.guard::<AdminUser>().await {
        Outcome::Success(user) => user.admin_theme().to_owned(),
        _ => ignite_panel::active_admin_theme(),
    };
    (Status::NotFound, not_found_page(theme == WORDPRESS))
}

/// The admin's 404 page: WordPress's error page look (`wp_die`) in the WordPress theme, the
/// Ignite theme's colours otherwise.
pub(crate) fn not_found_page(wordpress: bool) -> RawHtml<String> {
    let (page, text, heading, link, card, border) = if wordpress {
        (
            "#f0f0f1", "#3c434a", "#1d2327", "#2271b1", "#fff", "#c3c4c7",
        )
    } else {
        (
            "#f4f7fb", "#5a6a85", "#2a3547", "#5d87ff", "#fff", "#e5eaef",
        )
    };
    let font = if wordpress {
        "-apple-system, BlinkMacSystemFont, \"Segoe UI\", Roboto, Oxygen-Sans, Ubuntu, \
         Cantarell, \"Helvetica Neue\", sans-serif"
    } else {
        "\"Plus Jakarta Sans\", system-ui, -apple-system, \"Segoe UI\", Roboto, sans-serif"
    };
    let body_class = if wordpress { "wp-admin wp-core-ui" } else { "" };
    RawHtml(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{NOT_FOUND_TITLE} &lsaquo; {BRAND_NAME}</title>
<link rel="icon" href="{FAVICON}">
<style>
body {{ background: {page}; color: {text}; font-family: {font}; font-size: 14px; line-height: 1.5; margin: 0; }}
#error-page {{ background: {card}; border: 1px solid {border}; border-radius: 6px; box-sizing: border-box; margin: 64px auto; max-width: 560px; padding: 32px; }}
#error-page .brand {{ display: block; height: 3rem; margin: 0 auto 24px; width: auto; }}
#error-page h1 {{ color: {heading}; font-size: 22px; font-weight: 600; margin: 0 0 12px; }}
#error-page p {{ margin: 0 0 16px; }}
#error-page a {{ color: {link}; }}
</style>
</head>
<body class="{body_class}">
<main id="error-page">
<img class="brand" src="{BRAND_LOGO}" alt="{BRAND_NAME}">
<h1>{NOT_FOUND_TITLE}</h1>
<p>{NOT_FOUND_MESSAGE}</p>
<p><a href="{ADMIN_PATH}">{NOT_FOUND_BACK}</a></p>
</main>
</body>
</html>"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_name_replaces_the_username_in_both_themes() {
        let wordpress =
            "<span class=\"display-name\">ada</span> x <span class=\"display-name\">ada</span>";
        assert_eq!(
            with_display_name(wordpress, "ada", "Ada L"),
            "<span class=\"display-name\">Ada L</span> x <span class=\"display-name\">Ada L</span>"
        );
        let ignite = "<a class=\"flex\" href=\"/mc-admin/profile\"><span>A</span>ada</a>\
                      <h1 class=\"mb-1 text-2xl\">Welcome back, ada</h1><td>ada</td>";
        assert_eq!(
            with_display_name(ignite, "ada", "Ada <L>"),
            "<a class=\"flex\" href=\"/mc-admin/profile\"><span>A</span>Ada &lt;L&gt;</a>\
             <h1 class=\"mb-1 text-2xl\">Welcome back, Ada &lt;L&gt;</h1><td>ada</td>"
        );
    }

    #[test]
    fn the_refused_picture_is_an_error_notice() {
        let wordpress = format!("{}{PICTURE_REFUSED}</p></div>", WORDPRESS_NOTICE.0);
        assert_eq!(
            with_picture_error(&wordpress),
            format!("{}{PICTURE_REFUSED}</p></div>", WORDPRESS_NOTICE.1)
        );
        let ignite = format!("{}{PICTURE_REFUSED}</p>", IGNITE_NOTICE.0);
        assert_eq!(
            with_picture_error(&ignite),
            format!("{}{PICTURE_REFUSED}</p>", IGNITE_NOTICE.1)
        );
        let saved = format!("{}Picture saved.</p>", IGNITE_NOTICE.0);
        assert_eq!(with_picture_error(&saved), saved);
    }

    #[test]
    fn the_not_found_page_links_to_the_dashboard() {
        let page = not_found_page(true).0;
        assert!(page.contains("<a href=\"/mc-admin\">Go to the Dashboard</a>"));
        assert!(page.contains("background: #f0f0f1"));
        assert!(not_found_page(false).0.contains(NOT_FOUND_TITLE));
    }
}
