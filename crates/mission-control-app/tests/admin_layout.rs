//! The layout of the admin's forms: the post and page editors in two columns (a main column two
//! thirds wide and a sidebar one third wide, as WordPress's editor with its settings sidebar),
//! the settings screens and the Menus screen in two equal columns, Ignite's unsaved changes
//! alert on each of them, the slug that follows the title, and the category checklist's and the
//! tags box's new terms, created by the save.
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
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{NewTerm, Post, Taxonomy, Term};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::{Database, Model};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};

/// Held by every test here: the flow sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
const PASSWORD: &str = "correct horse battery staple";

/// The class of the form's grid of three columns, and of its main column and sidebar
/// (Ignite's `FormLayout::columns(3)` and `column_span`).
const GRID_3: &str = "lg:grid-cols-3";
const MAIN: &str = "lg:col-span-2";
const SIDE: &str = "lg:col-span-1";

/// The class of a grid of two equal columns.
const GRID_2: &str = "lg:grid-cols-2";

/// The attribute that turns on Ignite's unsaved changes alert on a form.
const ALERT: &str = "data-ignite-unsaved-changes-alert";

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

async fn sign_in(client: &Client, email: &str) {
    let (_, html) = get(client, "/mc-admin/login").await;
    let csrf = token_in(&html);
    let response = post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
}

/// The form of a page: from its `<form` to its `</form>`.
fn form_of(html: &str) -> &str {
    let start = html
        .find("<form class=\"grid gap-5\"")
        .unwrap_or_else(|| panic!("a form with a layout in: {html}"));
    let end = html[start..].find("</form>").unwrap() + start;
    &html[start..end]
}

/// The main column and the sidebar of a post editor's form: the markup from the main column's
/// class to the sidebar's, and from the sidebar's on.
fn editor_columns(form: &str) -> (&str, &str) {
    let main = form
        .find(MAIN)
        .unwrap_or_else(|| panic!("a main column in: {form}"));
    let side = form[main..]
        .find(SIDE)
        .unwrap_or_else(|| panic!("a sidebar in: {form}"))
        + main;
    (&form[main..side], &form[side..])
}

/// The form script the panel loads when a form asks before it is left.
fn has_form_script(html: &str) -> bool {
    html.contains("form.js") || html.contains("/form-")
}

