//! REST discovery (Task 10.1): `GET /wp-json/` in the shape of WordPress's `WP_REST_Server::get_index`,
//! the `wp/v2` namespace index, and the `rest_route` fallback.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mission_control::rocket_with_database;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{NewPost, Post, PostStatus, set_option};
use mission_control_core::migrations::migrator;
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::raw;
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::Client;
use serde_json::{Value, json};

/// A test site with the core tables and the options of a small site.
async fn site() -> (TestDatabase, Client) {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    for (name, value) in [
        ("blogname", json!("Test Site")),
        ("blogdescription", json!("Just another test site")),
        ("siteurl", json!("https://example.test/cms")),
        ("home", json!("https://example.test")),
        ("gmt_offset", json!("-5")),
        ("timezone_string", json!("America/Toronto")),
    ] {
        set_option(db, name, &value).await.expect("sets the option");
    }
    let config = Config {
        database_url: "unused: the database is passed in".to_owned(),
        secret_key: "unused".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../themes")
            .to_path_buf(),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        environment: Environment::Production,
        trusted_proxies: TrustedProxies::None,
    };
    let client = Client::tracked(rocket_with_database(config, test.shared()))
        .await
        .expect("the app ignites");
    (test, client)
}

async fn get_json(client: &Client, uri: &str) -> (Status, Value) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    let json = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{uri}: {e}: {body}"));
    (status, json)
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

#[rocket::async_test]
async fn the_index_has_wordpresss_top_level_keys_and_option_values() {
    let (_test, client) = site().await;
    let response = client.get("/wp-json/").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some("application/json; charset=UTF-8")
    );
    assert_eq!(
        response.headers().get_one("Link"),
        None,
        "the Link header is for HTML pages"
    );
    let index: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();

    let expected: BTreeSet<&str> = [
        "name",
        "description",
        "url",
        "home",
        "gmt_offset",
        "timezone_string",
        "page_for_posts",
        "page_on_front",
        "show_on_front",
        "namespaces",
        "authentication",
        "routes",
        "site_logo",
        "site_icon",
        "site_icon_url",
        "_links",
    ]
    .into();
    assert_eq!(keys(&index), expected);

    assert_eq!(index["name"], "Test Site");
    assert_eq!(index["description"], "Just another test site");
    assert_eq!(index["url"], "https://example.test/cms");
    assert_eq!(index["home"], "https://example.test");
    assert_eq!(index["gmt_offset"], "-5");
    assert_eq!(index["timezone_string"], "America/Toronto");
    assert_eq!(index["page_for_posts"], 0);
    assert_eq!(index["page_on_front"], 0);
    assert_eq!(index["show_on_front"], "posts");
    assert_eq!(index["namespaces"], json!(["wp/v2"]));
    assert_eq!(index["authentication"], json!({}));
    assert_eq!(index["site_logo"], 0);
    assert_eq!(index["site_icon"], 0);
    assert_eq!(index["site_icon_url"], "");
    assert_eq!(
        index["_links"],
        json!({"help": [{"href": "https://developer.wordpress.org/rest-api/"}]})
    );

    let routes = index["routes"].as_object().unwrap();
    assert_eq!(
        routes.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "/",
            "/wp/v2",
            "/wp/v2/categories",
            "/wp/v2/categories/(?P<id>[\\d]+)",
            "/wp/v2/media",
            "/wp/v2/media/(?P<id>[\\d]+)",
            "/wp/v2/pages",
            "/wp/v2/pages/(?P<id>[\\d]+)",
            "/wp/v2/posts",
            "/wp/v2/posts/(?P<id>[\\d]+)",
            "/wp/v2/search",
            "/wp/v2/settings",
            "/wp/v2/statuses",
            "/wp/v2/statuses/(?P<status>[\\w-]+)",
            "/wp/v2/tags",
            "/wp/v2/tags/(?P<id>[\\d]+)",
            "/wp/v2/taxonomies",
            "/wp/v2/taxonomies/(?P<taxonomy>[\\w-]+)",
            "/wp/v2/types",
            "/wp/v2/types/(?P<type>[\\w-]+)",
            "/wp/v2/users",
            "/wp/v2/users/(?P<id>[\\d]+)",
            "/wp/v2/users/me",
        ]
    );
    // A route with a regex parameter has no `self` link (Controller ruling R44).
    assert!(
        routes["/wp/v2/posts/(?P<id>[\\d]+)"]
            .get("_links")
            .is_none()
    );
    assert_eq!(
        routes["/"],
        json!({
            "namespace": "",
            "methods": ["GET"],
            "endpoints": [{"methods": ["GET"], "args": {"context": {"default": "view", "required": false}}}],
            "_links": {"self": [{"href": "https://example.test/wp-json/"}]},
        })
    );
    assert_eq!(routes["/wp/v2"]["namespace"], "wp/v2");
    assert_eq!(routes["/wp/v2"]["methods"], json!(["GET"]));
    assert_eq!(
        routes["/wp/v2"]["endpoints"][0]["args"]["namespace"],
        json!({"default": "wp/v2", "required": false})
    );
    assert_eq!(
        routes["/wp/v2"]["_links"],
        json!({"self": [{"href": "https://example.test/wp-json/wp/v2"}]})
    );
}

