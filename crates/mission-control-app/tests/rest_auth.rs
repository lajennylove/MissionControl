//! REST authentication (Task 10.5): application passwords by HTTP Basic auth, as WordPress's
//! `wp_validate_application_password` and `rest_application_password_check_errors`, and the
//! request guards for the current REST user.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.
//! The plain passwords are secrets even in tests: assertions never print them.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mission_control::{AppOptions, RestAuth, RestUser, rocket_with_options};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::migrations::migrator;
use mission_control_core::rest::auth::AuthMethod;
use mission_control_core::users::app_password::{self, PlainPassword};
use mission_control_core::users::{
    EDITOR, InMemoryUserRoles, RoleAssignment, RolesFuture, UserRoles, has_cap,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use rocket::http::{Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// A route that answers for anyone, with the current user's id and roles (0 and none for a
/// visitor).
#[rocket::get("/mc-json/test/v1/whoami")]
fn whoami(auth: RestAuth) -> String {
    let caps = auth.caps();
    json!({
        "id": caps.id(),
        "roles": caps.roles(),
        "edit_others_posts": has_cap(&caps, "edit_others_posts"),
    })
    .to_string()
}

/// A route that needs a signed-in user.
#[rocket::get("/mc-json/test/v1/me")]
fn me(user: RestUser) -> String {
    assert!(matches!(
        user.method(),
        AuthMethod::ApplicationPassword { .. }
    ));
    json!({
        "id": user.caps().id(),
        "roles": user.caps().roles(),
        "app_password": user.app_password_uuid(),
    })
    .to_string()
}

/// How many times [`open`] ran.
static OPEN_CALLS: AtomicUsize = AtomicUsize::new(0);

/// A REST route that forgot its guard, and writes: the central check must still stop bad
/// credentials before it runs.
#[rocket::post("/mc-json/test/v1/open")]
fn open() -> &'static str {
    OPEN_CALLS.fetch_add(1, Ordering::SeqCst);
    "ran"
}

/// A site route that answers any two-segment path, at Rocket's default rank: it must not answer
/// a request the central check refused.
#[rocket::get("/<a>/<b>")]
fn any_two_segments(a: &str, b: &str) -> String {
    format!("caught by the site: {a}/{b}")
}

/// Non-REST routes that fail, to compare their answers with Rocket's own.
#[rocket::get("/test-status/<code>")]
fn failing(code: u16) -> Status {
    Status::from_code(code).unwrap()
}

/// A roles provider whose database query fails.
struct FailingRoles;

impl UserRoles for FailingRoles {
    fn roles<'a>(&'a self, db: &'a Database, _user_id: i64) -> RolesFuture<'a> {
        Box::pin(async move {
            raw::execute(db, "SELECT * FROM mc_no_such_table_for_roles", &[]).await?;
            unreachable!("the query fails")
        })
    }
}

/// Where the test site's user gets roles.
#[derive(Clone, Copy)]
enum Roles {
    /// An in-memory provider that makes the user an editor.
    Editor,
    /// No provider.
    NoProvider,
    /// A provider that fails.
    Failing,
}

/// One test site: its database, a user with an application password, and a client.
struct Site {
    _test: TestDatabase,
    client: Client,
    user_id: i64,
    email: String,
    password: PlainPassword,
    uuid: String,
}

fn config(environment: Environment, trusted_proxies: TrustedProxies) -> Config {
    Config {
        database_url: "unused: the database is passed in".to_owned(),
        secret_key: "unused".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../themes")
            .to_path_buf(),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment,
        trusted_proxies,
    }
}

/// Inserts a user and returns its id and email.
async fn new_user(db: &Database, name: &str) -> (i64, String) {
    let email = unique_email(name);
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &[name.into(), email.clone().into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    let id = raw::fetch_scalar(
        db,
        "SELECT id FROM users WHERE email = ?",
        &[email.clone().into()],
    )
    .await
    .expect("reads the user id");
    (id, email)
}

/// A site in `environment` trusting `trusted_proxies`, whose user gets roles from `roles`.
async fn site_with(
    environment: Environment,
    trusted_proxies: TrustedProxies,
    roles: Roles,
) -> Site {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    let (user_id, email) = new_user(db, "ann").await;
    let (password, item) = app_password::create(db, user_id, "Laptop", None)
        .await
        .expect("creates the application password");
    let user_roles: Option<Arc<dyn UserRoles>> = match roles {
        Roles::Editor => {
            let roles = InMemoryUserRoles::new();
            roles.assign(user_id, RoleAssignment::new([EDITOR], BTreeMap::new()));
            Some(Arc::new(roles))
        }
        Roles::NoProvider => None,
        Roles::Failing => Some(Arc::new(FailingRoles)),
    };
    let options = AppOptions {
        database: Some(test.shared()),
        user_roles,
        ..AppOptions::default()
    };
    let rocket = rocket_with_options(config(environment, trusted_proxies), options).mount(
        "/",
        rocket::routes![whoami, me, open, failing, any_two_segments],
    );
    let client = Client::tracked(rocket).await.expect("the app ignites");
    Site {
        _test: test,
        client,
        user_id,
        email,
        password,
        uuid: item.uuid,
    }
}

async fn local_site() -> Site {
    site_with(Environment::Local, TrustedProxies::None, Roles::Editor).await
}

/// The `Authorization` header for `user:password`.
fn basic(user: &str, password: &str) -> Header<'static> {
    Header::new(
        "Authorization",
        format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
    )
}

async fn body(response: LocalResponse<'_>) -> (Status, Value) {
    let status = response.status();
    let text = response.into_string().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{status}: {e}: {text}"));
    (status, json)
}

/// Asserts that `response` is WordPress's REST error `code` with `status`, and no
/// `WWW-Authenticate` header (WordPress sends none for application passwords).
async fn assert_error(response: LocalResponse<'_>, status: Status, code: &str, message: &str) {
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some("application/json; charset=UTF-8")
    );
    assert_eq!(response.headers().get_one("WWW-Authenticate"), None);
    let (got, json) = body(response).await;
    assert_eq!(got, status);
    assert_eq!(
        json,
        json!({"code": code, "message": message, "data": {"status": status.code}})
    );
}

