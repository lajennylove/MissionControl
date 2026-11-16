//! The admin's post and page screens (Tasks 6.1 and 6.2): Ignite's panel at `/mc-admin`, with
//! the post type resources and Mission Control's authorization (Rulings R114 to R123).
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so the flows run when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of their own
//! ([`scratch_database`]) with the installer's migrations, dropped at the end; otherwise they say
//! why they are skipped and pass. Ignite reads its database from `DATABASE_URL`, so the flow test
//! sets it for the process; every test here holds [`LOCK`] so none runs while it changes.
//!
//! On SQLite, `/mc-admin` answers the admin's 503 page.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    NewPost, Post, PostStatus, get_post_meta, set_option, set_post_meta,
};
use mission_control_core::migrations::migrator;
use mission_control_testing::{ScratchDatabase, create_users_table, scratch_database};
use orbit_orm::{Database, Model};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::json;

/// Held by every test here: the flow test sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
const PASSWORD: &str = "correct horse battery staple";

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

/// A copy of the default theme in a folder of its own, with a page template `templates/wide`
/// ("Full width") declared in its manifest.
fn themes_with_a_page_template() -> tempfile::TempDir {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                if entry.file_name() != "node_modules" {
                    copy(&entry.path(), &target);
                }
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let theme = dir.path().join("default");
    copy(&shipped_themes().join("default"), &theme);
    std::fs::create_dir_all(theme.join("templates")).unwrap();
    std::fs::write(
        theme.join("templates/wide.propel"),
        "@extends('layouts.app')\n\n@section('content')\n<main data-template=\"wide\">\n  \
         @include('partials.entry')\n</main>\n@endsection\n",
    )
    .unwrap();
    let manifest = std::fs::read_to_string(theme.join("theme.toml")).unwrap();
    std::fs::write(
        theme.join("theme.toml"),
        format!("{manifest}\n[templates]\n\"templates/wide\" = \"Full width\"\n"),
    )
    .unwrap();
    dir
}

fn config(database_url: &str, themes_dir: PathBuf) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir,
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
    let (status, html) = get(client, "/mc-admin/login").await;
    assert_eq!(status, Status::Ok, "{html}");
    token_in(&html)
}

/// Signs `email` in through Ignite's form.
async fn sign_in(client: &Client, email: &str) {
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
    assert_eq!(location(&response), "/mc-admin");
}

async fn sign_out(client: &Client) {
    let csrf = token(client).await;
    let response = post(client, "/mc-admin/logout", &[("_token", &csrf)]).await;
    assert_eq!(response.status(), Status::SeeOther);
}

/// An Ignite user with the Ignite role `role` (a WordPress role's name). Returns the id.
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

async fn find(db: &Database, id: i64) -> Option<Post> {
    Post::query().find(db, id).await.expect("reads the post")
}

/// Opens the New screen of `post_type` and returns the auto-draft's id.
async fn new_draft(client: &Client, post_type: &str) -> i64 {
    let response = client
        .get(format!("/mc-admin/{post_type}/new"))
        .dispatch()
        .await;
    assert_eq!(
        response.status(),
        Status::SeeOther,
        "New answers with a redirect"
    );
    let to = location(&response);
    let id = to
        .strip_prefix(&format!("/mc-admin/{post_type}/"))
        .and_then(|rest| rest.strip_suffix("/edit"))
        .unwrap_or_else(|| panic!("redirects to the edit form: {to}"))
        .parse()
        .unwrap();
    let (status, html) = get(client, &to).await;
    assert_eq!(status, Status::Ok, "{html}");
    id
}

/// Saves the edit form of `post_type` post `id` with `fields` and the session's token.
async fn save(client: &Client, post_type: &str, id: i64, fields: &[(&str, &str)]) {
    let csrf = token(client).await;
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = post(client, &format!("/mc-admin/{post_type}/{id}"), &all).await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "saves: {body}");
}

async fn delete(client: &Client, post_type: &str, id: i64) -> Status {
    let csrf = token(client).await;
    post(
        client,
        &format!("/mc-admin/{post_type}/{id}/delete"),
        &[("_token", &csrf)],
    )
    .await
    .status()
}

/// Ruling R115's refusal: 403 with WordPress's message.
fn assert_refused(status: Status, body: &str, message: &str) {
    assert_eq!(status, Status::Forbidden, "{body}");
    assert!(body.contains(message), "{body}");
}

