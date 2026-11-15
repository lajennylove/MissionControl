//! The routes that describe the site's content model: `/mc/v2/types`, `/mc/v2/taxonomies` and
//! `/mc/v2/statuses`, as WordPress's `WP_REST_Post_Types_Controller`,
//! `WP_REST_Taxonomies_Controller` and `WP_REST_Post_Statuses_Controller` serve them.
//! [`MetaRoutesController`] answers one request, with no Rocket types: the `mission-control`
//! crate mounts it.
//!
//! As WordPress:
//!
//! - `types` lists the post types shown in the REST API ([`PostTypes`], with `attachment` after
//!   the built-in `post` and `page`), keyed by name. A type's JSON has `description`,
//!   `hierarchical`, `has_archive`, `name`, `slug`, `icon`, `taxonomies`, `rest_base`,
//!   `rest_namespace`, `template`, `template_lock` and `_links` (`collection`, `mc:items`); the
//!   edit context adds `capabilities`, `labels`, `supports`, `viewable` and `visibility`, and needs
//!   the type's `edit_posts` (the list keeps only the types the user may edit, and refuses with
//!   `rest_cannot_view` when there is none). `embed` keeps `name`, `slug`, `icon`, `rest_base`
//!   and `rest_namespace`. A type not shown in the REST API is `rest_cannot_read_type`; an unknown
//!   one `rest_type_invalid` (404).
//! - `taxonomies` lists the taxonomies shown in the REST API ([`Taxonomies`]), keyed by name,
//!   filtered by `type` (a post type). A taxonomy's JSON has `name`, `slug`, `description`,
//!   `types`, `hierarchical`, `rest_base`, `rest_namespace` and `_links`; the edit context adds
//!   `capabilities` ([`TaxonomyCaps`]), `labels`, `show_cloud` and `visibility`, and needs the
//!   taxonomy's `assign_terms`.
//! - `statuses` lists `publish`, `future`, `draft`, `pending`, `private` and `trash`, keyed by
//!   slug: `publish` for everyone, the others for a user who may edit posts of some type shown in
//!   the REST API. A status's JSON has `name`, `public`, `queryable`, `slug`, `date_floating` and
//!   `_links` (`archives`); the edit context adds `private`, `protected` and `show_in_list`. The
//!   internal statuses (`auto-draft`, `inherit`) are `rest_cannot_read_status`.
//! - `_fields` keeps only the keys named.
//!
//! Until the post type and taxonomy registries (Tasks 5.2 and 5.x) hold them, a custom type or
//! taxonomy is described by WordPress's defaults: its name as its label, an empty description,
//! no archive, public, and every label WordPress fills in for a type or taxonomy registered with
//! no labels ([`labels`]).

mod labels;

use serde_json::{Map, Value, json};

use super::params::{Reader, parse_form};
use super::posts::{PostType, PostTypes};
use super::shared::read_envelope;
use super::shared::{Context, Fields, Site, authorization_status, curies, read_context};
use super::terms::{Taxonomies, TaxonomySpec};
use super::{CORE_NAMESPACE, Endpoint, RestError, RestResponse, RestRoutes, RouteSpec};
use crate::users::{PostTypeCaps, UserCaps, can};
use orbit_orm::Database;

/// The view context, as a bit of a field's contexts.
const VIEW: u8 = 1;
/// The embed context.
const EMBED: u8 = 2;
/// The edit context.
const EDIT: u8 = 4;

/// A post type's fields and their contexts, as `WP_REST_Post_Types_Controller`'s schema lists
/// them.
const TYPE_FIELDS: &[(&str, u8)] = &[
    ("capabilities", EDIT),
    ("description", VIEW | EDIT),
    ("hierarchical", VIEW | EDIT),
    ("has_archive", VIEW | EDIT),
    ("visibility", EDIT),
    ("viewable", EDIT),
    ("labels", EDIT),
    ("name", VIEW | EDIT | EMBED),
    ("slug", VIEW | EDIT | EMBED),
    ("icon", VIEW | EDIT | EMBED),
    ("supports", EDIT),
    ("taxonomies", VIEW | EDIT),
    ("rest_base", VIEW | EDIT | EMBED),
    ("rest_namespace", VIEW | EDIT | EMBED),
    ("template", VIEW | EDIT),
    ("template_lock", VIEW | EDIT),
];

