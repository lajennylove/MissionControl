//! The admin's settings screens and Menus screen (Task 6.5, Rulings R132 and R133): each screen
//! saves to `options`, the Permalinks screen changes the links the site answers on, editors are
//! refused, and a menu assigned to a theme location shows in the default theme's header.
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so the flow runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of its own
//! ([`scratch_database`]) with the installer's migrations, dropped at the end; otherwise it says
//! why it is skipped and passes. Ignite reads its database from `DATABASE_URL`, so the flow sets
//! it for the process; every test here holds [`LOCK`] so none runs while it changes.
//!
//! On SQLite, the settings and menus paths answer the admin's 503 page.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, get_option, set_option,
};
use mission_control_core::media::ImageSizes;
use mission_control_core::migrations::migrator;
use mission_control_core::theme::{get_nav_menu, menu_items};
use mission_control_testing::{ScratchDatabase, create_users_table, scratch_database};
use orbit_orm::Database;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// Held by every test here: the flow sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
const PASSWORD: &str = "correct horse battery staple";

/// The site's address in the tests.
const HOME: &str = "http://localhost";

/// `MC_TEST_DATABASE_URL` when it names MySQL or MariaDB.
fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
}

/// The themes shipped with Mission Control.
fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

fn config(database_url: &str) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: shipped_themes(),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment: Environment::Local,
        trusted_proxies: TrustedProxies::None,
    }
}

/// A form body, URL-encoded.
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

/// The CSRF token in a page's form.
fn token_in(html: &str) -> String {
    let start = html
        .find("name=\"_token\" value=\"")
        .unwrap_or_else(|| panic!("a form token in: {html}"))
        + "name=\"_token\" value=\"".len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
}

fn location(response: &LocalResponse<'_>) -> String {
    response
        .headers()
        .get_one("Location")
        .unwrap_or_default()
        .to_owned()
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

async fn post<'c>(client: &'c Client, uri: &str, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(fields))
        .dispatch()
        .await
}

/// The CSRF token of the client's browser session, read from the sign-in page.
async fn token(client: &Client) -> String {
    // A signed-in client is sent away from the sign-in page, so its token comes from the dashboard.
    let (mut status, mut html) = get(client, "/mc-admin/login").await;
    if status == Status::SeeOther {
        (status, html) = get(client, "/mc-admin").await;
    }
    assert_eq!(status, Status::Ok, "{html}");
    token_in(&html)
}

/// The CSRF token of a signed-in client's session, read from the dashboard (the sign-in page
/// redirects a signed-in client).
async fn dashboard_token(client: &Client) -> String {
    let (status, html) = get(client, "/mc-admin").await;
    assert_eq!(status, Status::Ok, "{html}");
    token_in(&html)
}

async fn sign_in(client: &Client, email: &str) {
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
}

async fn sign_out(client: &Client) {
    // Signed in, the sign-in page redirects, so the token comes from the dashboard.
    let csrf = token_in(&get(client, "/mc-admin").await.1);
    let response = post(client, "/mc-admin/logout", &[("_token", &csrf)]).await;
    assert_eq!(response.status(), Status::SeeOther);
}

/// An Ignite user with the Ignite role `role`. Returns the id and the email.
async fn user(db: &Database, name: &str, role: &str) -> (i64, String) {
    let email = format!("{name}@example.test");
    let created = users::create_user(db, name, &email, PASSWORD)
        .await
        .expect("creates the user");
    authz::assign_role(db, &email, role)
        .await
        .expect("gives the role");
    (created.id, email)
}

async fn option(db: &Database, name: &str) -> Option<Value> {
    get_option(db, name).await.expect("reads the option")
}

/// Saves the settings screen `slug` with `fields` and the session's token, as its form does,
/// and follows the redirects back to the form, which says the changes are saved.
async fn save_screen(client: &Client, slug: &str, fields: &[(&str, &str)]) {
    let csrf = token(client).await;
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = post(client, &format!("/mc-admin/{slug}/1"), &all).await;
    let status = response.status();
    let to = location(&response);
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "saves {slug}: {body}");
    // The save stays on the screen's form (Ignite's `RedirectAfter::Edit`), as `options.php`
    // goes back to its screen.
    assert_eq!(to, format!("/mc-admin/{slug}/1/edit"));
    let (status, html) = get(client, &to).await;
    assert_eq!(status, Status::Ok);
    assert!(html.contains("Changes saved."), "{html}");
}

