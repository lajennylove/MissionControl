//! The installer's path once the site is installed: `/mc-admin/install.php` answers WordPress's
//! "Already Installed" page.
//!
//! WordPress's `wp-admin/install.php` checks `is_blog_installed()` (the `siteurl` option) before
//! anything else, for every step and method, and answers with a `200` page: "Already
//! Installed", "You appear to have already installed WordPress. To reinstall please clear your
//! old database tables first." and a Log In link. The installed app does the same, ahead of the
//! admin's own routes and of its 503 page on a database the admin does not run on, so a repeated
//! install learns the real reason. The page links to the site, and to the login only when the
//! admin is mounted.
//!
//! A database without `siteurl` (or whose options cannot be read) does not hold a site: the
//! request goes on to the admin's routes, as before.

use std::sync::Arc;

use mission_control_core::content::get_option;
use orbit_orm::Database;
use rocket::http::{Method, Status};
use rocket::response::content::RawHtml;
use rocket::route::{Handler, Outcome, Route};
use rocket::{Data, Request};

use crate::admin::ADMIN_PATH;

/// The installer's path, as setup mode serves it: WordPress's `wp-admin/install.php`.
const INSTALL_PATH: &str = "/mc-admin/install.php";

/// Ahead of every admin route, the admin's 503 page (rank -20) included.
const INSTALLED_RANK: isize = -40;

/// Answers the installer's path on an installed site.
#[derive(Debug, Clone, Copy)]
struct AlreadyInstalled {
    /// Whether the admin is mounted, so the page links to its login.
    admin: bool,
}

#[rocket::async_trait]
impl Handler for AlreadyInstalled {
    async fn handle<'r>(&self, request: &'r Request<'_>, data: Data<'r>) -> Outcome<'r> {
        let Some(db) = request.rocket().state::<Arc<Database>>() else {
            return Outcome::forward(data, Status::NotFound);
        };
        // WordPress's `is_blog_installed`: the `siteurl` option is set.
        let site_url = match get_option(db, "siteurl").await {
            Ok(Some(value)) => value,
            Ok(None) => return Outcome::forward(data, Status::NotFound),
            Err(e) => {
                log::warn!("The installer could not read the siteurl option: {e}");
                return Outcome::forward(data, Status::NotFound);
            }
        };
        let home = match get_option(db, "home").await {
            Ok(Some(value)) => value,
            _ => site_url,
        };
        let home = home.as_str().filter(|h| !h.is_empty()).unwrap_or("/");
        let login = format!("{ADMIN_PATH}/login");
        let page = already_installed_page(home, self.admin.then_some(login.as_str()));
        Outcome::from(request, (Status::Ok, RawHtml(page)))
    }
}

/// The routes at the installer's path, for `GET` and `POST` (every step of WordPress's form).
/// `admin` is whether the admin is mounted.
pub(crate) fn routes(admin: bool) -> Vec<Route> {
    let handler = AlreadyInstalled { admin };
    [Method::Get, Method::Post]
        .into_iter()
        .map(|method| Route::ranked(INSTALLED_RANK, method, INSTALL_PATH, handler))
        .collect()
}

/// WordPress's "Already Installed" page, in the installer's look: the heading, WordPress's
/// message with Mission Control's name, a link to the site at `home`, and the Log In link to
/// `login` when there is one.
fn already_installed_page(home: &str, login: Option<&str>) -> String {
    let login = login.map_or_else(String::new, |login| {
        format!("<a class=\"button\" href=\"{}\">Log In</a> ", escape(login))
    });
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex, nofollow">
<title>Mission Control &rsaquo; Already Installed</title>
<style>
body {{ background: #f0f0f1; color: #3c434a; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; font-size: 14px; line-height: 1.5; margin: 0; padding: 2em 16px; }}
.wizard {{ background: #fff; box-shadow: 0 1px 3px rgba(0, 0, 0, 0.13); margin: 0 auto; max-width: 700px; padding: 1em 2em 2em; }}
h1 {{ border-bottom: 1px solid #dcdcde; font-size: 1.6em; padding-bottom: 0.5em; }}
.button {{ background: #2271b1; border: 1px solid #2271b1; border-radius: 3px; color: #fff; font-size: 14px; padding: 6px 14px; text-decoration: none; }}
.step {{ margin-top: 1.5em; }}
</style>
</head>
<body>
<div class="wizard" id="already-installed">
<h1>Already Installed</h1>
<p>You appear to have already installed Mission Control. To reinstall please clear your old database tables first.</p>
<p class="step">{login}<a href="{home}">Visit the site</a></p>
</div>
</body>
</html>
"#,
        home = escape(home),
    )
}

/// `text` escaped for an HTML attribute or text.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#039;"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_has_wordpress_wording_and_escapes_the_address() {
        let page = already_installed_page("http://x.test/?a=\"b\"&c", Some("/mc-admin/login"));
        assert!(page.contains("<h1>Already Installed</h1>"));
        assert!(page.contains(
            "You appear to have already installed Mission Control. To reinstall please clear \
             your old database tables first."
        ));
        assert!(page.contains("href=\"/mc-admin/login\">Log In</a>"));
        assert!(page.contains("href=\"http://x.test/?a=&quot;b&quot;&amp;c\""));
        let page = already_installed_page("/", None);
        assert!(!page.contains("Log In"));
    }
}
