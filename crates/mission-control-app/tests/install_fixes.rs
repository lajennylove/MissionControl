//! The installer's fixes after the QA round, each as WordPress answers it:
//!
//! - The site title may be empty: WordPress's `wp-admin/install.php` checks the username, the
//!   passwords and the email, never the title, and `wp_install` stores it as it is.
//! - A repeated install on an installed site answers WordPress's "Already Installed" page with
//!   a `200` (`install.php` checks `is_blog_installed()` first, for every step), not the admin's
//!   503 page.
//! - `/favicon.ico` redirects (`302`) to the site's favicon, as WordPress's `do_favicon`
//!   redirects to the site icon, also when a page was given that slug; a site with no icon gets
//!   the Mission Control favicon.
//!
//! Each test runs the wizard on SQLite in its own folder, then starts the installed app from the
//! `.env` it wrote. On SQLite the admin is not mounted (Ruling R119), so the Already Installed
//! page has no Log In link; `install_admin.rs` checks the link on MySQL and MariaDB.

use std::path::Path;

use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::wizard::TOKEN_FIELD;
use mission_control_app::restart::RestartHandle;
use mission_control_app::setup::{INSTALL_PATH, Mode, setup_rocket_with_restart};
use mission_control_core::config::Config;
use mission_control_core::content::{
    NewPost, Post, PostStatus, get_option, set_option, set_post_meta,
};
use mission_control_testing::unique_email;
use orbit_orm::{Database, raw};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::uri::Host;
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::json;

/// The administrator password the tests type.
const ADMIN_PASSWORD: &str = "admin-pw-Install-Fix-5519";

/// WordPress's Already Installed message, with Mission Control's name.
const ALREADY_INSTALLED: &str = "You appear to have already installed Mission Control. To \
                                 reinstall please clear your old database tables first.";

/// Where `/favicon.ico` redirects a site with no site icon: the Mission Control favicon, as
/// WordPress's `do_favicon` falls back to its own logo.
const FAVICON: &str = "/mc-includes/images/favicon-32.png";

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, NON_ALPHANUMERIC),
                utf8_percent_encode(v, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The value of the hidden field `name` in a page.
fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page
        .find(&marker)
        .unwrap_or_else(|| panic!("the page has a {name} field:\n{page}"))
        + marker.len();
    let end = start + page[start..].find('"').expect("the value ends");
    page[start..end].to_owned()
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

async fn post<'c>(client: &'c Client, uri: &str, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    let mut request = client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(fields));
    request
        .inner_mut()
        .set_host(Host::parse("example.test").expect("a valid host"));
    request.dispatch().await
}

/// Runs the wizard on SQLite in `dir` with the site title `title`, and returns the finished
/// page and the configuration of the `.env` it wrote.
async fn install(dir: &Path, title: &str) -> (String, Config) {
    let handle = RestartHandle::new();
    let client = Client::tracked(setup_rocket_with_restart(dir, &handle))
        .await
        .expect("setup mode ignites");
    let token = hidden(&get(&client, INSTALL_PATH).await.1, TOKEN_FIELD);
    let response = post(
        &client,
        &format!("{INSTALL_PATH}?step=1"),
        &[(TOKEN_FIELD, &token), ("driver", "sqlite")],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    let choice = hidden(
        &get(&client, &format!("{INSTALL_PATH}?step=2")).await.1,
        "database_choice",
    );
    let response = post(
        &client,
        &format!("{INSTALL_PATH}?step=2"),
        &[
            (TOKEN_FIELD, &token),
            ("database_choice", &choice),
            ("weblog_title", title),
            ("user_name", "admin"),
            ("admin_password", ADMIN_PASSWORD),
            ("admin_password2", ADMIN_PASSWORD),
            ("admin_email", "owner@example.com"),
        ],
    )
    .await;
    let status = response.status();
    let page = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::Ok, "{page}");
    assert!(handle.requested(), "the finish asks for the restart");
    let mut config = match Mode::from_lookup(Some(&dir.join(".env")), |_| None) {
        Ok(Mode::Normal(config)) => config,
        other => panic!("the new .env starts normal mode, got {other:?}"),
    };
    config.themes_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes");
    config.plugins_dir = dir.join("plugins");
    config.uploads_dir = dir.join("uploads");
    (page, config)
}

/// The installed app for `config`.
async fn installed_app(config: Config) -> Client {
    Client::tracked(rocket_with_options(config, AppOptions::default()))
        .await
        .expect("the installed app ignites")
}

#[rocket::async_test]
async fn an_empty_site_title_installs_as_wordpress_does() {
    let dir = tempfile::tempdir().unwrap();
    let (page, config) = install(dir.path(), "   ").await;
    assert!(page.contains("Success!"), "{page}");
    let db = Database::connect(&config.database_url).await.unwrap();
    assert_eq!(
        get_option(&db, "blogname").await.unwrap(),
        Some(json!("")),
        "WordPress stores the trimmed, empty title"
    );
    db.close().await;
}

