//! The admin dashboard, as WordPress's (`wp-admin/includes/dashboard.php`): the "At a Glance"
//! counts (published posts and pages, post drafts, categories, tags, media and users), the
//! "Activity" box's five most recently published posts with their dates, and, in the WordPress
//! admin theme, the "Quick Draft" box, whose form saves a draft through the Posts screen's
//! create.
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so the flow runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of its own
//! ([`scratch_database`]) with the installer's migrations, dropped at the end; otherwise it says
//! why it is skipped and passes. Ignite reads its database from `DATABASE_URL`, so the flow sets
//! it for the process; every test here holds [`LOCK`] so none runs while it changes.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::admin::dashboard::{RECENTLY_PUBLISHED, recent_date};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{Post, PostStatus};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::{Database, Direction, Model, Value as Sql, raw};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};

/// Held by every test here: the flow sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
const PASSWORD: &str = "correct horse battery staple";

fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
}

fn config(database_url: &str) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes"),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment: Environment::Local,
        trusted_proxies: TrustedProxies::None,
    }
}

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

async fn token(client: &Client) -> String {
    let (mut status, mut html) = get(client, "/mc-admin/login").await;
    if status == Status::SeeOther {
        (status, html) = get(client, "/mc-admin").await;
    }
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
    let csrf = token_in(&get(client, "/mc-admin").await.1);
    let response = post(client, "/mc-admin/logout", &[("_token", &csrf)]).await;
    assert_eq!(response.status(), Status::SeeOther);
}

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

/// Chooses the admin theme `theme` on the profile page ("" for the site's default).
async fn choose_theme(client: &Client, theme: &str) {
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/profile/admin-theme",
        &[("_token", &csrf), ("admin_theme", theme)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "chooses {theme}");
}

/// Saves the New screen of `post_type` with `fields`; returns the new post's id.
async fn new_post(client: &Client, post_type: &str, fields: &[(&str, &str)]) -> i64 {
    let csrf = token(client).await;
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = post(client, &format!("/mc-admin/{post_type}"), &all).await;
    assert_eq!(response.status(), Status::SeeOther, "creates");
    location(&response)
        .strip_prefix(&format!("/mc-admin/{post_type}/"))
        .and_then(|rest| rest.strip_suffix("/edit"))
        .and_then(|id| id.parse().ok())
        .expect("redirects to the edit form")
}

/// The text between `open` (after `from`) and the next `</p>`.
fn text_after<'h>(html: &'h str, open: &str) -> Option<&'h str> {
    let start = html.find(open)? + open.len();
    let end = start + html[start..].find("</p>")?;
    Some(&html[start..end])
}

/// The dashboard's cards in the Ignite theme: (title, value, description).
fn ignite_cards(html: &str) -> Vec<(String, String, Option<String>)> {
    html.split("<div class=\"ig-card ig-tone-")
        .skip(1)
        .filter_map(|card| {
            let card = &card[..card.find("</div>").unwrap_or(card.len())];
            Some((
                text_after(card, "<p class=\"m-0 font-semibold\">")?.to_owned(),
                text_after(card, "<p class=\"m-0 text-2xl font-semibold\">")?.to_owned(),
                text_after(card, "<p class=\"m-0 text-sm\">").map(str::to_owned),
            ))
        })
        .collect()
}

/// The dashboard's cards in the WordPress theme's boxes: (title, value, description).
fn wordpress_cards(html: &str) -> Vec<(String, String, Option<String>)> {
    html.split("<div class=\"postbox\">\n<h2>")
        .skip(1)
        .filter_map(|card| {
            let card = &card[..card.find("\n</div>").unwrap_or(card.len())];
            let title = &card[..card.find("</h2>")?];
            let value = text_after(card, "<p class=\"mc-count\">")?;
            let rest = &card[card.find("<p class=\"mc-count\">")?..];
            let description = text_after(&rest[rest.find("</p>")?..], "<p>");
            Some((
                title.to_owned(),
                value.to_owned(),
                description.map(str::to_owned),
            ))
        })
        .collect()
}

async fn count(db: &Database, sql: &str, binds: &[Sql]) -> String {
    let n: i64 = raw::fetch_scalar(db, sql, binds).await.unwrap();
    n.to_string()
}

/// The At a Glance cards the database should give, in order.
async fn at_a_glance(db: &Database) -> Vec<(String, String, Option<String>)> {
    let posts = "SELECT COUNT(*) FROM posts WHERE post_type = ? AND status = ?";
    let terms = "SELECT COUNT(*) FROM term_taxonomy WHERE taxonomy = ?";
    vec![
        (
            "Posts",
            count(db, posts, &["post".into(), "publish".into()]).await,
        ),
        (
            "Pages",
            count(db, posts, &["page".into(), "publish".into()]).await,
        ),
        (
            "Drafts",
            count(db, posts, &["post".into(), "draft".into()]).await,
        ),
        ("Categories", count(db, terms, &["category".into()]).await),
        ("Tags", count(db, terms, &["post_tag".into()]).await),
        (
            "Media",
            count(
                db,
                "SELECT COUNT(*) FROM posts WHERE post_type = ? AND status <> ?",
                &["attachment".into(), "trash".into()],
            )
            .await,
        ),
        ("Users", count(db, "SELECT COUNT(*) FROM users", &[]).await),
    ]
    .into_iter()
    .map(|(label, value)| (label.to_owned(), value, None))
    .collect()
}

