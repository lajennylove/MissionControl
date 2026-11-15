//! Setup mode (Task 11.1): the binary's choice between setup mode and normal mode, and the
//! setup-mode app, which serves only the installer and opens no database.
//!
//! The normal-mode test runs on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite
//! file when it is unset.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mission_control::rocket_with_database;
use mission_control_app::setup::{INSTALL_PATH, Mode, setup_rocket};
use mission_control_core::config::{ConfigError, Environment};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database};
use orbit_orm::Database;
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::Client;

/// No environment variable is set.
fn no_env(_: &str) -> Option<String> {
    None
}

/// A lookup that answers from `vars`, as the process environment would.
fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

/// A `.env` with `contents` in a new temporary folder (kept alive by the returned guard).
fn env_file(contents: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("creates a folder");
    let path = dir.path().join(".env");
    std::fs::write(&path, contents).expect("writes .env");
    (dir, path)
}

/// The shipped themes: `MissionControl/themes`.
fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

#[test]
fn no_configuration_is_setup_mode() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join(".env");
    assert_eq!(Mode::from_lookup(Some(&missing), no_env), Ok(Mode::Setup));
    assert_eq!(Mode::from_lookup(None, no_env), Ok(Mode::Setup));
}

#[test]
fn no_env_file_and_incomplete_environment_is_setup_mode() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join(".env");
    let only_key = env(&[("IGNITE_SECRET_KEY", "k")]);
    assert_eq!(Mode::from_lookup(Some(&missing), only_key), Ok(Mode::Setup));
    let only_url = env(&[("DATABASE_URL", "sqlite://site.db")]);
    assert_eq!(Mode::from_lookup(Some(&missing), only_url), Ok(Mode::Setup));
    // Blank values count as missing.
    let blank = env(&[("DATABASE_URL", " "), ("IGNITE_SECRET_KEY", "")]);
    assert_eq!(Mode::from_lookup(Some(&missing), blank), Ok(Mode::Setup));
}

#[test]
fn incomplete_env_file_is_an_error_naming_the_keys() {
    // The installer never writes over an existing `.env`, so it is not setup mode.
    let (_dir, path) = env_file("DATABASE_URL=sqlite://site.db\n");
    let error = Mode::from_lookup(Some(&path), no_env).unwrap_err();
    assert_eq!(
        error,
        ConfigError::Incomplete {
            path: path.clone(),
            missing: vec!["IGNITE_SECRET_KEY"],
        }
    );
    let message = error.to_string();
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("IGNITE_SECRET_KEY"), "{message}");
    assert!(!message.contains("sqlite://site.db"), "{message}");

    // Both missing, one of them blank in the file.
    let (_dir, path) = env_file("MC_DEBUG=true\nIGNITE_SECRET_KEY=\n");
    assert_eq!(
        Mode::from_lookup(Some(&path), no_env),
        Err(ConfigError::Incomplete {
            path: path.clone(),
            missing: vec!["DATABASE_URL", "IGNITE_SECRET_KEY"],
        })
    );

    // An empty `.env` is still a file the installer would not write over.
    let (_dir, path) = env_file("");
    assert!(matches!(
        Mode::from_lookup(Some(&path), no_env),
        Err(ConfigError::Incomplete { .. })
    ));
}

#[test]
fn complete_configuration_from_env_file_is_normal_mode() {
    let (_dir, path) =
        env_file("DATABASE_URL=sqlite://site.db\nIGNITE_SECRET_KEY=secret\nMC_ENV=staging\n");
    match Mode::from_lookup(Some(&path), no_env) {
        Ok(Mode::Normal(config)) => {
            assert_eq!(config.database_url, "sqlite://site.db");
            assert_eq!(config.secret_key, "secret");
            assert_eq!(config.environment, Environment::Staging);
        }
        other => panic!("expected normal mode, got {other:?}"),
    }
}

#[test]
fn complete_configuration_from_environment_alone_is_normal_mode() {
    // A container: no `.env`, the variables come from the environment.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join(".env");
    let lookup = env(&[
        ("DATABASE_URL", "postgres://db/site"),
        ("IGNITE_SECRET_KEY", "secret"),
    ]);
    match Mode::from_lookup(Some(&missing), lookup) {
        Ok(Mode::Normal(config)) => {
            assert_eq!(config.database_url, "postgres://db/site");
            assert_eq!(config.secret_key, "secret");
        }
        other => panic!("expected normal mode, got {other:?}"),
    }
}

#[test]
fn environment_completes_a_partial_env_file() {
    let (_dir, path) = env_file("DATABASE_URL=sqlite://site.db\n");
    let lookup = env(&[("IGNITE_SECRET_KEY", "secret")]);
    assert!(matches!(
        Mode::from_lookup(Some(&path), lookup),
        Ok(Mode::Normal(_))
    ));
}

#[cfg(unix)]
#[test]
fn dangling_env_symlink_is_incomplete() {
    // A link at `.env`, even a broken one, is a file the installer would not write over.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::os::unix::fs::symlink(dir.path().join("nowhere.env"), &path).unwrap();
    assert!(matches!(
        Mode::from_lookup(Some(&path), no_env),
        Err(ConfigError::Incomplete { .. })
    ));
}

#[cfg(unix)]
#[test]
fn env_symlink_to_complete_file_is_normal_mode() {
    let (dir, target) = env_file("DATABASE_URL=sqlite://site.db\nIGNITE_SECRET_KEY=secret\n");
    let link = dir.path().join("link.env");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        Mode::from_lookup(Some(&link), no_env),
        Ok(Mode::Normal(_))
    ));
}

