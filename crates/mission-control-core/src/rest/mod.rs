//! The REST API: the route registry and the discovery documents, in WordPress's shape.
//!
//! [`RestRoutes`] lists every route the API serves. Each route group (posts, users, a plugin's
//! namespace) registers its routes on it, and [`discovery`] builds `GET /wp-json/` and each
//! namespace index from it, so discovery never changes when a route is added. The core types
//! here hold no Rocket types: the `mission-control` crate mounts them.
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
//! assert_eq!(routes.namespaces(), ["acme/v1", "wp/v2"]);
//! ```

pub mod discovery;

use std::collections::BTreeMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde_json::{Map, Value};

/// The namespace of WordPress's own routes. It is always listed, as `oembed/1.0` is not.
pub const CORE_NAMESPACE: &str = "wp/v2";

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

/// A route: a path under `/wp-json` and the endpoints that answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteSpec {
    /// The path as WordPress writes it, such as `/wp/v2/posts` or `/wp/v2/posts/(?P<id>[\d]+)`.
    pub path: String,
    /// The namespace: the first two path segments, such as `wp/v2`. Empty for the index (`/`).
    pub namespace: String,
    /// The endpoints, in the order they are listed.
    pub endpoints: Vec<Endpoint>,
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
        }
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

/// The namespace of `path`: its first two segments (`/wp/v2/posts` is `wp/v2`).
fn namespace_of(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).take(2).collect();
    if segments.len() < 2 && path.trim_matches('/').is_empty() {
        return String::new();
    }
    segments.join("/")
}

/// The registry of REST routes, shared by the app and the code that extends it.
///
/// A new namespace gets its own index route (`/wp/v2`) the first time one of its routes is
/// registered, as WordPress adds it. The registry starts with the index (`/`) and the `wp/v2`
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
    /// A registry holding the index route and the `wp/v2` namespace index.
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
        let namespace = route.namespace.clone();
        if !namespace.is_empty() && !self.read().contains_key(&format!("/{namespace}")) {
            self.add_route(namespace_route(&namespace));
        }
        self.add_route(route);
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
        assert_eq!(namespace_of("/wp/v2"), "wp/v2");
        assert_eq!(namespace_of("/wp/v2/posts/(?P<id>[\\d]+)"), "wp/v2");
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
        assert_eq!(paths, ["/", "/acme/v1", "/acme/v1/things", "/wp/v2"]);
        assert_eq!(routes.routes_in("acme/v1").len(), 2);
    }

    #[test]
    fn methods_are_the_union_of_the_endpoints() {
        let route = RouteSpec::new(
            "/wp/v2/posts",
            vec![
                Endpoint::new(["GET"], Value::Null),
                Endpoint::new(["POST", "GET"], Value::Null),
            ],
        );
        assert_eq!(route.methods(), ["GET", "POST"]);
    }
}