#[test]
fn an_editor_manages_posts_and_pages_in_the_admin() {
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
        .block_on(scratch_database("admin"));
    // Ignite reads DATABASE_URL when the app builds the panel. A run without it first: the
    // admin is not mounted on a database that is not Ignite's.
    rocket::async_test(an_admin_on_another_database_is_unavailable(&scratch));
    // SAFETY: no other thread reads or writes the environment now. Every test of this process
    // holds `LOCK`, and the runtime above is gone; the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(flows(&scratch));
    // SAFETY: as above: the runtime of the flows has ended, and `LOCK` is held.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

async fn an_admin_on_another_database_is_unavailable(scratch: &ScratchDatabase) {
    run_migrations(scratch.url()).await.expect("migrates");
    let client = Client::tracked(rocket_with_options(
        config(scratch.url(), shipped_themes()),
        AppOptions::default(),
    ))
    .await
    .expect("the app ignites");
    let (status, body) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::ServiceUnavailable, "{body}");
    assert!(body.contains("DATABASE_URL"), "{body}");
}

async fn flows(scratch: &ScratchDatabase) {
    let themes = themes_with_a_page_template();
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
    set_option(&db, "blogname", &json!("Admin Test"))
        .await
        .unwrap();
    let client = Client::tracked(rocket_with_options(
        config(scratch.url(), themes.path().to_path_buf()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");

    // The roles exist once the app has started (Ruling R121).
    let (editor, editor_email) = user(&db, "edna", "editor").await;
    let (_, author_email) = user(&db, "arthur", "author").await;
    let (_, subscriber_email) = user(&db, "sue", "subscriber").await;

    // A visitor is sent to the sign-in page, by Ignite, also from the New screen.
    for uri in ["/mc-admin", "/mc-admin/post", "/mc-admin/post/new"] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::SeeOther, "{uri}");
        assert_eq!(location(&response), "/mc-admin/login", "{uri}");
    }

    // The REST API keeps WordPress's JSON errors with the panel's catchers mounted.
    rest_errors_keep_their_json(&client).await;

    sign_in(&client, &editor_email).await;
    let (status, dashboard) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::Ok);
    assert!(dashboard.contains("Posts"), "{dashboard}");
    assert!(dashboard.contains("Pages"), "{dashboard}");

    posts(&client, &db, editor).await;
    pages(&client, &db).await;

    sign_out(&client).await;
    others_are_refused(&client, &db, &author_email, &subscriber_email).await;
    drop(client);

    // Fail closed: a second app in the process finds Ignite's `post` and `page` taken by the
    // first; it serves no post type screens, and refuses those slugs instead of letting Ignite's
    // coarse grants decide.
    let second = Client::tracked(rocket_with_options(
        config(scratch.url(), themes.path().to_path_buf()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the second app ignites");
    sign_in(&second, &editor_email).await;
    for uri in [
        "/mc-admin/post",
        "/mc-admin/page/new",
        "/mc-admin/post/1/edit",
    ] {
        let (status, body) = get(&second, uri).await;
        assert_eq!(status, Status::ServiceUnavailable, "{uri}: {body}");
        assert!(body.contains("not available"), "{uri}: {body}");
    }
    assert_eq!(get(&second, "/mc-admin").await.0, Status::Ok);
    drop(second);
    db.close().await;
}

async fn rest_errors_keep_their_json(client: &Client) {
    // Refused credentials, through the `rest_route` form: the fairing's internal route.
    let response = client
        .get("/?rest_route=/")
        .header(rocket::http::Header::new(
            "Authorization",
            "Basic bm9ib2R5QGV4YW1wbGUudGVzdDp3cm9uZw==",
        ))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some("application/json; charset=UTF-8")
    );
    let body: serde_json::Value =
        serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert_eq!(body["code"], "invalid_email");
    // A guard's 401 under `/mc-json`: the REST catcher, not Ignite's.
    let response = client.get("/mc-json/mc/v2/users/me").dispatch().await;
    assert_eq!(response.status(), Status::Unauthorized);
    let body: serde_json::Value =
        serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert_eq!(body["code"], "rest_not_logged_in");
}

async fn posts(client: &Client, db: &Database, editor: i64) {
    let hello = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Hello from the seed".to_owned(),
            ..NewPost::new(editor, "post")
        },
    )
    .await
    .unwrap();

    // HEAD on New creates nothing (Ruling R125).
    let count = || async {
        Post::query()
            .where_eq("status", PostStatus::AutoDraft)
            .count(db)
            .await
            .unwrap()
    };
    let before = count().await;
    let response = client.head("/mc-admin/post/new").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(count().await, before);

    // New: an auto-draft owned by the editor, and its edit form (Ruling R116).
    let id = new_draft(client, "post").await;
    let draft = find(db, id).await.unwrap();
    assert_eq!(draft.status, PostStatus::AutoDraft);
    assert_eq!(draft.author_id, editor);
    assert_eq!(draft.post_type, "post");

    // The list shows the posts, not the auto-draft.
    let (status, list) = get(client, "/mc-admin/post").await;
    assert_eq!(status, Status::Ok);
    assert!(list.contains("Hello from the seed"), "{list}");
    assert!(
        list.contains(&format!("/mc-admin/post/{}/edit", hello.id)),
        "{list}"
    );
    assert!(
        !list.contains(&format!("/mc-admin/post/{id}/edit")),
        "{list}"
    );

    // Ignite's create form is not used.
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/post",
        &[("_token", &csrf), ("title", "x"), ("status", "draft")],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);

    // Saving title, content and status; the HTML is stored as sent (an editor has
    // `unfiltered_html`).
    let html = "<p>Hello <script>alert(1)</script> <b onclick=\"x()\">there</b></p>";
    save(
        client,
        "post",
        id,
        &[
            ("title", "Written in the admin"),
            ("content", html),
            ("status", "draft"),
        ],
    )
    .await;
    let saved = find(db, id).await.unwrap();
    assert_eq!(saved.status, PostStatus::Draft);
    assert_eq!(saved.title, "Written in the admin");
    assert_eq!(saved.content, html);
    let (_, list) = get(client, "/mc-admin/post").await;
    assert!(list.contains("Written in the admin"), "{list}");

    // A draft is not on the front end; published, it is.
    let front = format!("/?p={id}");
    assert_eq!(get(client, &front).await.0, Status::NotFound);
    save(client, "post", id, &[("status", "publish")]).await;
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Publish);
    let (status, page) = get(client, &front).await;
    assert_eq!(status, Status::Ok);
    assert!(page.contains("Written in the admin"), "{page}");

    // Delete moves it to the trash (Ruling R117); it leaves the front end.
    assert_eq!(delete(client, "post", id).await, Status::SeeOther);
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Trash);
    assert_eq!(get(client, &front).await.0, Status::NotFound);
    // Saving it with another status restores it.
    save(client, "post", id, &[("status", "draft")]).await;
    let restored = find(db, id).await.unwrap();
    assert_eq!(restored.status, PostStatus::Draft);
    assert_eq!(restored.content, html);
    // Delete on a trashed post deletes it for good.
    assert_eq!(delete(client, "post", id).await, Status::SeeOther);
    assert_eq!(delete(client, "post", id).await, Status::SeeOther);
    assert!(find(db, id).await.is_none());
    assert_eq!(
        get(client, &format!("/mc-admin/post/{id}/edit")).await.0,
        Status::NotFound
    );
}

