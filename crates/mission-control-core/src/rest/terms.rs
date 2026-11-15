//! The terms controller: `/mc/v2/categories`, `/mc/v2/tags` and each taxonomy shown in the REST
//! API, as WordPress's `WP_REST_Terms_Controller` serves them.
//!
//! A [`TaxonomySpec`] describes what the controller reads from a taxonomy's registration: the
//! taxonomy (its name, whether it is hierarchical, its post types), its `rest_base` and whether
//! the REST API shows it. [`Taxonomies`] holds the taxonomies the API serves; it starts with
//! `category` and `post_tag`, and the type registry ([`Registry`](crate::types::Registry))
//! registers custom taxonomies on it. A taxonomy whose
//! `show_in_rest` is false has no route and is not listed in discovery. [`TermsController`]
//! answers one request for one taxonomy, with no Rocket types: the `mission-control` crate
//! mounts it.
//!
//! As WordPress:
//!
//! - A term's JSON has `id`, `count`, `description`, `link`, `name`, `slug`, `taxonomy`,
//!   `parent` (hierarchical taxonomies only), `meta` and `_links`, by `context` (`embed` keeps
//!   `id`, `link`, `name`, `slug` and `taxonomy`). `_fields` keeps only the keys named; `_embed`
//!   embeds the parent (`up`).
//! - Lists take `page`, `per_page` (1 to 100), `search` (in the name and slug), `include`,
//!   `exclude`, `order` (default `asc`), `orderby` (default `name`), `hide_empty`, `parent`
//!   (hierarchical), `offset` (flat), `post` and `slug`, and answer with `X-MC-Total`,
//!   `X-MC-TotalPages` and `Link` headers. A page past the end is an empty list, as WordPress
//!   gives it for terms. With `hide_empty`, a hierarchical taxonomy keeps an empty term that has
//!   a term with posts under it, as `get_terms` keeps it.
//! - Writes check the taxonomy's capabilities ([`TaxonomyCaps`]) through
//!   [`can`]: creating a term of a hierarchical taxonomy needs its
//!   `edit_terms`, of a flat one its `assign_terms` (so an author may add tags but not
//!   categories); updating needs `edit_term` and deleting `delete_term`, which nobody holds on
//!   the default category. Terms have no trash: a delete needs `force=true`.
//! - The name passes through [`sanitize_text_field`](crate::content::sanitize_text_field) and
//!   the description through [`kses_data`](crate::content::kses_data), for every user, as
//!   WordPress's `pre_term_name` and `pre_term_description` filters run for every user; both on a
//!   blocking thread behind [`KsesLimiter`]. The slug is sanitized as
//!   [`make_slug`](crate::content::make_slug) sanitizes a title.
//!
//! Mission Control's codes and statuses, where WordPress has none or answers 500:
//!
//! - `empty_term_name`, `invalid_term_name` (also for a name over
//!   [`MAX_TERM_LENGTH`](crate::content::MAX_TERM_LENGTH) characters), `invalid_term_slug` (a
//!   slug over that length) and `duplicate_term_slug` answer 400. WordPress returns the first,
//!   second and last from `wp_insert_term` and `wp_update_term` without a status, which its REST
//!   server sends as 500.
//! - `rest_content_too_large` (413): a name or description longer than
//!   [`MAX_FILTERED_BYTES`](crate::content::MAX_FILTERED_BYTES) (Ruling R42).
//!
//! `meta` shows the term meta registered in [`MetaKeys`] for the taxonomy, and creates and
//! updates write it, as [`super::meta`] describes.
//!
//! Left out, for now: `/mc/v2/taxonomies`
//! (Task 10.8), the `Allow` header, term aliases (`term_group` is 0), a custom taxonomy's
//! `default_term` and its own query var in links (a custom taxonomy's link is
//! `?taxonomy={name}&term={slug}`), and the `?rest_route=` form for these routes.

mod prepare;
mod query;
mod write;

use std::sync::{RwLock, RwLockReadGuard};

use orbit_orm::Database;
use serde_json::{Map, Value, json};

use super::meta::{MetaKeys, MetaObject};
use super::params::Params;
use super::posts::PostTypes;
use super::shared::{authorization_status, database_error};
use super::{Endpoint, KsesLimiter, RestError, RestResponse, RestRoutes, RouteSpec};
use crate::content::{Taxonomy, Term, default_term_ids, terms_by_term_id};
use crate::users::{Object, TaxonomyCaps, TermContext, UserCaps, can};