/// A taxonomy's fields and their contexts, as `WP_REST_Taxonomies_Controller`'s schema lists
/// them.
const TAXONOMY_FIELDS: &[(&str, u8)] = &[
    ("name", VIEW | EDIT | EMBED),
    ("slug", VIEW | EDIT | EMBED),
    ("capabilities", EDIT),
    ("description", VIEW | EDIT),
    ("labels", EDIT),
    ("types", VIEW | EDIT),
    ("show_cloud", EDIT),
    ("hierarchical", VIEW | EDIT),
    ("rest_base", VIEW | EDIT | EMBED),
    ("rest_namespace", VIEW | EDIT | EMBED),
    ("visibility", EDIT),
];

/// A status's fields and their contexts, as `WP_REST_Post_Statuses_Controller`'s schema lists
/// them.
const STATUS_FIELDS: &[(&str, u8)] = &[
    ("name", VIEW | EDIT | EMBED),
    ("private", EDIT),
    ("protected", EDIT),
    ("public", VIEW | EDIT),
    ("queryable", VIEW | EDIT),
    ("show_in_list", EDIT),
    ("slug", VIEW | EDIT | EMBED),
    ("date_floating", VIEW | EDIT),
];

/// A post status as `register_post_status` registers WordPress's own.
#[derive(Debug, Clone, Copy)]
struct Status {
    slug: &'static str,
    label: &'static str,
    public: bool,
    private: bool,
    protected: bool,
    internal: bool,
    queryable: bool,
    show_in_list: bool,
    date_floating: bool,
}

impl Status {
    const fn new(slug: &'static str, label: &'static str) -> Self {
        Self {
            slug,
            label,
            public: false,
            private: false,
            protected: false,
            internal: false,
            queryable: false,
            show_in_list: true,
            date_floating: false,
        }
    }
}

/// WordPress's statuses, in the order `create_initial_post_types` registers them.
const STATUSES: &[Status] = &[
    Status {
        public: true,
        queryable: true,
        ..Status::new("publish", "Published")
    },
    Status {
        protected: true,
        ..Status::new("future", "Scheduled")
    },
    Status {
        protected: true,
        date_floating: true,
        ..Status::new("draft", "Draft")
    },
    Status {
        protected: true,
        date_floating: true,
        ..Status::new("pending", "Pending")
    },
    Status {
        private: true,
        ..Status::new("private", "Private")
    },
    Status {
        internal: true,
        show_in_list: false,
        ..Status::new("trash", "Trash")
    },
    Status {
        internal: true,
        show_in_list: false,
        date_floating: true,
        ..Status::new("auto-draft", "auto-draft")
    },
    Status {
        internal: true,
        show_in_list: false,
        ..Status::new("inherit", "inherit")
    },
];

/// The `context` argument of these routes.
fn context_arg() -> Value {
    json!({
        "description": "Scope under which the request is made; determines fields present in response.",
        "type": "string",
        "enum": ["view", "embed", "edit"],
        "default": "view",
        "required": false,
    })
}

/// Registers the types, taxonomies and statuses routes on `routes`, so discovery lists them:
/// each a collection and an item, `GET` only.
pub fn register_routes(routes: &RestRoutes) {
    let item = |path: &str, name: &str, description: &str| {
        let mut args = Map::new();
        args.insert(
            name.to_owned(),
            json!({"description": description, "type": "string", "required": false}),
        );
        args.insert("context".to_owned(), context_arg());
        RouteSpec::new(
            format!("/{CORE_NAMESPACE}/{path}/(?P<{name}>[\\w-]+)"),
            vec![Endpoint::new(["GET"], Value::Object(args))],
        )
    };
    routes.register(RouteSpec::new(
        "/mc/v2/types",
        vec![Endpoint::new(["GET"], json!({"context": context_arg()}))],
    ));
    routes.register(item(
        "types",
        "type",
        "An alphanumeric identifier for the post type.",
    ));
    routes.register(RouteSpec::new(
        "/mc/v2/taxonomies",
        vec![Endpoint::new(
            ["GET"],
            json!({
                "context": context_arg(),
                "type": {
                    "description": "Limit results to taxonomies associated with a specific post type.",
                    "type": "string",
                    "required": false,
                },
            }),
        )],
    ));
    routes.register(item(
        "taxonomies",
        "taxonomy",
        "An alphanumeric identifier for the taxonomy.",
    ));
    routes.register(RouteSpec::new(
        "/mc/v2/statuses",
        vec![Endpoint::new(["GET"], json!({"context": context_arg()}))],
    ));
    routes.register(item(
        "statuses",
        "status",
        "An alphanumeric identifier for the status.",
    ));
}