fn assert_refused(status: Status, body: &str, message: &str) {
    assert_eq!(status, Status::Forbidden, "{body}");
    assert!(body.contains(message), "{body}");
}

#[test]
fn administrators_manage_settings_and_menus() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(_) = mysql_test_url() else {
        eprintln!(
            "skipped: the admin runs on MySQL or MariaDB only (Ruling R119); set \
             MC_TEST_DATABASE_URL to a mysql:// URL to run this test"
        );
        return;
    };
    let scratch = rocket::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scratch_database("settings"));
    // SAFETY: no other thread reads or writes the environment now. Every test of this process
    // holds `LOCK`, and the runtime above is gone; the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(flows(&scratch));
    // SAFETY: as above: the runtime of the flows has ended, and `LOCK` is held.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

async fn flows(scratch: &ScratchDatabase) {
    run_migrations(scratch.url()).await.expect("migrates");
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
    set_option(&db, "blogname", &json!("Settings Test"))
        .await
        .unwrap();
    set_option(&db, "home", &json!(HOME)).await.unwrap();
    // The `default` theme, by name: the front end's default is Twenty Twenty-Six.
    set_option(&db, "stylesheet", &json!("default"))
        .await
        .unwrap();
    set_option(&db, "siteurl", &json!(HOME)).await.unwrap();
    set_option(&db, "admin_email", &json!("owner@example.com"))
        .await
        .unwrap();
    let client = Client::tracked(rocket_with_options(
        config(scratch.url()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    let (admin_id, admin) = user(&db, "ada", "administrator").await;
    let (_, editor) = user(&db, "edna", "editor").await;

    // A visitor is sent to the sign-in page.
    for uri in ["/mc-admin/options-general", "/mc-admin/nav-menus"] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::SeeOther, "{uri}");
        assert_eq!(location(&response), "/mc-admin/login", "{uri}");
    }

    sign_in(&client, &admin).await;
    let (status, dashboard) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::Ok);
    // The sidebar's Settings entry is a label (Ignite's `NavigationItem`) that opens General,
    // its first screen; the seven screens, General first, are its children, shown on a
    // settings screen.
    for entry in [
        "href=\"/mc-admin/options-general\"><i class=\"ti ti-settings text-lg\"></i>Settings</a>",
        "</i>Menus</a>",
    ] {
        assert!(dashboard.contains(entry), "{entry}: {dashboard}");
    }
    let (_, general) = get(&client, "/mc-admin/options-general/1/edit").await;
    for child in [
        "General",
        "Writing",
        "Reading",
        "Discussion",
        "Media",
        "Permalinks",
        "Privacy",
    ] {
        let slug = match child {
            "Permalinks" => "permalink".to_owned(),
            other => other.to_lowercase(),
        };
        // General is the current page.
        let current = if child == "General" {
            " aria-current=\"page\""
        } else {
            ""
        };
        let link = format!("href=\"/mc-admin/options-{slug}\"{current}>{child}</a>");
        assert!(general.contains(&link), "{link}: {general}");
    }
    // The list address opens the one record's form.
    {
        let response = client.get("/mc-admin/options-general").dispatch().await;
        assert_eq!(response.status(), Status::SeeOther);
        assert_eq!(location(&response), "/mc-admin/options-general/1/edit");
    }
    let (status, html) = get(&client, "/mc-admin/options-general/1/edit").await;
    assert_eq!(status, Status::Ok);
    assert!(html.contains("Settings Test"), "{html}");
    assert!(html.contains("America/Toronto"), "{html}");

    settings_screens(&client, &db, admin_id).await;
    permalinks(&client, &db, admin_id).await;
    menus(&client, &db, admin_id).await;

    // A settings screen has no New, create or Delete.
    let (status, body) = get(&client, "/mc-admin/options-general/new").await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to access this page.",
    );
    let csrf = dashboard_token(&client).await;
    for uri in [
        "/mc-admin/options-reading",
        "/mc-admin/options-reading/1/delete",
    ] {
        let response = post(&client, uri, &[("_token", &csrf)]).await;
        let status = response.status();
        let body = response.into_string().await.unwrap_or_default();
        assert_refused(
            status,
            &body,
            "Sorry, you are not allowed to access this page.",
        );
    }
    // Another record is not there.
    assert_eq!(
        get(&client, "/mc-admin/options-general/2/edit").await.0,
        Status::NotFound
    );
    sign_out(&client).await;

    // An editor is refused with WordPress's messages, and sees neither screen in the menu.
    sign_in(&client, &editor).await;
    let (_, dashboard) = get(&client, "/mc-admin").await;
    assert!(
        !dashboard.contains("/mc-admin/options-general"),
        "{dashboard}"
    );
    // The Settings entry has no screen the editor may see, so it is not listed at all.
    assert!(!dashboard.contains("</i>Settings</a>"), "{dashboard}");
    assert!(!dashboard.contains("/mc-admin/nav-menus"), "{dashboard}");
    for uri in [
        "/mc-admin/options-general",
        "/mc-admin/options-general/1/edit",
        "/mc-admin/options-permalink/1/edit",
    ] {
        let (status, body) = get(&client, uri).await;
        assert_refused(
            status,
            &body,
            "Sorry, you are not allowed to manage options for this site.",
        );
    }
    let (status, body) = get(&client, "/mc-admin/options-privacy/1/edit").await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to manage privacy options on this site.",
    );
    let csrf = dashboard_token(&client).await;
    let response = post(
        &client,
        "/mc-admin/options-general/1",
        &[("_token", &csrf), ("blogname", "Hijacked")],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);
    assert_eq!(
        option(&db, "blogname").await,
        Some(json!("Jenny&#039;s Blog"))
    );
    for uri in ["/mc-admin/nav-menus", "/mc-admin/nav-menus/new"] {
        let (status, body) = get(&client, uri).await;
        assert_refused(
            status,
            &body,
            "Sorry, you are not allowed to edit theme options on this site.",
        );
    }
    let response = post(
        &client,
        "/mc-admin/nav-menus",
        &[("_token", &csrf), ("name", "Editor menu")],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);
    sign_out(&client).await;
    db.close().await;
}

