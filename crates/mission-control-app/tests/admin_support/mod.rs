//! What the admin's role, user and dashboard tests share: the app on a scratch MySQL or MariaDB
//! database with the installer's migrations, and a browser's requests to it.
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so a flow runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL; otherwise [`run`] says why it is
//! skipped and passes. Ignite reads its database from `DATABASE_URL`, so [`run`] sets it for the
//! process while it holds [`LOCK`].

#![allow(dead_code)]

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_testing::scratch_database;
use orbit_orm::Database;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};

/// Held by every flow: it sets `DATABASE_URL` for the process.
pub static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
pub const PASSWORD: &str = "correct horse battery staple";

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

/// Runs `flow` with the app on a scratch database named after `label`, and a client of it.
pub fn run<F, Fut>(label: &str, flow: F)
where
    F: FnOnce(Client, Arc<Database>) -> Fut,
    Fut: Future<Output = ()>,
{
    run_with(label, AppOptions::default(), flow);
}

/// [`run`], with the app built with `options` (its database is the scratch one).
pub fn run_with<F, Fut>(label: &str, options: AppOptions, flow: F)
where
    F: FnOnce(Client, Arc<Database>) -> Fut,
    Fut: Future<Output = ()>,
{
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
        .block_on(scratch_database(label));
    // SAFETY: no other thread reads or writes the environment now. Every flow of this process
    // holds `LOCK`; the runtime above is gone, and the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    let url = scratch.url().to_owned();
    rocket::async_test(async move {
        run_migrations(&url).await.expect("migrates");
        let db = Arc::new(Database::connect(&url).await.expect("connects"));
        let client = Client::tracked(rocket_with_options(
            config(&url),
            AppOptions {
                database: Some(Arc::clone(&db)),
                ..options
            },
        ))
        .await
        .expect("the app ignites");
        flow(client, Arc::clone(&db)).await;
        db.close().await;
    });
    // SAFETY: as above.
    unsafe { std::env::remove_var("DATABASE_URL") };
    drop(scratch);
}

pub fn form(fields: &[(&str, &str)]) -> String {
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

pub fn token_in(html: &str) -> String {
    let start = html
        .find("name=\"_token\" value=\"")
        .unwrap_or_else(|| panic!("a form token in: {html}"))
        + "name=\"_token\" value=\"".len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
}

pub fn location(response: &LocalResponse<'_>) -> String {
    response
        .headers()
        .get_one("Location")
        .unwrap_or_default()
        .to_owned()
}

pub async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

pub async fn post<'c>(client: &'c Client, uri: &str, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(fields))
        .dispatch()
        .await
}

/// A form token: the sign-in page's, or the dashboard's when signed in.
pub async fn token(client: &Client) -> String {
    let (mut status, mut html) = get(client, "/mc-admin/login").await;
    if status == Status::SeeOther {
        (status, html) = get(client, "/mc-admin").await;
    }
    assert_eq!(status, Status::Ok, "{html}");
    token_in(&html)
}

/// Signs in with `email` and `password`; the sign-in's status (303 when it worked).
pub async fn try_sign_in(client: &Client, email: &str, password: &str) -> Status {
    let csrf = token(client).await;
    post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", password)],
    )
    .await
    .status()
}

pub async fn sign_in(client: &Client, email: &str) {
    assert_eq!(
        try_sign_in(client, email, PASSWORD).await,
        Status::SeeOther,
        "signs {email} in"
    );
}

pub async fn sign_out(client: &Client) {
    let csrf = token_in(&get(client, "/mc-admin").await.1);
    let response = post(client, "/mc-admin/logout", &[("_token", &csrf)]).await;
    assert_eq!(response.status(), Status::SeeOther);
}

/// A user named `name` with the role `role`: `(id, email)`.
pub async fn user(db: &Database, name: &str, role: &str) -> (i64, String) {
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
pub async fn choose_theme(client: &Client, theme: &str) {
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
pub async fn new_post(client: &Client, post_type: &str, fields: &[(&str, &str)]) -> i64 {
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

/// The sidebar of a signed-in page: the `<nav>` or menu markup before the page's content.
pub fn sidebar(html: &str) -> &str {
    let end = html.find("<main").unwrap_or(html.len());
    &html[..end]
}