#[rocket::async_test]
async fn the_index_answers_without_the_trailing_slash_and_by_rest_route() {
    let (_test, client) = site().await;
    let (status, with_slash) = get_json(&client, "/wp-json/").await;
    assert_eq!(status, Status::Ok);
    for uri in ["/wp-json", "/?rest_route=/"] {
        let (status, index) = get_json(&client, uri).await;
        assert_eq!(status, Status::Ok, "{uri}");
        assert_eq!(index, with_slash, "{uri}");
    }
    let (status, ns) = get_json(&client, "/?rest_route=/wp/v2").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(ns["namespace"], "wp/v2");
}

#[rocket::async_test]
async fn an_unset_site_uses_wordpresss_defaults() {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator().unwrap().run(db).await.unwrap();
    let config = Config {
        database_url: "unused".to_owned(),
        secret_key: "unused".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../themes"),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        environment: Environment::Production,
        trusted_proxies: TrustedProxies::None,
    };
    let client = Client::tracked(rocket_with_database(config, test.shared()))
        .await
        .unwrap();
    let (status, index) = get_json(&client, "/wp-json/").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(index["name"], "");
    assert_eq!(index["gmt_offset"], "0");
    assert_eq!(index["show_on_front"], "posts");
    // With no `home` option the links start at the root.
    assert_eq!(
        index["routes"]["/"]["_links"]["self"][0]["href"],
        "/wp-json/"
    );
}

#[rocket::async_test]
async fn the_namespace_index_lists_its_routes() {
    let (_test, client) = site().await;
    for uri in ["/wp-json/wp/v2", "/wp-json/wp/v2/"] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::Ok, "{uri}");
        assert_eq!(response.content_type(), Some(ContentType::JSON), "{uri}");
        let ns: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
        assert_eq!(keys(&ns), ["namespace", "routes"].into(), "{uri}");
        assert_eq!(ns["namespace"], "wp/v2");
        let routes = ns["routes"].as_object().unwrap();
        assert_eq!(
            routes.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "/wp/v2",
                "/wp/v2/categories",
                "/wp/v2/categories/(?P<id>[\\d]+)",
                "/wp/v2/media",
                "/wp/v2/media/(?P<id>[\\d]+)",
                "/wp/v2/pages",
                "/wp/v2/pages/(?P<id>[\\d]+)",
                "/wp/v2/posts",
                "/wp/v2/posts/(?P<id>[\\d]+)",
                "/wp/v2/search",
                "/wp/v2/settings",
                "/wp/v2/statuses",
                "/wp/v2/statuses/(?P<status>[\\w-]+)",
                "/wp/v2/tags",
                "/wp/v2/tags/(?P<id>[\\d]+)",
                "/wp/v2/taxonomies",
                "/wp/v2/taxonomies/(?P<taxonomy>[\\w-]+)",
                "/wp/v2/types",
                "/wp/v2/types/(?P<type>[\\w-]+)",
                "/wp/v2/users",
                "/wp/v2/users/(?P<id>[\\d]+)",
                "/wp/v2/users/me",
            ]
        );
        assert_eq!(routes["/wp/v2"]["namespace"], "wp/v2");
    }
}