/// Each screen saves to `options`, through the edit form.
async fn settings_screens(client: &Client, db: &Database, author: i64) {
    save_screen(
        client,
        "options-general",
        &[
            ("blogname", "Renamed & Co"),
            ("blogdescription", "A tagline"),
            ("siteurl", HOME),
            ("home", HOME),
            ("admin_email", "admin@example.com"),
            ("timezone_string", "UTC-3"),
            ("date_format", "Y-m-d"),
            ("date_format_custom", "Y-m-d"),
            ("time_format", "custom"),
            ("time_format_custom", "H\\hi"),
            ("start_of_week", "0"),
        ],
    )
    .await;
    assert_eq!(
        option(db, "blogname").await,
        Some(json!("Renamed &amp; Co"))
    );
    assert_eq!(
        option(db, "blogdescription").await,
        Some(json!("A tagline"))
    );
    assert_eq!(
        option(db, "admin_email").await,
        Some(json!("admin@example.com"))
    );
    assert_eq!(option(db, "gmt_offset").await, Some(json!("-3")));
    assert_eq!(option(db, "timezone_string").await, Some(json!("")));
    assert_eq!(option(db, "date_format").await, Some(json!("Y-m-d")));
    assert_eq!(option(db, "time_format").await, Some(json!("H\\hi")));
    assert_eq!(option(db, "start_of_week").await, Some(json!(0)));
    // The name is stored escaped (`esc_html`), and shown escaped once on the site and the form,
    // as WordPress's `esc_html` and `esc_attr` do not encode an entity twice.
    let (_, home) = get(client, "/").await;
    assert!(home.contains("A tagline"), "{home}");
    assert!(home.contains(">Renamed &amp; Co</a>"), "{home}");
    assert!(!home.contains("&amp;amp;"), "{home}");
    let (_, form_html) = get(client, "/mc-admin/options-general/1/edit").await;
    assert!(
        form_html.contains("value=\"Renamed &amp; Co\""),
        "{form_html}"
    );
    assert!(!form_html.contains("&amp;amp;"), "{form_html}");

    // Ignite's checks show on the form (422), and nothing is saved.
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/options-general/1",
        &[
            ("_token", &csrf),
            ("blogname", "Not saved"),
            ("siteurl", HOME),
            ("home", HOME),
            ("admin_email", "not an email"),
            ("timezone_string", "UTC-3"),
            ("date_format", "Y-m-d"),
            ("time_format", "H:i"),
            ("start_of_week", "0"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap();
    assert!(body.contains("must be a valid email address"), "{body}");
    assert_eq!(
        option(db, "blogname").await,
        Some(json!("Renamed &amp; Co"))
    );

    // An address Ignite takes but WordPress's `is_email` refuses (no dot in the domain): the
    // form comes back (422) with WordPress's message under the field and the typed values, and
    // nothing is saved (Ruling R135).
    let response = post(
        client,
        "/mc-admin/options-general/1",
        &[
            ("_token", &csrf),
            ("blogname", "Not saved either"),
            ("siteurl", HOME),
            ("home", HOME),
            ("admin_email", "x@y"),
            ("timezone_string", "UTC-3"),
            ("date_format", "Y-m-d"),
            ("time_format", "H:i"),
            ("start_of_week", "0"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap();
    assert!(
        body.contains(
            "The email address entered did not appear to be a valid email address. Please enter \
             a valid email address."
        ),
        "{body}"
    );
    assert!(!body.contains("is already taken."), "{body}");
    assert!(body.contains("value=\"Not saved either\""), "{body}");
    assert!(body.contains("value=\"x@y\""), "{body}");
    assert_eq!(
        option(db, "blogname").await,
        Some(json!("Renamed &amp; Co"))
    );
    assert_eq!(
        option(db, "admin_email").await,
        Some(json!("admin@example.com"))
    );

    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .expect("creates a category");
    // The Writing screen always shows a category that exists: with the option naming none, it
    // shows the first category, as WordPress's dropdown does, and offers no other value.
    set_option(db, "default_category", &json!(987_654))
        .await
        .unwrap();
    let (status, writing) = get(client, "/mc-admin/options-writing/1/edit").await;
    assert_eq!(status, Status::Ok);
    assert!(
        writing.contains(&format!(
            "<option value=\"{}\" selected>News</option>",
            news.term_id
        )),
        "{writing}"
    );
    assert!(!writing.contains("987654"), "{writing}");
    save_screen(
        client,
        "options-writing",
        &[("default_category", &news.term_id.to_string())],
    )
    .await;
    assert_eq!(
        option(db, "default_category").await,
        Some(json!(news.term_id))
    );

    let page = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Welcome".to_owned(),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .expect("creates the front page");
    save_screen(
        client,
        "options-reading",
        &[
            ("show_on_front", "page"),
            ("page_on_front", &page.id.to_string()),
            ("page_for_posts", ""),
            ("posts_per_page", "5"),
            ("posts_per_rss", "15"),
            ("rss_use_excerpt", "1"),
            ("blog_public", "on"),
        ],
    )
    .await;
    assert_eq!(option(db, "show_on_front").await, Some(json!("page")));
    assert_eq!(option(db, "page_on_front").await, Some(json!(page.id)));
    assert_eq!(option(db, "posts_per_page").await, Some(json!(5)));
    assert_eq!(option(db, "posts_per_rss").await, Some(json!(15)));
    assert_eq!(option(db, "rss_use_excerpt").await, Some(json!(1)));
    assert_eq!(option(db, "blog_public").await, Some(json!(0)));
    // The front page is the static page now.
    let (status, home) = get(client, "/").await;
    assert_eq!(status, Status::Ok);
    assert!(home.contains("Welcome"), "{home}");

    save_screen(
        client,
        "options-discussion",
        &[
            ("default_comment_status", "on"),
            ("close_comments_days_old", "21"),
            ("thread_comments_depth", "3"),
            ("comments_per_page", "25"),
            ("default_comments_page", "newest"),
            ("comment_order", "asc"),
            ("comment_max_links", "1"),
            ("moderation_keys", "spam"),
            ("disallowed_keys", ""),
            ("avatar_rating", "G"),
            ("avatar_default", "retro"),
        ],
    )
    .await;
    assert_eq!(
        option(db, "default_comment_status").await,
        Some(json!("open"))
    );
    assert_eq!(
        option(db, "default_ping_status").await,
        Some(json!("closed"))
    );
    assert_eq!(option(db, "thread_comments_depth").await, Some(json!(3)));
    assert_eq!(option(db, "avatar_default").await, Some(json!("retro")));
    assert_eq!(option(db, "thread_comments").await, Some(json!(0)));

    save_screen(
        client,
        "options-media",
        &[
            ("thumbnail_size_w", "100"),
            ("thumbnail_size_h", "100"),
            ("thumbnail_crop", "on"),
            ("medium_size_w", "320"),
            ("medium_size_h", "320"),
            ("large_size_w", "1200"),
            ("large_size_h", "1200"),
        ],
    )
    .await;
    assert_eq!(option(db, "thumbnail_size_w").await, Some(json!(100)));
    assert_eq!(option(db, "medium_size_w").await, Some(json!(320)));
    assert_eq!(option(db, "thumbnail_crop").await, Some(json!(1)));

    save_screen(
        client,
        "options-privacy",
        &[("wp_page_for_privacy_policy", &page.id.to_string())],
    )
    .await;
    assert_eq!(
        option(db, "wp_page_for_privacy_policy").await,
        Some(json!(page.id))
    );
    // Back to the latest posts on the front page, for the menu below.
    save_screen(
        client,
        "options-reading",
        &[
            ("show_on_front", "posts"),
            ("page_on_front", ""),
            ("page_for_posts", ""),
            ("posts_per_page", "10"),
            ("posts_per_rss", "10"),
            ("rss_use_excerpt", "0"),
        ],
    )
    .await;
    assert_eq!(option(db, "blog_public").await, Some(json!(1)));
}

/// Permalinks: post name changes the address a published post answers on; the plain address
/// redirects to it, as `redirect_canonical` does.
async fn permalinks(client: &Client, db: &Database, author: i64) {
    let hello = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Hello Permalinks".to_owned(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("publishes a post");
    let plain = format!("/?p={}", hello.id);
    let pretty = format!("/{}/", hello.slug);
    assert_eq!(get(client, &plain).await.0, Status::Ok);
    assert_eq!(get(client, &pretty).await.0, Status::NotFound);

    save_screen(
        client,
        "options-permalink",
        &[
            ("permalink_selection", "postname"),
            ("permalink_structure", ""),
            ("category_base", "topics"),
            ("tag_base", ""),
        ],
    )
    .await;
    assert_eq!(
        option(db, "permalink_structure").await,
        Some(json!("/%postname%/"))
    );
    assert_eq!(option(db, "category_base").await, Some(json!("/topics")));
    let (status, html) = get(client, &pretty).await;
    assert_eq!(status, Status::Ok, "{html}");
    assert!(html.contains("Hello Permalinks"), "{html}");
    let response = client.get(plain.clone()).dispatch().await;
    assert_eq!(response.status(), Status::MovedPermanently);
    assert!(
        location(&response).ends_with(&pretty),
        "{}",
        location(&response)
    );
    // The site links to the new address.
    let (_, home) = get(client, "/").await;
    assert!(home.contains(&format!("{HOME}{pretty}")), "{home}");

    // The category base: a category's archive answers under it.
    let news = Term::find_by_slug(db, &Taxonomy::CATEGORY, "news")
        .await
        .unwrap()
        .expect("the category of the Writing screen");
    hello
        .sync_terms(db, &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    let (status, archive) = get(client, "/topics/news/").await;
    assert_eq!(status, Status::Ok, "{archive}");
    assert!(archive.contains("Hello Permalinks"), "{archive}");
    assert_eq!(get(client, "/category/news/").await.0, Status::NotFound);

    // Ruling R135: a custom structure without a tag is refused, as WordPress's `sanitize_option`
    // refuses it. Nothing of the form is saved (not the category base either), and the form
    // comes back (422) with WordPress's message under the structure and the typed values.
    let csrf = token(client).await;
    for structure in ["/archives/", "/blog/static"] {
        let response = post(
            client,
            "/mc-admin/options-permalink/1",
            &[
                ("_token", csrf.as_str()),
                ("permalink_selection", "custom"),
                ("permalink_structure", structure),
                ("category_base", "sections"),
                ("tag_base", ""),
            ],
        )
        .await;
        assert_eq!(
            response.status(),
            Status::UnprocessableEntity,
            "{structure}"
        );
        let body = response.into_string().await.unwrap_or_default();
        assert!(!body.contains("Changes saved."), "{body}");
        assert!(
            body.contains("A structure tag is required when using custom permalinks."),
            "{body}"
        );
        assert!(!body.contains("is already taken."), "{body}");
        assert!(body.contains(&format!("value=\"{structure}\"")), "{body}");
        assert!(body.contains("value=\"sections\""), "{body}");
    }
    assert_eq!(
        option(db, "permalink_structure").await,
        Some(json!("/%postname%/"))
    );
    assert_eq!(option(db, "category_base").await, Some(json!("/topics")));
    assert_eq!(get(client, &pretty).await.0, Status::Ok);
    let (_, form_html) = get(client, "/mc-admin/options-permalink/1/edit").await;
    assert!(!form_html.contains("Changes saved."), "{form_html}");

    // "Jenny's Blog", once escaped on the site.
    save_screen(
        client,
        "options-general",
        &[
            ("blogname", "Jenny's Blog"),
            ("blogdescription", "A tagline"),
            ("siteurl", HOME),
            ("home", HOME),
            ("admin_email", "admin@example.com"),
            ("timezone_string", "UTC-3"),
            ("date_format", "Y-m-d"),
            ("time_format", "H:i"),
            ("start_of_week", "0"),
        ],
    )
    .await;
    assert_eq!(
        option(db, "blogname").await,
        Some(json!("Jenny&#039;s Blog"))
    );
    let (_, home) = get(client, "/").await;
    assert!(!home.contains("&amp;#"), "{home}");
    assert!(home.contains(">Jenny&#x27;s Blog</a>"), "{home}");
}

/// Menus: a menu with items assigned to the default theme's `primary` location shows in its
/// header; renaming, and Delete.
async fn menus(client: &Client, db: &Database, author: i64) {
    let about = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "About Us".to_owned(),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .expect("publishes a page");
    let (status, html) = get(client, "/mc-admin/nav-menus/new").await;
    assert_eq!(status, Status::Ok);
    assert!(html.contains("loc_primary"), "{html}");
    assert!(html.contains("Menu Name"), "{html}");
    // WordPress's menu builder joins the form; the items field stays for the form without it.
    assert!(html.contains("id=\"mc-menu-builder\""), "{html}");
    assert!(html.contains("name=\"items\""), "{html}");
    let items = json!([
        {"title": "", "type": "page", "object_id": about.id},
        {"title": "Docs", "type": "custom", "url": "https://rocketboost.rs/docs/", "new_tab": true},
        {"title": "Guide", "type": "custom", "url": "/guide/", "parent": 2},
    ])
    .to_string();
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/nav-menus",
        &[
            ("_token", &csrf),
            ("name", "Main menu"),
            ("loc_primary", "on"),
            ("items", &items),
        ],
    )
    .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "{body}");
    let (status, list) = get(client, "/mc-admin/nav-menus").await;
    assert_eq!(status, Status::Ok);
    assert!(list.contains("Main menu"), "{list}");
    assert!(list.contains("Primary menu"), "{list}");

    // The default theme's header shows it, nested.
    let (status, home) = get(client, "/").await;
    assert_eq!(status, Status::Ok);
    assert!(home.contains("id=\"menu-main-menu\""), "{home}");
    assert!(home.contains("About Us"), "{home}");
    assert!(
        home.contains(
            "href=\"https://rocketboost.rs/docs/\" target=\"_blank\" rel=\"noopener\">Docs</a>"
        ),
        "{home}"
    );
    assert!(home.contains("class=\"sub-menu\""), "{home}");
    assert!(home.contains(">Guide</a>"), "{home}");

    // A second menu cannot take the name: the form says so.
    let response = post(
        client,
        "/mc-admin/nav-menus",
        &[("_token", &csrf), ("name", "main menu")],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap();
    assert!(body.contains("Menu Name is already taken."), "{body}");

    // A blank name is refused with `nav-menus.php`'s message, under the name.
    let response = post(
        client,
        "/mc-admin/nav-menus",
        &[("_token", &csrf), ("name", ""), ("items", "[]")],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap();
    assert!(body.contains("Please enter a valid menu name."), "{body}");
    assert!(body.contains("id=\"mc-menu-builder\""), "{body}");

    // Rename and unassign, through the edit form.
    let id = list
        .split("/mc-admin/nav-menus/")
        .filter_map(|rest| rest.split('/').next()?.parse::<i64>().ok())
        .next()
        .expect("the menu's edit link");
    let (status, form_html) = get(client, &format!("/mc-admin/nav-menus/{id}/edit")).await;
    assert_eq!(status, Status::Ok);
    assert!(form_html.contains("Guide"), "{form_html}");
    // The linked page goes to the trash: WordPress keeps the item ("Invalid") and the menu still
    // saves when its rows come back unchanged, renamed and moved off `primary`.
    let mut about = about;
    about.trash(db).await.expect("trashes the page");
    let menu = get_nav_menu(db, id).await.unwrap().expect("the menu");
    let stored = menu_items(db, &menu).await.unwrap();
    assert_eq!(stored.len(), 3);
    let rows = json!([
        {"id": stored[0].id, "title": "", "type": "page", "object_id": about.id},
        {"id": stored[1].id, "title": "Docs", "type": "custom", "url": "https://rocketboost.rs/docs/", "new_tab": true},
        {"id": stored[2].id, "title": "Guide", "type": "custom", "url": "/guide/", "parent": 2},
    ])
    .to_string();
    let response = post(
        client,
        &format!("/mc-admin/nav-menus/{id}"),
        &[("_token", &csrf), ("name", "Header"), ("items", &rows)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    let menu = get_nav_menu(db, id).await.unwrap().expect("the menu");
    assert_eq!(menu.name, "Header");
    let kept = menu_items(db, &menu).await.unwrap();
    assert_eq!(kept.len(), 3);
    assert_eq!(kept[0].id, stored[0].id);
    assert_eq!(kept[0].object, stored[0].object);
    let (_, home) = get(client, "/").await;
    assert!(!home.contains("menu-main-menu"), "{home}");
    assert!(!home.contains(">Guide</a>"), "{home}");
    assert!(!home.contains("About Us"), "{home}");

    wordpress_menu_items(client, db, author, &csrf).await;

    // Delete removes the menu and its items.
    let response = post(
        client,
        &format!("/mc-admin/nav-menus/{id}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(
        get(client, &format!("/mc-admin/nav-menus/{id}/edit"))
            .await
            .0,
        Status::NotFound
    );
}

/// The `nav_menu` term of the menu named `slug`.
async fn menu_by_slug(db: &Database, slug: &str) -> Term {
    let term = Term::find_by_slug(db, &Taxonomy::NAV_MENU, slug)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("the menu {slug}"));
    get_nav_menu(db, term.term_id)
        .await
        .unwrap()
        .expect("a menu")
}

/// Item rows in WordPress's format save: `post_type` with the post type in `object`, or read
/// from the object id; a custom link with its URL; the parent as the parent item's id. A row
/// that links to nothing comes back on the form (422) with the reason under the items.
async fn wordpress_menu_items(client: &Client, db: &Database, author: i64, csrf: &str) {
    let contact = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Contact".to_owned(),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .expect("publishes a page");
    let rows = json!([
        {"id": "", "title": "", "type": "post_type", "object": "page", "object_id": contact.id,
         "url": "", "parent": 0, "classes": "", "new_tab": false},
        {"id": "", "title": "Example", "type": "custom", "object": "custom", "object_id": 0,
         "url": "https://example.com/", "parent": "", "classes": "", "new_tab": false},
    ])
    .to_string();
    let response = post(
        client,
        "/mc-admin/nav-menus",
        &[("_token", csrf), ("name", "Footer"), ("items", &rows)],
    )
    .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "{body}");
    let menu = menu_by_slug(db, "footer").await;
    let stored = menu_items(db, &menu).await.unwrap();
    assert_eq!(stored.len(), 2);
    let (contact_item, custom_item) = (stored[0].id, stored[1].id);

    // Edit: the custom link and a new item (`post_type` with no `object`) under the page's item,
    // named by its id.
    let rows = json!([
        {"id": contact_item, "title": "", "type": "post_type", "object": "page",
         "object_id": contact.id, "parent": 0},
        {"id": custom_item, "title": "Example", "type": "custom", "url": "https://example.com/",
         "parent": contact_item},
        {"title": "Contact again", "type": "post_type", "object_id": contact.id,
         "parent": contact_item},
    ])
    .to_string();
    let response = post(
        client,
        &format!("/mc-admin/nav-menus/{}", menu.term_id),
        &[("_token", csrf), ("name", "Footer"), ("items", &rows)],
    )
    .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "{body}");
    let saved = menu_items(db, &menu).await.unwrap();
    assert_eq!(saved.len(), 3);
    assert_eq!(saved[0].id, contact_item);
    assert_eq!(saved[1].id, custom_item, "the item keeps its id");
    assert_eq!(
        saved[1].parent,
        Some(contact_item),
        "the child keeps its parent"
    );
    assert_eq!(saved[2].parent, Some(contact_item));
    assert_eq!(
        saved[2].object, saved[0].object,
        "the post type is read from the id"
    );
    // The form shows each parent as its item's id, so the rows save back unchanged.
    let (_, form_html) = get(
        client,
        &format!("/mc-admin/nav-menus/{}/edit", menu.term_id),
    )
    .await;
    assert!(
        form_html.contains(&format!("&quot;parent&quot;:{contact_item}"))
            || form_html.contains(&format!("\"parent\":{contact_item}")),
        "{form_html}"
    );

    // A row that links to nothing (no such post) refuses the save, on the form.
    let rows = json!([
        {"id": contact_item, "title": "", "type": "post_type", "object_id": contact.id},
        {"title": "Gone", "type": "post_type", "object_id": 987_654},
    ])
    .to_string();
    let response = post(
        client,
        &format!("/mc-admin/nav-menus/{}", menu.term_id),
        &[("_token", csrf), ("name", "Footer"), ("items", &rows)],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap_or_default();
    assert!(body.contains("Item row 2 links to nothing."), "{body}");
    assert!(!body.contains("is already taken."), "{body}");
    assert_eq!(
        menu_items(db, &menu).await.unwrap().len(),
        3,
        "nothing is saved"
    );
    let response = post(
        client,
        &format!("/mc-admin/nav-menus/{}/delete", menu.term_id),
        &[("_token", csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
}

#[test]
fn on_sqlite_the_settings_and_menus_answer_that_the_admin_needs_mysql() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    rocket::async_test(on_sqlite());
}

async fn on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("settings.db").display()
    );
    let db = Arc::new(Database::connect(&url).await.unwrap());
    create_users_table(&db).await;
    migrator().unwrap().run(&db).await.unwrap();
    // The Media settings' sizes are read at start, on every database.
    set_option(&db, "thumbnail_size_w", &json!(77))
        .await
        .unwrap();
    set_option(&db, "large_size_h", &json!("900"))
        .await
        .unwrap();
    let client = Client::tracked(rocket_with_options(
        config(&url),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    for uri in [
        "/mc-admin/options-general",
        "/mc-admin/options-permalink/1/edit",
        "/mc-admin/nav-menus",
    ] {
        let (status, body) = get(&client, uri).await;
        assert_eq!(status, Status::ServiceUnavailable, "{uri}");
        assert!(body.contains("MySQL or MariaDB"), "{uri}: {body}");
    }
    let sizes = client
        .rocket()
        .state::<Arc<ImageSizes>>()
        .expect("the size registry");
    assert_eq!(sizes.get("thumbnail").unwrap().width, 77);
    assert_eq!(sizes.get("thumbnail").unwrap().height, 150);
    assert_eq!(sizes.get("large").unwrap().height, 900);
    db.close().await;
}