#[test]
fn the_dashboard_has_wordpress_boxes() {
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
        .block_on(scratch_database("dashboard"));
    // SAFETY: no other thread reads or writes the environment now. Every test of this process
    // holds `LOCK`; the runtime above is gone, and the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(flow(&scratch));
    // SAFETY: as above.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

async fn flow(scratch: &ScratchDatabase) {
    run_migrations(scratch.url()).await.expect("migrates");
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
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
    let (_, subscriber) = user(&db, "sue", "subscriber").await;

    sign_in(&client, &admin).await;
    for n in 1..=6 {
        new_post(
            &client,
            "post",
            &[("title", &format!("Story {n}")), ("status", "publish")],
        )
        .await;
    }
    new_post(&client, "post", &[("title", "Not yet")]).await;
    new_post(
        &client,
        "page",
        &[("title", "About"), ("status", "publish")],
    )
    .await;

    // The Ignite theme: At a Glance's counts, then Activity's five newest published posts.
    let (status, dashboard) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::Ok, "{dashboard}");
    let cards = ignite_cards(&dashboard);
    let glance = at_a_glance(&db).await;
    assert_eq!(cards[..glance.len()], glance[..], "{dashboard}");
    assert_eq!(glance[0].1, "6", "six published posts");
    let newest: Vec<Post> = Post::query()
        .where_eq("post_type", "post")
        .where_eq("status", PostStatus::Publish.as_str())
        .order_by("published_at", Direction::Desc)
        .order_by("id", Direction::Desc)
        .get(&*db)
        .await
        .unwrap()
        .into_iter()
        .take(5)
        .collect();
    let now = chrono::Utc::now().naive_utc();
    let activity: Vec<(String, String, Option<String>)> = newest
        .iter()
        .map(|post| {
            (
                RECENTLY_PUBLISHED.to_owned(),
                post.title.clone(),
                Some(recent_date(post.published_at.unwrap(), now)),
            )
        })
        .collect();
    assert_eq!(cards[glance.len()..], activity[..], "{dashboard}");
    assert_eq!(activity[0].1, "Story 6");
    assert!(activity[0].2.as_deref().unwrap().starts_with("Today, "));
    // The Ignite theme has no Quick Draft.
    assert!(!dashboard.contains("Quick Draft"), "{dashboard}");

    // The WordPress theme: the same cards as WordPress's boxes, and Quick Draft.
    choose_theme(&client, "wordpress").await;
    let (status, dashboard) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::Ok, "{dashboard}");
    assert!(dashboard.contains("<h1>Dashboard</h1>"), "{dashboard}");
    assert_eq!(wordpress_cards(&dashboard), cards, "{dashboard}");
    for expected in [
        "<h2>Quick Draft</h2>",
        "<form name=\"post\" action=\"/mc-admin/post\" method=\"post\" id=\"quick-press\"",
        "<label for=\"title\">Title</label>",
        "<input type=\"text\" name=\"title\" id=\"title\" autocomplete=\"off\">",
        "<label for=\"content\">Content</label>",
        "placeholder=\"What&#8217;s on your mind?\"",
        "value=\"Save Draft\"",
    ] {
        assert!(dashboard.contains(expected), "`{expected}` in {dashboard}");
    }
    // Quick Draft saves a draft by the user, through the Posts screen's save, and opens it.
    let csrf = token_in(&dashboard);
    let response = post(
        &client,
        "/mc-admin/post",
        &[
            ("_token", &csrf),
            ("title", "A quick idea"),
            ("content", "Written on the dashboard."),
            ("save", "Save Draft"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    let to = location(&response);
    drop(response);
    let id: i64 = to
        .strip_prefix("/mc-admin/post/")
        .and_then(|rest| rest.strip_suffix("/edit"))
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("opens the draft: {to}"));
    let draft = Post::query().find(&*db, id).await.unwrap().unwrap();
    assert_eq!(draft.status, PostStatus::Draft);
    assert_eq!(draft.title, "A quick idea");
    assert!(
        draft.content.contains("Written on the dashboard."),
        "{}",
        draft.content
    );
    assert_eq!(draft.author_id, admin_id);
    let (_, dashboard) = get(&client, "/mc-admin").await;
    let drafts = wordpress_cards(&dashboard)
        .into_iter()
        .find(|(label, _, _)| label == "Drafts")
        .unwrap();
    let before: i64 = glance[2].1.parse().unwrap();
    assert_eq!(drafts.1, (before + 1).to_string(), "{dashboard}");
    choose_theme(&client, "").await;
    sign_out(&client).await;

    // A subscriber sees the dashboard; Quick Draft's save is refused, as WordPress shows the
    // box only to a user who may create posts.
    sign_in(&client, &subscriber).await;
    choose_theme(&client, "wordpress").await;
    let (status, dashboard) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::Ok, "{dashboard}");
    let csrf = token_in(&dashboard);
    let response = post(
        &client,
        "/mc-admin/post",
        &[("_token", &csrf), ("title", "Not mine to write")],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);
    let body = response.into_string().await.unwrap_or_default();
    assert!(
        body.contains("Sorry, you are not allowed to create posts as this user."),
        "{body}"
    );
    sign_out(&client).await;
    drop(client);
    db.close().await;
}
