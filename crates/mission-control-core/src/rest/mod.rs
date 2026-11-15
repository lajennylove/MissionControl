//! The REST API: the route registry, the discovery documents and authentication, in WordPress's
//! shape.
//!
//! [`RestRoutes`] lists every route the API serves. Each route group (posts, terms, users, a
//! plugin's namespace) registers its routes on it, and [`discovery`] builds `GET /mc-json/` and
//! each namespace index from it, so discovery never changes when a route is added. [`auth`]
//! decides who a request runs as, and [`RestError`] is WordPress's error body. [`posts`],
//! [`terms`], [`users`] and [`media`] are WordPress's posts, terms, users and attachments
//! controllers; [`meta`] is the registered meta they show under `meta`; [`meta_routes`] serves
//! the types, taxonomies and statuses, [`search`] the search and [`settings`] the site's
//! settings, with the registry code extends them through; [`params`] reads a
//! request's parameters as `WP_REST_Request` holds them, and [`KsesLimiter`] runs the
//! controllers' kses filters off the async threads. The core types here hold no Rocket types:
//! the `mission-control` crate mounts them.
//!
//! ```
//! use mission_control_core::rest::{Endpoint, RestRoutes, RouteSpec};
//! use serde_json::json;
//!
//! let routes = RestRoutes::new();
//! routes.register(RouteSpec::new(
//!     "/acme/v1/things",
//!     vec![Endpoint::new(["GET"], json!({"context": {"default": "view", "required": false}}))],
//! ));
//! assert_eq!(routes.namespaces(), ["acme/v1", "mc/v2"]);
//! ```

pub mod auth;
pub mod discovery;
pub mod media;
pub mod meta;
pub mod meta_routes;
pub mod params;
pub mod posts;
pub mod search;
pub mod settings;
mod shared;
pub mod terms;
pub mod users;

pub use params::{MAX_INPUT_VARS, Params, parse_form};
pub use shared::KsesLimiter;
pub(crate) use shared::id_of;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde_json::{Map, Value, json};

/// The namespace of WordPress's own routes. It is always listed, as `oembed/1.0` is not.
pub const CORE_NAMESPACE: &str = "mc/v2";

/// An error a REST request ends with, as WordPress's `WP_Error` once `WP_REST_Server` turns it
/// into a response: `{"code": ..., "message": ..., "data": {"status": ...}}` with that status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestError {
    /// WordPress's error code, such as `rest_no_route` or `incorrect_password`.
    pub code: Cow<'static, str>,
    /// The message for people, as WordPress words it.
    pub message: Cow<'static, str>,
    /// The HTTP status, such as 401.
    pub status: u16,
    /// What `data` holds besides `status`, such as `params` and `details` for
    /// `rest_invalid_param`. Empty for most errors.
    pub data: Map<String, Value>,
}

impl RestError {
    /// The error `code` with `message` and HTTP `status`.
    pub fn new(
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
        status: u16,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
            data: Map::new(),
        }
    }

    /// This error with `key` set to `value` in its `data`, beside `status`.
    pub fn with_data(mut self, key: &str, value: Value) -> Self {
        self.data.insert(key.to_owned(), value);
        self
    }

    /// WordPress's answer when the site itself fails, such as a database error: status 500. The
    /// cause is logged, never sent.
    pub fn internal() -> Self {
        Self::new(
            "internal_server_error",
            "The site is experiencing technical difficulties.",
            500,
        )
    }

    /// The JSON body: `code`, `message` and `data` (`status` and the rest of [`data`](Self::data)).
    pub fn to_json(&self) -> Value {
        let mut data = self.data.clone();
        data.insert("status".to_owned(), json!(self.status));
        json!({
            "code": self.code,
            "message": self.message,
            "data": data,
        })
    }
}

impl fmt::Display for RestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.status, self.message)
    }
}

impl std::error::Error for RestError {}