/// One request to the types, taxonomies or statuses routes.
///
/// ```ignore
/// let controller = MetaRoutesController {
///     db: &db,
///     post_types: &post_types,
///     taxonomies: &taxonomies,
///     user: &caps,
/// };
/// let response = controller.types("context=edit").await?;
/// ```
pub struct MetaRoutesController<'a> {
    /// The database, for the site's address in links.
    pub db: &'a Database,
    /// The post types the REST API serves.
    pub post_types: &'a PostTypes,
    /// The taxonomies the REST API serves.
    pub taxonomies: &'a Taxonomies,
    /// The current user's capabilities: a visitor's (id 0, no role) when nobody signed in.
    pub user: &'a UserCaps,
}

/// The request's `context` and `_fields`, and its `type` filter when `with_type`.
struct Request {
    context: Context,
    fields: Fields,
    post_type: Option<String>,
}

impl Request {
    fn read(query: &str, with_type: bool) -> Result<Self, RestError> {
        let params = parse_form(query);
        let mut r = Reader::new(&params);
        let context = read_context(&mut r);
        let post_type = if with_type {
            r.string("type").filter(|t| !t.is_empty())
        } else {
            None
        };
        r.invalid.finish()?;
        let (fields, _) = read_envelope(&params);
        Ok(Self {
            context,
            fields,
            post_type,
        })
    }
}

