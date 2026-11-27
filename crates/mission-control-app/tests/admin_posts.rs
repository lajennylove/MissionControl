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
use orbit_orm::{Database, Model, Operator};
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
/// ("Full width", from its `Template Name:` header).
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
        "{{-- Template Name: Full width --}}\n@extends('layouts.app')\n\n@section('content')\n<main data-template=\"wide\">\n  \
         @include('partials.entry')\n</main>\n@endsection\n",
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
    // A signed-in client is sent away from the sign-in page, so its token comes from the dashboard.
    let (mut status, mut html) = get(client, "/mc-admin/login").await;
    if status == Status::SeeOther {
        (status, html) = get(client, "/mc-admin").await;
    }
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
    // Signed in, the sign-in page redirects, so the token comes from the dashboard.
    let csrf = token_in(&get(client, "/mc-admin").await.1);
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

/// Opens the New screen of `post_type` (a form with no post yet), saves it with `fields` and
/// the session's token, and returns the new post's id, read from the redirect to its edit form,
/// which is then opened.
async fn new_post(client: &Client, post_type: &str, fields: &[(&str, &str)]) -> i64 {
    let (status, form) = get(client, &format!("/mc-admin/{post_type}/new")).await;
    assert_eq!(status, Status::Ok, "New shows the form: {form}");
    let mut all = vec![("_token", token_in(&form))];
    all.extend(fields.iter().map(|(k, v)| (*k, (*v).to_owned())));
    let all: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let response = post(client, &format!("/mc-admin/{post_type}"), &all).await;
    let status = response.status();
    let to = location(&response);
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "creates: {body}");
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

/// The New screen's refusal (Ruling R115): 403 with WordPress's message.
/// Whether the list row titled `title` shows the state badge `state` in `color` inside its
/// title cell, after the title, as the All tab shows a post's state.
fn badge_after(list: &str, title: &str, color: &str, state: &str) -> bool {
    list.contains(&format!(
        "<td>{title} <span class=\"ig-badge ig-badge-{color}\">{state}</span></td>"
    ))
}

/// The count a list's tab `label` shows, as "Drafts (3)".
fn tab_count(list: &str, label: &str) -> Option<i64> {
    let marker = format!(">{label} <span class=\"ig-tab-count\">(");
    let at = list.find(&marker)? + marker.len();
    list[at..].split(')').next()?.parse().ok()
}

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
    let (_, contributor_email) = user(&db, "cora", "contributor").await;
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
    status_tabs(&client, &db, editor).await;

    sign_out(&client).await;
    post_locks(&client, &db).await;
    brand_and_sidebar(&client, &db).await;
    others_are_refused(
        &client,
        &db,
        &author_email,
        &contributor_email,
        &subscriber_email,
    )
    .await;
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