/// A REST answer that is not an error, as WordPress's `WP_REST_Response`: a status, a JSON body
/// and headers such as `X-MC-Total` or `Location`. A header may repeat (`Link`).
#[derive(Debug, Clone, PartialEq)]
pub struct RestResponse {
    /// The HTTP status, such as 200 or 201.
    pub status: u16,
    /// The JSON body.
    pub body: Value,
    /// The headers, in order; a name may repeat.
    pub headers: Vec<(String, String)>,
}

impl RestResponse {
    /// A 200 answer with `body` and no headers.
    pub fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            headers: Vec::new(),
        }
    }

    /// This answer with the header `name: value` added.
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }
}

/// One way to call a route: the HTTP methods it takes and its arguments, as WordPress's
/// `register_rest_route` endpoint (`methods` and `args`).
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    /// The HTTP methods, such as `GET` or `POST`.
    pub methods: Vec<String>,
    /// The arguments, a JSON object keyed by name (`default`, `required`, `type`, ...).
    pub args: Map<String, Value>,
}

impl Endpoint {
    /// An endpoint for `methods` with `args`, which must be a JSON object (anything else is no
    /// arguments).
    pub fn new<M, S>(methods: M, args: Value) -> Self
    where
        M: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            methods: methods.into_iter().map(Into::into).collect(),
            args: match args {
                Value::Object(args) => args,
                _ => Map::new(),
            },
        }
    }
}

/// A route: a path under `/mc-json` and the endpoints that answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteSpec {
    /// The path as WordPress writes it, such as `/mc/v2/posts` or `/mc/v2/posts/(?P<id>[\d]+)`.
    pub path: String,
    /// The namespace: the first two path segments, such as `mc/v2`. Empty for the index (`/`).
    pub namespace: String,
    /// The endpoints, in the order they are listed.
    pub endpoints: Vec<Endpoint>,
    /// The object type and subtype (post type or taxonomy) whose registered meta the endpoints'
    /// `meta` argument lists in discovery, for a route that writes `meta`. `None` for any other.
    pub meta: Option<(meta::MetaObject, String)>,
}

impl RouteSpec {
    /// A route at `path` (leading slash) with `endpoints`. The namespace is the first two
    /// segments of the path.
    pub fn new(path: impl Into<String>, endpoints: Vec<Endpoint>) -> Self {
        let path = path.into();
        let namespace = namespace_of(&path);
        Self {
            path,
            namespace,
            endpoints,
            meta: None,
        }
    }

    /// This route with the `meta` argument of its endpoints listing the keys registered for
    /// `object` and `subtype` (see [`meta::MetaKeys::schemas`]).
    pub fn with_meta(mut self, object: meta::MetaObject, subtype: impl Into<String>) -> Self {
        self.meta = Some((object, subtype.into()));
        self
    }

    /// The methods of all endpoints, without repeats, in the order first seen.
    pub fn methods(&self) -> Vec<&str> {
        let mut methods: Vec<&str> = Vec::new();
        for method in self.endpoints.iter().flat_map(|e| &e.methods) {
            if !methods.contains(&method.as_str()) {
                methods.push(method);
            }
        }
        methods
    }
}

/// The namespace of `path`: its first two segments (`/mc/v2/posts` is `mc/v2`).
fn namespace_of(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).take(2).collect();
    if segments.len() < 2 && path.trim_matches('/').is_empty() {
        return String::new();
    }
    segments.join("/")
}

/// The registry of REST routes, shared by the app and the code that extends it.
///
/// A new namespace gets its own index route (`/mc/v2`) the first time one of its routes is
/// registered, as WordPress adds it. The registry starts with the index (`/`) and the `mc/v2`
/// namespace.
#[derive(Debug)]
pub struct RestRoutes {
    routes: RwLock<BTreeMap<String, RouteSpec>>,
}

impl Default for RestRoutes {
    fn default() -> Self {
        Self::new()
    }
}