#[rocket::async_test]
async fn an_unknown_rest_route_is_wordpresss_404() {
    let (_test, client) = site().await;
    let response = client.get("/wp-json/nope/v1/x").dispatch().await;
    assert_eq!(response.status(), Status::NotFound);
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert_eq!(body["code"], "rest_no_route");
    assert_eq!(
        body["message"],
        "No route was found matching the URL and request method."
    );
    assert_eq!(body["data"], json!({"status": 404}));
}

#[rocket::async_test]
async fn html_pages_advertise_the_api() {
    let (_test, client) = site().await;
    let response = client.get("/").dispatch().await;
    assert_eq!(
        response.headers().get_one("Link"),
        Some("<https://example.test/wp-json/>; rel=\"https://api.w.org/\"")
    );
}

#[rocket::async_test]
async fn an_empty_or_relative_rest_route_is_the_front_page() {
    let (_test, client) = site().await;
    for uri in ["/?rest_route=", "/?rest_route=wp/v2"] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::Ok, "{uri}");
        let body = response.into_string().await.unwrap();
        assert!(body.starts_with("<!doctype html>"), "{uri}: {body}");
    }
}

#[rocket::async_test]
async fn unknown_rest_paths_are_always_rest_no_route() {
    let (_test, client) = site().await;
    for uri in ["/wp-json/.hidden", "/wp-json/a:b/c", "/wp-json/wp/v2/.x"] {
        let (status, body) = get_json(&client, uri).await;
        assert_eq!(status, Status::NotFound, "{uri}");
        assert_eq!(body["code"], "rest_no_route", "{uri}");
    }
}

#[rocket::async_test]
async fn a_post_slugged_wp_json_is_shadowed_by_the_api() {
    let (test, client) = site().await;
    let db = test.db();
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .unwrap();
    let author = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .unwrap();
    Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Shadowed".to_owned(),
            slug: "wp-json".to_owned(),
            content: "Body".to_owned(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    let (status, index) = get_json(&client, "/wp-json/").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(index["name"], "Test Site");
}

#[rocket::async_test]
async fn an_unsafe_home_option_gives_relative_links() {
    for home in [
        "javascript:alert(1)",
        "//evil.test",
        "https://a.test/a\"b",
        "https://a.test/a>b",
        "https://a.test/a b",
        "https://a.test/\u{1}x",
        "https://a.test/\r\nSet-Cookie: x=1",
    ] {
        let (test, client) = site().await;
        set_option(test.db(), "home", &json!(home)).await.unwrap();
        set_option(test.db(), "siteurl", &json!(home))
            .await
            .unwrap();
        let (status, index) = get_json(&client, "/wp-json/").await;
        assert_eq!(status, Status::Ok, "{home:?}");
        assert_eq!(index["home"], "", "{home:?}");
        assert_eq!(index["url"], "", "{home:?}");
        assert_eq!(
            index["routes"]["/"]["_links"]["self"][0]["href"], "/wp-json/",
            "{home:?}"
        );
        let (_, ns) = get_json(&client, "/wp-json/wp/v2").await;
        assert_eq!(
            ns["routes"]["/wp/v2"]["_links"]["self"][0]["href"], "/wp-json/wp/v2",
            "{home:?}"
        );
        let response = client.get("/").dispatch().await;
        assert_eq!(
            response.headers().get_one("Link"),
            Some("</wp-json/>; rel=\"https://api.w.org/\""),
            "{home:?}"
        );
    }
}
