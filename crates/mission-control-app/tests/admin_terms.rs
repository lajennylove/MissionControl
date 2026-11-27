//! The admin's Categories and Tags screens (WordPress's Posts > Categories and Posts > Tags) at
//! `/mc-admin/category` and `/mc-admin/post_tag`: who may open them, adding a tag, the name
//! required, the slug unique within the taxonomy, the default category kept, and a deleted tag
//! leaving its posts.
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so the flow runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of its own
//! ([`scratch_database`]) with the installer's migrations, dropped at the end; otherwise it says
//! why it is skipped and passes. Ignite reads its database from `DATABASE_URL`, so the flow sets
//! it for the process; every test here holds [`LOCK`] so none runs while it changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, set_option,
};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::Database;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::json;

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

/// Posts `fields` with the session's CSRF token, read from the dashboard.
async fn post(client: &Client, uri: &str, fields: &[(&str, &str)]) -> (Status, String, String) {
    let (status, dashboard) = get(client, "/mc-admin").await;
    assert_eq!(status, Status::Ok, "{dashboard}");
    let csrf = token_in(&dashboard);
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(&all))
        .dispatch()
        .await;
    let status = response.status();
    let to = location(&response);
    (status, to, response.into_string().await.unwrap_or_default())
}

async fn sign_in(client: &Client, email: &str) {
    let (_, html) = get(client, "/mc-admin/login").await;
    let csrf = token_in(&html);
    let response = client
        .post("/mc-admin/login")
        .header(ContentType::Form)
        .body(form(&[
            ("_token", &csrf),
            ("email", email),
            ("password", PASSWORD),
        ]))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
}

async fn sign_out(client: &Client) {
    let csrf = token_in(&get(client, "/mc-admin").await.1);
    let response = client
        .post("/mc-admin/logout")
        .header(ContentType::Form)
        .body(form(&[("_token", &csrf)]))
        .dispatch()
        .await;
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

async fn tag_by_slug(db: &Database, slug: &str) -> Option<Term> {
    Term::find_by_slug(db, &Taxonomy::POST_TAG, slug)
        .await
        .expect("reads the tag")
}

#[test]
fn categories_and_tags_screens() {
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
        .block_on(scratch_database("terms"));
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
    for (name, value) in [
        ("blogname", json!("Terms Test")),
        ("home", json!(HOME)),
        ("siteurl", json!(HOME)),
    ] {
        set_option(&db, name, &value).await.unwrap();
    }
    // WordPress's "Uncategorized", the default category.
    let uncategorized = Term::create(&db, &Taxonomy::CATEGORY, NewTerm::new("Uncategorized"))
        .await
        .expect("creates the default category");
    set_option(&db, "default_category", &json!(uncategorized.term_id))
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
    let (_, author) = user(&db, "arthur", "author").await;

    // A visitor is sent to the sign-in page.
    for uri in ["/mc-admin/category", "/mc-admin/post_tag"] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::SeeOther, "{uri}");
        assert_eq!(location(&response), "/mc-admin/login", "{uri}");
    }

    who_may_open(&client, &admin, &editor, &author).await;

    sign_in(&client, &admin).await;
    add_a_tag(&client, &db).await;
    name_required(&client, &db).await;
    slug_unique(&client, &db).await;
    default_category_kept(&client, &db, &uncategorized).await;
    deleted_tag_leaves_posts(&client, &db, admin_id).await;
    sign_out(&client).await;
    db.close().await;
}