const INCORRECT_PASSWORD: &str = "The provided password is an invalid application password.";
const NOT_LOGGED_IN: &str = "You are not currently logged in.";

#[rocket::async_test]
async fn a_valid_application_password_signs_the_user_in_with_the_providers_roles() {
    let site = local_site().await;
    // The grouped form WordPress shows works too.
    let response = site
        .client
        .get("/mc-json/test/v1/me")
        .header(basic(&site.email, &site.password.chunked()))
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(
        json,
        json!({"id": site.user_id, "roles": ["editor"], "app_password": site.uuid})
    );

    let response = site
        .client
        .get("/mc-json/test/v1/whoami")
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    let (_, json) = body(response).await;
    assert_eq!(json["id"], site.user_id);
    assert_eq!(json["edit_others_posts"], true);

    // Usage is recorded, as WordPress's `record_application_password_usage`.
    let item = app_password::find(site._test.db(), site.user_id, &site.uuid)
        .await
        .unwrap()
        .unwrap();
    assert!(item.last_used_at.is_some());
}

#[rocket::async_test]
async fn with_no_provider_a_signed_in_user_has_no_role() {
    let site = site_with(Environment::Local, TrustedProxies::None, Roles::NoProvider).await;
    let response = site
        .client
        .get("/mc-json/test/v1/whoami")
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(
        json,
        json!({"id": site.user_id, "roles": [], "edit_others_posts": false})
    );
}

#[rocket::async_test]
async fn a_wrong_password_is_refused_with_wordpresss_error_on_every_rest_route() {
    let site = local_site().await;
    for uri in [
        "/mc-json/test/v1/me",
        "/mc-json/test/v1/whoami",
        "/mc-json/",
        "/mc-json/mc/v2",
        "/?rest_route=/",
    ] {
        let response = site
            .client
            .get(uri)
            .header(basic(&site.email, "abcd efgh ijkl mnop qrst uvwx"))
            .dispatch()
            .await;
        assert_error(
            response,
            Status::Unauthorized,
            "incorrect_password",
            INCORRECT_PASSWORD,
        )
        .await;
    }
}

#[rocket::async_test]
async fn a_revoked_password_is_refused() {
    let site = local_site().await;
    assert!(
        app_password::revoke(site._test.db(), site.user_id, &site.uuid)
            .await
            .unwrap()
    );
    let response = site
        .client
        .get("/mc-json/test/v1/whoami")
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::Unauthorized,
        "incorrect_password",
        INCORRECT_PASSWORD,
    )
    .await;
}