impl MetaRoutesController<'_> {
    /// `GET /mc/v2/types`: the post types shown in the REST API, keyed by name.
    ///
    /// # Errors
    ///
    /// `rest_invalid_param` for a bad `context`, and `rest_cannot_view` for the edit context when
    /// the user may edit no type.
    pub async fn types(&self, query: &str) -> Result<RestResponse, RestError> {
        let request = Request::read(query, false)?;
        let types = self.rest_types();
        if request.context == Context::Edit
            && !types
                .iter()
                .any(|t| can(self.user, &t.caps().edit_posts, None))
        {
            return Err(self.forbidden(
                "rest_cannot_view",
                "Sorry, you are not allowed to edit posts in this post type.",
            ));
        }
        let site = Site::load(self.db).await?;
        let mut data = Map::new();
        for post_type in &types {
            if request.context == Context::Edit && !can(self.user, &edit_posts_of(post_type), None)
            {
                continue;
            }
            data.insert(
                post_type.name.clone(),
                Value::Object(self.type_json(&site, post_type, &request, false)),
            );
        }
        Ok(RestResponse::ok(Value::Object(request.fields.filter(data))))
    }

    /// `GET /mc/v2/types/{type}`.
    ///
    /// # Errors
    ///
    /// `rest_type_invalid` (404) for an unknown type, `rest_cannot_read_type` for one not shown
    /// in the REST API, and `rest_forbidden_context` for the edit context without the type's
    /// `edit_posts`.
    pub async fn post_type(&self, name: &str, query: &str) -> Result<RestResponse, RestError> {
        let request = Request::read(query, false)?;
        let post_type = if name == "attachment" {
            Some(PostType::attachment())
        } else {
            self.post_types.registered(name)
        };
        let Some(post_type) = post_type else {
            return Err(RestError::new(
                "rest_type_invalid",
                "Invalid post type.",
                404,
            ));
        };
        if !post_type.show_in_rest {
            return Err(self.forbidden("rest_cannot_read_type", "Cannot view post type."));
        }
        if request.context == Context::Edit && !can(self.user, &edit_posts_of(&post_type), None) {
            return Err(self.forbidden(
                "rest_forbidden_context",
                "Sorry, you are not allowed to edit posts in this post type.",
            ));
        }
        let site = Site::load(self.db).await?;
        let item = self.type_json(&site, &post_type, &request, true);
        Ok(RestResponse::ok(Value::Object(item)))
    }

    /// `GET /mc/v2/taxonomies`: the taxonomies shown in the REST API, keyed by name; with
    /// `type`, those of that post type.
    ///
    /// # Errors
    ///
    /// `rest_invalid_param` for a bad `context` or `type`, and `rest_cannot_view` for the edit
    /// context when the user may assign the terms of none of them.
    pub async fn taxonomies(&self, query: &str) -> Result<RestResponse, RestError> {
        let request = Request::read(query, true)?;
        let taxonomies: Vec<TaxonomySpec> = self
            .taxonomies
            .shown_in_rest()
            .into_iter()
            .filter(|t| {
                request
                    .post_type
                    .as_deref()
                    .is_none_or(|post_type| t.taxonomy.is_for(post_type))
            })
            .collect();
        let may_assign = |t: &TaxonomySpec| can(self.user, &t.caps().assign_terms, None);
        if request.context == Context::Edit && !taxonomies.iter().any(may_assign) {
            return Err(self.forbidden(
                "rest_cannot_view",
                "Sorry, you are not allowed to manage terms in this taxonomy.",
            ));
        }
        let site = Site::load(self.db).await?;
        let mut data = Map::new();
        for taxonomy in &taxonomies {
            if request.context == Context::Edit && !may_assign(taxonomy) {
                continue;
            }
            data.insert(
                taxonomy.taxonomy.name.to_string(),
                Value::Object(taxonomy_json(&site, taxonomy, &request, false)),
            );
        }
        Ok(RestResponse::ok(Value::Object(request.fields.filter(data))))
    }

    /// `GET /mc/v2/taxonomies/{taxonomy}`.
    ///
    /// # Errors
    ///
    /// `rest_taxonomy_invalid` (404) for an unknown taxonomy, `rest_forbidden` for one not shown
    /// in the REST API, and `rest_forbidden_context` for the edit context without its
    /// `assign_terms`.
    pub async fn taxonomy(&self, name: &str, query: &str) -> Result<RestResponse, RestError> {
        let request = Request::read(query, false)?;
        let Some(taxonomy) = self.taxonomies.registered(name) else {
            return Err(RestError::new(
                "rest_taxonomy_invalid",
                "Invalid taxonomy.",
                404,
            ));
        };
        if !taxonomy.show_in_rest {
            return Err(self.forbidden("rest_forbidden", "Sorry, you are not allowed to do that."));
        }
        if request.context == Context::Edit && !can(self.user, &taxonomy.caps().assign_terms, None)
        {
            return Err(self.forbidden(
                "rest_forbidden_context",
                "Sorry, you are not allowed to manage terms in this taxonomy.",
            ));
        }
        let site = Site::load(self.db).await?;
        Ok(RestResponse::ok(Value::Object(taxonomy_json(
            &site, &taxonomy, &request, true,
        ))))
    }

    /// `GET /mc/v2/statuses`: the statuses the user may see, keyed by slug.
    ///
    /// # Errors
    ///
    /// `rest_invalid_param` for a bad `context`, and `rest_cannot_view` for the edit context when
    /// the user may edit no type.
    pub async fn statuses(&self, query: &str) -> Result<RestResponse, RestError> {
        let request = Request::read(query, false)?;
        if request.context == Context::Edit && !self.edits_some_type() {
            return Err(self.forbidden(
                "rest_cannot_view",
                "Sorry, you are not allowed to manage post statuses.",
            ));
        }
        let site = Site::load(self.db).await?;
        let mut data = Map::new();
        for status in STATUSES.iter().filter(|s| !s.internal || s.slug == "trash") {
            if self.may_read_status(status) {
                data.insert(
                    status.slug.to_owned(),
                    Value::Object(status_json(&site, status, &request, false)),
                );
            }
        }
        Ok(RestResponse::ok(Value::Object(request.fields.filter(data))))
    }

    /// `GET /mc/v2/statuses/{status}`.
    ///
    /// # Errors
    ///
    /// `rest_status_invalid` (404) for an unknown status, and `rest_cannot_read_status` for one
    /// the user may not see.
    pub async fn status(&self, slug: &str, query: &str) -> Result<RestResponse, RestError> {
        let request = Request::read(query, false)?;
        let Some(status) = STATUSES.iter().find(|s| s.slug == slug) else {
            return Err(RestError::new(
                "rest_status_invalid",
                "Invalid status.",
                404,
            ));
        };
        if !self.may_read_status(status) {
            return Err(self.forbidden("rest_cannot_read_status", "Cannot view status."));
        }
        let site = Site::load(self.db).await?;
        Ok(RestResponse::ok(Value::Object(status_json(
            &site, status, &request, true,
        ))))
    }

    /// The post types shown in the REST API, as `get_post_types( array( 'show_in_rest' => true
    /// ) )` orders them: `post` and `page`, then `attachment`, then the others as registered.
    fn rest_types(&self) -> Vec<PostType> {
        let (mut types, custom): (Vec<PostType>, Vec<PostType>) = self
            .post_types
            .shown_in_rest()
            .into_iter()
            .partition(|t| t.name == "post" || t.name == "page");
        types.sort_by_key(|t| t.name != "post");
        types.push(PostType::attachment());
        types.extend(custom);
        types
    }

    /// Whether the user may edit posts of some type shown in the REST API.
    fn edits_some_type(&self) -> bool {
        self.rest_types()
            .iter()
            .any(|t| can(self.user, &edit_posts_of(t), None))
    }

    /// `check_read_permission`: a public status, or for a user who may edit posts of some type,
    /// a status that is not internal, or `trash`.
    fn may_read_status(&self, status: &Status) -> bool {
        status.public || ((!status.internal || status.slug == "trash") && self.edits_some_type())
    }

    /// The JSON of `post_type` for the request. `filter` applies `_fields` to the item, as
    /// WordPress filters a single item's response.
    fn type_json(
        &self,
        site: &Site,
        post_type: &PostType,
        request: &Request,
        filter: bool,
    ) -> Map<String, Value> {
        let taxonomies: Vec<String> = self
            .taxonomies
            .shown_in_rest()
            .into_iter()
            .filter(|t| t.taxonomy.is_for(&post_type.name))
            .map(|t| t.taxonomy.name.into_owned())
            .collect();
        let labels = labels::post_type(post_type);
        let mut item = Map::new();
        let mut put = |key: &str, value: Value| {
            item.insert(key.to_owned(), value);
        };
        put("capabilities", type_capabilities(post_type));
        put("description", json!(""));
        put("hierarchical", json!(post_type.hierarchical));
        put("has_archive", json!(false));
        put(
            "visibility",
            json!({
                "show_in_nav_menus": !post_type.is_attachment(),
                "show_ui": true,
            }),
        );
        put("viewable", json!(true));
        put("name", labels["name"].clone());
        put("labels", labels);
        put("slug", json!(post_type.name));
        put("icon", icon(post_type));
        put("supports", supports(post_type));
        put("taxonomies", json!(taxonomies));
        put("rest_base", json!(post_type.rest_base));
        put("rest_namespace", json!(CORE_NAMESPACE));
        put("template", json!([]));
        put("template_lock", json!(false));
        let links = json!({
            "collection": [{"href": site.rest_url("mc/v2/types")}],
            "mc:items": [{"href": site.rest_url(&format!("mc/v2/{}", post_type.rest_base))}],
            "curies": curies(),
        });
        finish(item, TYPE_FIELDS, request, links, filter)
    }

    /// WordPress's `rest_authorization_required_code` error.
    fn forbidden(&self, code: &'static str, message: &'static str) -> RestError {
        RestError::new(code, message, authorization_status(self.user))
    }
}

