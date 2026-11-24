//! The installer wizard (Task 11.2), through its HTTP forms: the steps in WordPress's order, a
//! full run on SQLite, the administrator fields' checks, a database that refuses the login, an
//! existing `.env` at the finish, and forms without the token. The server run also checks the
//! administrator account the finish creates; `install_admin.rs` signs it in.
//!
//! The full run uses SQLite, the wizard's default. When `MC_TEST_DATABASE_URL` names a MySQL,
//! MariaDB or PostgreSQL server, the server test also runs the wizard against a new database of
//! its own on that server ([`scratch_database`]): the finish runs Ignite's panel migrations, which
//! must not run in the shared test database. The wrong
//! password test uses the MariaDB test server on 127.0.0.1:3398 with a user that does not exist;
//! when that server is not running, the connection is refused, which the step reports the same
//! way.

use ignite_panel::{authz, users};
use mission_control_app::install::wizard::{SQLITE_FILE_NAME, TOKEN_FIELD};
use mission_control_app::restart::RestartHandle;
use mission_control_app::setup::{INSTALL_PATH, Mode, setup_rocket_with_restart};
use mission_control_core::content::get_option;
use mission_control_testing::{CORE_TABLES, scratch_database, table_exists};
use orbit_orm::{Database, raw};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::uri::Host;
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// The administrator password the tests type. It must never show up in a page or a file.
const ADMIN_PASSWORD: &str = "admin-pw-Do-Not-Store-7731";

/// A wizard on a new install folder, with its restart handle.
async fn wizard() -> (tempfile::TempDir, RestartHandle, Client) {
    let dir = tempfile::tempdir().expect("creates a folder");
    let handle = RestartHandle::new();
    let client = Client::tracked(setup_rocket_with_restart(dir.path(), &handle))
        .await
        .expect("setup mode ignites");
    (dir, handle, client)
}

async fn body(response: LocalResponse<'_>) -> String {
    response.into_string().await.unwrap_or_default()
}

/// The value of the hidden field `name` in a wizard page.
fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page
        .find(&marker)
        .unwrap_or_else(|| panic!("the page has a {name} field:\n{page}"))
        + marker.len();
    let end = start + page[start..].find('"').expect("the value ends");
    page[start..end].to_owned()
}

/// The form token in a wizard page.
fn token(page: &str) -> String {
    hidden(page, TOKEN_FIELD)
}

/// The database choice id on the site form, as the owner's browser has it.
async fn choice(client: &Client) -> String {
    let page = body(
        client
            .get(format!("{INSTALL_PATH}?step=2"))
            .dispatch()
            .await,
    )
    .await;
    hidden(&page, "database_choice")
}

/// `fields` as a form body.
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

/// The wizard's token, from its first page.
async fn first_token(client: &Client) -> String {
    let page = body(client.get(INSTALL_PATH).dispatch().await).await;
    token(&page)
}

async fn post<'c>(client: &'c Client, step: u8, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    client
        .post(format!("{INSTALL_PATH}?step={step}"))
        .header(ContentType::Form)
        .body(form(fields))
        .dispatch()
        .await
}

/// Posts the SQLite choice and checks the wizard moves to step 2.
async fn choose_sqlite(client: &Client, token: &str) {
    let response = post(client, 1, &[(TOKEN_FIELD, token), ("driver", "sqlite")]).await;
    assert_eq!(
        response.status(),
        Status::SeeOther,
        "{}",
        body(response).await
    );
    assert_eq!(
        response.headers().get_one("Location"),
        Some("/mc-admin/install.php?step=2")
    );
}

/// The site form with the test's values.
fn site_fields<'a>(token: &'a str, choice: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        (TOKEN_FIELD, token),
        ("database_choice", choice),
        ("weblog_title", "Mission <Control> & Co"),
        ("user_name", "admin"),
        ("admin_password", ADMIN_PASSWORD),
        ("admin_password2", ADMIN_PASSWORD),
        ("admin_email", "owner@example.com"),
        ("blog_public", "0"),
    ]
}

/// Posts the site form, for the database choice `choice`, with the request's host set to
/// `host`.
async fn finish<'c>(
    client: &'c Client,
    token: &str,
    choice: &str,
    host: &'static str,
) -> LocalResponse<'c> {
    let mut request = client
        .post(format!("{INSTALL_PATH}?step=2"))
        .header(ContentType::Form)
        .body(form(&site_fields(token, choice)));
    request
        .inner_mut()
        .set_host(Host::parse(host).expect("a valid host"));
    request.dispatch().await
}