/// The screens need `manage_categories`, as WordPress's `edit-tags.php` needs the taxonomy's
/// `manage_terms`: administrators and editors hold it, authors do not. The sidebar lists them
/// under Posts, as WordPress's Posts submenu, shown on the Posts screens.
async fn who_may_open(client: &Client, admin: &str, editor: &str, author: &str) {
    for email in [admin, editor] {
        sign_in(client, email).await;
        let (_, posts) = get(client, "/mc-admin/post").await;
        let under_posts = &posts[posts.find("</i>Posts</a>").expect("the Posts entry")..];
        for uri in ["/mc-admin/category", "/mc-admin/post_tag"] {
            assert!(
                under_posts.contains(&format!("href=\"{uri}\"")),
                "{email} sees {uri} under Posts: {posts}"
            );
            let (status, html) = get(client, uri).await;
            assert_eq!(status, Status::Ok, "{email} opens {uri}: {html}");
        }
        let (status, html) = get(client, "/mc-admin/category").await;
        assert_eq!(status, Status::Ok);
        assert!(html.contains("Categories"), "{html}");
        assert!(html.contains("Uncategorized"), "{html}");
        sign_out(client).await;
    }

    sign_in(client, author).await;
    let (_, dashboard) = get(client, "/mc-admin/post").await;
    for uri in [
        "/mc-admin/category",
        "/mc-admin/post_tag",
        "/mc-admin/category/new",
        "/mc-admin/post_tag/new",
    ] {
        assert!(
            !dashboard.contains(&format!("href=\"{uri}\"")),
            "{dashboard}"
        );
        assert_eq!(get(client, uri).await.0, Status::Forbidden, "{uri}");
    }
    let (status, _, _) = post(client, "/mc-admin/post_tag", &[("name", "Sneaky")]).await;
    assert_eq!(status, Status::Forbidden);
    sign_out(client).await;
}