/// A taxonomy as the REST API serves it: what the controller reads from the taxonomy's
/// registration (`register_taxonomy`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxonomySpec {
    /// The taxonomy: its name, whether it is hierarchical and its post types.
    pub taxonomy: Taxonomy,
    /// The route under `mc/v2`, such as `categories`.
    pub rest_base: String,
    /// Whether the REST API serves the taxonomy. One with `false` has no route and is not
    /// listed.
    pub show_in_rest: bool,
    /// The taxonomy's capability names: from its registration (its definition's
    /// [`caps`](crate::types::TaxonomyDef::caps)), or [`TaxonomyCaps::for_taxonomy`] for
    /// WordPress's own taxonomies.
    pub caps: TaxonomyCaps,
}

impl TaxonomySpec {
    /// Categories, at `categories`.
    pub fn category() -> Self {
        Self {
            taxonomy: Taxonomy::CATEGORY,
            rest_base: "categories".to_owned(),
            show_in_rest: true,
            caps: TaxonomyCaps::for_taxonomy("category"),
        }
    }

    /// Tags, at `tags`.
    pub fn post_tag() -> Self {
        Self {
            taxonomy: Taxonomy::POST_TAG,
            rest_base: "tags".to_owned(),
            show_in_rest: true,
            caps: TaxonomyCaps::for_taxonomy("post_tag"),
        }
    }

    /// The taxonomy's capability names: [`caps`](Self::caps).
    pub fn caps(&self) -> TaxonomyCaps {
        self.caps.clone()
    }

    /// The taxonomy's collection route, `/mc/v2/{rest_base}`.
    pub fn route(&self) -> String {
        format!("/{}/{}", super::CORE_NAMESPACE, self.rest_base)
    }
}

/// The taxonomies the REST API serves, shared by the app and the code that registers
/// taxonomies. Starts with `category` and `post_tag`.
#[derive(Debug)]
pub struct Taxonomies {
    taxonomies: RwLock<Vec<TaxonomySpec>>,
}

impl Default for Taxonomies {
    fn default() -> Self {
        Self::new()
    }
}

impl Taxonomies {
    /// The registry with `category` and `post_tag`.
    pub fn new() -> Self {
        Self {
            taxonomies: RwLock::new(vec![TaxonomySpec::category(), TaxonomySpec::post_tag()]),
        }
    }

    /// Adds `taxonomy`, replacing one with the same name. Call
    /// [`register_routes`](Self::register_routes) afterwards so discovery lists it.
    pub fn register(&self, taxonomy: TaxonomySpec) {
        let mut taxonomies = self.taxonomies.write().unwrap_or_else(|e| e.into_inner());
        taxonomies.retain(|t| t.taxonomy.name != taxonomy.taxonomy.name);
        taxonomies.push(taxonomy);
    }

    /// The taxonomy served at `rest_base`, if one is shown in the REST API.
    pub fn by_rest_base(&self, rest_base: &str) -> Option<TaxonomySpec> {
        self.read()
            .iter()
            .find(|t| t.show_in_rest && t.rest_base == rest_base)
            .cloned()
    }

    /// Removes the taxonomy named `name`. True when there was one. Its routes stay in a
    /// [`RestRoutes`] they were registered on, but no request is served on them.
    pub fn unregister(&self, name: &str) -> bool {
        let mut taxonomies = self.taxonomies.write().unwrap_or_else(|e| e.into_inner());
        let before = taxonomies.len();
        taxonomies.retain(|t| t.taxonomy.name != name);
        taxonomies.len() != before
    }

    /// The taxonomy named `name`, whether the REST API shows it or not.
    pub fn registered(&self, name: &str) -> Option<TaxonomySpec> {
        self.read()
            .iter()
            .find(|t| t.taxonomy.name == name)
            .cloned()
    }

    /// The taxonomies shown in the REST API, in the order registered.
    pub fn shown_in_rest(&self) -> Vec<TaxonomySpec> {
        self.read()
            .iter()
            .filter(|t| t.show_in_rest)
            .cloned()
            .collect()
    }

    /// Registers the routes of every taxonomy shown in the REST API on `routes`, so discovery
    /// lists them: the collection (`GET`, `POST`) and the item (`GET`, `POST`/`PUT`/`PATCH`,
    /// `DELETE`).
    pub fn register_routes(&self, routes: &RestRoutes) {
        for taxonomy in self.shown_in_rest() {
            for route in route_specs(&taxonomy) {
                routes.register(route);
            }
        }
    }