/// The type's `edit_posts` capability.
fn edit_posts_of(post_type: &PostType) -> String {
    post_type.caps().edit_posts
}

/// `item` with the keys of `context` (as `filter_response_by_context`), the keys `_fields` names
/// (as `get_fields_for_response`), and `links` when `_links` is wanted. With `filter`, `_fields`
/// also filters nested keys, as `rest_filter_response_fields` filters a single item.
fn finish(
    item: Map<String, Value>,
    table: &[(&str, u8)],
    request: &Request,
    links: Value,
    filter: bool,
) -> Map<String, Value> {
    let bit = match request.context {
        Context::View => VIEW,
        Context::Embed => EMBED,
        Context::Edit => EDIT,
    };
    let mut kept: Map<String, Value> = item
        .into_iter()
        .filter(|(key, _)| {
            table
                .iter()
                .any(|(name, contexts)| name == key && contexts & bit != 0)
                && request.fields.includes(key)
        })
        .collect();
    if request.fields.includes("_links") {
        kept.insert("_links".to_owned(), links);
    }
    if filter {
        request.fields.filter(kept)
    } else {
        kept
    }
}

/// The type's `cap` object, as `get_post_type_capabilities` builds it for a type that maps meta
/// capabilities: the meta capabilities first, then the primitive ones, then `create_posts`
/// (`upload_files` for attachments).
fn type_capabilities(post_type: &PostType) -> Value {
    // The meta capability names come from the capability type's singular (`edit_story` for
    // `["story", "stories"]`); a type WordPress does not know has every name one capability.
    let caps: PostTypeCaps = post_type.caps();
    let create = if post_type.is_attachment() {
        "upload_files".to_owned()
    } else {
        caps.edit_posts.clone()
    };
    json!({
        "edit_post": caps.edit_post,
        "read_post": caps.read_post,
        "delete_post": caps.delete_post,
        "edit_posts": caps.edit_posts,
        "edit_others_posts": caps.edit_others_posts,
        "delete_posts": caps.delete_posts,
        "publish_posts": caps.publish_posts,
        "read_private_posts": caps.read_private_posts,
        "read": caps.read,
        "delete_private_posts": caps.delete_private_posts,
        "delete_published_posts": caps.delete_published_posts,
        "delete_others_posts": caps.delete_others_posts,
        "edit_private_posts": caps.edit_private_posts,
        "edit_published_posts": caps.edit_published_posts,
        "create_posts": create,
    })
}

