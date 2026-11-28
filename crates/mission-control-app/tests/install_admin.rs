//! The administrator the installer creates signs in to the admin: a full run of the wizard on a
//! new database of the test server, then the installed app, with the email and the password typed
//! in the site step.
//!
//! The admin runs on MySQL and MariaDB only (Ruling R119), so the test runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL; otherwise it says why it is skipped
//! and passes. Ignite reads its database from `DATABASE_URL`, so the test sets it for the process.
//! It is the only test of this file, so nothing else reads the environment meanwhile. The
//! install also creates the default category, "Uncategorized", as `wp_install_defaults` does.

use std::path::Path;

use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::wizard::TOKEN_FIELD;
use mission_control_app::restart::RestartHandle;
use mission_control_app::setup::{INSTALL_PATH, Mode, setup_rocket_with_restart};
use mission_control_core::config::Config;
use mission_control_core::content::{Taxonomy, Term, get_option};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::Database;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::json;

/// The administrator's password, typed in the site step.
const ADMIN_PASSWORD: &str = "admin-pw-Sign-In-4402";

/// The administrator's email, which signs in.
const ADMIN_EMAIL: &str = "owner@example.com";

/// `MC_TEST_DATABASE_URL` when it names MySQL or MariaDB.
fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
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

async fn post<'c>(client: &'c Client, uri: &str, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(fields))
        .dispatch()
        .await
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
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

fn location(response: &LocalResponse<'_>) -> String {
    response
        .headers()
        .get_one("Location")
        .unwrap_or_default()
        .to_owned()
}

/// Runs the wizard on the scratch database and returns the configuration of the `.env` it
/// wrote.
async fn install(scratch: &ScratchDatabase, dir: &Path) -> Config {
    let (_, rest) = scratch.url().split_once("://").expect("a server URL");
    let (authority, name) = rest.split_once('/').expect("a database name");
    let (credentials, address) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (user, password) = credentials.split_once(':').unwrap_or((credentials, ""));
    let (host, port) = address.rsplit_once(':').unwrap_or((address, ""));
    let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    let (user, password, name) = (decode(user), decode(password), decode(name));
    let name = name.split('?').next().unwrap_or_default();

    let handle = RestartHandle::new();
    let client = Client::tracked(setup_rocket_with_restart(dir, &handle))
        .await
        .expect("setup mode ignites");
    let token = hidden(&get(&client, INSTALL_PATH).await.1, TOKEN_FIELD);
    let response = post(
        &client,
        &format!("{INSTALL_PATH}?step=1"),
        &[
            (TOKEN_FIELD, &token),
            ("driver", "mariadb"),
            ("dbname", name),
            ("uname", &user),
            ("pwd", &password),
            ("dbhost", host),
            ("dbport", port),
        ],
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
            ("weblog_title", "Signed In"),
            ("user_name", "owner"),
            ("admin_password", ADMIN_PASSWORD),
            ("admin_password2", ADMIN_PASSWORD),
            ("admin_email", ADMIN_EMAIL),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::Ok);
    assert!(handle.requested());
    match Mode::from_lookup(Some(&dir.join(".env")), |_| None) {
        Ok(Mode::Normal(config)) => config,
        other => panic!("the new .env starts normal mode, got {other:?}"),
    }
}

/// Signs in with `password` through Ignite's form and returns the response's status and
/// location.
async fn sign_in(client: &Client, email: &str, password: &str) -> (Status, String) {
    let (status, page) = get(client, "/mc-admin/login").await;
    assert_eq!(status, Status::Ok, "{page}");
    let csrf = hidden(&page, "_token");
    let response = post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", password)],
    )
    .await;
    (response.status(), location(&response))
}

#[test]
fn the_installed_administrator_signs_in() {
    let Some(_) = mysql_test_url() else {
        eprintln!(
            "skipped: the admin runs on MySQL or MariaDB only (Ruling R119); set \
             MC_TEST_DATABASE_URL to a mysql:// URL to run this test"
        );
        return;
    };
    let runtime = || {
        rocket::tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let scratch = runtime().block_on(scratch_database("install_admin"));
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime().block_on(install(&scratch, dir.path()));
    config.themes_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes");
    // SAFETY: this is the only test of the process, and the runtimes above are gone, so no
    // other thread reads or writes the environment now.
    unsafe { std::env::set_var("DATABASE_URL", &config.database_url) };
    let database_url = config.database_url.clone();
    rocket::async_test(async {
        let client = Client::tracked(rocket_with_options(config, AppOptions::default()))
            .await
            .expect("the installed app ignites");
        // A repeated install gets WordPress's Already Installed page, with its Log In link,
        // since the admin is mounted here; not the admin's own pages.
        for uri in [INSTALL_PATH.to_owned(), format!("{INSTALL_PATH}?step=2")] {
            let (status, page) = get(&client, &uri).await;
            assert_eq!(status, Status::Ok, "{uri}: {page}");
            assert!(page.contains("<h1>Already Installed</h1>"), "{page}");
            assert!(
                page.contains(
                    "You appear to have already installed Mission Control. To reinstall please \
                     clear your old database tables first."
                ),
                "{page}"
            );
            assert!(
                page.contains("href=\"/mc-admin/login\">Log In</a>"),
                "{page}"
            );
        }
        let response = post(&client, &format!("{INSTALL_PATH}?step=2"), &[]).await;
        assert_eq!(response.status(), Status::Ok);

        // A wrong password is refused; the one typed in the installer signs in, by email.
        let (status, to) = sign_in(&client, ADMIN_EMAIL, "not-the-password").await;
        assert_ne!(to, "/mc-admin", "{status}");
        let (status, to) = sign_in(&client, ADMIN_EMAIL, ADMIN_PASSWORD).await;
        assert_eq!(status, Status::SeeOther, "signs the administrator in");
        assert_eq!(to, "/mc-admin");
        let (status, page) = get(&client, "/mc-admin").await;
        assert_eq!(status, Status::Ok, "{page}");

        // The default category, as `wp_install_defaults` creates it: "Uncategorized", stored as
        // `default_category`, so the Categories screen lists it without a Delete.
        let db = Database::connect(&database_url).await.expect("connects");
        let uncategorized = Term::find_by_slug(&db, &Taxonomy::CATEGORY, "uncategorized")
            .await
            .unwrap()
            .expect("the installer creates Uncategorized");
        assert_eq!(uncategorized.name, "Uncategorized");
        assert_eq!(uncategorized.parent_id, None);
        assert_eq!(
            get_option(&db, "default_category").await.unwrap(),
            Some(json!(uncategorized.term_id))
        );
        assert_eq!(Term::all(&db, &Taxonomy::CATEGORY).await.unwrap().len(), 1);
        db.close().await;
        let (status, list) = get(&client, "/mc-admin/category").await;
        assert_eq!(status, Status::Ok, "{list}");
        assert!(list.contains("<td>Uncategorized</td>"), "{list}");
        assert!(
            !list.contains(&format!(
                "/mc-admin/category/{}/delete",
                uncategorized.term_id
            )),
            "{list}"
        );
    });
    // SAFETY: as above: the runtime of the test has ended.
    unsafe { std::env::remove_var("DATABASE_URL") };
    drop(scratch);
}