async fn pages(client: &Client, db: &Database) {
    let (status, html) = get(client, "/mc-admin/page").await;
    assert_eq!(status, Status::Ok, "{html}");
    let parent = new_draft(client, "page").await;
    save(
        client,
        "page",
        parent,
        &[("title", "About"), ("status", "publish")],
    )
    .await;

    // Parent, order and template (Task 6.2).
    let id = new_draft(client, "page").await;
    let (_, edit) = get(client, &format!("/mc-admin/page/{id}/edit")).await;
    assert!(edit.contains("name=\"template\""), "{edit}");
    assert!(edit.contains("Default template"), "{edit}");
    assert!(edit.contains("Full width"), "{edit}");
    assert!(edit.contains("name=\"parent\""), "{edit}");
    assert!(edit.contains("name=\"menu_order\""), "{edit}");
    let parent_id = parent.to_string();
    save(
        client,
        "page",
        id,
        &[
            ("title", "Team"),
            ("status", "publish"),
            ("parent", &parent_id),
            ("menu_order", "3"),
            ("template", "templates/wide"),
        ],
    )
    .await;
    let page = find(db, id).await.unwrap();
    assert_eq!(page.parent_id, Some(parent));
    assert_eq!(page.menu_order, 3);
    assert_eq!(
        get_post_meta(db, id, "_wp_page_template").await.unwrap(),
        Some(json!("templates/wide"))
    );
    let (status, front) = get(client, &format!("/?page_id={id}")).await;
    assert_eq!(status, Status::Ok);
    assert!(front.contains("data-template=\"wide\""), "{front}");
    // The edit form shows the stored choices.
    let (_, edit) = get(client, &format!("/mc-admin/page/{id}/edit")).await;
    assert!(edit.contains("value=\"templates/wide\" selected"), "{edit}");

    // The default template stores none, as the REST API does.
    save(
        client,
        "page",
        id,
        &[("status", "publish"), ("template", "default")],
    )
    .await;
    assert_eq!(
        get_post_meta(db, id, "_wp_page_template").await.unwrap(),
        None
    );

    // A template the theme does not have is refused by the form.
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/page/{id}"),
        &[
            ("_token", &csrf),
            ("status", "publish"),
            ("template", "templates/missing"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);

    // The front page: an editor edits it, but its Trash status is refused by the form; it stays.
    set_option(db, "page_on_front", &json!(id)).await.unwrap();
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/page/{id}"),
        &[("_token", &csrf), ("status", "trash")],
    )
    .await;
    assert_eq!(response.status(), Status::InternalServerError);
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Publish);
    save(
        client,
        "page",
        id,
        &[("title", "Team!"), ("status", "publish")],
    )
    .await;
    assert_eq!(find(db, id).await.unwrap().title, "Team!");
    set_option(db, "page_on_front", &json!(0)).await.unwrap();

    // `edit_post` on the privacy policy page needs `manage_options`, which an editor lacks.
    set_option(db, "wp_page_for_privacy_policy", &json!(parent))
        .await
        .unwrap();
    let (status, body) = get(client, &format!("/mc-admin/page/{parent}/edit")).await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to edit this item.",
    );
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/page/{parent}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to move this item to the Trash.",
    );
    assert_eq!(find(db, parent).await.unwrap().status, PostStatus::Publish);

    // A stored template the theme no longer has shows as the default, and the next save
    // stores none, as `wp_insert_post` resets it.
    set_post_meta(db, id, "_wp_page_template", &json!("templates/gone"))
        .await
        .unwrap();
    let (_, edit) = get(client, &format!("/mc-admin/page/{id}/edit")).await;
    assert!(edit.contains("value=\"default\" selected"), "{edit}");
}