    // A poisoned lock only means a registering thread panicked; the list is still whole.
    fn read(&self) -> RwLockReadGuard<'_, Vec<TaxonomySpec>> {
        self.taxonomies.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// The two routes of `taxonomy`, with their arguments as WordPress's discovery lists them.
fn route_specs(taxonomy: &TaxonomySpec) -> [RouteSpec; 2] {
    let collection = taxonomy.route();
    let item = format!("{collection}/(?P<id>[\\d]+)");
    let id = json!({"description": "Unique identifier for the term.", "type": "integer", "required": false});
    let mut get_item = Map::new();
    get_item.insert("id".into(), id.clone());
    get_item.insert("context".into(), query::context_arg());
    let mut update = write::schema_args(taxonomy, false);
    update.insert("id".into(), id.clone());
    let delete = json!({
        "id": id,
        "force": {
            "type": "boolean",
            "default": false,
            "description": "Required to be true, as terms do not support trashing.",
            "required": false,
        },
    });
    [
        RouteSpec::new(
            collection,
            vec![
                Endpoint::new(["GET"], Value::Object(query::collection_args(taxonomy))),
                Endpoint::new(["POST"], Value::Object(write::schema_args(taxonomy, true))),
            ],
        )
        .with_meta(MetaObject::Term, &*taxonomy.taxonomy.name),
        RouteSpec::new(
            item,
            vec![
                Endpoint::new(["GET"], Value::Object(get_item)),
                Endpoint::new(["POST", "PUT", "PATCH"], Value::Object(update)),
                Endpoint::new(["DELETE"], delete),
            ],
        )
        .with_meta(MetaObject::Term, &*taxonomy.taxonomy.name),
    ]
}

/// One request to the terms controller for one taxonomy.
///
/// ```ignore
/// let controller = TermsController {
///     db: &db,
///     taxonomy: &TaxonomySpec::category(),
///     post_types: &post_types,
///     user: &caps,
///     kses: &kses,
///     meta: &meta_keys,
/// };
/// let response = controller.list("hide_empty=true").await?;
/// ```
pub struct TermsController<'a> {
    /// The database.
    pub db: &'a Database,
    /// The taxonomy the route serves.
    pub taxonomy: &'a TaxonomySpec,
    /// The post types the REST API serves, for the links to a term's posts.
    pub post_types: &'a PostTypes,
    /// The current user's capabilities: a visitor's (id 0, no role) when nobody signed in.
    pub user: &'a UserCaps,
    /// Where writes filter the term's name and description.
    pub kses: &'a KsesLimiter,
    /// The registered meta keys, shown and written under `meta`.
    pub meta: &'a MetaKeys,
}

