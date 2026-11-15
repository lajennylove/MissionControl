//! The posts controller: `/wp/v2/posts`, `/wp/v2/pages` and each post type shown in the REST
//! API, as WordPress's `WP_REST_Posts_Controller` serves them.
//!
//! A [`PostType`] describes what the controller reads from a post type's registration: its
//! `rest_base`, whether it is hierarchical, what it supports and its taxonomies. [`PostTypes`]
//! holds the types the API serves; it starts with `post` and `page`, and the post type registry
//! (Task 5.2) registers the rest. A type whose `show_in_rest` is false has no route and is not
//! listed in discovery. [`PostsController`] answers one request for one type, with no Rocket
//! types: the `mission-control` crate mounts it.
//!
//! As WordPress:
//!
//! - The JSON has the keys of `prepare_item_for_response` for the request's `context` (`view`,
//!   `embed`, or `edit` for a user who may edit), and `_links`. `_fields` keeps only the keys
//!   named (dotted paths for nested ones); `_embed` embeds the author and the terms.
//! - Lists take `page`, `per_page` (1 to 100), `search`, `orderby`, `order`, `status`, `include`,
//!   `exclude`, `slug`, `author`, `offset`, `parent` (hierarchical types), `menu_order`, and the
//!   taxonomies (`categories`, `tags` and their `_exclude`), and answer with `X-WP-Total`,
//!   `X-WP-TotalPages` and `Link` (`prev`, `next`) headers. A status other than `publish` needs
//!   the type's `edit_posts` (or `read_private_posts` for `private`).
//! - Writes check capabilities with [`can`](crate::users::can) and the site's special pages:
//!   creating needs `edit_posts` (with `publish_posts` to publish, `edit_others_posts` to write
//!   as someone else), updating `edit_post` and deleting `delete_post`. A delete trashes the post,
//!   or deletes it for good with `force=true`.
//! - Every create and update passes the title, content and excerpt through
//!   [`kses_post_fields`](crate::content::kses_post_fields) for the acting user, on a blocking
//!   thread behind [`KsesLimiter`]; every update records a revision.
//!
//! Mission Control's own codes, where WordPress has none:
//!
//! - `rest_content_too_large` (413): a title, content or excerpt longer than
//!   [`MAX_FILTERED_BYTES`](crate::content::MAX_FILTERED_BYTES) from a user whose writing kses
//!   filters (Ruling R42).
//! - `rest_post_conflict` (412): an update sent with `If-Unmodified-Since` when the post changed
//!   after that time (Ruling R49). Without the header the last write wins, as in WordPress.
//! - `rest_invalid_header` (400): an `If-Unmodified-Since` that is not an HTTP date. The update is
//!   refused rather than made without the check the client asked for.
//!
//! Left out, for now: comments (`comment_status` and `ping_status` read `closed` and writes are
//! checked but not stored; no `replies` link), post formats (`format` reads `standard`; writes
//! are checked, not stored), registered meta (`meta` is `{}` until Task 10.6), the `Allow`
//! header, `before`/`after` and the other date filters, `search_columns`, `tax_relation`,
//! embedding featured media (the media routes do not exist yet), autosaves and revisions routes,
//! and the `?rest_route=` form for these routes.

mod params;
mod prepare;
mod query;
mod write;

pub use params::{MAX_INPUT_VARS, Params, parse_form};

use std::sync::{Arc, RwLock, RwLockReadGuard};

use chrono::FixedOffset;
use orbit_orm::{Database, Model};
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;

use crate::content::{
    FieldTooLarge, MetaError, PermalinkStructure, Post, PostFields, PostStatus, Taxonomy,
    get_option, get_post_meta, kses_post_fields,
};
use crate::front::site_address;
use crate::hooks::Hooks;
use crate::rest::{Endpoint, RestError, RestRoutes, RouteSpec};
use crate::theme::Theme;
use crate::users::{Object, PostContext, PostTypeCaps, SitePages, UserCaps, can, has_cap};

/// A taxonomy as a post type shows it in the REST API: the taxonomy and the name of its field
/// and route (`rest_base`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestTaxonomy {
    /// The taxonomy.
    pub taxonomy: Taxonomy,
    /// Its `rest_base`: the post's field and the taxonomy's route, such as `categories`.
    pub rest_base: &'static str,
}