#[test]
fn env_file_syntax_error_is_an_error_not_setup_mode() {
    // Nothing is configured, but the file cannot be read: the owner must fix it.
    let (_dir, path) = env_file("MC_DEBUG=\"unclosed\n");
    assert!(matches!(
        Mode::from_lookup(Some(&path), no_env),
        Err(ConfigError::EnvSyntax { .. })
    ));
}

#[test]
fn unreadable_env_file_is_an_error_not_setup_mode() {
    // A folder named `.env` exists but cannot be read as a file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(
        Mode::from_lookup(Some(&path), no_env),
        Err(ConfigError::EnvUnreadable { .. })
    ));
}

#[test]
fn invalid_value_is_an_error_even_when_not_configured() {
    let lookup = env(&[("MC_UPLOAD_MAX_BYTES", "lots")]);
    assert!(matches!(
        Mode::from_lookup(None, lookup),
        Err(ConfigError::Invalid {
            name: "MC_UPLOAD_MAX_BYTES",
            ..
        })
    ));
}

/// A client for the setup-mode app, with its preflight checks on `install_dir`.
async fn setup_client(install_dir: &Path) -> Client {
    Client::tracked(setup_rocket(install_dir))
        .await
        .expect("the setup app ignites")
}

#[rocket::async_test]
async fn setup_mode_serves_only_the_installer() {
    let dir = tempfile::tempdir().unwrap();
    let client = setup_client(dir.path()).await;

    let page = client.get(INSTALL_PATH).dispatch().await;
    assert_eq!(page.status(), Status::Ok);
    assert_eq!(page.content_type(), Some(ContentType::HTML));
    let body = page.into_string().await.unwrap();
    assert!(body.contains("not configured yet"), "{body}");

    for path in [
        "/",
        "/hello-world/",
        "/mc-json/",
        "/mc-json/mc/v2/posts",
        "/?rest_route=/",
        "/mc-login.php",
        "/mc-admin/",
        "/mc-content/uploads/2026/01/a.jpg",
    ] {
        let response = client.get(path).dispatch().await;
        assert_eq!(response.status(), Status::Found, "GET {path}");
        assert_eq!(
            response.headers().get_one("Location"),
            Some(INSTALL_PATH),
            "GET {path}"
        );
    }
    for response in [
        client.post("/mc-json/mc/v2/posts").dispatch().await,
        client.put("/mc-json/mc/v2/posts/1").dispatch().await,
        client.delete("/mc-json/mc/v2/posts/1").dispatch().await,
    ] {
        assert_eq!(response.status(), Status::Found);
        assert_eq!(response.headers().get_one("Location"), Some(INSTALL_PATH));
    }
}

#[rocket::async_test]
async fn setup_mode_opens_no_database() {
    // The guarantee is structural: `setup_rocket` takes no configuration and has no database
    // code path at all, so there is no URL it could dial. This test pins that down: the setup
    // app ignites and serves without any database state.
    // Even a DATABASE_URL pointing nowhere is never dialled: without the secret key the site is
    // not configured, and the setup app has no database at all.
    let lookup = env(&[("DATABASE_URL", "postgres://nobody@127.0.0.1:1/nowhere")]);
    assert_eq!(Mode::from_lookup(None, lookup), Ok(Mode::Setup));

    let dir = tempfile::tempdir().unwrap();
    let client = setup_client(dir.path()).await;
    assert!(client.rocket().state::<Arc<Database>>().is_none());
    assert!(client.rocket().state::<Database>().is_none());
    assert_eq!(
        client.get(INSTALL_PATH).dispatch().await.status(),
        Status::Ok
    );
}

#[rocket::async_test]
async fn setup_page_shows_preflight_issues_escaped() {
    // The install folder is a file whose name holds markup: the preflight issue names it, and
    // the page escapes it.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("<b>site & \"co\"");
    std::fs::write(&file, "").unwrap();
    let client = setup_client(&file).await;

    let body = client
        .get(INSTALL_PATH)
        .dispatch()
        .await
        .into_string()
        .await
        .unwrap();
    assert!(body.contains("is a file, not a folder"), "{body}");
    assert!(
        body.contains("&lt;b&gt;site &amp; &quot;co&quot;"),
        "{body}"
    );
    assert!(!body.contains("<b>site"), "{body}");
}

#[rocket::async_test]
async fn normal_mode_does_not_mount_the_installer() {
    let test = database().await;
    create_users_table(test.db()).await;
    migrator()
        .unwrap()
        .run(test.db())
        .await
        .expect("runs the migrations");

    let (_dir, path) = env_file("DATABASE_URL=unused\nIGNITE_SECRET_KEY=secret\n");
    let themes = shipped_themes();
    let lookup = env(&[("MC_THEMES_DIR", themes.to_str().unwrap())]);
    let Ok(Mode::Normal(config)) = Mode::from_lookup(Some(&path), lookup) else {
        panic!("expected normal mode");
    };

    let client = Client::tracked(rocket_with_database(config, test.shared()))
        .await
        .expect("the app ignites");
    // The installer is not mounted: the path goes to the front end, which has no such page.
    let response = client.get(INSTALL_PATH).dispatch().await;
    assert_eq!(response.status(), Status::NotFound);
    // And the site is not redirected to it.
    let home = client.get("/").dispatch().await;
    assert_ne!(home.status(), Status::Found);
    assert!(client.rocket().state::<Arc<Database>>().is_some());
}