/// Where `needle` is in `page`.
fn at(page: &str, needle: &str) -> usize {
    page.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` is on the page:\n{page}"))
}

#[rocket::async_test]
async fn steps_render_in_order() {
    let (_dir, _handle, client) = wizard().await;

    // Step 1: the database, SQLite first and selected, with WordPress's labels.
    let response = client.get(INSTALL_PATH).dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let page = body(response).await;
    assert!(
        page.contains("<li class=\"current\">1. Database</li>"),
        "{page}"
    );
    assert!(
        page.contains("action=\"/mc-admin/install.php?step=1\""),
        "{page}"
    );
    let sqlite = at(&page, "<option value=\"sqlite\" selected>");
    assert!(sqlite < at(&page, "<option value=\"mysql\">"));
    assert!(at(&page, "<option value=\"mysql\">") < at(&page, "<option value=\"mariadb\">"));
    assert!(at(&page, "<option value=\"mariadb\">") < at(&page, "<option value=\"postgres\">"));
    for label in [
        "Database Name",
        "Username",
        "Password",
        "Database Host",
        "Port",
    ] {
        assert!(page.contains(&format!(">{label}</label>")), "{label}");
    }
    let token = token(&page);
    assert!(!token.is_empty());

    // Step 2 is not reachable before a database is chosen.
    let response = client
        .get(format!("{INSTALL_PATH}?step=2"))
        .dispatch()
        .await;
    assert!(response.status().class().is_redirection());
    assert_eq!(response.headers().get_one("Location"), Some(INSTALL_PATH));

    // Step 2: site title, the administrator, then search engine visibility, as WordPress's
    // install.php orders them. On SQLite the account is not created, and the form says so.
    choose_sqlite(&client, &token).await;
    let response = client
        .get(format!("{INSTALL_PATH}?step=2"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let page = body(response).await;
    assert!(page.contains("<li class=\"current\">2. Site and administrator</li>"));
    assert!(page.contains("action=\"/mc-admin/install.php?step=2\""));
    let order = [
        ">Site Title</label>",
        ">Username</label>",
        ">Password</label>",
        ">Repeat Password</label>",
        ">Your Email</label>",
        "Search engine visibility",
        "Discourage search engines from indexing this site",
        "The administrator account is not created on this database.",
        "Install Mission Control",
    ];
    for pair in order.windows(2) {
        assert!(at(&page, pair[0]) < at(&page, pair[1]), "{pair:?}");
    }
    assert!(page.contains("SQLite"));
}

#[rocket::async_test]
async fn full_run_on_sqlite_installs_and_restarts() {
    let (dir, handle, client) = wizard().await;
    let token = first_token(&client).await;
    choose_sqlite(&client, &token).await;
    let sqlite = dir.path().join(SQLITE_FILE_NAME);
    assert!(
        sqlite.is_file(),
        "the database step creates the SQLite file"
    );
    assert!(
        !dir.path().join(".env").exists(),
        "nothing is written before the finish"
    );
    assert!(!handle.requested());

    let choice = choice(&client).await;
    let response = finish(&client, &token, &choice, "example.test:8080").await;
    assert_eq!(response.status(), Status::Ok);
    let page = body(response).await;
    assert!(page.contains("Success!"), "{page}");
    assert!(page.contains("Database: SQLite"), "{page}");
    assert!(page.contains("href=\"http://example.test:8080\""), "{page}");
    assert!(page.contains("The administrator account is not created on this database."));
    assert!(
        !page.contains("Log In"),
        "no account to sign in with: {page}"
    );
    assert!(!page.contains(ADMIN_PASSWORD));
    assert!(handle.requested(), "the finish asks for the restart");

    // `.env`: owner-only, the SQLite URL and a generated key, no administrator password.
    let env_path = dir.path().join(".env");
    let env = std::fs::read_to_string(&env_path).expect(".env is written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let mode = std::fs::metadata(&sqlite).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(!env.contains(ADMIN_PASSWORD));
    let config = match Mode::from_lookup(Some(&env_path), |_| None) {
        Ok(Mode::Normal(config)) => config,
        other => panic!("the new .env starts normal mode, got {other:?}"),
    };
    assert_eq!(
        config.database_url,
        format!("sqlite://{}", sqlite.display())
    );
    assert_eq!(config.secret_key.len(), 44, "32 random bytes in base64");

    // The migrations ran and the options are stored, on the database `.env` names.
    let db = Database::connect(&config.database_url)
        .await
        .expect("opens the installed database");
    check_tables(&db).await;
    check_options(&db, "http://example.test:8080").await;
    db.close().await;

    // The wizard is done: a visit shows the finished page, and the forms are refused.
    let page = body(client.get(INSTALL_PATH).dispatch().await).await;
    assert!(page.contains("Success!"));
    let response = post(&client, 1, &[(TOKEN_FIELD, &token), ("driver", "sqlite")]).await;
    assert_eq!(response.status(), Status::Conflict);
    let response = finish(&client, &token, &choice, "example.test:8080").await;
    assert_eq!(response.status(), Status::Conflict);
}

#[rocket::async_test]
async fn a_new_database_choice_refuses_the_older_site_form() {
    let (dir, handle, client) = wizard().await;
    let token = first_token(&client).await;
    choose_sqlite(&client, &token).await;
    let owners_choice = choice(&client).await;

    // Another visitor submits step 1 between the owner's steps.
    choose_sqlite(&client, &token).await;
    let response = finish(&client, &token, &owners_choice, "example.test").await;
    assert_eq!(response.status(), Status::Conflict);
    let page = body(response).await;
    assert!(page.contains("The database was changed"), "{page}");
    assert!(!dir.path().join(".env").exists());
    assert!(!handle.requested());

    // A form without the choice id is refused the same way.
    let mut fields = site_fields(&token, "");
    fields.retain(|(name, _)| *name != "database_choice");
    let response = post(&client, 2, &fields).await;
    assert_eq!(response.status(), Status::Conflict);

    // The site form opened after the new choice installs.
    let current = choice(&client).await;
    assert_ne!(current, owners_choice);
    let response = finish(&client, &token, &current, "example.test").await;
    assert_eq!(response.status(), Status::Ok);
    assert!(handle.requested());
}

/// The tables of an installed site: the panel's (Ignite's `users` and roles) and the core ones.
async fn check_tables(db: &Database) {
    for table in ["users", "roles", "permissions", "role_user", "sessions"]
        .iter()
        .chain(CORE_TABLES)
    {
        assert!(table_exists(db, table).await, "{table}");
    }
}

/// The options WordPress's installer stores, as the full runs set them.
async fn check_options(db: &Database, address: &str) {
    let option = async |name: &str| get_option(db, name).await.expect("reads an option");
    assert_eq!(
        option("blogname").await,
        Some(json!("Mission &lt;Control&gt; &amp; Co")),
        "sanitized as WordPress's sanitize_option escapes it"
    );
    assert_eq!(option("blog_public").await, Some(json!(0)));
    assert_eq!(option("siteurl").await, Some(json!(address)));
    assert_eq!(option("home").await, Some(json!(address)));
    assert_eq!(
        option("admin_email").await,
        Some(json!("owner@example.com"))
    );
    assert_eq!(option("posts_per_page").await, Some(json!(10)));
    assert_eq!(option("show_on_front").await, Some(json!("posts")));
    assert_eq!(option("permalink_structure").await, Some(json!("")));
    // No option holds the username or the password.
    for name in ["admin_user", "admin_password", "user_login"] {
        assert_eq!(option(name).await, None::<Value>);
    }
}

/// Posts the site form with `fields` in place of the test's values, and checks it comes back
/// refused with `message`, the username and email typed kept, and neither password.
async fn refused_site_form(client: &Client, token: &str, fields: &[(&str, &str)], message: &str) {
    let choice = choice(client).await;
    let mut form = site_fields(token, &choice);
    for (name, value) in fields {
        match form.iter_mut().find(|(n, _)| n == name) {
            Some(field) => field.1 = value,
            None => form.push((name, value)),
        }
    }
    let response = post(client, 2, &form).await;
    assert_eq!(response.status(), Status::UnprocessableEntity, "{fields:?}");
    let page = body(response).await;
    assert!(page.contains(message), "`{message}` in {page}");
    assert!(page.contains("<li class=\"current\">2. Site and administrator</li>"));
    let typed = |name: &str| form.iter().find(|(n, _)| *n == name).unwrap().1;
    assert!(
        page.contains(&format!("value=\"{}\"", typed("user_name"))),
        "the username is kept: {page}"
    );
    assert!(
        page.contains(&format!("value=\"{}\"", typed("admin_email"))),
        "the email is kept: {page}"
    );
    for name in ["admin_password", "admin_password2"] {
        let password = typed(name);
        assert!(
            password.is_empty() || !page.contains(password),
            "{name} is not echoed"
        );
    }
    assert!(page.contains("name=\"admin_password\" id=\"pass1\" type=\"password\" value=\"\""));
}

#[rocket::async_test]
async fn the_site_step_checks_the_administrator() {
    let (dir, handle, client) = wizard().await;
    let token = first_token(&client).await;
    choose_sqlite(&client, &token).await;

    // A missing password, and both fields missing.
    refused_site_form(
        &client,
        &token,
        &[("admin_password", ""), ("admin_password2", "")],
        "Please enter a password.",
    )
    .await;
    // A confirmation that is not the password.
    refused_site_form(
        &client,
        &token,
        &[("admin_password2", "admin-pw-Do-Not-Store-7732")],
        "Your passwords do not match. Please try again.",
    )
    .await;
    // Shorter than Ignite's minimum.
    refused_site_form(
        &client,
        &token,
        &[("admin_password", "short7"), ("admin_password2", "short7")],
        "The password must be at least 8 characters.",
    )
    .await;
    // An email that is not one.
    refused_site_form(
        &client,
        &token,
        &[("admin_email", "owner-at-example")],
        "Sorry, that is not a valid email address.",
    )
    .await;
    // A missing username.
    refused_site_form(
        &client,
        &token,
        &[("user_name", "")],
        "Please provide a valid username.",
    )
    .await;
    // A username sanitize_user would change.
    refused_site_form(
        &client,
        &token,
        &[("user_name", "ad/min")],
        "The username you provided has invalid characters.",
    )
    .await;
    // A missing email.
    refused_site_form(
        &client,
        &token,
        &[("admin_email", "")],
        "You must provide an email address.",
    )
    .await;

    // Nothing was installed, and the corrected form installs.
    assert!(!dir.path().join(".env").exists());
    assert!(!handle.requested());
    let choice = choice(&client).await;
    let response = finish(&client, &token, &choice, "example.test").await;
    assert_eq!(response.status(), Status::Ok);
    assert!(handle.requested());
}

#[rocket::async_test]
async fn wrong_mysql_password_stays_on_the_database_step() {
    // The MariaDB test server must be up, so the test reaches the access-denied path rather
    // than a refused connection.
    let reachable = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:3398".parse().unwrap(),
        std::time::Duration::from_secs(2),
    )
    .is_ok();
    if !reachable {
        eprintln!(
            "SKIPPED wrong_mysql_password_stays_on_the_database_step: no MariaDB test server \
             on 127.0.0.1:3398"
        );
        return;
    }
    let (dir, _handle, client) = wizard().await;
    let token = first_token(&client).await;
    let password = "wrong-Db-Password-4471";
    let response = post(
        &client,
        1,
        &[
            (TOKEN_FIELD, &token),
            ("driver", "mysql"),
            ("dbname", "test"),
            ("uname", "mc_wizard_nobody"),
            ("pwd", password),
            ("dbhost", "127.0.0.1"),
            ("dbport", "3398"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let page = body(response).await;
    assert!(
        page.contains("Error establishing a database connection"),
        "{page}"
    );
    assert!(
        page.contains("refused the username and password you entered"),
        "the access-denied path: {page}"
    );
    assert!(page.contains("Are you sure you have the correct username and password?"));
    assert!(page.contains("<li class=\"current\">1. Database</li>"));
    assert!(!page.contains(password), "the password is never echoed");
    // The other fields come back, escaped, with MySQL still chosen.
    assert!(page.contains("value=\"mc_wizard_nobody\""));
    assert!(page.contains("<option value=\"mysql\" selected>"));

    // The wizard did not move on.
    let response = client
        .get(format!("{INSTALL_PATH}?step=2"))
        .dispatch()
        .await;
    assert!(response.status().class().is_redirection());
    assert!(!dir.path().join(".env").exists());
}

#[rocket::async_test]
async fn echoed_values_are_escaped() {
    let (_dir, _handle, client) = wizard().await;
    let token = first_token(&client).await;
    let response = post(
        &client,
        1,
        &[
            (TOKEN_FIELD, &token),
            ("driver", "postgres"),
            ("dbname", "\"><script>x</script>"),
            ("dbhost", "bad host<b>"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let page = body(response).await;
    assert!(!page.contains("<script>x"), "{page}");
    assert!(
        page.contains("&quot;&gt;&lt;script&gt;x&lt;/script&gt;"),
        "{page}"
    );
    assert!(!page.contains("<b>"));

    // A site form with a bad email comes back with the error and escaped values, never the
    // password.
    choose_sqlite(&client, &token).await;
    let choice = choice(&client).await;
    let response = post(
        &client,
        2,
        &[
            (TOKEN_FIELD, &token),
            ("database_choice", &choice),
            ("weblog_title", "<i>Title</i>"),
            ("admin_password", ADMIN_PASSWORD),
            ("admin_email", "not-an-email"),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let page = body(response).await;
    assert!(
        page.contains("Sorry, that is not a valid email address."),
        "{page}"
    );
    assert!(
        page.contains("value=\"&lt;i&gt;Title&lt;/i&gt;\""),
        "{page}"
    );
    assert!(!page.contains(ADMIN_PASSWORD));
}

#[rocket::async_test]
async fn existing_env_at_finish_is_refused() {
    let (dir, handle, client) = wizard().await;
    let token = first_token(&client).await;
    choose_sqlite(&client, &token).await;
    // Someone configures the site by hand between the steps.
    let env_path = dir.path().join(".env");
    std::fs::write(&env_path, "KEEP=me\n").unwrap();

    let choice = choice(&client).await;
    let response = finish(&client, &token, &choice, "example.test").await;
    assert_eq!(response.status(), Status::Conflict);
    let page = body(response).await;
    assert!(
        page.contains("already exists, so Mission Control did not change it"),
        "{page}"
    );
    assert_eq!(std::fs::read_to_string(&env_path).unwrap(), "KEEP=me\n");
    assert!(!handle.requested());
    // Nothing was installed.
    let url = format!("sqlite://{}", dir.path().join(SQLITE_FILE_NAME).display());
    let db = Database::connect(&url).await.unwrap();
    assert!(!table_exists(&db, "options").await);
    db.close().await;
}

#[rocket::async_test]
async fn posts_without_the_token_are_refused() {
    let (dir, handle, client) = wizard().await;
    let token = first_token(&client).await;

    for fields in [
        vec![("driver", "sqlite")],
        vec![(TOKEN_FIELD, "not-the-token"), ("driver", "sqlite")],
        vec![(TOKEN_FIELD, ""), ("driver", "sqlite")],
    ] {
        let response = post(&client, 1, &fields).await;
        assert_eq!(response.status(), Status::Forbidden);
        assert!(
            body(response)
                .await
                .contains("The link you followed has expired")
        );
    }
    assert!(!dir.path().join(SQLITE_FILE_NAME).exists());

    // Step 2 too, even with a database chosen.
    choose_sqlite(&client, &token).await;
    let choice = choice(&client).await;
    let mut fields = site_fields(&token, &choice);
    fields.remove(0);
    let response = post(&client, 2, &fields).await;
    assert_eq!(response.status(), Status::Forbidden);
    assert!(!dir.path().join(".env").exists());
    assert!(!handle.requested());
}

/// The parts of a server URL such as `mysql://user:password@host:port/name`.
struct ServerUrl {
    driver: &'static str,
    user: String,
    password: String,
    host: String,
    port: String,
    name: String,
}

fn parse_server_url(url: &str) -> Option<ServerUrl> {
    let (scheme, rest) = url.split_once("://")?;
    let driver = match scheme {
        "mysql" | "mariadb" => "mysql",
        "postgres" | "postgresql" => "postgres",
        _ => return None,
    };
    let (authority, name) = rest.split_once('/')?;
    let name = name.split('?').next().unwrap_or_default();
    let (credentials, address) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (user, password) = credentials.split_once(':').unwrap_or((credentials, ""));
    let (host, port) = address.rsplit_once(':').unwrap_or((address, ""));
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    Some(ServerUrl {
        driver,
        user: decode(user),
        password: decode(password),
        host: host.to_owned(),
        port: port.to_owned(),
        name: decode(name),
    })
}

#[rocket::async_test]
async fn full_run_on_the_test_server() {
    // A new, empty database on the test server: no `users` table, stand-in or other. The finish
    // creates it with the panel's migrations.
    let scratch = scratch_database("wizard").await;
    if !scratch.is_server() {
        return; // SQLite: covered by the full run above.
    }
    let server = parse_server_url(scratch.url()).expect("a server URL");

    let (dir, handle, client) = wizard().await;
    let token = first_token(&client).await;
    // SQLite first: the empty file it creates is removed when the owner switches to the server.
    choose_sqlite(&client, &token).await;
    assert!(dir.path().join(SQLITE_FILE_NAME).exists());
    let response = post(
        &client,
        1,
        &[
            (TOKEN_FIELD, &token),
            ("driver", server.driver),
            ("dbname", &server.name),
            ("uname", &server.user),
            ("pwd", &server.password),
            ("dbhost", &server.host),
            ("dbport", &server.port),
        ],
    )
    .await;
    assert_eq!(
        response.status(),
        Status::SeeOther,
        "{}",
        body(response).await
    );
    assert!(!dir.path().join(SQLITE_FILE_NAME).exists());

    let choice = choice(&client).await;
    let page = body(
        client
            .get(format!("{INSTALL_PATH}?step=2"))
            .dispatch()
            .await,
    )
    .await;
    let creates_account = server.driver == "mysql";
    assert_eq!(
        !page.contains("The administrator account is not created on this database."),
        creates_account,
        "{page}"
    );
    let response = finish(&client, &token, &choice, "site.example").await;
    assert_eq!(response.status(), Status::Ok);
    let page = body(response).await;
    assert!(
        page.contains(&format!("the database {}", server.name)),
        "{page}"
    );
    assert!(server.password.is_empty() || !page.contains(&server.password));
    assert!(!page.contains(ADMIN_PASSWORD));
    if creates_account {
        // WordPress's success page: the username, and the sign-in link.
        assert!(page.contains("<td>admin</td>"), "{page}");
        assert!(page.contains("<em>Your chosen password.</em>"), "{page}");
        assert!(page.contains("href=\"/mc-admin/login\""), "{page}");
    }
    assert!(handle.requested());
    let env_path = dir.path().join(".env");
    let config = match Mode::from_lookup(Some(&env_path), |_| None) {
        Ok(Mode::Normal(config)) => config,
        other => panic!("the new .env starts normal mode, got {other:?}"),
    };
    assert!(
        config
            .database_url
            .starts_with(&format!("{}://", server.driver))
    );
    let env = std::fs::read_to_string(&env_path).unwrap();
    assert!(!env.contains(ADMIN_PASSWORD));
    let db = Database::connect(scratch.url()).await.expect("connects");
    check_tables(&db).await;
    check_options(&db, "http://site.example").await;
    if creates_account {
        check_administrator(&db).await;
    } else {
        assert_eq!(
            user_count(&db).await,
            0,
            "PostgreSQL: the account is not created"
        );
    }

    // A second install on the same database is refused, and makes no second user.
    let (other_dir, other_handle, other) = wizard().await;
    let other_token = first_token(&other).await;
    let response = post(
        &other,
        1,
        &[
            (TOKEN_FIELD, &other_token),
            ("driver", server.driver),
            ("dbname", &server.name),
            ("uname", &server.user),
            ("pwd", &server.password),
            ("dbhost", &server.host),
            ("dbport", &server.port),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    assert!(body(response).await.contains("Already installed"));
    assert!(!other_dir.path().join(".env").exists());
    assert!(!other_handle.requested());
    // The first wizard refuses its own form again too.
    let response = finish(&client, &token, &choice, "site.example").await;
    assert_eq!(response.status(), Status::Conflict);
    assert_eq!(
        user_count(&db).await,
        u64::from(creates_account),
        "no second user"
    );
    db.close().await;
}

async fn user_count(db: &Database) -> u64 {
    let count: i64 = raw::fetch_scalar(db, "SELECT COUNT(*) FROM users", &[])
        .await
        .expect("counts the users");
    u64::try_from(count).unwrap()
}

/// The account the finish creates: one user, named with the username, with the email, the
/// password hashed by Ignite, and the `administrator` role; and the five WordPress roles.
async fn check_administrator(db: &Database) {
    assert_eq!(user_count(db).await, 1);
    let user = users::find_by_email(db, "owner@example.com")
        .await
        .expect("reads the user")
        .expect("the administrator exists");
    assert_eq!(user.name, "admin");
    assert_ne!(user.password_hash, ADMIN_PASSWORD);
    assert!(
        ignite_panel::ignite_core::verify_password(&user.password_hash, ADMIN_PASSWORD),
        "the hash is of the password typed"
    );
    assert_eq!(
        authz::user_roles(db, "owner@example.com").await.unwrap(),
        vec!["administrator".to_owned()]
    );
    for role in mission_control_core::users::WORDPRESS_ROLES {
        assert!(
            matches!(
                authz::create_role(db, role).await,
                Err(authz::AuthzError::Exists(_))
            ),
            "the role {role} exists"
        );
    }
}