#[rocket::async_test]
async fn an_unknown_user_is_refused_with_wordpresss_codes() {
    let site = local_site().await;
    let response = site
        .client
        .get("/mc-json/")
        .header(basic(&unique_email("nobody"), site.password.as_str()))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::Unauthorized,
        "invalid_email",
        "<strong>Error:</strong> Unknown email address. Check again or try your username.",
    )
    .await;

    let response = site
        .client
        .get("/mc-json/")
        .header(basic("nobody", site.password.as_str()))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::Unauthorized,
        "invalid_username",
        "<strong>Error:</strong> Unknown username. Check again or try your email address.",
    )
    .await;
}

#[rocket::async_test]
async fn a_request_without_credentials_is_anonymous() {
    let site = local_site().await;
    let (status, json) = body(site.client.get("/mc-json/test/v1/whoami").dispatch().await).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(
        json,
        json!({"id": 0, "roles": [], "edit_others_posts": false})
    );

    // Credentials that are not Basic (a bearer token for another plugin) are not application
    // passwords: the request is anonymous, as in WordPress.
    let (status, json) = body(
        site.client
            .get("/mc-json/test/v1/whoami")
            .header(Header::new("Authorization", "Bearer abc"))
            .dispatch()
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(json["id"], 0);

    let (status, _) = body(site.client.get("/mc-json/").dispatch().await).await;
    assert_eq!(status, Status::Ok);
}

#[rocket::async_test]
async fn a_route_that_needs_a_user_refuses_a_visitor() {
    let site = local_site().await;
    let response = site.client.get("/mc-json/test/v1/me").dispatch().await;
    assert_error(
        response,
        Status::Unauthorized,
        "rest_not_logged_in",
        "You are not currently logged in.",
    )
    .await;
}

#[rocket::async_test]
async fn an_oversized_authorization_header_is_not_tried() {
    let site = local_site().await;
    let name = "a".repeat(4096);
    let response = site
        .client
        .get("/mc-json/test/v1/whoami")
        .header(basic(&name, site.password.as_str()))
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(json["id"], 0);
}

#[rocket::async_test]
async fn over_plain_http_outside_a_local_environment_the_credentials_are_ignored() {
    // WordPress's `wp_validate_application_password` skips application passwords when
    // `wp_is_application_passwords_available` is false (no HTTPS, not `local`): the request is
    // anonymous, even with the right password.
    for environment in [
        Environment::Development,
        Environment::Staging,
        Environment::Production,
    ] {
        let site = site_with(environment, TrustedProxies::None, Roles::Editor).await;
        let response = site
            .client
            .get("/mc-json/test/v1/me")
            .header(basic(&site.email, site.password.as_str()))
            .dispatch()
            .await;
        assert_error(
            response,
            Status::Unauthorized,
            "rest_not_logged_in",
            "You are not currently logged in.",
        )
        .await;
        // A wrong password is not even checked.
        let response = site
            .client
            .get("/mc-json/")
            .header(basic(&site.email, "wrong"))
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
    }
}

#[rocket::async_test]
async fn https_is_believed_only_from_a_trusted_proxy() {
    let proxy: SocketAddr = "10.0.0.5:4000".parse().unwrap();
    let site = site_with(
        Environment::Production,
        TrustedProxies::Addresses(vec![proxy.ip()]),
        Roles::Editor,
    )
    .await;
    let response = site
        .client
        .get("/mc-json/test/v1/me")
        .remote(proxy)
        .header(Header::new("X-Forwarded-Proto", "https"))
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(json["id"], site.user_id);

    // The same header from a peer that is not the proxy is not believed.
    let response = site
        .client
        .get("/mc-json/test/v1/me")
        .remote("10.0.0.6:4000".parse().unwrap())
        .header(Header::new("X-Forwarded-Proto", "https"))
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::Unauthorized,
        "rest_not_logged_in",
        NOT_LOGGED_IN,
    )
    .await;

    // Nor a proxy saying the client came over plain HTTP somewhere along the way.
    let response = site
        .client
        .get("/mc-json/test/v1/me")
        .remote(proxy)
        .header(Header::new("X-Forwarded-Proto", "https, http"))
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::Unauthorized,
        "rest_not_logged_in",
        NOT_LOGGED_IN,
    )
    .await;

    // An IPv4 peer seen as an IPv4-mapped IPv6 address is the same proxy.
    let response = site
        .client
        .get("/mc-json/test/v1/me")
        .remote("[::ffff:10.0.0.5]:4000".parse().unwrap())
        .header(Header::new("X-Forwarded-Proto", "https"))
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(json["id"], site.user_id);
}