#[test]
fn the_editor_settings_and_menus_are_laid_out_in_columns() {
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
        .block_on(scratch_database("layout"));
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
    let client = Client::tracked(rocket_with_options(
        config(scratch.url()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    let email = "ada@example.test";
    users::create_user(&db, "ada", email, PASSWORD)
        .await
        .expect("creates the user");
    authz::assign_role(&db, email, "administrator")
        .await
        .expect("gives the role");
    sign_in(&client, email).await;

    editors(&client).await;
    settings(&client).await;
    menus(&client).await;
    new_terms(&client, &db).await;
}

/// The post and page editors: a main column and a sidebar, the slug from the title, and the
/// unsaved changes alert.
async fn editors(client: &Client) {
    for uri in ["/mc-admin/post/new", "/mc-admin/page/new"] {
        let (status, html) = get(client, uri).await;
        assert_eq!(status, Status::Ok, "{uri}: {html}");
        let form = form_of(&html);
        assert!(form.contains(GRID_3), "{uri}: {form}");
        let (main, side) = editor_columns(form);
        for name in ["title", "slug", "content"] {
            assert!(
                main.contains(&format!("name=\"{name}\"")),
                "{uri}: {name} in the main column: {main}"
            );
        }
        for name in ["author", "featured_media", "status", "date", "password"] {
            assert!(
                side.contains(&format!("name=\"{name}\"")),
                "{uri}: {name} in the sidebar: {side}"
            );
            assert!(
                !main.contains(&format!("name=\"{name}\"")),
                "{uri}: {name} not in the main column"
            );
        }
        // The slug follows the title (Ignite's `slug_from`).
        let slug = form
            .find(r#"id="slug" name="slug""#)
            .unwrap_or_else(|| panic!("{uri}: a slug input in {form}"));
        let tag = &form[slug..slug + form[slug..].find('>').unwrap()];
        assert!(
            tag.contains(r#"data-ignite-slug-from="title""#),
            "{uri}: {tag}"
        );
        // Ignite's unsaved changes alert, and the script that asks.
        assert!(form.contains(ALERT), "{uri}: {form}");
        assert!(has_form_script(&html), "{uri}: {html}");
    }
    // Posts: the excerpt in the main column, Categories and Tags in the sidebar.
    let (_, html) = get(client, "/mc-admin/post/new").await;
    let (main, side) = editor_columns(form_of(&html));
    assert!(main.contains("name=\"excerpt\""), "{main}");
    for name in [
        "tax_category[]",
        "new_category",
        "tax_post_tag[]",
        "tax_input_post_tag",
    ] {
        assert!(side.contains(&format!("name=\"{name}\"")), "{name}: {side}");
    }
}

/// The settings screens: two equal columns, and the unsaved changes alert.
async fn settings(client: &Client) {
    for (slug, columns) in [
        ("options-general", 2),
        ("options-writing", 1),
        ("options-reading", 2),
        ("options-discussion", 2),
        ("options-media", 2),
        ("options-permalink", 2),
        ("options-privacy", 1),
    ] {
        let uri = format!("/mc-admin/{slug}/1/edit");
        let (status, html) = get(client, &uri).await;
        assert_eq!(status, Status::Ok, "{uri}: {html}");
        let form = form_of(&html);
        assert!(form.contains(GRID_2), "{uri}: {form}");
        assert_eq!(form.matches(SIDE).count(), columns, "{uri}: {form}");
        assert!(!form.contains(MAIN), "{uri}: {form}");
        assert!(form.contains(ALERT), "{uri}: {form}");
        assert!(has_form_script(&html), "{uri}: {html}");
    }
}

/// The Menus screen: two equal columns, and the unsaved changes alert.
async fn menus(client: &Client) {
    let (status, html) = get(client, "/mc-admin/nav-menus/new").await;
    assert_eq!(status, Status::Ok, "{html}");
    let form = form_of(&html);
    assert!(form.contains(GRID_2), "{form}");
    assert_eq!(form.matches(SIDE).count(), 2, "{form}");
    let side = &form[form.rfind(SIDE).unwrap()..];
    assert!(side.contains("Menu Structure"), "{side}");
    assert!(form.contains(ALERT), "{form}");
    assert!(has_form_script(&html), "{html}");
}

/// The category checklist's tree and the new terms the editor's boxes send.
async fn new_terms(client: &Client, db: &Database) {
    let fruit = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("Fruit"))
        .await
        .expect("creates a category");
    let apples = Term::create(
        db,
        &Taxonomy::CATEGORY,
        NewTerm {
            parent_id: Some(fruit.term_id),
            ..NewTerm::new("Apples")
        },
    )
    .await
    .expect("creates a child category");
    let rust = Term::create(db, &Taxonomy::POST_TAG, NewTerm::new("Rust"))
        .await
        .expect("creates a tag");

    let (_, html) = get(client, "/mc-admin/post/new").await;
    // A child category follows its parent, indented by three no-break spaces.
    let fruit_at = html.find(">Fruit</option>").expect("Fruit is a choice");
    let apples_at = html
        .find(">\u{a0}\u{a0}\u{a0}Apples</option>")
        .or_else(|| html.find(">&nbsp;&nbsp;&nbsp;Apples</option>"))
        .unwrap_or_else(|| panic!("Apples is an indented choice: {html}"));
    assert!(fruit_at < apples_at);

    let csrf = token_in(&html);
    let apples_id = apples.term_id.to_string();
    let rust_id = rust.term_id.to_string();
    let response = post(
        client,
        "/mc-admin/post",
        &[
            ("_token", &csrf),
            ("title", "Terms from the sidebar"),
            ("tax_category[]", ""),
            ("tax_category[]", &apples_id),
            ("new_category", "Vegetables"),
            ("tax_post_tag[]", ""),
            ("tax_post_tag[]", &rust_id),
            ("tax_input_post_tag", "web, RUST, Async"),
        ],
    )
    .await;
    let to = location(&response);
    assert_eq!(response.status(), Status::SeeOther, "creates the post");
    let id: i64 = to
        .strip_prefix("/mc-admin/post/")
        .and_then(|rest| rest.strip_suffix("/edit"))
        .unwrap_or_else(|| panic!("redirects to the edit form: {to}"))
        .parse()
        .unwrap();
    let post = Post::query().find(db, id).await.unwrap().expect("saved");
    let mut categories: Vec<String> = post
        .terms(db, &Taxonomy::CATEGORY)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    categories.sort();
    assert_eq!(categories, ["Apples", "Vegetables"]);
    let mut tags: Vec<String> = post
        .terms(db, &Taxonomy::POST_TAG)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    tags.sort();
    // "RUST" is the existing Rust, compared without case.
    assert_eq!(tags, ["Async", "Rust", "web"]);
    // The new category is a choice on the next form.
    let (_, html) = get(client, &to).await;
    assert!(html.contains(">Vegetables</option>"), "{html}");
}