/// WordPress's status links on the Posts and Pages lists, as Ignite's tabs at `?tab=`: All
/// (every status but Trash), Published, Scheduled, Drafts, Pending, Private and Trash, each with
/// the number of all the type's posts in it, whatever the search, as WordPress counts them. A tab
/// with no posts is left out, as WordPress leaves out an empty status link; All always shows. A
/// tab lists only its posts, and the search, the sort and the pages stay in it. On the All tab,
/// a post's state (Draft, Pending, Private, Scheduled) is a small badge inside the title cell,
/// after the title, with no column of its own; the other tabs show none.
async fn status_tabs(client: &Client, db: &Database, editor: i64) {
    let future = chrono::Utc::now().naive_utc() + chrono::Duration::days(30);
    for (post_type, status, title) in [
        ("post", PostStatus::Publish, "Tabbed published"),
        ("post", PostStatus::Draft, "Tabbed draft"),
        ("post", PostStatus::Pending, "Tabbed pending"),
        ("post", PostStatus::Private, "Tabbed private"),
        ("post", PostStatus::Future, "Tabbed scheduled"),
        ("post", PostStatus::Trash, "Tabbed trashed"),
        ("page", PostStatus::Pending, "Tabbed page pending"),
        ("page", PostStatus::Trash, "Tabbed page trashed"),
    ] {
        Post::create(
            db,
            NewPost {
                status,
                title: title.to_owned(),
                published_at: (status == PostStatus::Future).then_some(future),
                ..NewPost::new(editor, post_type)
            },
        )
        .await
        .unwrap();
    }
    // The type's posts in `status`, as `wp_count_posts` counts them.
    let count = |post_type: &'static str, status: Option<PostStatus>| async move {
        let query = Post::query().where_eq("post_type", post_type).where_op(
            "status",
            Operator::Ne,
            PostStatus::AutoDraft.as_str(),
        );
        match status {
            Some(status) => query.where_eq("status", status.as_str()),
            None => query.where_op("status", Operator::Ne, PostStatus::Trash.as_str()),
        }
        .count(db)
        .await
        .unwrap() as i64
    };
    let tabs = [
        ("All", None),
        ("Published", Some(PostStatus::Publish)),
        ("Scheduled", Some(PostStatus::Future)),
        ("Drafts", Some(PostStatus::Draft)),
        ("Pending", Some(PostStatus::Pending)),
        ("Private", Some(PostStatus::Private)),
        ("Trash", Some(PostStatus::Trash)),
    ];
    for post_type in ["post", "page"] {
        let (status, list) = get(client, &format!("/mc-admin/{post_type}")).await;
        assert_eq!(status, Status::Ok, "{list}");
        // The tabs, in WordPress's order, with their links; an empty one is left out, but All.
        let mut at = 0;
        for (label, status) in tabs {
            let key = status.map_or("all", PostStatus::as_str);
            let link = format!("href=\"/mc-admin/{post_type}?tab={key}\"");
            let total = count(post_type, status).await;
            if total == 0 && status.is_some() {
                assert!(
                    !list.contains(&link),
                    "{post_type}: {label} is empty: {list}"
                );
                assert_eq!(tab_count(&list, label), None, "{post_type}: {label}");
                continue;
            }
            let found = list[at..]
                .find(&link)
                .unwrap_or_else(|| panic!("{post_type}: the {label} tab after byte {at}: {list}"));
            at += found + link.len();
            assert_eq!(tab_count(&list, label), Some(total), "{post_type}: {label}");
        }
        // All is the default tab and leaves out the trash.
        assert!(
            list.contains(&format!(
                "class=\"ig-tab ig-tab-active\" href=\"/mc-admin/{post_type}?tab=all\" aria-current=\"page\""
            )),
            "{list}"
        );
        assert!(!list.contains("Tabbed trashed"), "{list}");
        assert!(!list.contains("Tabbed page trashed"), "{list}");
    }

    // The All tab: each state is a badge after the title, in the title cell; a published post
    // has none. The state has no column of its own (see the core's `types_resource` tests).
    let (_, all) = get(client, "/mc-admin/post?tab=all&q=Tabbed").await;
    // The counts are of all the type's posts, not of the search's, as WordPress's status links.
    for (label, status) in [("All", None), ("Drafts", Some(PostStatus::Draft))] {
        assert_eq!(
            tab_count(&all, label),
            Some(count("post", status).await),
            "{label}: {all}"
        );
    }
    assert!(
        count("post", None).await > 5,
        "the seed's posts are not counted"
    );
    for (title, color, state) in [
        ("Tabbed draft", "gray", "Draft"),
        ("Tabbed pending", "warning", "Pending"),
        ("Tabbed private", "info", "Private"),
        ("Tabbed scheduled", "primary", "Scheduled"),
    ] {
        assert!(badge_after(&all, title, color, state), "{title}: {all}");
    }
    assert!(all.contains("<td>Tabbed published</td>"), "{all}");
    assert!(!all.contains("Tabbed trashed"), "{all}");
    assert_eq!(all.matches("class=\"ig-badge ").count(), 4, "{all}");

    // Each other tab lists its status only, with no badge.
    for (key, title, others) in [
        ("publish", "Tabbed published", "Tabbed draft"),
        ("draft", "Tabbed draft", "Tabbed published"),
        ("pending", "Tabbed pending", "Tabbed draft"),
        ("private", "Tabbed private", "Tabbed draft"),
        ("future", "Tabbed scheduled", "Tabbed draft"),
        ("trash", "Tabbed trashed", "Tabbed draft"),
    ] {
        let (status, list) = get(client, &format!("/mc-admin/post?tab={key}&q=Tabbed")).await;
        assert_eq!(status, Status::Ok, "{key}");
        assert!(list.contains(&format!("<td>{title}</td>")), "{key}: {list}");
        assert!(!list.contains(others), "{key}: {list}");
        assert!(!list.contains("class=\"ig-badge "), "{key}: {list}");
        assert!(
            list.contains(&format!(
                "class=\"ig-tab ig-tab-active\" href=\"/mc-admin/post?q=Tabbed&amp;tab={key}\""
            )),
            "{key}: {list}"
        );
    }
    let (_, pages) = get(client, "/mc-admin/page?tab=pending").await;
    assert!(pages.contains("<td>Tabbed page pending</td>"), "{pages}");
    assert!(!pages.contains("<td>Tabbed pending</td>"), "{pages}");

    // The search and the sort stay in the tab: the search box carries it, the column links keep
    // it, and a search inside a tab finds only that tab's posts.
    let (_, drafts) = get(
        client,
        "/mc-admin/post?tab=draft&q=Tabbed&sort=col_title&dir=desc",
    )
    .await;
    assert!(
        drafts.contains("<input type=\"hidden\" name=\"tab\" value=\"draft\">"),
        "{drafts}"
    );
    assert!(
        drafts.contains(
            "href=\"/mc-admin/post?q=Tabbed&amp;tab=draft&amp;sort=col_title&amp;dir=asc\""
        ),
        "{drafts}"
    );
    assert!(drafts.contains("<td>Tabbed draft</td>"), "{drafts}");
    let (_, none) = get(client, "/mc-admin/post?tab=publish&q=Tabbed+draft").await;
    assert!(!none.contains("<td>Tabbed draft</td>"), "{none}");

    // Paging stays in the tab: a page of the Drafts tab is drafts only.
    for n in 0..25 {
        Post::create(
            db,
            NewPost {
                status: PostStatus::Draft,
                title: format!("Paged draft {n:02}"),
                ..NewPost::new(editor, "post")
            },
        )
        .await
        .unwrap();
    }
    let (_, first) = get(client, "/mc-admin/post?tab=draft").await;
    assert!(
        first.contains("href=\"/mc-admin/post?tab=draft&amp;page=2\""),
        "{first}"
    );
    let (status, second) = get(client, "/mc-admin/post?tab=draft&page=2").await;
    assert_eq!(status, Status::Ok);
    assert!(second.contains("Paged draft"), "{second}");
    assert!(!second.contains("Tabbed published"), "{second}");
    assert!(!second.contains("Hello from the seed"), "{second}");
}