#[rocket::async_test]
async fn the_index_lists_no_authentication_until_the_authorize_screen_exists() {
    let site = local_site().await;
    let (_, index) = body(site.client.get("/mc-json/").dispatch().await).await;
    assert_eq!(index["authentication"], json!({}));
}

#[rocket::async_test]
async fn bad_credentials_are_refused_before_any_rest_route_runs() {
    let site = local_site().await;
    let wrong = || basic(&site.email, "abcd efgh ijkl mnop qrst uvwx");

    // A route without a guard does not run.
    let before = OPEN_CALLS.load(Ordering::SeqCst);
    let response = site
        .client
        .post("/mc-json/test/v1/open")
        .header(wrong())
        .dispatch()
        .await;
    assert_error(
        response,
        Status::Unauthorized,
        "incorrect_password",
        INCORRECT_PASSWORD,
    )
    .await;
    assert_eq!(
        OPEN_CALLS.load(Ordering::SeqCst),
        before,
        "the route did not run"
    );

    // With the right password it runs.
    let response = site
        .client
        .post("/mc-json/test/v1/open")
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(OPEN_CALLS.load(Ordering::SeqCst), before + 1);

    // An unknown REST path, any method, either form, and a percent-encoded prefix.
    for (method, uri) in [
        ("POST", "/mc-json/no/such/route"),
        ("DELETE", "/mc-json/mc/v2/posts/1"),
        ("POST", "/?rest_route=/no/such/route"),
        ("GET", "/%6Dc-json/"),
        ("PUT", "/%6Dc-json/test/v1/open"),
    ] {
        let request = match method {
            "POST" => site.client.post(uri),
            "DELETE" => site.client.delete(uri),
            "PUT" => site.client.put(uri),
            _ => site.client.get(uri),
        };
        let response = request.header(wrong()).dispatch().await;
        assert_error(
            response,
            Status::Unauthorized,
            "incorrect_password",
            INCORRECT_PASSWORD,
        )
        .await;
    }
}

#[rocket::async_test]
async fn a_failing_roles_provider_refuses_the_request() {
    let site = site_with(Environment::Local, TrustedProxies::None, Roles::Failing).await;
    let response = site
        .client
        .get("/mc-json/test/v1/whoami")
        .header(basic(&site.email, site.password.as_str()))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::InternalServerError,
        "internal_server_error",
        "The site is experiencing technical difficulties.",
    )
    .await;
}

#[rocket::async_test]
async fn errors_outside_the_rest_api_keep_rockets_answers() {
    let site = local_site().await;
    let plain = Client::tracked(rocket::build().mount("/", rocket::routes![failing]))
        .await
        .unwrap();
    for code in [401, 403, 500] {
        for accept in ["text/html", "application/json"] {
            let uri = format!("/test-status/{code}");
            let ours = site
                .client
                .get(uri.clone())
                .header(Header::new("Accept", accept))
                .dispatch()
                .await;
            let rockets = plain
                .get(uri)
                .header(Header::new("Accept", accept))
                .dispatch()
                .await;
            assert_eq!(ours.status(), rockets.status());
            assert_eq!(
                ours.headers().get_one("Content-Type"),
                rockets.headers().get_one("Content-Type"),
                "{code} {accept}"
            );
            assert_eq!(ours.into_string().await, rockets.into_string().await);
        }
    }
    // A missing page is still the theme's 404.
    let response = site.client.get("/no-such-page-anywhere").dispatch().await;
    assert_eq!(response.status(), Status::NotFound);
    assert_ne!(
        response.headers().get_one("Content-Type"),
        Some("application/json; charset=UTF-8")
    );
}

#[rocket::async_test]
async fn a_refused_request_is_not_caught_by_a_site_route() {
    let site = local_site().await;
    for uri in ["/mc-json/no/such/route", "/mc-json/test/v1/whoami"] {
        let response = site
            .client
            .get(uri)
            .header(basic(&site.email, "wrong"))
            .dispatch()
            .await;
        assert_error(
            response,
            Status::Unauthorized,
            "incorrect_password",
            INCORRECT_PASSWORD,
        )
        .await;
    }
}

#[rocket::async_test]
async fn a_refused_head_request_gets_401_and_no_body() {
    let site = local_site().await;
    let response = site
        .client
        .head("/mc-json/")
        .header(basic(&site.email, "wrong"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some("application/json; charset=UTF-8")
    );
    assert_eq!(response.into_string().await.unwrap_or_default(), "");
}