impl RestTaxonomy {
    /// Categories, at `categories`.
    pub const CATEGORIES: Self = Self {
        taxonomy: Taxonomy::CATEGORY,
        rest_base: "categories",
    };

    /// Tags, at `tags`.
    pub const TAGS: Self = Self {
        taxonomy: Taxonomy::POST_TAG,
        rest_base: "tags",
    };
}

/// The features a post type supports that the REST API reads, as `add_post_type_support` names
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Supports {
    /// `title`.
    pub title: bool,
    /// `editor`: the content.
    pub editor: bool,
    /// `author`.
    pub author: bool,
    /// `excerpt`.
    pub excerpt: bool,
    /// `thumbnail`: the featured image.
    pub thumbnail: bool,
    /// `comments`.
    pub comments: bool,
    /// `revisions`.
    pub revisions: bool,
    /// `page-attributes`: the template, and for a hierarchical type the parent and order.
    pub page_attributes: bool,
    /// `post-formats`.
    pub post_formats: bool,
    /// `custom-fields`: the `meta` field.
    pub custom_fields: bool,
}

impl Supports {
    /// Every feature.
    pub const ALL: Self = Self {
        title: true,
        editor: true,
        author: true,
        excerpt: true,
        thumbnail: true,
        comments: true,
        revisions: true,
        page_attributes: true,
        post_formats: true,
        custom_fields: true,
    };
}

/// A post type as the REST API serves it: what the controller reads from the type's
/// registration (`register_post_type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostType {
    /// The type's name, stored in `posts.post_type`.
    pub name: String,
    /// The route under `wp/v2`, such as `posts`.
    pub rest_base: String,
    /// Whether posts have parents, as pages do.
    pub hierarchical: bool,
    /// Whether the REST API serves the type. A type with `false` has no route and is not listed.
    pub show_in_rest: bool,
    /// The fields the type has.
    pub supports: Supports,
    /// The taxonomies shown in the REST API, in order.
    pub taxonomies: Vec<RestTaxonomy>,
}

impl PostType {
    /// `post`, at `posts`: every field but the page attributes, categories and tags, as
    /// WordPress's REST schema fixes them for posts.
    pub fn post() -> Self {
        Self {
            name: "post".to_owned(),
            rest_base: "posts".to_owned(),
            hierarchical: false,
            show_in_rest: true,
            supports: Supports {
                page_attributes: false,
                ..Supports::ALL
            },
            taxonomies: vec![RestTaxonomy::CATEGORIES, RestTaxonomy::TAGS],
        }
    }

    /// `page`, at `pages`: hierarchical, with no post formats and no taxonomies. WordPress's
    /// schema for pages keeps the excerpt although the type does not declare it.
    pub fn page() -> Self {
        Self {
            name: "page".to_owned(),
            rest_base: "pages".to_owned(),
            hierarchical: true,
            show_in_rest: true,
            supports: Supports {
                post_formats: false,
                ..Supports::ALL
            },
            taxonomies: Vec::new(),
        }
    }

    /// The type's capability names.
    // TODO(Task 5.2): from the registered type.
    pub fn caps(&self) -> PostTypeCaps {
        PostTypeCaps::for_post_type(&self.name)
    }

    /// Whether the type has `parent` and `menu_order`.
    fn has_parent(&self) -> bool {
        self.hierarchical && self.supports.page_attributes
    }

    /// Whether the type has the `sticky` field: `post` only, as in WordPress.
    fn has_sticky(&self) -> bool {
        self.name == "post"
    }

    /// The type's collection route, `/wp/v2/{rest_base}`.
    pub fn route(&self) -> String {
        format!("/{}/{}", super::CORE_NAMESPACE, self.rest_base)
    }
}

/// The post types the REST API serves, shared by the app and the code that registers types.
/// Starts with `post` and `page`.
#[derive(Debug)]
pub struct PostTypes {
    types: RwLock<Vec<PostType>>,
}

impl Default for PostTypes {
    fn default() -> Self {
        Self::new()
    }
}