/// Task A6: post locks, as WordPress's `_edit_lock`: opening a post's form locks it, a second
/// editor is shown who is editing and may take over, and the heartbeat renews or reports it.
async fn post_locks(client: &Client, db: &Database) {
    let (first, first_email) = user(db, "lena", "editor").await;
    let (second, second_email) = user(db, "otto", "editor").await;
    sign_in(client, &first_email).await;
    let id = new_post(client, "post", &[("title", "Locked")]).await;
    let lock = get_post_meta(db, id, "_edit_lock").await.unwrap();
    let lock = lock.and_then(|v| v.as_str().map(str::to_owned)).unwrap();
    assert!(lock.ends_with(&format!(":{first}")), "{lock}");
    sign_out(client).await;

    // The second editor sees the lock and nothing else.
    sign_in(client, &second_email).await;
    let edit = format!("/mc-admin/post/{id}/edit");
    let (status, page) = get(client, &edit).await;
    assert_eq!(status, Status::Ok);
    assert!(
        page.contains("lena is currently editing this post. Do you want to take over?"),
        "{page}"
    );
    assert!(!page.contains("data-ignite-editor"), "{page}");
    // Take over needs the session's token.
    let response = post(
        client,
        &format!("/mc-admin/post/{id}/lock"),
        &[("_token", "x")],
    )
    .await;
    assert_eq!(response.status().code, 419);
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/post/{id}/lock"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(location(&response), edit);
    let (status, page) = get(client, &edit).await;
    assert_eq!(status, Status::Ok);
    assert!(!page.contains("currently editing"), "{page}");
    let heartbeat = |token: String| {
        let id = id.to_string();
        async move {
            let response = post(
                client,
                "/mc-admin/admin-ajax.php?action=heartbeat",
                &[
                    ("action", "heartbeat"),
                    ("post_type", "post"),
                    ("post_id", &id),
                    ("_token", &token),
                ],
            )
            .await;
            assert_eq!(response.status(), Status::Ok);
            serde_json::from_str::<serde_json::Value>(&response.into_string().await.unwrap())
                .unwrap()
        }
    };
    let beat = heartbeat(token(client).await).await;
    assert!(
        beat["wp-refresh-post-lock"]["new_lock"]
            .as_str()
            .unwrap()
            .ends_with(&format!(":{second}")),
        "{beat}"
    );
    sign_out(client).await;

    // The first editor's heartbeat reports the take-over.
    sign_in(client, &first_email).await;
    let beat = heartbeat(token(client).await).await;
    assert_eq!(
        beat["wp-refresh-post-lock"]["lock_error"]["text"],
        "otto has taken over and is currently editing."
    );
    // Leaving the form releases only one's own lock.
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/admin-ajax.php?action=wp-remove-post-lock",
        &[
            ("action", "wp-remove-post-lock"),
            ("post_type", "post"),
            ("post_id", &id.to_string()),
            ("_token", &csrf),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::Ok);
    let (_, page) = get(client, &edit).await;
    assert!(page.contains("otto is currently editing"), "{page}");
    // A save locks the post for the one who saves, as WordPress's `edit_post`.
    save(
        client,
        "post",
        id,
        &[("title", "Locked"), ("status", "draft")],
    )
    .await;
    let lock = get_post_meta(db, id, "_edit_lock").await.unwrap();
    let lock = lock.and_then(|v| v.as_str().map(str::to_owned)).unwrap();
    assert!(lock.ends_with(&format!(":{first}")), "{lock}");
    // A heartbeat with a wrong token is refused.
    let response = post(
        client,
        "/mc-admin/admin-ajax.php?action=heartbeat",
        &[
            ("action", "heartbeat"),
            ("post_type", "post"),
            ("post_id", &id.to_string()),
            ("_token", "x"),
        ],
    )
    .await;
    assert_eq!(response.status().code, 419);
    sign_out(client).await;

    // An author cannot edit an editor's post: no lock page, no name.
    let (_, author_email) = user(db, "lockless", "author").await;
    sign_in(client, &author_email).await;
    let (status, page) = get(client, &edit).await;
    assert_eq!(status, Status::NotFound);
    assert!(!page.contains("lena"), "{page}");
    sign_out(client).await;
    // Signed out, the heartbeat says so.
    let response = post(
        client,
        "/mc-admin/admin-ajax.php?action=heartbeat",
        &[
            ("action", "heartbeat"),
            ("post_type", "post"),
            ("post_id", &id.to_string()),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::Unauthorized);
}

/// The sidebar of a signed-in administrator on the page `uri`, the Ignite theme's.
async fn sidebar(client: &Client, uri: &str) -> String {
    let (status, html) = get(client, uri).await;
    assert_eq!(status, Status::Ok, "{uri}: {html}");
    let start = html.find("aria-label=\"Main\"").expect("the sidebar");
    html[start..start + html[start..].find("</nav>").unwrap()].to_owned()
}

/// Asserts each of `entries` comes in `nav`, in order.
fn assert_in_order(nav: &str, entries: &[&str]) {
    let mut at = 0;
    for entry in entries {
        let found = nav[at..]
            .find(entry)
            .unwrap_or_else(|| panic!("`{entry}` after byte {at} of the sidebar: {nav}"));
        at += found + entry.len();
    }
}

/// WordPress's submenus as Ignite's child entries: Categories and Tags under Posts, in Content;
/// the seven settings screens under Settings, in Site: a label-only entry (Ignite's
/// `NavigationItem`) that opens General, its first screen. Menus stays in Site, alone.
async fn sidebar_children(client: &Client) {
    let child = |uri: &str, label: &str, active: bool| {
        if active {
            format!(
                "<a class=\"ig-nav-link ig-nav-link-active py-1.5 pl-11 text-[13px]\" href=\"{uri}\" aria-current=\"page\">{label}</a>"
            )
        } else {
            format!("<a class=\"ig-nav-link py-1.5 pl-11 text-[13px]\" href=\"{uri}\">{label}</a>")
        }
    };
    for (uri, current) in [
        ("/mc-admin/post", ""),
        ("/mc-admin/category", "/mc-admin/category"),
        ("/mc-admin/post_tag", "/mc-admin/post_tag"),
    ] {
        let nav = sidebar(client, uri).await;
        assert_in_order(
            &nav,
            &[
                "data-ig-nav-toggle>Content<",
                "</i>Posts</a>",
                &child(
                    "/mc-admin/category",
                    "Categories",
                    current == "/mc-admin/category",
                ),
                &child(
                    "/mc-admin/post_tag",
                    "Tags",
                    current == "/mc-admin/post_tag",
                ),
                "</i>Media</a>",
                "</i>Pages</a>",
                "data-ig-nav-toggle>Site<",
                "</i>Menus</a>",
                "</i>Settings</a>",
            ],
        );
        assert_eq!(nav.matches("/mc-admin/category\"").count(), 1, "{nav}");
        assert!(!nav.contains("/mc-admin/options-writing\""), "{uri}: {nav}");
    }
    let settings = [
        ("options-general", "General"),
        ("options-writing", "Writing"),
        ("options-reading", "Reading"),
        ("options-discussion", "Discussion"),
        ("options-media", "Media"),
        ("options-permalink", "Permalinks"),
        ("options-privacy", "Privacy"),
    ];
    for (current, uri) in [
        ("options-general", "/mc-admin/options-general/1/edit"),
        ("options-reading", "/mc-admin/options-reading/1/edit"),
        ("options-privacy", "/mc-admin/options-privacy/1/edit"),
    ] {
        let nav = sidebar(client, uri).await;
        let mut order = vec![
            "data-ig-nav-toggle>Site<".to_owned(),
            "</i>Menus</a>".to_owned(),
            "href=\"/mc-admin/options-general\"".to_owned(),
            "</i>Settings</a>".to_owned(),
        ];
        order.extend(
            settings
                .iter()
                .map(|(slug, label)| child(&format!("/mc-admin/{slug}"), label, *slug == current)),
        );
        let order: Vec<&str> = order.iter().map(String::as_str).collect();
        assert_in_order(&nav, &order);
        assert!(!nav.contains("/mc-admin/category\""), "{uri}: {nav}");
    }
}

/// The admin's brand (Ignite's brand options) and its sidebar in WordPress's menu order: the
/// dashboard, then Content (Posts 5, Media 10, Pages 20), then Site (Menus 60, then the settings
/// screens from 80), each with its Dashicons class.
async fn brand_and_sidebar(client: &Client, db: &Database) {
    // The signed-out bar of the sign-in page shows the logo too. The site's default admin theme
    // is Ignite's, so the sign-in page is Ignite's.
    let (_, login) = get(client, "/mc-admin/login").await;
    assert!(
        login.contains("src=\"/mc-includes/images/mission-control-logo.webp\" alt=\"RocketBoost Mission Control\""),
        "{login}"
    );
    assert!(login.contains(">Sign in</button>"), "{login}");
    assert!(!login.contains("id=\"loginform\""), "{login}");
    let (_, admin_email) = user(db, "ada", "administrator").await;
    sign_in(client, &admin_email).await;
    let (status, dashboard) = get(client, "/mc-admin").await;
    assert_eq!(status, Status::Ok);
    assert!(
        dashboard.contains(
            "<img src=\"/mc-includes/images/mission-control-logo.webp\" \
             alt=\"RocketBoost Mission Control\" class=\"w-auto\" style=\"height: 4.3rem\">"
        ),
        "{dashboard}"
    );
    assert!(
        dashboard.contains("href=\"/mc-includes/images/favicon-32.png\""),
        "{dashboard}"
    );
    assert!(
        dashboard.contains("· RocketBoost Mission Control</title>"),
        "{dashboard}"
    );
    assert!(!dashboard.contains("ignite-mark.png"), "{dashboard}");
    // The default theme is Ignite's, so WordPress's Dashicons stylesheet is not linked on any
    // panel page, the sign-in page too; it is served with the fonts it names, for the WordPress
    // theme (which links it while active).
    let dashicons = format!(
        "<link rel=\"stylesheet\" href=\"/mc-includes/css/dashicons.css?ver={}\">",
        mission_control::editor_version()
    );
    assert!(!dashboard.contains(&dashicons), "{dashboard}");
    assert!(!login.contains(&dashicons), "{login}");
    for (uri, kind) in [
        (
            format!(
                "/mc-includes/css/dashicons.css?ver={}",
                mission_control::editor_version()
            ),
            "text/css; charset=utf-8",
        ),
        (
            "/mc-includes/fonts/dashicons.eot?99ac726223c749443b642ce33df8b800".to_owned(),
            "application/vnd.ms-fontobject",
        ),
        (
            "/mc-includes/fonts/dashicons.ttf?99ac726223c749443b642ce33df8b800".to_owned(),
            "font/ttf",
        ),
    ] {
        let response = client.get(uri.as_str()).dispatch().await;
        assert_eq!(response.status(), Status::Ok, "{uri}");
        assert_eq!(response.content_type().unwrap().to_string(), kind, "{uri}");
        if kind.starts_with("text/css") {
            let css = response.into_string().await.unwrap();
            assert!(css.contains("font-family: dashicons;"), "{uri}");
            assert!(css.contains(".dashicons-admin-post::before"), "{uri}");
            assert!(css.contains("url(\"../fonts/dashicons.ttf?"), "{uri}");
        }
    }
    // The favicon and the logo are served.
    for (uri, kind) in [
        ("/mc-includes/images/favicon-32.png", "image/png"),
        (
            "/mc-includes/images/mission-control-logo.webp",
            "image/webp",
        ),
    ] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::Ok, "{uri}");
        assert_eq!(response.content_type().unwrap().to_string(), kind, "{uri}");
    }
    let nav_start = dashboard.find("aria-label=\"Main\"").expect("the sidebar");
    let nav = &dashboard[nav_start..dashboard[nav_start..].find("</nav>").unwrap() + nav_start];
    let order = [
        "href=\"/mc-admin\"",
        "data-ig-nav-toggle>Content<",
        "<i class=\"ti ti-pin text-lg\"></i>Posts",
        "<i class=\"ti ti-photo text-lg\"></i>Media",
        "<i class=\"ti ti-file-text text-lg\"></i>Pages",
        "data-ig-nav-toggle>Site<",
        "<i class=\"ti ti-brush text-lg\"></i>Menus",
        "<i class=\"ti ti-settings text-lg\"></i>Settings</a>",
    ];
    // Nothing between the dashboard and the Content group: every screen is in a group.
    let dashboard_end = nav.find("href=\"/mc-admin\"").unwrap();
    let content = nav.find("data-ig-nav-toggle>Content<").unwrap();
    assert!(
        !nav[dashboard_end..content].contains("ig-nav-link\" href"),
        "{nav}"
    );
    let mut at = 0;
    for entry in order {
        let found = nav[at..]
            .find(entry)
            .unwrap_or_else(|| panic!("`{entry}` after byte {at} of the sidebar: {nav}"));
        at += found + entry.len();
    }
    // The children show under their entry when it, or one of them, is the current page, as
    // Filament's: none on the dashboard, and no Post Types, Taxonomies or Field Groups yet.
    for child in [
        "/mc-admin/category\"",
        "/mc-admin/post_tag\"",
        "/mc-admin/options-writing\"",
        "Post Types",
        "Taxonomies",
        "Field Groups",
    ] {
        assert!(!nav.contains(child), "{child}: {nav}");
    }
    sidebar_children(client).await;
    // Choosing the WordPress theme on the profile page shows its Dashicons and loads their
    // stylesheet; an empty choice goes back to the site's default, Ignite's theme.
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/profile/admin-theme",
        &[("_token", &csrf), ("admin_theme", "wordpress")],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "chooses WordPress");
    let (_, wordpress) = get(client, "/mc-admin").await;
    assert!(wordpress.contains(&dashicons), "{wordpress}");
    // The theme's own layout: wp-admin's admin bar, admin menu and content area, not Ignite's.
    for expected in [
        "<body class=\"wp-admin\">",
        "<div id=\"wpadminbar\"",
        "Howdy, <span class=\"display-name\">ada</span>",
        "<ul id=\"adminmenu\">",
        "<span class=\"dashicons dashicons-admin-post\"></span></div><div class=\"wp-menu-name\">Posts</div>",
        "<li><a href=\"/mc-admin/post/new\">Add New</a></li><li><a href=\"/mc-admin/category\">Categories</a></li><li><a href=\"/mc-admin/post_tag\">Tags</a></li></ul>",
        "<div class=\"wp-menu-name\">Settings</div></a>\n<ul class=\"wp-submenu\"><li class=\"wp-submenu-head\" aria-hidden=\"true\">Settings</li><li><a href=\"/mc-admin/options-general\">General</a></li><li><a href=\"/mc-admin/options-writing\">Writing</a></li>",
        "<li><a href=\"/mc-admin/options-permalink\">Permalinks</a></li><li><a href=\"/mc-admin/options-privacy\">Privacy</a></li></ul>",
        "<li class=\"wp-menu-separator\" aria-hidden=\"true\"></li>",
        "<div id=\"wpbody\" role=\"main\">",
        "&lsaquo; RocketBoost Mission Control</title>",
        "--color-primary:#2271b1;",
    ] {
        assert!(wordpress.contains(expected), "`{expected}` in {wordpress}");
    }
    assert!(!wordpress.contains("ig-nav-link"), "{wordpress}");
    assert!(!wordpress.contains("aria-label=\"Main\""), "{wordpress}");
    // The groups are separated, without their headings.
    assert!(!wordpress.contains(">Content<"), "{wordpress}");
    // On a child's page its parent's submenu is open, with the child current.
    let (_, categories) = get(client, "/mc-admin/category").await;
    assert!(
        categories.contains("<li class=\"menu-top current wp-has-current-submenu\"><a class=\"menu-top\" href=\"/mc-admin/post\">"),
        "{categories}"
    );
    assert!(
        categories.contains("<li class=\"current\"><a href=\"/mc-admin/category\" aria-current=\"page\">Categories</a></li>"),
        "{categories}"
    );
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/profile/admin-theme",
        &[("_token", &csrf), ("admin_theme", "")],
    )
    .await;
    assert_eq!(
        response.status(),
        Status::SeeOther,
        "goes back to the default"
    );
    let (_, default) = get(client, "/mc-admin").await;
    assert!(!default.contains(&dashicons), "{default}");
    assert!(default.contains("ig-nav-link"), "{default}");
    assert!(!default.contains("id=\"wpadminbar\""), "{default}");
    sign_out(client).await;
    // A signed-out page renders in the site's default admin theme, not a user's choice: with
    // WordPress's as the default, the sign-in and lost password pages are wp-login.php's. Every
    // test here holds `LOCK`, so the process-wide default changes for this test only.
    ignite_panel::set_admin_theme("wordpress").expect("the WordPress theme is registered");
    let (status, login) = get(client, "/mc-admin/login").await;
    let (_, forgot) = get(client, "/mc-admin/password/forgot").await;
    ignite_panel::set_admin_theme(ignite_panel::theme::IGNITE_THEME).unwrap();
    assert_eq!(status, Status::Ok, "{login}");
    for expected in [
        "<body class=\"login\">",
        "Log In &lsaquo; RocketBoost Mission Control</title>",
        "<img src=\"/mc-includes/images/mission-control-logo.webp\" alt=\"RocketBoost Mission Control\">",
        "<form name=\"loginform\" id=\"loginform\" method=\"post\" action=\"/mc-admin/login\">",
        "<label for=\"user_login\">Email Address</label>",
        "<label for=\"user_pass\">Password</label>",
        "<input name=\"remember\" type=\"checkbox\" id=\"rememberme\" value=\"1\"> <label for=\"rememberme\">Remember Me</label>",
        "class=\"button button-primary button-large\" value=\"Log In\"",
        "<a href=\"/mc-admin/password/forgot\">Lost your password?</a>",
        dashicons.as_str(),
    ] {
        assert!(login.contains(expected), "`{expected}` in {login}");
    }
    assert!(!login.contains(">Sign in</button>"), "{login}");
    assert!(forgot.contains("id=\"lostpasswordform\""), "{forgot}");
    assert!(forgot.contains("value=\"Get New Password\""), "{forgot}");
    // The WordPress sign-in form signs in, as the panel's does.
    let csrf = token_in(&login);
    let response = post(
        client,
        "/mc-admin/login",
        &[
            ("_token", &csrf),
            ("email", &admin_email),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(
        response.status(),
        Status::SeeOther,
        "signs in from wp-login"
    );
    assert_eq!(location(&response), "/mc-admin");
    sign_out(client).await;
    // Back on Ignite's default, the sign-in page is Ignite's again.
    let (_, login) = get(client, "/mc-admin/login").await;
    assert!(login.contains(">Sign in</button>"), "{login}");
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

    // New shows the form and creates nothing: a post gets its id when it is saved (the
    // owner's rule; WordPress creates an auto-draft here). HEAD creates nothing either.
    // The posts, revisions aside (the first save records one, as `wp_save_post_revision`).
    let count = || async {
        Post::query()
            .where_op("post_type", Operator::Ne, "revision")
            .count(db)
            .await
            .unwrap()
    };
    let before = count().await;
    let (status, form) = get(client, "/mc-admin/post/new").await;
    assert_eq!(status, Status::Ok, "{form}");
    assert!(form.contains("action=\"/mc-admin/post\""), "{form}");
    assert!(form.contains("name=\"title\""), "{form}");
    let response = client.head("/mc-admin/post/new").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    for _ in 0..3 {
        assert_eq!(get(client, "/mc-admin/post/new").await.0, Status::Ok);
    }
    assert_eq!(count().await, before, "opening New creates no post");

    // A save without a title is refused, with the message on the field, and creates nothing.
    let response = post(
        client,
        "/mc-admin/post",
        &[
            ("_token", &token_in(&form)),
            ("title", "  "),
            ("content", "<p>x</p>"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap_or_default();
    assert!(body.contains("Title is required."), "{body}");
    assert_eq!(count().await, before, "a refused save creates no post");

    // The first save creates the post, owned by the editor, a Draft when no status is chosen,
    // and opens its edit form; the list shows it.
    let id = new_post(client, "post", &[("title", "Written in the admin")]).await;
    assert_eq!(count().await, before + 1);
    let created = find(db, id).await.unwrap();
    assert_eq!(created.status, PostStatus::Draft);
    assert_eq!(created.author_id, editor);
    assert_eq!(created.post_type, "post");
    assert_eq!(created.title, "Written in the admin");
    let (status, list) = get(client, "/mc-admin/post").await;
    assert_eq!(status, Status::Ok);
    assert!(list.contains("Hello from the seed"), "{list}");
    assert!(
        list.contains(&format!("/mc-admin/post/{}/edit", hello.id)),
        "{list}"
    );
    assert!(
        list.contains(&format!("/mc-admin/post/{id}/edit")),
        "{list}"
    );
    // The saved draft is listed with its state after the title, as `_post_states` writes it,
    // as a badge inside the title cell on the All tab; the published post has none.
    assert!(
        badge_after(&list, "Written in the admin", "gray", "Draft"),
        "{list}"
    );
    assert!(list.contains("<td>Hello from the seed</td>"), "{list}");
    assert!(
        !badge_after(&list, "Hello from the seed", "gray", "Draft"),
        "{list}"
    );

    // An empty title is refused on the edit form too.
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/post/{id}"),
        &[("_token", &csrf), ("title", ""), ("status", "draft")],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap_or_default();
    assert!(body.contains("Title is required."), "{body}");
    assert_eq!(find(db, id).await.unwrap().title, "Written in the admin");

    // A published post's empty slug is made from the title (`sanitize_title`), and a slug the
    // type already has gets -2, -3, as `wp_unique_post_slug` does.
    let mut same = Vec::new();
    for _ in 0..3 {
        let id = new_post(
            client,
            "post",
            &[
                ("title", "Ünïcode & Friends: Part 1"),
                ("status", "publish"),
            ],
        )
        .await;
        same.push(find(db, id).await.unwrap().slug);
    }
    assert_eq!(
        same,
        [
            "unicode-friends-part-1",
            "unicode-friends-part-1-2",
            "unicode-friends-part-1-3"
        ]
    );
    // A given slug is sanitized and made unique the same way.
    let given = new_post(
        client,
        "post",
        &[
            ("title", "Another"),
            ("slug", "Unicode Friends Part 1"),
            ("status", "publish"),
        ],
    )
    .await;
    assert_eq!(
        find(db, given).await.unwrap().slug,
        "unicode-friends-part-1-4"
    );

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
    let title = ("title", "Written in the admin");
    save(client, "post", id, &[title, ("status", "publish")]).await;
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Publish);
    let (_, list) = get(client, "/mc-admin/post").await;
    assert!(list.contains("<td>Written in the admin</td>"), "{list}");
    assert!(!list.contains("Written in the admin —"), "{list}");
    let (status, page) = get(client, &front).await;
    assert_eq!(status, Status::Ok);
    assert!(page.contains("Written in the admin"), "{page}");

    // Delete moves it to the trash (Ruling R117); it leaves the front end.
    assert_eq!(delete(client, "post", id).await, Status::SeeOther);
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Trash);
    assert_eq!(get(client, &front).await.0, Status::NotFound);
    // Saving it with another status restores it.
    save(client, "post", id, &[title, ("status", "draft")]).await;
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
    let parent = new_post(client, "page", &[("title", "About"), ("status", "publish")]).await;
    assert_eq!(find(db, parent).await.unwrap().status, PostStatus::Publish);

    // Parent, order and template (Task 6.2).
    let (_, edit) = get(client, "/mc-admin/page/new").await;
    assert!(edit.contains("name=\"template\""), "{edit}");
    assert!(edit.contains("Default template"), "{edit}");
    assert!(edit.contains("Full width"), "{edit}");
    assert!(edit.contains("name=\"parent\""), "{edit}");
    assert!(edit.contains("name=\"menu_order\""), "{edit}");
    let parent_id = parent.to_string();
    let id = new_post(client, "page", &[("title", "Team")]).await;
    // A draft page is in the Pages list, tagged; the published one is not.
    let (_, list) = get(client, "/mc-admin/page").await;
    assert!(badge_after(&list, "Team", "gray", "Draft"), "{list}");
    assert!(list.contains("<td>About</td>"), "{list}");
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
        &[
            ("title", "Team"),
            ("status", "publish"),
            ("template", "default"),
        ],
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
            ("title", "Team"),
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
        &[("_token", &csrf), ("title", "Team"), ("status", "trash")],
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
    // Ignite's policy refuses (Task A1): 403.
    let (status, body) = get(client, &format!("/mc-admin/page/{parent}/edit")).await;
    assert_eq!(status, Status::Forbidden, "{body}");
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/page/{parent}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);
    assert_eq!(find(db, parent).await.unwrap().status, PostStatus::Publish);

    // A stored template the theme no longer has shows as the default, and the next save
    // stores none, as `wp_insert_post` resets it.
    set_post_meta(db, id, "_wp_page_template", &json!("templates/gone"))
        .await
        .unwrap();
    let (_, edit) = get(client, &format!("/mc-admin/page/{id}/edit")).await;
    assert!(edit.contains("value=\"default\" selected"), "{edit}");
}

/// Task A1: authors and contributors on the post screens, through Ignite's policy and the
/// resource's user-aware queries; subscribers are refused.
async fn others_are_refused(
    client: &Client,
    db: &Database,
    author_email: &str,
    contributor_email: &str,
    subscriber_email: &str,
) {
    let someones_post: i64 = Post::query()
        .where_eq("post_type", "post")
        .where_eq("title", "Hello from the seed")
        .first(db)
        .await
        .unwrap()
        .unwrap()
        .id;
    let any_page: i64 = Post::query()
        .where_eq("post_type", "page")
        .first(db)
        .await
        .unwrap()
        .unwrap()
        .id;

    // An author: their own posts, published, with kses; not other users' posts, not pages.
    sign_in(client, author_email).await;
    let (status, dashboard) = get(client, "/mc-admin").await;
    assert_eq!(status, Status::Ok);
    assert!(dashboard.contains("href=\"/mc-admin/post\""), "{dashboard}");
    assert!(
        !dashboard.contains("href=\"/mc-admin/page\""),
        "{dashboard}"
    );
    let id = new_post(client, "post", &[("title", "Draft by the author")]).await;
    let author_id = find(db, id).await.unwrap().author_id;
    save(
        client,
        "post",
        id,
        &[
            ("title", "By <b>the</b> author"),
            ("content", "<p>Hi <script>alert(1)</script>there</p>"),
            ("author", &author_id.to_string()),
            ("status", "publish"),
        ],
    )
    .await;
    let saved = find(db, id).await.unwrap();
    assert_eq!(saved.status, PostStatus::Publish);
    assert_eq!(saved.title, "By the author");
    assert!(!saved.content.contains("<script"), "{}", saved.content);
    let (status, list) = get(client, "/mc-admin/post").await;
    assert_eq!(status, Status::Ok);
    assert!(list.contains("By the author"), "{list}");
    assert!(!list.contains("Hello from the seed"), "{list}");
    // Another user's post is outside their list: 404 on its form, its save and its Delete.
    let edit = format!("/mc-admin/post/{someones_post}/edit");
    assert_eq!(get(client, &edit).await.0, Status::NotFound);
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/post/{someones_post}"),
        &[("_token", &csrf), ("title", "Taken over")],
    )
    .await;
    assert_eq!(response.status(), Status::NotFound);
    assert_ne!(find(db, someones_post).await.unwrap().title, "Taken over");
    assert_eq!(
        delete(client, "post", someones_post).await,
        Status::NotFound
    );
    // An author has no `edit_pages`.
    assert_eq!(get(client, "/mc-admin/page").await.0, Status::Forbidden);
    let (status, body) = get(client, "/mc-admin/page/new").await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to create posts as this user.",
    );
    // Nor may they save the New screen of pages (Ignite's create): nothing is created.
    let pages = || async {
        Post::query()
            .where_eq("post_type", "page")
            .count(db)
            .await
            .unwrap()
    };
    let before = pages().await;
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/page",
        &[("_token", &csrf), ("title", "Not mine to add")],
    )
    .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to create posts as this user.",
    );
    assert_eq!(pages().await, before);
    assert_eq!(
        get(client, &format!("/mc-admin/page/{any_page}/edit"))
            .await
            .0,
        Status::Forbidden
    );
    // Their own post goes to the trash and comes back.
    assert_eq!(delete(client, "post", id).await, Status::SeeOther);
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Trash);
    save(
        client,
        "post",
        id,
        &[("title", "By the author"), ("status", "draft")],
    )
    .await;
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Draft);
    sign_out(client).await;

    // A contributor cannot publish: Published is saved as Pending, as WordPress does; once
    // published by someone else, their post is no longer theirs to edit.
    sign_in(client, contributor_email).await;
    // The New screen's save too.
    let id = new_post(
        client,
        "post",
        &[("title", "For review"), ("status", "publish")],
    )
    .await;
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Pending);
    let title = ("title", "For review");
    save(client, "post", id, &[title, ("status", "publish")]).await;
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Pending);
    save(client, "post", id, &[title, ("status", "private")]).await;
    assert_eq!(find(db, id).await.unwrap().status, PostStatus::Pending);
    // A user who cannot publish sets no password (`_wp_translate_postdata`).
    save(
        client,
        "post",
        id,
        &[title, ("status", "pending"), ("password", "secret")],
    )
    .await;
    assert_eq!(find(db, id).await.unwrap().password, None);
    // Another author cannot be chosen: the only choice is themselves.
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/post/{id}"),
        &[("_token", &csrf), title, ("author", &author_id.to_string())],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    assert_ne!(find(db, id).await.unwrap().author_id, author_id);
    let mut published = find(db, id).await.unwrap();
    published.status = PostStatus::Publish;
    published.save(db).await.unwrap();
    assert_eq!(
        get(client, &format!("/mc-admin/post/{id}/edit")).await.0,
        Status::Forbidden
    );
    sign_out(client).await;

    // A subscriber reaches the dashboard and their profile (Ruling R23), not the screens.
    sign_in(client, subscriber_email).await;
    let (status, dashboard) = get(client, "/mc-admin").await;
    assert_eq!(status, Status::Ok);
    assert!(!dashboard.contains("/mc-admin/post\""), "{dashboard}");
    assert_eq!(get(client, "/mc-admin/profile").await.0, Status::Ok);
    assert_eq!(get(client, "/mc-admin/page").await.0, Status::Forbidden);
    let (status, body) = get(client, "/mc-admin/post/new").await;
    assert_refused(
        status,
        &body,
        "Sorry, you are not allowed to create posts as this user.",
    );
    assert_eq!(
        get(client, &format!("/mc-admin/page/{any_page}/edit"))
            .await
            .0,
        Status::Forbidden
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
