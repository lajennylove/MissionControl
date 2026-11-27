//! The trash of the admin's post lists, as WordPress's (`wp-admin/edit.php`,
//! `WP_Posts_List_Table::handle_row_actions`, `wp_trash_post`, `wp_untrash_post` and
//! `wp_delete_post`): the row action "Trash" on a post out of the trash, asked for nothing; the
//! Trash tab, whose rows have "Restore" and "Delete Permanently" (asked first, with WordPress's
//! text) and no Edit; the slug freed while the post is trashed (`__trashed`) and given back on
//! Restore (made unique when another post took it), which makes the post a draft; WordPress's
//! messages; and `delete_post` for each of them.
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
use mission_control_core::content::{Post, PostStatus, get_post_meta, set_option};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::{Database, Model};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::json;

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

/// The CSRF token of the client's session.
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

/// An Ignite user with the role `role`. Returns the email.
async fn user(db: &Database, name: &str, role: &str) -> String {
    let email = format!("{name}@example.test");
    users::create_user(db, name, &email, PASSWORD)
        .await
        .expect("creates the user");
    authz::assign_role(db, &email, role)
        .await
        .expect("gives the role");
    email
}

async fn find(db: &Database, id: i64) -> Option<Post> {
    Post::query().find(db, id).await.expect("reads the post")
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

/// Runs the row action `name` on `post_type` post `id`, sent back to `back`: the status, and the
/// address it sends the browser to.
async fn row_action(
    client: &Client,
    post_type: &str,
    id: i64,
    name: &str,
    back: &str,
) -> (Status, String) {
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/{post_type}/{id}/actions/{name}"),
        &[("_token", &csrf), ("_back", back)],
    )
    .await;
    (response.status(), location(&response))
}

/// The list row of the post `id`: from its `<tr>` to its `</tr>`, found by its edit or action
/// links.
fn row_of<'l>(list: &'l str, post_type: &str, id: i64) -> Option<&'l str> {
    let marks = [
        format!("/mc-admin/{post_type}/{id}/edit\""),
        format!("/mc-admin/{post_type}/{id}/actions/"),
    ];
    let at = marks.iter().filter_map(|mark| list.find(mark)).min()?;
    let start = list[..at].rfind("<tr>")?;
    let end = at + list[at..].find("</tr>")?;
    Some(&list[start..end])
}

/// The form of the row action `name` in `row`: from its `<form` to its `</form>`.
fn action_form<'r>(row: &'r str, name: &str) -> Option<&'r str> {
    let at = row.find(&format!("/actions/{name}\""))?;
    let start = row[..at].rfind("<form")?;
    let end = at + row[at..].find("</form>")?;
    Some(&row[start..end])
}

#[test]
fn posts_go_to_the_trash_and_come_back_as_wordpress_does() {
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
        .block_on(scratch_database("trash"));
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
    let editor = user(&db, "edna", "editor").await;
    let contributor = user(&db, "cora", "contributor").await;

    sign_in(&client, &editor).await;
    the_trash_of_a_post(&client, &db).await;
    the_front_page_is_not_trashed_without_manage_options(&client, &db).await;
    sign_out(&client).await;

    sign_in(&client, &contributor).await;
    a_contributor_trashes_only_what_they_may_delete(&client, &db).await;
    sign_out(&client).await;
    drop(client);
    db.close().await;
}