impl PostTypes {
    /// The registry with `post` and `page`.
    pub fn new() -> Self {
        Self {
            types: RwLock::new(vec![PostType::post(), PostType::page()]),
        }
    }

    /// Adds `post_type`, replacing a type with the same name. Call [`register_routes`]
    /// (Self::register_routes) afterwards so discovery lists it.
    pub fn register(&self, post_type: PostType) {
        let mut types = self.types.write().unwrap_or_else(|e| e.into_inner());
        types.retain(|t| t.name != post_type.name);
        types.push(post_type);
    }

    /// The type served at `rest_base`, if one is shown in the REST API.
    pub fn by_rest_base(&self, rest_base: &str) -> Option<PostType> {
        self.read()
            .iter()
            .find(|t| t.show_in_rest && t.rest_base == rest_base)
            .cloned()
    }

    /// The types shown in the REST API, in the order registered.
    pub fn shown_in_rest(&self) -> Vec<PostType> {
        self.read()
            .iter()
            .filter(|t| t.show_in_rest)
            .cloned()
            .collect()
    }

    /// Registers the routes of every type shown in the REST API on `routes`, so discovery lists
    /// them: the collection (`GET`, `POST`) and the item (`GET`, `POST`/`PUT`/`PATCH`, `DELETE`).
    pub fn register_routes(&self, routes: &RestRoutes) {
        for post_type in self.shown_in_rest() {
            for route in route_specs(&post_type) {
                routes.register(route);
            }
        }
    }

    // A poisoned lock only means a registering thread panicked; the list is still whole.
    fn read(&self) -> RwLockReadGuard<'_, Vec<PostType>> {
        self.types.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// The two routes of `post_type`, with their arguments as WordPress's discovery lists them.
fn route_specs(post_type: &PostType) -> [RouteSpec; 2] {
    let collection = post_type.route();
    let item = format!("{collection}/(?P<id>[\\d]+)");
    let id = json!({"description": "Unique identifier for the post.", "type": "integer", "required": false});
    let context = json!({
        "description": "Scope under which the request is made; determines fields present in response.",
        "type": "string",
        "enum": ["view", "embed", "edit"],
        "default": "view",
        "required": false,
    });
    let mut get_item = Map::new();
    get_item.insert("id".into(), id.clone());
    get_item.insert("context".into(), context);
    get_item.insert(
        "password".into(),
        json!({"description": "The password for the post if it is password protected.", "type": "string", "required": false}),
    );
    let mut update = write::schema_args(post_type);
    update.insert("id".into(), id.clone());
    let delete = json!({
        "id": id,
        "force": {
            "type": "boolean",
            "default": false,
            "description": "Whether to bypass Trash and force deletion.",
            "required": false,
        },
    });
    [
        RouteSpec::new(
            collection,
            vec![
                Endpoint::new(["GET"], Value::Object(query::collection_args(post_type))),
                Endpoint::new(["POST"], Value::Object(write::schema_args(post_type))),
            ],
        ),
        RouteSpec::new(
            item,
            vec![
                Endpoint::new(["GET"], Value::Object(get_item)),
                Endpoint::new(["POST", "PUT", "PATCH"], Value::Object(update)),
                Endpoint::new(["DELETE"], delete),
            ],
        ),
    ]
}

/// Bounds how many kses filters run at once, each on a blocking thread, so a burst of saves
/// cannot take every core (Rulings R37 and R45). The app holds one, shared by every request.
#[derive(Debug, Clone)]
pub struct KsesLimiter {
    permits: Arc<Semaphore>,
}

impl Default for KsesLimiter {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CONCURRENCY)
    }
}

impl KsesLimiter {
    /// How many filters run at once by default.
    pub const DEFAULT_CONCURRENCY: usize = 4;