#[rocket::async_test]
async fn a_repeated_install_answers_already_installed() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config) = install(dir.path(), "Fixed Site").await;
    let client = installed_app(config).await;

    for uri in [INSTALL_PATH.to_owned(), format!("{INSTALL_PATH}?step=2")] {
        let (status, page) = get(&client, &uri).await;
        assert_eq!(status, Status::Ok, "{uri}: {page}");
        assert!(page.contains("<h1>Already Installed</h1>"), "{page}");
        assert!(page.contains(ALREADY_INSTALLED), "{page}");
        assert!(page.contains("href=\"http://example.test\""), "{page}");
        assert!(!page.contains("The admin is not available"), "{page}");
        // The admin is not mounted on SQLite: no login to link to.
        assert!(!page.contains("Log In"), "{page}");
        assert!(!page.contains("<form"), "{page}");
    }
    // A form sent to any step gets the same page, and changes nothing.
    for step in ["1", "2"] {
        let response = post(
            &client,
            &format!("{INSTALL_PATH}?step={step}"),
            &[("weblog_title", "Other"), ("driver", "sqlite")],
        )
        .await;
        assert_eq!(response.status(), Status::Ok);
        let page = response.into_string().await.unwrap_or_default();
        assert!(page.contains(ALREADY_INSTALLED), "{page}");
    }
    // The rest of the admin path still answers the admin's 503 page.
    let (status, page) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::ServiceUnavailable, "{page}");
}

#[rocket::async_test]
async fn the_favicon_redirects_ahead_of_a_page_with_its_slug() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config) = install(dir.path(), "Favicon Site").await;
    let db = Database::connect(&config.database_url).await.unwrap();
    raw::execute(
        &db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .expect("inserts the author");
    let author: i64 = raw::fetch_scalar(&db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the author id");
    let page = Post::create(
        &db,
        NewPost {
            status: PostStatus::Publish,
            title: "favicon.ico".to_owned(),
            slug: "favicon.ico".to_owned(),
            content: "A page named after the favicon.".to_owned(),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .expect("creates the page");
    // As WordPress's `sanitize_title`, the dot becomes a hyphen: no post has the favicon's path.
    assert_eq!(page.slug, "favicon-ico");
    db.close().await;

    let client = installed_app(config).await;
    // WordPress answers `/favicon.ico` with a redirect, not the file (`do_favicon`).
    let response = client.get("/favicon.ico").dispatch().await;
    assert_eq!(response.status(), Status::Found);
    assert_eq!(response.headers().get_one("Location"), Some(FAVICON));
    // The redirect's target is the favicon itself.
    let response = client.get(FAVICON).dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.content_type(), Some(ContentType::PNG));
    // The page is still reachable at its own address.
    let (status, body) = get(&client, &format!("/?page_id={}", page.id)).await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains("A page named after the favicon."), "{body}");
}

#[rocket::async_test]
async fn the_favicon_redirects_to_the_site_icon() {
    let dir = tempfile::tempdir().unwrap();
    let (_, config) = install(dir.path(), "Icon Site").await;
    let db = Database::connect(&config.database_url).await.unwrap();
    raw::execute(
        &db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .expect("inserts the author");
    let author: i64 = raw::fetch_scalar(&db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the author id");
    let icon = Post::create(
        &db,
        NewPost {
            status: PostStatus::Inherit,
            title: "icon".to_owned(),
            mime_type: "image/png".to_owned(),
            ..NewPost::new(author, "attachment")
        },
    )
    .await
    .expect("creates the attachment");
    set_post_meta(
        &db,
        icon.id,
        "_wp_attached_file",
        &json!("2026/11/icon.png"),
    )
    .await
    .expect("stores the file");
    // As WordPress's `get_site_icon_url( 32 )`: the smallest size that covers 32 by 32.
    set_post_meta(
        &db,
        icon.id,
        "_wp_attachment_metadata",
        &json!({
            "width": 512, "height": 512, "file": "2026/11/icon.png", "filesize": 1000,
            "sizes": {
                "thumbnail": {"file": "icon-150x150.png", "width": 150, "height": 150,
                              "mime-type": "image/png", "filesize": 100},
                "tiny": {"file": "icon-16x16.png", "width": 16, "height": 16,
                         "mime-type": "image/png", "filesize": 10},
            },
        }),
    )
    .await
    .expect("stores the metadata");
    set_option(&db, "site_icon", &json!(icon.id))
        .await
        .expect("sets the site icon");
    db.close().await;

    let client = installed_app(config).await;
    let response = client.get("/favicon.ico").dispatch().await;
    assert_eq!(response.status(), Status::Found);
    assert_eq!(
        response.headers().get_one("Location"),
        Some("/mc-content/uploads/2026/11/icon-150x150.png")
    );
}
