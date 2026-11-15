//! The discovery documents: `GET /wp-json/` as `WP_REST_Server::get_index` builds it, and a
//! namespace index as `WP_REST_Server::get_namespace_index`.
//!
//! Both are plain JSON values built from the options and a [`RestRoutes`] registry.
//!
//! The differences from WordPress, on purpose: `namespaces` lists only the namespaces that
//! exist (WordPress also lists `oembed/1.0`), `authentication` is empty until application
//! passwords are served, `site_icon_url` is empty until the media library can resolve an
//! attachment's URL, and links always use `/wp-json/` (WordPress falls back to `?rest_route=`
//! when permalinks are plain).

use serde_json::{Map, Value, json};

use crate::content::{MetaError, get_option};
use crate::rest::{RestRoutes, RouteSpec};
use orbit_orm::Database;

/// The path the API is served under.
pub const REST_PREFIX: &str = "wp-json";

/// The REST handbook, the `help` link of the index.
const HANDBOOK_URL: &str = "https://developer.wordpress.org/rest-api/";

/// `home` as a web address without a trailing slash, or an empty string when the option is not
/// set or not an http(s) address without characters that break out of a header or an attribute.
/// An empty string makes links start at the root.
pub fn site_address(home: &str) -> String {
    let home = home.trim().trim_end_matches('/');
    let lower = home.to_ascii_lowercase();
    let is_web = ["http://", "https://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len());
    if is_web
        && !home
            .chars()
            .any(|c| c.is_control() || "\"<>\\ ".contains(c))
    {
        home.to_owned()
    } else {
        String::new()
    }
}

/// The address of the API's index under `home` (already through [`site_address`]):
/// `https://example.com/wp-json/`.
pub fn index_url(home: &str) -> String {
    format!("{home}/{REST_PREFIX}/")
}

/// `rest_url( $path )`: `path` under the API, such as `/wp/v2` or `/`.
fn rest_url(home: &str, path: &str) -> String {
    if path == "/" {
        index_url(home)
    } else {
        format!("{home}/{REST_PREFIX}/{}", path.trim_start_matches('/'))
    }
}

/// The index document, `GET /wp-json/`, from the site's options and the registered routes.
///
/// # Errors
///
/// Fails with the database's error when an option cannot be read.
pub async fn index(db: &Database, routes: &RestRoutes) -> Result<Value, MetaError> {
    let home_option = text(db, "home").await?;
    let home = site_address(&home_option);
    let mut document = Map::new();
    let mut put = |key: &str, value: Value| document.insert(key.to_owned(), value);
    put("name", json!(text(db, "blogname").await?));
    put("description", json!(text(db, "blogdescription").await?));
    put("url", json!(text(db, "siteurl").await?));
    put("home", json!(home_option));
    put("gmt_offset", json!(offset(db).await?));
    put("timezone_string", json!(text(db, "timezone_string").await?));
    put(
        "page_for_posts",
        json!(integer(db, "page_for_posts").await?),
    );
    put("page_on_front", json!(integer(db, "page_on_front").await?));
    let show_on_front = text(db, "show_on_front").await?;
    put(
        "show_on_front",
        json!(if show_on_front.is_empty() {
            "posts".to_owned()
        } else {
            show_on_front
        }),
    );
    put("namespaces", json!(routes.namespaces()));
    // Filled by the application passwords route (Task 10.5).
    put("authentication", json!({}));
    put("routes", routes_document(&home, &routes.routes()));
    put("site_logo", json!(integer(db, "site_logo").await?));
    let site_icon = integer(db, "site_icon").await?;
    put("site_icon", json!(site_icon));
    put("site_icon_url", json!(""));
    put("_links", json!({"help": [{"href": HANDBOOK_URL}]}));
    Ok(Value::Object(document))
}

/// The index of `namespace`: its name and its routes. `None` when no route is registered in it.
///
/// # Errors
///
/// Fails with the database's error when the `home` option cannot be read.
pub async fn namespace_index(
    db: &Database,
    routes: &RestRoutes,
    namespace: &str,
) -> Result<Option<Value>, MetaError> {
    let in_namespace = routes.routes_in(namespace);
    if namespace.is_empty() || in_namespace.is_empty() {
        return Ok(None);
    }
    let home = home_address(db).await?;
    Ok(Some(json!({
        "namespace": namespace,
        "routes": routes_document(&home, &in_namespace),
    })))
}

/// The `home` option as a web address without a trailing slash (see [`site_address`]).
///
/// # Errors
///
/// Fails with the database's error when the option cannot be read.
pub async fn home_address(db: &Database) -> Result<String, MetaError> {
    Ok(site_address(&text(db, "home").await?))
}

/// The error body of a path with no route, as WordPress's `rest_no_route`.
pub fn no_route() -> Value {
    json!({
        "code": "rest_no_route",
        "message": "No route was found matching the URL and request method.",
        "data": {"status": 404},
    })
}

/// The `routes` object: each route's `namespace`, `methods`, `endpoints` and `_links`.
fn routes_document(home: &str, routes: &[RouteSpec]) -> Value {
    let mut document = Map::new();
    for route in routes {
        let endpoints: Vec<Value> = route
            .endpoints
            .iter()
            .map(|e| json!({"methods": e.methods, "args": e.args}))
            .collect();
        let mut entry = json!({
            "namespace": route.namespace,
            "methods": route.methods(),
            "endpoints": endpoints,
        });
        // A route with a pattern in it has no address of its own to link to.
        if !route.path.contains("(?P<") {
            entry["_links"] = json!({"self": [{"href": rest_url(home, &route.path)}]});
        }
        document.insert(route.path.clone(), entry);
    }
    Value::Object(document)
}

/// The text of option `name`; empty when it is not set or not a string or a number.
async fn text(db: &Database, name: &str) -> Result<String, MetaError> {
    Ok(match get_option(db, name).await? {
        Some(Value::String(s)) => s,
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    })
}

/// The whole number in option `name`, which WordPress stores as text; 0 when it is not one.
async fn integer(db: &Database, name: &str) -> Result<i64, MetaError> {
    Ok(match get_option(db, name).await? {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

/// `gmt_offset` as WordPress's index gives it, a string (`"0"`, `"-5"`, `"5.5"`).
async fn offset(db: &Database) -> Result<String, MetaError> {
    let offset = text(db, "gmt_offset").await?;
    Ok(if offset.is_empty() {
        "0".to_owned()
    } else {
        offset
    })
}