impl TermsController<'_> {
    /// `GET /mc/v2/{base}`: page `page` of the terms the query asks for. `query` is the request's
    /// query string.
    ///
    /// # Errors
    ///
    /// WordPress's error for a bad parameter, a context the user may not see, or a `post` whose
    /// terms the user may not see.
    pub async fn list(&self, query: &str) -> Result<RestResponse, RestError> {
        query::list(self, query).await
    }

    /// `GET /mc/v2/{base}/{id}`.
    ///
    /// # Errors
    ///
    /// `rest_term_invalid` (404) when there is no such term in this taxonomy, and
    /// `rest_forbidden_context` for the edit context without `edit_term`.
    pub async fn get(&self, id: i64, query: &str) -> Result<RestResponse, RestError> {
        query::get(self, id, query).await
    }

    /// `POST /mc/v2/{base}`: creates a term from `params` (the body, over the query string).
    ///
    /// # Errors
    ///
    /// WordPress's errors for a missing or bad parameter, a missing capability, a parent that
    /// does not exist, or a term that exists already (`term_exists`).
    pub async fn create(&self, params: &Params) -> Result<RestResponse, RestError> {
        write::create(self, params).await
    }

    /// `POST`, `PUT` or `PATCH /mc/v2/{base}/{id}`: updates the term from `params`.
    ///
    /// # Errors
    ///
    /// As [`create`](Self::create), `rest_term_invalid`, `rest_cannot_update` and
    /// `duplicate_term_slug`.
    pub async fn update(&self, id: i64, params: &Params) -> Result<RestResponse, RestError> {
        write::update(self, id, params).await
    }

    /// `DELETE /mc/v2/{base}/{id}`: deletes the term, which needs `force=true`.
    ///
    /// # Errors
    ///
    /// `rest_term_invalid`, `rest_cannot_delete`, and `rest_trash_not_supported` (501) without
    /// `force`.
    pub async fn delete(&self, id: i64, params: &Params) -> Result<RestResponse, RestError> {
        write::delete(self, id, params).await
    }

    /// Term `id` (a `term_id`) of this taxonomy, or `rest_term_invalid` (404), as the
    /// controller's `get_term`.
    async fn find(&self, id: i64) -> Result<Term, RestError> {
        if id <= 0 {
            return Err(invalid_term());
        }
        self.find_in(id).await?.ok_or_else(invalid_term)
    }

    /// Term `id` of this taxonomy, if there is one.
    async fn find_in(&self, id: i64) -> Result<Option<Term>, RestError> {
        let mut found = terms_by_term_id(self.db, &self.taxonomy.taxonomy, &[id])
            .await
            .map_err(|e| database_error(&e))?;
        Ok(found.pop())
    }

    /// The taxonomy's default terms, which nobody may delete: the `default_{taxonomy}` and
    /// `default_term_{taxonomy}` options, as WordPress's `delete_term` check reads them.
    async fn default_terms(&self) -> Result<Vec<i64>, RestError> {
        default_term_ids(self.db, &self.taxonomy.taxonomy.name)
            .await
            .map_err(|e| database_error(&e))
    }

    /// Whether the user may do term meta capability `cap` (`edit_term`, `delete_term`) on `term`.
    fn can_on(&self, cap: &str, term: &Term, default_terms: &[i64]) -> bool {
        let context = TermContext {
            term_id: term.term_id,
            taxonomy: &term.taxonomy,
            default_terms,
            caps: Some(&self.taxonomy.caps),
        };
        can(self.user, cap, Some(&Object::Term(context)))
    }

    /// Whether the user holds the taxonomy capability `cap` (such as its `edit_terms`), mapped
    /// as `current_user_can` maps it.
    fn has(&self, cap: &str) -> bool {
        can(self.user, cap, None)
    }

    /// An error with the status `rest_authorization_required_code` gives.
    fn forbidden(&self, code: &'static str, message: &'static str) -> RestError {
        RestError::new(code, message, authorization_status(self.user))
    }
}

/// `rest_term_invalid`, 404.
fn invalid_term() -> RestError {
    RestError::new("rest_term_invalid", "Term does not exist.", 404)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_taxonomies_shown_in_rest_are_served() {
        let taxonomies = Taxonomies::new();
        taxonomies.register(TaxonomySpec {
            show_in_rest: false,
            ..TaxonomySpec::post_tag()
        });
        assert!(taxonomies.by_rest_base("tags").is_none());
        assert_eq!(
            taxonomies.by_rest_base("categories").map(|t| t.taxonomy),
            Some(Taxonomy::CATEGORY)
        );
        let routes = RestRoutes::new();
        taxonomies.register_routes(&routes);
        let paths: Vec<String> = routes.routes().into_iter().map(|r| r.path).collect();
        assert!(paths.contains(&"/mc/v2/categories".to_owned()));
        assert!(paths.contains(&"/mc/v2/categories/(?P<id>[\\d]+)".to_owned()));
        assert!(!paths.iter().any(|p| p.contains("tags")));

        taxonomies.register(TaxonomySpec::post_tag());
        taxonomies.register_routes(&routes);
        assert!(routes.routes().iter().any(|r| r.path == "/mc/v2/tags"));
    }

    #[test]
    fn the_routes_take_wordpresss_methods_and_arguments() {
        let [collection, item] = route_specs(&TaxonomySpec::category());
        assert_eq!(collection.methods(), ["GET", "POST"]);
        assert_eq!(item.methods(), ["GET", "POST", "PUT", "PATCH", "DELETE"]);
        let create = &collection.endpoints[1].args;
        assert_eq!(create["name"]["required"], json!(true));
        assert!(create.contains_key("parent"));
        let list = &collection.endpoints[0].args;
        assert!(list.contains_key("parent") && !list.contains_key("offset"));

        let [collection, item] = route_specs(&TaxonomySpec::post_tag());
        let list = &collection.endpoints[0].args;
        assert!(!list.contains_key("parent") && list.contains_key("offset"));
        assert!(!item.endpoints[1].args.contains_key("parent"));
        assert_eq!(item.endpoints[1].args["name"]["required"], json!(false));
    }
}