async fn the_trash_of_a_post(client: &Client, db: &Database) {
    let id = new_post(
        client,
        "post",
        &[
            ("title", "Trash me"),
            ("slug", "trash-me"),
            ("status", "publish"),
        ],
    )
    .await;
    assert_eq!(find(db, id).await.unwrap().slug, "trash-me");

    // Out of the trash: Edit and "Trash", which asks for nothing; no Restore, no Delete
    // Permanently, and not Ignite's Delete, which is for the trash only.
    let (_, list) = get(client, "/mc-admin/post").await;
    let row = row_of(&list, "post", id).unwrap_or_else(|| panic!("the row: {list}"));
    assert!(
        row.contains(&format!("href=\"/mc-admin/post/{id}/edit\"")),
        "{row}"
    );
    let trash = action_form(row, "trash").unwrap_or_else(|| panic!("Trash: {row}"));
    assert!(trash.contains(">Trash</button>"), "{trash}");
    assert!(!trash.contains("data-confirm"), "{trash}");
    assert!(action_form(row, "untrash").is_none(), "{row}");
    assert!(action_form(row, "delete").is_none(), "{row}");
    assert!(
        !row.contains(&format!("/mc-admin/post/{id}/delete\"")),
        "{row}"
    );
    // Ignite's Delete refuses a post out of the trash.
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/post/{id}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);

    // Trash: back on the list, with WordPress's message; the slug is freed (`__trashed`, the
    // slug kept in `_wp_desired_post_slug`) and the status it had is kept.
    let (status, to) = row_action(client, "post", id, "trash", "/mc-admin/post?tab=all").await;
    assert_eq!(status, Status::SeeOther);
    assert_eq!(to, "/mc-admin/post?tab=all");
    let (_, list) = get(client, &to).await;
    assert!(list.contains("1 post moved to the Trash."), "{list}");
    assert!(row_of(&list, "post", id).is_none(), "{list}");
    let trashed = find(db, id).await.unwrap();
    assert_eq!(trashed.status, PostStatus::Trash);
    assert_eq!(trashed.slug, "trash-me__trashed");
    assert_eq!(
        get_post_meta(db, id, "_wp_desired_post_slug")
            .await
            .unwrap(),
        Some(json!("trash-me"))
    );
    assert_eq!(
        get_post_meta(db, id, "_wp_trash_meta_status")
            .await
            .unwrap(),
        Some(json!("publish"))
    );
    // It leaves the site.
    assert_eq!(get(client, &format!("/?p={id}")).await.0, Status::NotFound);

    // The Trash tab: Restore and Delete Permanently, asked first with WordPress's text; no Edit
    // and no Trash.
    let (_, list) = get(client, "/mc-admin/post?tab=trash").await;
    let row = row_of(&list, "post", id).unwrap_or_else(|| panic!("the trashed row: {list}"));
    assert!(row.contains("Trash me"), "{row}");
    assert!(
        !row.contains(&format!("/mc-admin/post/{id}/edit\"")),
        "{row}"
    );
    assert!(action_form(row, "trash").is_none(), "{row}");
    let restore = action_form(row, "untrash").unwrap_or_else(|| panic!("Restore: {row}"));
    assert!(restore.contains(">Restore</button>"), "{restore}");
    assert!(!restore.contains("data-confirm"), "{restore}");
    let delete = action_form(row, "delete").unwrap_or_else(|| panic!("Delete: {row}"));
    assert!(delete.contains(">Delete Permanently</button>"), "{delete}");
    assert!(
        delete.contains(
            "data-confirm=\"You are about to permanently delete these items from your site.\n\
             This action cannot be undone.\n"
        ),
        "{delete}"
    );
    assert!(delete.contains("to stop, "), "{delete}");
    assert!(delete.contains("to delete.\""), "{delete}");

    // A trashed post is not edited: `post.php`'s 409 and message, for the form and the save.
    let (status, body) = get(client, &format!("/mc-admin/post/{id}/edit")).await;
    assert_eq!(status, Status::Conflict, "{body}");
    assert!(
        body.contains(
            "You cannot edit this item because it is in the Trash. Please restore it and try \
             again."
        ),
        "{body}"
    );
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/post/{id}"),
        &[("_token", &csrf), ("title", "Edited in the trash")],
    )
    .await;
    assert_eq!(response.status(), Status::Conflict);
    assert_eq!(find(db, id).await.unwrap().title, "Trash me");

    // Its slug is free: a new post takes it as it is.
    let other = new_post(
        client,
        "post",
        &[
            ("title", "Trash me"),
            ("slug", "trash-me"),
            ("status", "publish"),
        ],
    )
    .await;
    assert_eq!(find(db, other).await.unwrap().slug, "trash-me");

    // Restore: a draft (`wp_untrash_post` since WordPress 5.6) with its slug back, made unique
    // because a published post has it now (`wp_unique_post_slug`'s `-2`): two posts never share
    // a slug after a restore.
    let (status, to) = row_action(client, "post", id, "untrash", "/mc-admin/post?tab=trash").await;
    assert_eq!(status, Status::SeeOther);
    let (_, list) = get(client, &to).await;
    assert!(list.contains("1 post restored from the Trash."), "{list}");
    let restored = find(db, id).await.unwrap();
    assert_eq!(restored.status, PostStatus::Draft);
    assert_eq!(restored.slug, "trash-me-2");
    assert_eq!(find(db, other).await.unwrap().slug, "trash-me");
    for key in [
        "_wp_desired_post_slug",
        "_wp_trash_meta_status",
        "_wp_trash_meta_time",
    ] {
        assert_eq!(get_post_meta(db, id, key).await.unwrap(), None, "{key}");
    }
    // Back in All, with Edit and Trash.
    let (_, list) = get(client, "/mc-admin/post").await;
    let row = row_of(&list, "post", id).unwrap_or_else(|| panic!("the restored row: {list}"));
    assert!(
        row.contains(&format!("href=\"/mc-admin/post/{id}/edit\"")),
        "{row}"
    );
    assert!(action_form(row, "trash").is_some(), "{row}");
    // Published again with the taken slug, it stays unique, as `wp_unique_post_slug` makes it.
    let csrf = token(client).await;
    let response = post(
        client,
        &format!("/mc-admin/post/{id}"),
        &[
            ("_token", &csrf),
            ("title", "Trash me"),
            ("slug", "trash-me"),
            ("status", "publish"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(find(db, id).await.unwrap().slug, "trash-me-2");

    // Delete Permanently, from the trash: gone, with WordPress's message.
    let (status, _) = row_action(client, "post", id, "trash", "/mc-admin/post").await;
    assert_eq!(status, Status::SeeOther);
    // Restore and Delete Permanently are refused on a post out of the trash.
    let (status, _) = row_action(client, "post", other, "untrash", "/mc-admin/post").await;
    assert_eq!(status, Status::Forbidden);
    let (status, _) = row_action(client, "post", other, "delete", "/mc-admin/post").await;
    assert_eq!(status, Status::Forbidden);
    assert_eq!(find(db, other).await.unwrap().status, PostStatus::Publish);
    let (status, to) = row_action(client, "post", id, "delete", "/mc-admin/post?tab=trash").await;
    assert_eq!(status, Status::SeeOther);
    let (_, list) = get(client, &to).await;
    assert!(list.contains("1 post permanently deleted."), "{list}");
    assert!(find(db, id).await.is_none());

    // A page's messages say "page", as `$bulk_messages['page']`.
    let page = new_post(
        client,
        "page",
        &[("title", "Old page"), ("status", "publish")],
    )
    .await;
    let (_, to) = row_action(client, "page", page, "trash", "/mc-admin/page").await;
    let (_, list) = get(client, &to).await;
    assert!(list.contains("1 page moved to the Trash."), "{list}");
    let (_, to) = row_action(client, "page", page, "untrash", "/mc-admin/page").await;
    let (_, list) = get(client, &to).await;
    assert!(list.contains("1 page restored from the Trash."), "{list}");
    assert_eq!(find(db, page).await.unwrap().status, PostStatus::Draft);
}

/// `delete_post` on the static front page needs `manage_options` (`map_meta_cap`), which an
/// editor lacks: no Trash on its row, and the action refused.
async fn the_front_page_is_not_trashed_without_manage_options(client: &Client, db: &Database) {
    let front = new_post(client, "page", &[("title", "Home"), ("status", "publish")]).await;
    set_option(db, "page_on_front", &json!(front))
        .await
        .unwrap();
    let (_, list) = get(client, "/mc-admin/page").await;
    let row = row_of(&list, "page", front).unwrap_or_else(|| panic!("the row: {list}"));
    assert!(
        row.contains(&format!("href=\"/mc-admin/page/{front}/edit\"")),
        "{row}"
    );
    assert!(action_form(row, "trash").is_none(), "{row}");
    // The panel refuses an action its row does not show.
    let (status, _) = row_action(client, "page", front, "trash", "/mc-admin/page").await;
    assert_eq!(status, Status::Forbidden);
    assert_eq!(find(db, front).await.unwrap().status, PostStatus::Publish);
    set_option(db, "page_on_front", &json!(0)).await.unwrap();
}

/// A contributor deletes their own unpublished posts (`delete_posts`), not a published one
/// (`delete_published_posts`): Trash shows on their draft only.
async fn a_contributor_trashes_only_what_they_may_delete(client: &Client, db: &Database) {
    let draft = new_post(client, "post", &[("title", "My draft")]).await;
    let published = new_post(client, "post", &[("title", "My published post")]).await;
    let mut post = find(db, published).await.unwrap();
    post.status = PostStatus::Publish;
    post.save(db).await.unwrap();

    let (_, list) = get(client, "/mc-admin/post").await;
    let row = row_of(&list, "post", draft).unwrap_or_else(|| panic!("the draft: {list}"));
    assert!(action_form(row, "trash").is_some(), "{row}");
    // Their published post is no longer theirs to edit or trash: no row actions at all.
    let row = list
        .find("My published post")
        .map(|at| &list[list[..at].rfind("<tr>").unwrap()..at + list[at..].find("</tr>").unwrap()])
        .unwrap_or_else(|| panic!("the published post: {list}"));
    assert!(!row.contains("/actions/"), "{row}");
    assert!(!row.contains("/edit\""), "{row}");
    let (status, _) = row_action(client, "post", published, "trash", "/mc-admin/post").await;
    assert_eq!(status, Status::Forbidden);
    assert_eq!(
        find(db, published).await.unwrap().status,
        PostStatus::Publish
    );

    let (status, _) = row_action(client, "post", draft, "trash", "/mc-admin/post").await;
    assert_eq!(status, Status::SeeOther);
    assert_eq!(find(db, draft).await.unwrap().status, PostStatus::Trash);
    let (status, _) = row_action(client, "post", draft, "untrash", "/mc-admin/post").await;
    assert_eq!(status, Status::SeeOther);
    assert_eq!(find(db, draft).await.unwrap().status, PostStatus::Draft);
}