    /// A limiter that runs at most `concurrency` filters at once (at least one).
    pub fn new(concurrency: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        }
    }

    /// `fields` as they may be stored for `user` ([`kses_post_fields`]), filtered on a blocking
    /// thread once a permit is free.
    ///
    /// # Errors
    ///
    /// `rest_content_too_large` (413) when a field is too long to filter, or a 500 when the
    /// filter's thread fails.
    pub async fn filter(
        &self,
        user: &UserCaps,
        fields: PostFields,
    ) -> Result<PostFields, RestError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|e| {
                log::error!("The kses limiter is closed: {e}");
                RestError::internal()
            })?;
        let user = user.clone();
        let filtered = tokio::task::spawn_blocking(move || {
            let filtered = kses_post_fields(&user, fields);
            drop(permit);
            filtered
        })
        .await
        .map_err(|e| {
            log::error!("The kses filter failed: {e}");
            RestError::internal()
        })?;
        filtered.map_err(|e| too_large(&e))
    }
}

/// `rest_content_too_large`, 413, for a field kses will not filter.
fn too_large(error: &FieldTooLarge) -> RestError {
    RestError::new(
        "rest_content_too_large",
        format!("The {} is too large to save: {error}.", error.field),
        413,
    )
    .with_data("field", json!(error.field))
}

/// The options a request reads, once.
#[derive(Debug, Clone)]
pub(crate) struct Site {
    /// `home`, without a trailing slash.
    pub(crate) home: String,
    /// `permalink_structure`; plain when unset or not valid.
    pub(crate) structure: PermalinkStructure,
    /// `gmt_offset`.
    pub(crate) offset: FixedOffset,
    /// `sticky_posts`.
    pub(crate) sticky: Vec<i64>,
    /// `page_on_front`, `page_for_posts` and `wp_page_for_privacy_policy`.
    pub(crate) pages: SitePages,
    /// `default_category`.
    pub(crate) default_category: Option<i64>,
}

impl Site {
    /// Reads the options. A value that is not valid JSON counts as unset.
    pub(crate) async fn load(db: &Database) -> Result<Self, RestError> {
        let structure = text(&option(db, "permalink_structure").await?)
            .parse()
            .unwrap_or_else(|e| {
                log::warn!(
                    "The permalink_structure option is not valid ({e}); plain links are used."
                );
                PermalinkStructure::plain()
            });
        let hours = number(&option(db, "gmt_offset").await?).unwrap_or(0.0);
        let offset = FixedOffset::east_opt((hours * 3600.0).round() as i32)
            .unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero is a valid offset"));
        let sticky = match option(db, "sticky_posts").await? {
            Some(Value::Array(ids)) => ids.iter().filter_map(id_of).collect(),
            _ => Vec::new(),
        };
        let page = async |name| -> Result<Option<i64>, RestError> {
            Ok(option(db, name).await?.as_ref().and_then(id_of))
        };
        Ok(Self {
            home: site_address(&text(&option(db, "home").await?)),
            structure,
            offset,
            sticky,
            pages: SitePages {
                page_on_front: page("page_on_front").await?,
                page_for_posts: page("page_for_posts").await?,
                privacy_policy: page("wp_page_for_privacy_policy").await?,
            },
            default_category: page("default_category").await?,
        })
    }

    /// `rest_url( path )`: `path` (no leading slash) under the API.
    pub(crate) fn rest_url(&self, path: &str) -> String {
        format!("{}/{}/{path}", self.home, super::discovery::REST_PREFIX)
    }
}

/// Option `name`, with a value that is not valid JSON counted as unset.
async fn option(db: &Database, name: &str) -> Result<Option<Value>, RestError> {
    match get_option(db, name).await {
        Ok(value) => Ok(value),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(database_error(&e)),
    }
}