async fn others_are_refused(
    client: &Client,
    db: &Database,
    author_email: &str,
    subscriber_email: &str,
) {
    let any_post: i64 = Post::query()
        .where_eq("post_type", "page")
        .first(db)
        .await
        .unwrap()
        .unwrap()
        .id;

    // An author cannot use the post screens yet (Ruling R115, owner item O8).
    sign_in(client, author_email).await;
    let (status, body) = get(client, "/mc-admin/post").await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to edit posts in this post type.",
    );
    let (status, body) = get(client, "/mc-admin/post/new").await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to create posts as this user.",
    );
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/page/{any_post}"),
        &[
            ("_token", &csrf),
            ("title", "Taken over"),
            ("status", "draft"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);
    assert_ne!(find(db, any_post).await.unwrap().title, "Taken over");
    // Ignite's own pages stay open to them.
    assert_eq!(get(client, "/mc-admin").await.0, Status::Ok);
    sign_out(client).await;

    // A subscriber reaches the dashboard and their profile (Ruling R23), not the screens.
    sign_in(client, subscriber_email).await;
    let (status, dashboard) = get(client, "/mc-admin").await;
    assert_eq!(status, Status::Ok);
    assert!(!dashboard.contains("/mc-admin/post\""), "{dashboard}");
    assert_eq!(get(client, "/mc-admin/profile").await.0, Status::Ok);
    let (status, body) = get(client, "/mc-admin/page").await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to edit posts in this post type.",
    );
    let (status, body) = get(client, &format!("/mc-admin/page/{any_post}/edit")).await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to edit posts in this post type.",
    );
    sign_out(client).await;
}

#[test]
fn on_sqlite_the_admin_answers_that_it_needs_mysql_or_mariadb() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    rocket::async_test(on_sqlite());
}

async fn on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("admin.db").display()
    );
    let db = Arc::new(Database::connect(&url).await.unwrap());
    create_users_table(&db).await;
    migrator().unwrap().run(&db).await.unwrap();
    let client = Client::tracked(rocket_with_options(
        config(&url, shipped_themes()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");

    for uri in [
        "/mc-admin",
        "/mc-admin/",
        "/mc-admin/post",
        "/mc-admin/login",
    ] {
        let (status, body) = get(&client, uri).await;
        assert_eq!(status, Status::ServiceUnavailable, "{uri}");
        assert!(body.contains("MySQL or MariaDB"), "{uri}: {body}");
    }
    let response = post(&client, "/mc-admin/login", &[("email", "a@b.c")]).await;
    assert_eq!(response.status(), Status::ServiceUnavailable);
    // The site and the REST API run as before.
    assert_eq!(get(&client, "/").await.0, Status::Ok);
    assert_eq!(get(&client, "/mc-json/").await.0, Status::Ok);
    let response = client.get("/mc-json/mc/v2/users/me").dispatch().await;
    assert_eq!(response.status(), Status::Unauthorized);
    let body: serde_json::Value =
        serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert_eq!(body["code"], "rest_not_logged_in");
    db.close().await;
}