/// The type's menu icon: WordPress's dashicons for its own types, none for others.
fn icon(post_type: &PostType) -> Value {
    match post_type.name.as_str() {
        "post" => json!("dashicons-admin-post"),
        "page" => json!("dashicons-admin-page"),
        "attachment" => json!("dashicons-admin-media"),
        _ => Value::Null,
    }
}

/// `get_all_post_type_supports`: each feature the type supports, set to `true`. WordPress's own
/// types declare what `create_initial_post_types` gives them: `post` also `trackbacks`; `page`
/// no `excerpt` (Mission Control's schema for pages keeps one, as WordPress's does); and
/// `attachment` only `title`, `author` and `comments`.
fn supports(post_type: &PostType) -> Value {
    let s = post_type.supports;
    let is = |name: &str| post_type.name == name;
    let features = [
        ("title", s.title),
        ("editor", s.editor),
        ("author", s.author),
        ("thumbnail", s.thumbnail),
        ("excerpt", s.excerpt && !is("page")),
        ("trackbacks", is("post")),
        ("page-attributes", s.page_attributes),
        ("custom-fields", s.custom_fields && !is("attachment")),
        ("comments", s.comments),
        ("revisions", s.revisions),
        ("post-formats", s.post_formats),
    ];
    Value::Object(
        features
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| (name.to_owned(), Value::Bool(true)))
            .collect(),
    )
}