/// An option's text; empty when unset or not text.
fn text(value: &Option<Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// An option's number, from a JSON number or a numeric string.
fn number(value: &Option<Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A positive id from a JSON number or a numeric string; 0 (WordPress's "none") is `None`.
fn id_of(value: &Value) -> Option<i64> {
    let n = match value {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (n > 0).then_some(n)
}

/// A database error, logged, as WordPress's 500.
pub(crate) fn database_error(error: &dyn std::fmt::Display) -> RestError {
    log::error!("A REST posts request failed: {error}");
    RestError::internal()
}

/// The request's `context`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Context {
    /// `view`, the default.
    View,
    /// `embed`: the fields an embedding response shows.
    Embed,
    /// `edit`: the raw fields, for users who may edit.
    Edit,
}

/// One request to the posts controller for one post type.
///
/// ```ignore
/// let controller = PostsController {
///     db: &db,
///     hooks: &hooks,
///     post_type: &PostType::post(),
///     user: &caps,
///     kses: &kses,
///     theme: Some(&theme),
/// };
/// let response = controller.list("per_page=5").await?;
/// ```
pub struct PostsController<'a> {
    /// The database.
    pub db: &'a Database,
    /// The hooks, for `the_content`.
    pub hooks: &'a Hooks,
    /// The type the route serves.
    pub post_type: &'a PostType,
    /// The current user's capabilities: a visitor's (id 0, no role) when nobody signed in.
    pub user: &'a UserCaps,
    /// Where writes filter the post's HTML.
    pub kses: &'a KsesLimiter,
    /// The active theme, which decides which page templates may be chosen. `None` refuses every
    /// template but the default.
    pub theme: Option<&'a Theme>,
}

impl PostsController<'_> {
    /// `GET /wp/v2/{base}`: page `page` of the posts the query asks for. `query` is the request's
    /// query string.
    ///
    /// # Errors
    ///
    /// WordPress's error for a bad parameter, a context or status the user may not see, or a
    /// page past the end.
    pub async fn list(&self, query: &str) -> Result<super::RestResponse, RestError> {
        query::list(self, query).await
    }

    /// `GET /wp/v2/{base}/{id}`.
    ///
    /// # Errors
    ///
    /// `rest_post_invalid_id` (404) when there is no such post of this type, and WordPress's
    /// errors for a context, a password or a post the user may not read.
    pub async fn get(&self, id: i64, query: &str) -> Result<super::RestResponse, RestError> {
        query::get(self, id, query).await
    }

    /// `POST /wp/v2/{base}`: creates a post from `params` (the body, over the query string).
    ///
    /// # Errors
    ///
    /// WordPress's errors for a bad parameter or a missing capability, and
    /// `rest_content_too_large`.
    pub async fn create(&self, params: &Params) -> Result<super::RestResponse, RestError> {
        write::create(self, params).await
    }

    /// `POST`, `PUT` or `PATCH /wp/v2/{base}/{id}`: updates the post from `params`. With
    /// `if_unmodified_since` (the header's value), the update is made only if the post has not
    /// changed since then.
    ///
    /// # Errors
    ///
    /// As [`create`](Self::create), `rest_post_invalid_id`, and `rest_post_conflict` (412).
    pub async fn update(
        &self,
        id: i64,
        params: &Params,
        if_unmodified_since: Option<&str>,
    ) -> Result<super::RestResponse, RestError> {
        write::update(self, id, params, if_unmodified_since).await
    }

    /// `DELETE /wp/v2/{base}/{id}`: trashes the post, or deletes it with `force`.
    ///
    /// # Errors
    ///
    /// `rest_post_invalid_id`, `rest_cannot_delete` and `rest_already_trashed` (410).
    pub async fn delete(&self, id: i64, params: &Params) -> Result<super::RestResponse, RestError> {
        write::delete(self, id, params).await
    }

    /// Post `id` if it is of this controller's type, or `rest_post_invalid_id` (404).
    async fn find(&self, id: i64) -> Result<Post, RestError> {
        match Post::query().find(self.db, id).await {
            Ok(Some(post)) if post.post_type == self.post_type.name => Ok(post),
            Ok(_) => Err(invalid_id()),
            Err(e) => Err(database_error(&e)),
        }
    }

    /// The status `post` had before it was trashed, for a trashed post.
    async fn trash_status(&self, post: &Post) -> Result<Option<PostStatus>, RestError> {
        if post.status != PostStatus::Trash {
            return Ok(None);
        }
        match get_post_meta(self.db, post.id, "_wp_trash_meta_status").await {
            Ok(Some(Value::String(s))) => Ok(s.parse().ok()),
            Ok(_) | Err(MetaError::InvalidJson(_)) => Ok(None),
            Err(MetaError::Database(e)) => Err(database_error(&e)),
        }
    }

    /// Whether the user may do meta capability `cap` on `post`, with its trash status and the
    /// site's special pages.
    fn can_on(&self, cap: &str, post: &Post, trash: Option<PostStatus>, site: &Site) -> bool {
        let context = PostContext {
            post,
            trash_meta_status: trash,
            parent: None,
            site_pages: Some(site.pages),
        };
        can(self.user, cap, Some(&Object::PostWithContext(context)))
    }

    /// Whether the user may read `post`, as `check_read_permission`: a published post, or one
    /// `read_post` allows. An `inherit` post (an attachment, a revision) is readable when its
    /// parent is, checked the same way up the chain, or when it has no parent (or the parent is
    /// gone), as WordPress decides.
    async fn can_read(
        &self,
        post: &Post,
        trash: Option<PostStatus>,
        site: &Site,
    ) -> Result<bool, RestError> {
        /// How far up a chain of `inherit` posts is followed; a longer chain, or a loop, is
        /// refused.
        const MAX_DEPTH: usize = 16;
        let mut current = post.clone();
        let mut trash = trash;
        for _ in 0..MAX_DEPTH {
            if current.status == PostStatus::Publish
                || self.can_on("read_post", &current, trash, site)
            {
                return Ok(true);
            }
            if current.status != PostStatus::Inherit {
                return Ok(false);
            }
            let Some(parent_id) = current.parent_id.filter(|p| *p > 0 && *p != current.id) else {
                return Ok(true);
            };
            match Post::query().find(self.db, parent_id).await {
                Ok(Some(parent)) => {
                    trash = self.trash_status(&parent).await?;
                    current = parent;
                }
                Ok(None) => return Ok(true),
                Err(e) => return Err(database_error(&e)),
            }
        }
        Ok(false)
    }

    /// Whether the user holds primitive capability `cap`.
    fn has(&self, cap: &str) -> bool {
        has_cap(self.user, cap)
    }

    /// WordPress's `rest_authorization_required_code`: 401 for a visitor, 403 for a user.
    fn auth_status(&self) -> u16 {
        if self.user.id() == 0 { 401 } else { 403 }
    }

    /// An error with the status `rest_authorization_required_code` gives.
    fn forbidden(&self, code: &'static str, message: &'static str) -> RestError {
        RestError::new(code, message, self.auth_status())
    }
}

/// `rest_post_invalid_id`, 404.
fn invalid_id() -> RestError {
    RestError::new("rest_post_invalid_id", "Invalid post ID.", 404)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_types_shown_in_rest_are_served() {
        let types = PostTypes::new();
        types.register(PostType {
            name: "book".into(),
            rest_base: "books".into(),
            hierarchical: false,
            show_in_rest: false,
            supports: Supports::ALL,
            taxonomies: Vec::new(),
        });
        assert!(types.by_rest_base("books").is_none());
        assert_eq!(
            types.by_rest_base("pages").map(|t| t.name),
            Some("page".into())
        );
        let routes = RestRoutes::new();
        types.register_routes(&routes);
        let paths: Vec<String> = routes.routes().into_iter().map(|r| r.path).collect();
        assert!(paths.contains(&"/wp/v2/posts".to_owned()));
        assert!(paths.contains(&"/wp/v2/pages/(?P<id>[\\d]+)".to_owned()));
        assert!(!paths.iter().any(|p| p.contains("books")));

        // Read first: the read guard must be gone before `register` takes the write lock.
        let book = types
            .read()
            .iter()
            .find(|t| t.name == "book")
            .cloned()
            .unwrap();
        types.register(PostType {
            show_in_rest: true,
            ..book
        });
        types.register_routes(&routes);
        assert!(routes.routes().iter().any(|r| r.path == "/wp/v2/books"));
    }

    #[test]
    fn the_item_route_takes_every_method() {
        let [collection, item] = route_specs(&PostType::post());
        assert_eq!(collection.methods(), ["GET", "POST"]);
        assert_eq!(item.methods(), ["GET", "POST", "PUT", "PATCH", "DELETE"]);
    }

    #[test]
    fn field_too_large_is_413() {
        let error = too_large(&FieldTooLarge {
            field: "content",
            size: 10,
        });
        assert_eq!(
            (error.code.as_ref(), error.status),
            ("rest_content_too_large", 413)
        );
    }
}