impl RestRoutes {
    /// A registry holding the index route and the `mc/v2` namespace index.
    pub fn new() -> Self {
        let registry = Self {
            routes: RwLock::new(BTreeMap::new()),
        };
        registry.add_route(index_route());
        registry.add_route(namespace_route(CORE_NAMESPACE));
        registry
    }

    /// Registers `route`, replacing a route with the same path. Registering a route in a new
    /// namespace adds the namespace's index route.
    pub fn register(&self, route: RouteSpec) {
        let mut routes = self.write();
        let namespace_path = format!("/{}", route.namespace);
        if !route.namespace.is_empty() && !routes.contains_key(&namespace_path) {
            routes.insert(namespace_path, namespace_route(&route.namespace));
        }
        routes.insert(route.path.clone(), route);
    }

    fn add_route(&self, route: RouteSpec) {
        self.write().insert(route.path.clone(), route);
    }

    /// Every route, ordered by path.
    pub fn routes(&self) -> Vec<RouteSpec> {
        self.read().values().cloned().collect()
    }

    /// The routes of `namespace`, ordered by path.
    pub fn routes_in(&self, namespace: &str) -> Vec<RouteSpec> {
        self.read()
            .values()
            .filter(|route| route.namespace == namespace)
            .cloned()
            .collect()
    }

    /// The namespaces with routes, sorted. The index (`/`) has none of its own.
    pub fn namespaces(&self) -> Vec<String> {
        let mut namespaces: Vec<String> = self
            .read()
            .values()
            .map(|route| route.namespace.clone())
            .filter(|namespace| !namespace.is_empty())
            .collect();
        namespaces.sort();
        namespaces.dedup();
        namespaces
    }

    // A poisoned lock only means a registering thread panicked; the map is still whole.
    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<String, RouteSpec>> {
        self.routes.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, RouteSpec>> {
        self.routes.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// `GET /`, the index: WordPress's `context` argument only.
fn index_route() -> RouteSpec {
    RouteSpec::new(
        "/",
        vec![Endpoint::new(
            ["GET"],
            serde_json::json!({"context": {"default": "view", "required": false}}),
        )],
    )
}

/// `GET /<namespace>`, a namespace index: `namespace` and `context` arguments.
fn namespace_route(namespace: &str) -> RouteSpec {
    RouteSpec::new(
        format!("/{namespace}"),
        vec![Endpoint::new(
            ["GET"],
            serde_json::json!({
                "namespace": {"default": namespace, "required": false},
                "context": {"default": "view", "required": false},
            }),
        )],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_come_from_the_first_two_segments() {
        assert_eq!(namespace_of("/"), "");
        assert_eq!(namespace_of("/mc/v2"), "mc/v2");
        assert_eq!(namespace_of("/mc/v2/posts/(?P<id>[\\d]+)"), "mc/v2");
        assert_eq!(namespace_of("/oembed/1.0/embed"), "oembed/1.0");
    }

    #[test]
    fn a_new_namespace_gets_an_index_route() {
        let routes = RestRoutes::new();
        routes.register(RouteSpec::new(
            "/acme/v1/things",
            vec![Endpoint::new(["GET", "POST"], Value::Null)],
        ));
        let paths: Vec<String> = routes.routes().into_iter().map(|r| r.path).collect();
        assert_eq!(paths, ["/", "/acme/v1", "/acme/v1/things", "/mc/v2"]);
        assert_eq!(routes.routes_in("acme/v1").len(), 2);
    }

    #[test]
    fn methods_are_the_union_of_the_endpoints() {
        let route = RouteSpec::new(
            "/mc/v2/posts",
            vec![
                Endpoint::new(["GET"], Value::Null),
                Endpoint::new(["POST", "GET"], Value::Null),
            ],
        );
        assert_eq!(route.methods(), ["GET", "POST"]);
    }
}