/// The JSON of `taxonomy` for the request.
fn taxonomy_json(
    site: &Site,
    taxonomy: &TaxonomySpec,
    request: &Request,
    filter: bool,
) -> Map<String, Value> {
    let caps = taxonomy.caps();
    let labels = labels::taxonomy(taxonomy);
    let mut item = Map::new();
    let mut put = |key: &str, value: Value| {
        item.insert(key.to_owned(), value);
    };
    put("name", labels["name"].clone());
    put("slug", json!(taxonomy.taxonomy.name));
    put(
        "capabilities",
        json!({
            "manage_terms": caps.manage_terms,
            "edit_terms": caps.edit_terms,
            "delete_terms": caps.delete_terms,
            "assign_terms": caps.assign_terms,
        }),
    );
    put("description", json!(""));
    put("labels", labels);
    put("types", json!(taxonomy.taxonomy.object_types));
    put("show_cloud", json!(true));
    put("hierarchical", json!(taxonomy.taxonomy.hierarchical));
    put("rest_base", json!(taxonomy.rest_base));
    put("rest_namespace", json!(CORE_NAMESPACE));
    put(
        "visibility",
        json!({
            "public": true,
            "publicly_queryable": true,
            "show_admin_column": true,
            "show_in_nav_menus": true,
            "show_in_quick_edit": true,
            "show_ui": true,
        }),
    );
    let links = json!({
        "collection": [{"href": site.rest_url("mc/v2/taxonomies")}],
        "mc:items": [{"href": site.rest_url(&format!("mc/v2/{}", taxonomy.rest_base))}],
        "curies": curies(),
    });
    finish(item, TAXONOMY_FIELDS, request, links, filter)
}

/// The JSON of `status` for the request, with its `archives` link: the posts in that status.
fn status_json(
    site: &Site,
    status: &Status,
    request: &Request,
    filter: bool,
) -> Map<String, Value> {
    let mut item = Map::new();
    let mut put = |key: &str, value: Value| {
        item.insert(key.to_owned(), value);
    };
    put("name", json!(status.label));
    put("private", json!(status.private));
    put("protected", json!(status.protected));
    put("public", json!(status.public));
    put("queryable", json!(status.queryable));
    put("show_in_list", json!(status.show_in_list));
    put("slug", json!(status.slug));
    put("date_floating", json!(status.date_floating));
    let posts = site.rest_url("mc/v2/posts");
    let archives = if status.slug == "publish" {
        posts
    } else {
        format!("{posts}?status={}", status.slug)
    };
    let links = json!({"archives": [{"href": archives}]});
    finish(item, STATUS_FIELDS, request, links, filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(context: Context) -> Request {
        Request {
            context,
            fields: Fields(None),
            post_type: None,
        }
    }

    fn keys(item: &Map<String, Value>) -> Vec<&str> {
        let mut keys: Vec<&str> = item.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn contexts_keep_wordpresss_status_fields() {
        let item: Map<String, Value> = STATUS_FIELDS
            .iter()
            .map(|(name, _)| ((*name).to_owned(), Value::Null))
            .collect();
        let view = finish(
            item.clone(),
            STATUS_FIELDS,
            &request(Context::View),
            json!({}),
            false,
        );
        assert_eq!(
            keys(&view),
            [
                "_links",
                "date_floating",
                "name",
                "public",
                "queryable",
                "slug"
            ]
        );
        let embed = finish(
            item,
            STATUS_FIELDS,
            &request(Context::Embed),
            json!({}),
            false,
        );
        assert_eq!(keys(&embed), ["_links", "name", "slug"]);
    }

    #[test]
    fn capabilities_follow_the_capability_type() {
        let page = type_capabilities(&PostType::page());
        assert_eq!(page["edit_post"], "edit_page");
        assert_eq!(page["create_posts"], "edit_pages");
        let media = type_capabilities(&PostType::attachment());
        assert_eq!(media["read_post"], "read_post");
        assert_eq!(media["create_posts"], "upload_files");
    }

    #[test]
    fn supports_are_wordpresss_for_its_own_types() {
        let page = supports(&PostType::page());
        assert!(page.get("excerpt").is_none() && page["page-attributes"] == true);
        let post = supports(&PostType::post());
        assert!(post["trackbacks"] == true && post.get("page-attributes").is_none());
        let media = supports(&PostType::attachment());
        assert_eq!(
            media,
            json!({"title": true, "author": true, "comments": true})
        );
    }
}