/// A tag added on its screen shows in the list, with the slug made from its name.
async fn add_a_tag(client: &Client, db: &Database) {
    let (status, html) = get(client, "/mc-admin/post_tag/new").await;
    assert_eq!(status, Status::Ok, "{html}");
    let (status, to, body) = post(
        client,
        "/mc-admin/post_tag",
        &[
            ("name", "Rust Lang"),
            ("slug", ""),
            ("description", "About <b>Rust</b><script>x</script>"),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(to, "/mc-admin/post_tag");
    let tag = tag_by_slug(db, "rust-lang")
        .await
        .expect("the tag is saved");
    assert_eq!(tag.name, "Rust Lang");
    // The description passes through `wp_filter_kses`.
    assert!(!tag.description.contains("<script>"), "{}", tag.description);
    let (status, list) = get(client, "/mc-admin/post_tag").await;
    assert_eq!(status, Status::Ok);
    for text in [
        "Rust Lang",
        "rust-lang",
        "Name",
        "Description",
        "Slug",
        "Count",
    ] {
        assert!(list.contains(text), "{text}: {list}");
    }
    // The search matches the name or the slug.
    let (_, found) = get(client, "/mc-admin/post_tag?q=lang").await;
    assert!(found.contains("Rust Lang"), "{found}");
    let (_, none) = get(client, "/mc-admin/post_tag?q=python").await;
    assert!(!none.contains("rust-lang"), "{none}");
}

/// A category without a name is refused on the form, and nothing is saved.
async fn name_required(client: &Client, db: &Database) {
    let before = Term::all(db, &Taxonomy::CATEGORY).await.unwrap().len();
    let (status, _, body) = post(
        client,
        "/mc-admin/category",
        &[
            ("name", "   "),
            ("slug", "nameless"),
            ("parent", ""),
            ("description", ""),
        ],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    assert_eq!(
        Term::all(db, &Taxonomy::CATEGORY).await.unwrap().len(),
        before
    );
    // With a name it is saved, under its parent.
    let parent = Term::all(db, &Taxonomy::CATEGORY).await.unwrap()[0].term_id;
    let (status, _, body) = post(
        client,
        "/mc-admin/category",
        &[
            ("name", "News"),
            ("slug", ""),
            ("parent", &parent.to_string()),
            ("description", ""),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let news = Term::find_by_slug(db, &Taxonomy::CATEGORY, "news")
        .await
        .unwrap()
        .expect("the category is saved");
    assert_eq!(news.parent_id, Some(parent));
    // Listed as a tree: the child under its parent, indented.
    let (_, list) = get(client, "/mc-admin/category").await;
    assert!(list.contains("— News"), "{list}");
}

/// Slugs are unique within the taxonomy: a new tag's taken slug is made unique, as
/// `wp_insert_term` does; a taken slug given on edit, and a name that exists, are refused on
/// the form.
async fn slug_unique(client: &Client, db: &Database) {
    let (status, _, body) = post(
        client,
        "/mc-admin/post_tag",
        &[("name", "Rust"), ("slug", "rust-lang"), ("description", "")],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let rust = tag_by_slug(db, "rust-lang-2")
        .await
        .expect("the slug is made unique");
    assert_eq!(rust.name, "Rust");

    let (status, _, body) = post(
        client,
        &format!("/mc-admin/post_tag/{}", rust.term_id),
        &[("name", "Rust"), ("slug", "rust-lang"), ("description", "")],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    assert!(body.contains("Slug is already taken."), "{body}");
    assert_eq!(
        tag_by_slug(db, "rust-lang-2").await.map(|t| t.term_id),
        Some(rust.term_id)
    );

    let (status, _, body) = post(
        client,
        "/mc-admin/post_tag",
        &[("name", "rust lang"), ("slug", ""), ("description", "")],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    assert!(body.contains("Name is already taken."), "{body}");

    // A category's slug may repeat a tag's: unique within its own taxonomy.
    let (status, _, body) = post(
        client,
        "/mc-admin/category",
        &[
            ("name", "Rust"),
            ("slug", "rust-lang"),
            ("parent", ""),
            ("description", ""),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert!(
        Term::find_by_slug(db, &Taxonomy::CATEGORY, "rust-lang")
            .await
            .unwrap()
            .is_some()
    );

    // Editing with the slug emptied makes it from the name again.
    let (status, _, body) = post(
        client,
        &format!("/mc-admin/post_tag/{}", rust.term_id),
        &[("name", "Rust Edition"), ("slug", ""), ("description", "")],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert!(tag_by_slug(db, "rust-edition").await.is_some());
}

/// The default category cannot be deleted, as WordPress keeps "Uncategorized": the request is
/// refused before anything changes, with the message.
async fn default_category_kept(client: &Client, db: &Database, uncategorized: &Term) {
    let (status, _, body) = post(
        client,
        &format!("/mc-admin/category/{}/delete", uncategorized.term_id),
        &[],
    )
    .await;
    assert_eq!(status, Status::Forbidden, "{body}");
    assert!(
        body.contains("The default category cannot be deleted."),
        "{body}"
    );
    assert!(
        Term::find(db, uncategorized.term_taxonomy_id)
            .await
            .unwrap()
            .is_some()
    );
    // Another category can be deleted.
    let news = Term::find_by_slug(db, &Taxonomy::CATEGORY, "news")
        .await
        .unwrap()
        .unwrap();
    let (status, to, body) = post(
        client,
        &format!("/mc-admin/category/{}/delete", news.term_id),
        &[],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(to, "/mc-admin/category");
    assert!(
        Term::find(db, news.term_taxonomy_id)
            .await
            .unwrap()
            .is_none()
    );
}

/// Deleting a tag removes it from the posts that had it.
async fn deleted_tag_leaves_posts(client: &Client, db: &Database, author: i64) {
    let tag = tag_by_slug(db, "rust-lang").await.unwrap();
    let post_row = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Tagged".to_owned(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("creates a post");
    post_row
        .attach_terms(db, &Taxonomy::POST_TAG, &[tag.term_taxonomy_id])
        .await
        .unwrap();
    let tag = tag_by_slug(db, "rust-lang").await.unwrap();
    assert_eq!(tag.count, 1);
    let (_, list) = get(client, "/mc-admin/post_tag?sort=col_count&dir=desc").await;
    assert!(list.contains("Rust Lang"), "{list}");

    let (status, _, body) = post(
        client,
        &format!("/mc-admin/post_tag/{}/delete", tag.term_id),
        &[],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert!(tag_by_slug(db, "rust-lang").await.is_none());
    assert!(
        post_row
            .terms(db, &Taxonomy::POST_TAG)
            .await
            .unwrap()
            .is_empty()
    );
    let (_, list) = get(client, "/mc-admin/post_tag").await;
    assert!(!list.contains(">Rust Lang<"), "{list}");
}
