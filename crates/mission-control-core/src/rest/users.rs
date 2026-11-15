//! The users controller: `/wp/v2/users`, `/wp/v2/users/{id}` and `/wp/v2/users/me`, as
//! WordPress's `WP_REST_Users_Controller` reads them. Users are Ignite's (its `users` table:
//! `id`, `name`, `email`, `created_at`); their roles come from the app's [`UserRoles`]
//! provider. [`UsersController`] answers one request, with no Rocket types: the
//! `mission-control` crate mounts it.
//!
//! As WordPress:
//!
//! - A user's JSON has `id`, `name`, `url`, `description`, `link`, `slug`, `avatar_urls`, `meta`
//!   and `_links` in the view context (`embed` drops `meta`), and in the edit context also
//!   `username`, `first_name`, `last_name`, `email`, `locale`, `nickname`, `registered_date`,
//!   `roles`, `capabilities` and `extra_capabilities`. `_fields` keeps only the keys named.
//! - `avatar_urls` are Gravatar's, at 24, 48 and 96 pixels, from the SHA-256 of the trimmed,
//!   lowercased email, with the `avatar_default` and `avatar_rating` options (`mm` and `g` by
//!   default); none when the `show_avatars` option is off.
//! - The edit context needs `list_users`, or the user is the current user. A user who may not
//!   `list_users` sees only users who have published posts of a type shown in the REST API
//!   (`has_published_posts`), cannot filter by `roles` or `capabilities`, and cannot order by
//!   `email` or `registered_date`. `/users/me` is the current user, or 401
//!   `rest_not_logged_in`.
//! - Lists take `page`, `per_page`, `search`, `include`, `exclude`, `offset`, `order` (default
//!   `asc`), `orderby` (default `name`), `slug`, `roles`, `capabilities` and `who=authors`, and
//!   answer with `X-WP-Total`, `X-WP-TotalPages` and `Link` headers. A page past the end is an
//!   empty list.
//!
//! Until Mission Control's profile layer (design 5.6, Task 8.1), Ruling R59: `slug` is the
//! user's name through [`make_slug`] (WordPress's `user_nicename`); `link` is the author archive
//! by id (`{home}/?author={id}`); `username` is the email, which is what a user signs in with;
//! `url` is empty. `first_name`, `last_name`, `nickname`, `description` and `locale` are the
//! user meta WordPress keeps them in, when set (`locale` falls back to the `WPLANG` option, then
//! `en_US`). A visitor's `search` matches the name and the id only, never the email, which is
//! the username here.
//!
//! Left out: creating, updating and deleting users (`POST`, `PUT`, `PATCH` and `DELETE`, and
//! `/users/me` writes). Users belong to Ignite; Mission Control does not write them yet. Also
//! left out: `has_published_posts` and `search_columns` as parameters, registered meta (`meta` is
//! `{}` until Task 10.6), and the `?rest_route=` form for these routes.

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDateTime;
use orbit_orm::{Database, Value as Sql, raw};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::auth::not_logged_in;
use super::params::{Reader, parse_form};
use super::posts::PostTypes;
use super::shared::{
    Context, Fields, Site, authorization_status, database_error, option, page_links, position_case,
    read_context, read_envelope,
};
use super::{Endpoint, RestError, RestResponse, RestRoutes, RouteSpec};
use crate::content::make_slug;
use crate::front::escape_like;
use crate::users::{RoleAssignment, UserCaps, UserRoles, has_cap};

/// How many ids one `IN (...)` binds.
const IN_CHUNK: usize = 500;

/// The most ids `include`, `exclude` and the other lists take, so one query stays bounded.
const MAX_LIST_IDS: usize = 500;

/// The avatar sizes, as `rest_get_avatar_sizes`.
const AVATAR_SIZES: [u32; 3] = [24, 48, 96];

/// The user meta the response reads.
const META_KEYS: [&str; 5] = [
    "first_name",
    "last_name",
    "nickname",
    "description",
    "locale",
];

/// The `orderby` values, as WordPress lists them.
const ORDERBY: &[&str] = &[
    "id",
    "include",
    "name",
    "registered_date",
    "slug",
    "include_slugs",
    "email",
    "url",
];

/// Registers the users routes on `routes`, so discovery lists them: the collection, the item
/// and `me`, each `GET` only.
pub fn register_routes(routes: &RestRoutes) {
    let context = json!({
        "description": "Scope under which the request is made; determines fields present in response.",
        "type": "string",
        "enum": ["view", "embed", "edit"],
        "default": "view",
        "required": false,
    });
    routes.register(RouteSpec::new(
        "/wp/v2/users",
        vec![Endpoint::new(["GET"], Value::Object(collection_args()))],
    ));
    routes.register(RouteSpec::new(
        "/wp/v2/users/(?P<id>[\\d]+)",
        vec![Endpoint::new(
            ["GET"],
            json!({
                "id": {"description": "Unique identifier for the user.", "type": "integer", "required": false},
                "context": context,
            }),
        )],
    ));
    routes.register(RouteSpec::new(
        "/wp/v2/users/me",
        vec![Endpoint::new(["GET"], json!({"context": context}))],
    ));
}

/// The arguments of the collection's `GET`, as WordPress's `get_collection_params` lists them.
fn collection_args() -> Map<String, Value> {
    let ids = |description: &str| json!({"description": description, "type": "array", "items": {"type": "integer"}, "default": [], "required": false});
    let strings = |description: &str| json!({"description": description, "type": "array", "items": {"type": "string"}, "required": false});
    let mut args = Map::new();
    let mut put = |name: &str, value: Value| {
        args.insert(name.to_owned(), value);
    };
    put(
        "context",
        json!({"description": "Scope under which the request is made; determines fields present in response.", "type": "string", "enum": ["view", "embed", "edit"], "default": "view", "required": false}),
    );
    put(
        "page",
        json!({"description": "Current page of the collection.", "type": "integer", "default": 1, "minimum": 1, "required": false}),
    );
    put(
        "per_page",
        json!({"description": "Maximum number of items to be returned in result set.", "type": "integer", "default": 10, "minimum": 1, "maximum": 100, "required": false}),
    );
    put(
        "search",
        json!({"description": "Limit results to those matching a string.", "type": "string", "required": false}),
    );
    put("exclude", ids("Ensure result set excludes specific IDs."));
    put("include", ids("Limit result set to specific IDs."));
    put(
        "offset",
        json!({"description": "Offset the result set by a specific number of items.", "type": "integer", "required": false}),
    );
    put(
        "order",
        json!({"description": "Order sort attribute ascending or descending.", "type": "string", "default": "asc", "enum": ["asc", "desc"], "required": false}),
    );
    put(
        "orderby",
        json!({"description": "Sort collection by user attribute.", "type": "string", "default": "name", "enum": ORDERBY, "required": false}),
    );
    put(
        "slug",
        strings("Limit result set to users with one or more specific slugs."),
    );
    put(
        "roles",
        strings(
            "Limit result set to users matching at least one specific role provided. Accepts csv list or single role.",
        ),
    );
    put(
        "capabilities",
        strings(
            "Limit result set to users matching at least one specific capability provided. Accepts csv list or single capability.",
        ),
    );
    put(
        "who",
        json!({"description": "Limit result set to users who are considered authors.", "type": "string", "enum": ["authors"], "required": false}),
    );
    args
}

/// A row of Ignite's `users`.
#[derive(Debug, Clone)]
struct UserRow {
    id: i64,
    name: String,
    email: String,
    created_at: Option<NaiveDateTime>,
}

/// How a list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderBy {
    Id,
    Include,
    Name,
    RegisteredDate,
    Slug,
    IncludeSlugs,
    Email,
    Url,
}

/// A list request's parameters.
struct ListParams {
    context: Context,
    page: i64,
    per_page: i64,
    offset: Option<i64>,
    search: Option<String>,
    order_desc: bool,
    orderby: OrderBy,
    include: Vec<i64>,
    exclude: Vec<i64>,
    slug: Vec<String>,
    roles: Vec<String>,
    capabilities: Vec<String>,
    authors: bool,
    fields: Fields,
}

/// One request to the users controller.
///
/// ```ignore
/// let controller = UsersController {
///     db: &db,
///     user: &caps,
///     roles: &*user_roles,
///     post_types: &post_types,
/// };
/// let me = controller.me("context=edit").await?;
/// ```
pub struct UsersController<'a> {
    /// The database.
    pub db: &'a Database,
    /// The current user's capabilities: a visitor's (id 0, no role) when nobody signed in.
    pub user: &'a UserCaps,
    /// Where users' roles come from.
    pub roles: &'a dyn UserRoles,
    /// The post types the REST API serves: a user with published posts of one is public.
    pub post_types: &'a PostTypes,
}

impl UsersController<'_> {
    /// `GET /wp/v2/users/me`: the current user. `query` is the request's query string.
    ///
    /// # Errors
    ///
    /// `rest_not_logged_in` (401) for a visitor, and WordPress's error for a bad parameter.
    pub async fn me(&self, query: &str) -> Result<RestResponse, RestError> {
        if self.user.id() == 0 {
            return Err(not_logged_in());
        }
        self.respond_one(self.user.id(), query, true).await
    }

    /// `GET /wp/v2/users/{id}`.
    ///
    /// # Errors
    ///
    /// `rest_user_invalid_id` (404) when there is no such user, and `rest_user_cannot_view`
    /// for the edit context without `list_users`, or for a user with no published posts the
    /// current user may not see.
    pub async fn get(&self, id: i64, query: &str) -> Result<RestResponse, RestError> {
        self.respond_one(id, query, false).await
    }

    /// The JSON of user `id` for a single-item request; `me` skips the permission check, as
    /// `get_current_item` has none.
    async fn respond_one(&self, id: i64, query: &str, me: bool) -> Result<RestResponse, RestError> {
        let params = parse_form(query);
        let mut r = Reader::new(&params);
        let context = read_context(&mut r);
        r.invalid.finish()?;
        let (fields, _) = read_envelope(&params);
        let row = self
            .rows(&[id])
            .await?
            .pop()
            .filter(|_| id > 0)
            .ok_or_else(invalid_user)?;
        if !me && row.id != self.user.id() {
            let cannot_view = || {
                RestError::new(
                    "rest_user_cannot_view",
                    "Sorry, you are not allowed to list users.",
                    authorization_status(self.user),
                )
            };
            if context == Context::Edit && !self.has("list_users") {
                return Err(cannot_view());
            }
            if !self.has("list_users")
                && !self.has("edit_users")
                && !self.published_authors(&[row.id]).await?.contains(&row.id)
            {
                return Err(cannot_view());
            }
        }
        let site = Site::load(self.db).await?;
        let mut items = self.items(&site, &[row], context, &fields).await?;
        Ok(RestResponse::ok(items.pop().unwrap_or(Value::Null)))
    }

    /// `GET /wp/v2/users`: page `page` of the users the query asks for.
    ///
    /// # Errors
    ///
    /// WordPress's error for a bad parameter, or for a context, filter or order the user may
    /// not use.
    pub async fn list(&self, query: &str) -> Result<RestResponse, RestError> {
        let params = parse_form(query);
        let list = self.read_list(&params)?;
        self.check_list_permission(&list)?;
        let (filter, binds) = self.filter(&list);
        let needs_scan = !list.slug.is_empty()
            || !list.roles.is_empty()
            || !list.capabilities.is_empty()
            || list.authors
            || list.orderby == OrderBy::IncludeSlugs;
        let offset = list
            .offset
            .unwrap_or_else(|| (list.page - 1).saturating_mul(list.per_page));
        let (total, ids) = if needs_scan {
            self.scan(&list, &filter, binds, offset).await?
        } else {
            let total: i64 =
                raw::fetch_scalar(self.db, &format!("SELECT COUNT(*) {filter}"), &binds)
                    .await
                    .map_err(|e| database_error(&e))?;
            let (order, order_binds) = order_clause(&list);
            let mut binds = binds;
            binds.extend(order_binds);
            binds.push(list.per_page.into());
            binds.push(offset.into());
            let ids: Vec<(i64,)> = raw::fetch_all(
                self.db,
                &format!("SELECT u.id {filter} ORDER BY {order} LIMIT ? OFFSET ?"),
                &binds,
            )
            .await
            .map_err(|e| database_error(&e))?;
            (total, ids.into_iter().map(|(id,)| id).collect())
        };
        let mut rows = self.rows(&ids).await?;
        rows.sort_by_key(|row| ids.iter().position(|id| *id == row.id));
        let site = Site::load(self.db).await?;
        let items = self.items(&site, &rows, list.context, &list.fields).await?;
        // As `get_items`, the page is worked out from the offset.
        let page = offset / list.per_page + 1;
        let max_pages = (total + list.per_page - 1) / list.per_page;
        let mut response = RestResponse::ok(Value::Array(items))
            .with_header("X-WP-Total", total.to_string())
            .with_header("X-WP-TotalPages", max_pages.to_string());
        for link in page_links(&site.rest_url("wp/v2/users"), query, page, max_pages) {
            response = response.with_header("Link", link);
        }
        Ok(response)
    }

    /// Reads and checks a list's parameters.
    fn read_list(&self, params: &super::Params) -> Result<ListParams, RestError> {
        let mut r = Reader::new(params);
        let context = read_context(&mut r);
        let page = r.integer("page", Some(1), None).unwrap_or(1);
        let per_page = r.integer("per_page", Some(1), Some(100)).unwrap_or(10);
        let offset = r.integer("offset", None, None).map(|n| n.max(0));
        let search = r.string("search").filter(|s| !s.trim().is_empty());
        let order_desc = r.one_of("order", &["asc", "desc"]).as_deref() == Some("desc");
        let orderby = match r.one_of("orderby", ORDERBY).as_deref() {
            Some("id") => OrderBy::Id,
            Some("include") => OrderBy::Include,
            Some("registered_date") => OrderBy::RegisteredDate,
            Some("slug") => OrderBy::Slug,
            Some("include_slugs") => OrderBy::IncludeSlugs,
            Some("email") => OrderBy::Email,
            Some("url") => OrderBy::Url,
            _ => OrderBy::Name,
        };
        let include = r.integers("include", 0);
        let exclude = r.integers("exclude", 0);
        let slug = r.strings("slug");
        let roles = r.strings("roles");
        let capabilities = r.strings("capabilities");
        let authors = r.one_of("who", &["authors"]).is_some();
        for (name, len) in [
            ("include", include.len()),
            ("exclude", exclude.len()),
            ("slug", slug.len()),
            ("roles", roles.len()),
            ("capabilities", capabilities.len()),
        ] {
            if len > MAX_LIST_IDS {
                r.invalid.add(
                    name,
                    "rest_too_many_items",
                    format!("{name} must contain at most {MAX_LIST_IDS} items."),
                    json!({"param": name}),
                );
            }
        }
        r.invalid.finish()?;
        let (fields, _) = read_envelope(params);
        Ok(ListParams {
            context,
            page,
            per_page,
            offset,
            search,
            order_desc,
            orderby,
            include,
            exclude,
            slug,
            roles,
            capabilities,
            authors,
            fields,
        })
    }

    /// `get_items_permissions_check`.
    fn check_list_permission(&self, list: &ListParams) -> Result<(), RestError> {
        let refuse = |code: &'static str, message: &'static str| {
            Err(RestError::new(
                code,
                message,
                authorization_status(self.user),
            ))
        };
        let list_users = self.has("list_users");
        if !list.roles.is_empty() && !list_users {
            return refuse(
                "rest_user_cannot_view",
                "Sorry, you are not allowed to filter users by role.",
            );
        }
        if !list.capabilities.is_empty() && !list_users {
            return refuse(
                "rest_user_cannot_view",
                "Sorry, you are not allowed to filter users by capability.",
            );
        }
        if list.context == Context::Edit && !list_users {
            return refuse(
                "rest_forbidden_context",
                "Sorry, you are not allowed to list users.",
            );
        }
        if matches!(list.orderby, OrderBy::Email | OrderBy::RegisteredDate) && !list_users {
            return refuse(
                "rest_forbidden_orderby",
                "Sorry, you are not allowed to order users by this parameter.",
            );
        }
        if list.authors {
            // Any type shown in the REST API with authors, whose `edit_posts` the user holds.
            let allowed = self
                .post_types
                .shown_in_rest()
                .iter()
                .any(|t| t.supports.author && self.has(&t.caps().edit_posts));
            if !allowed {
                return refuse(
                    "rest_forbidden_who",
                    "Sorry, you are not allowed to query users by this parameter.",
                );
            }
        }
        Ok(())
    }

    /// The `FROM` and `WHERE` of a list, and their binds: every filter SQL can apply.
    fn filter(&self, list: &ListParams) -> (String, Vec<Sql>) {
        let mut sql = String::from("FROM users u WHERE 1 = 1");
        let mut binds: Vec<Sql> = Vec::new();
        let marks = |n: usize| vec!["?"; n].join(", ");
        if !self.has("list_users") {
            // `has_published_posts`: a user who may not list users sees only authors.
            let types = self.public_types();
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM posts p WHERE p.author_id = u.id AND p.status = ? \
                 AND p.post_type IN ({}))",
                marks(types.len())
            ));
            binds.push("publish".into());
            binds.extend(types.into_iter().map(Sql::from));
        }
        if !list.include.is_empty() {
            sql.push_str(&format!(" AND u.id IN ({})", marks(list.include.len())));
            binds.extend(list.include.iter().map(|id| Sql::from(*id)));
        }
        if !list.exclude.is_empty() {
            sql.push_str(&format!(" AND u.id NOT IN ({})", marks(list.exclude.len())));
            binds.extend(list.exclude.iter().map(|id| Sql::from(*id)));
        }
        if let Some(search) = &list.search {
            // `*term*`, as `get_items` wraps it: the term anywhere.
            let term = search.trim().trim_matches('*').to_lowercase();
            let like = format!("%{}%", escape_like(&term));
            let mut parts = vec!["LOWER(u.name) LIKE ? ESCAPE '!'".to_owned()];
            binds.push(like.clone().into());
            if self.has("list_users") {
                parts.push("LOWER(u.email) LIKE ? ESCAPE '!'".to_owned());
                binds.push(like.into());
            }
            if let Ok(id) = term.parse::<i64>() {
                parts.push("u.id = ?".to_owned());
                binds.push(id.into());
            }
            sql.push_str(&format!(" AND ({})", parts.join(" OR ")));
        }
        (sql, binds)
    }

    /// A list whose filters or order need each user's slug or roles: every user the SQL
    /// filter matches, in order, checked one by one. Returns the total and the page's ids.
    async fn scan(
        &self,
        list: &ListParams,
        filter: &str,
        mut binds: Vec<Sql>,
        offset: i64,
    ) -> Result<(i64, Vec<i64>), RestError> {
        let (order, order_binds) = order_clause(list);
        binds.extend(order_binds);
        let candidates: Vec<(i64, String)> = raw::fetch_all(
            self.db,
            &format!("SELECT u.id, u.name {filter} ORDER BY {order}"),
            &binds,
        )
        .await
        .map_err(|e| database_error(&e))?;
        let mut matched: Vec<(i64, String)> = Vec::new();
        for (id, name) in candidates {
            let slug = make_slug(&name);
            if !list.slug.is_empty() && !list.slug.contains(&slug) {
                continue;
            }
            if !list.roles.is_empty() || !list.capabilities.is_empty() || list.authors {
                let assignment = self.assignment(id).await?;
                let caps = assignment.user_caps(id);
                if !list.roles.is_empty()
                    && !assignment.roles.iter().any(|r| list.roles.contains(r))
                {
                    continue;
                }
                if !list.capabilities.is_empty()
                    && !list.capabilities.iter().any(|cap| has_cap(&caps, cap))
                {
                    continue;
                }
                // `who=authors`: a user level above 0.
                if list.authors && !has_cap(&caps, "level_1") {
                    continue;
                }
            }
            matched.push((id, slug));
        }
        if list.orderby == OrderBy::IncludeSlugs {
            matched.sort_by_key(|(_, slug)| {
                list.slug
                    .iter()
                    .position(|s| s == slug)
                    .unwrap_or(list.slug.len())
            });
        }
        let total = i64::try_from(matched.len()).unwrap_or(i64::MAX);
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let per_page = usize::try_from(list.per_page).unwrap_or(10);
        let ids = matched
            .into_iter()
            .skip(start)
            .take(per_page)
            .map(|(id, _)| id)
            .collect();
        Ok((total, ids))
    }

    /// The names of the post types shown in the REST API, whose published posts make their
    /// author public.
    fn public_types(&self) -> Vec<String> {
        let mut types: Vec<String> = self
            .post_types
            .shown_in_rest()
            .into_iter()
            .map(|t| t.name)
            .collect();
        types.push("attachment".to_owned());
        types
    }

    /// Which of `ids` have published posts of a type shown in the REST API.
    async fn published_authors(&self, ids: &[i64]) -> Result<Vec<i64>, RestError> {
        published_authors(self.db, &self.public_types(), ids).await
    }

    /// The rows of users `ids`, in no particular order.
    async fn rows(&self, ids: &[i64]) -> Result<Vec<UserRow>, RestError> {
        user_rows(self.db, ids).await
    }

    /// User `id`'s roles and own capabilities, from the provider.
    async fn assignment(&self, id: i64) -> Result<RoleAssignment, RestError> {
        self.roles.roles(self.db, id).await.map_err(|e| {
            log::error!("Could not read the roles of user {id}: {e}");
            RestError::internal()
        })
    }

    /// The JSON of `rows` for `context` and `fields`, in order.
    async fn items(
        &self,
        site: &Site,
        rows: &[UserRow],
        context: Context,
        fields: &Fields,
    ) -> Result<Vec<Value>, RestError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
        let meta = user_meta(self.db, &ids).await?;
        let avatars = Avatars::load(self.db).await?;
        let site_locale = match option(self.db, "WPLANG").await? {
            Some(Value::String(locale)) if !locale.trim().is_empty() => locale,
            _ => "en_US".to_owned(),
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let text = |key: &str| {
                meta.get(&(row.id, key.to_owned()))
                    .cloned()
                    .unwrap_or_default()
            };
            let mut item = base_item(site, row, &avatars, &text("description"));
            if context == Context::Edit {
                let assignment = self.assignment(row.id).await?;
                let caps = assignment.user_caps(row.id);
                let mut extra: BTreeMap<String, bool> = assignment
                    .roles
                    .iter()
                    .map(|role| (role.clone(), true))
                    .collect();
                extra.extend(assignment.caps.clone());
                let locale = Some(text("locale"))
                    .filter(|l| !l.trim().is_empty())
                    .unwrap_or_else(|| site_locale.clone());
                let mut put = |key: &str, value: Value| {
                    item.insert(key.to_owned(), value);
                };
                put("username", json!(row.email));
                put("first_name", json!(text("first_name")));
                put("last_name", json!(text("last_name")));
                put("email", json!(row.email));
                put("locale", json!(locale));
                put("nickname", json!(text("nickname")));
                put(
                    "registered_date",
                    json!(
                        row.created_at
                            .map(|d| d.format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
                            .unwrap_or_default()
                    ),
                );
                put("roles", json!(assignment.roles));
                put("capabilities", json!(caps.allcaps()));
                put("extra_capabilities", json!(extra));
            }
            if context == Context::Embed {
                item.remove("meta");
            }
            let mut item = fields.filter(item);
            if fields.includes("_links") {
                item.insert("_links".to_owned(), links(site, row.id));
            }
            out.push(Value::Object(item));
        }
        Ok(out)
    }

    /// Whether the user holds primitive capability `cap`.
    fn has(&self, cap: &str) -> bool {
        has_cap(self.user, cap)
    }
}

/// `rest_user_invalid_id`, 404.
fn invalid_user() -> RestError {
    RestError::new("rest_user_invalid_id", "Invalid user ID.", 404)
}

/// The `ORDER BY` of a list and its binds; the id breaks ties.
fn order_clause(list: &ListParams) -> (String, Vec<Sql>) {
    let direction = if list.order_desc { "DESC" } else { "ASC" };
    let mut binds = Vec::new();
    let first = match list.orderby {
        OrderBy::Id | OrderBy::Url => None,
        // The slug is the name made a slug (Ruling R59), so the name orders it.
        OrderBy::Name | OrderBy::Slug | OrderBy::IncludeSlugs => {
            Some(format!("u.name {direction}"))
        }
        OrderBy::Email => Some(format!("u.email {direction}")),
        OrderBy::RegisteredDate => Some(format!("u.created_at {direction}")),
        OrderBy::Include if list.include.is_empty() => None,
        OrderBy::Include => {
            binds.extend(list.include.iter().map(|id| Sql::from(*id)));
            Some(position_case("u.id", list.include.len()))
        }
    };
    let order = match first {
        Some(first) => format!("{first}, u.id {direction}"),
        None => format!("u.id {direction}"),
    };
    (order, binds)
}

/// The rows of users `ids`.
async fn user_rows(db: &Database, ids: &[i64]) -> Result<Vec<UserRow>, RestError> {
    let mut rows = Vec::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
        let found: Vec<(i64, String, String, Option<NaiveDateTime>)> = raw::fetch_all(
            db,
            &format!(
                "SELECT id, name, email, created_at FROM users WHERE id IN ({})",
                vec!["?"; chunk.len()].join(", ")
            ),
            &binds,
        )
        .await
        .map_err(|e| database_error(&e))?;
        rows.extend(
            found
                .into_iter()
                .map(|(id, name, email, created_at)| UserRow {
                    id,
                    name,
                    email,
                    created_at,
                }),
        );
    }
    Ok(rows)
}

/// The user meta the response reads, for users `ids`: the first value of each key, when it is
/// a JSON string.
async fn user_meta(
    db: &Database,
    ids: &[i64],
) -> Result<HashMap<(i64, String), String>, RestError> {
    let mut meta = HashMap::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let mut binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
        binds.extend(META_KEYS.iter().map(|k| Sql::from(*k)));
        let rows: Vec<(i64, String, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT user_id, meta_key, meta_value FROM user_meta WHERE user_id IN ({}) \
                 AND meta_key IN ({}) ORDER BY id",
                vec!["?"; chunk.len()].join(", "),
                vec!["?"; META_KEYS.len()].join(", ")
            ),
            &binds,
        )
        .await
        .map_err(|e| database_error(&e))?;
        for (user_id, key, value) in rows {
            // Exact keys: MySQL compares text without case.
            if !META_KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Ok(Value::String(text)) = serde_json::from_str(&value) {
                meta.entry((user_id, key)).or_insert(text);
            }
        }
    }
    Ok(meta)
}

/// The avatar options, read once per request.
#[derive(Debug, Clone)]
struct Avatars {
    /// `show_avatars`: whether `avatar_urls` is shown.
    show: bool,
    /// The `d` argument: `avatar_default`, as `get_avatar_data` maps it.
    default: Option<String>,
    /// The `r` argument: `avatar_rating`, lowercased.
    rating: String,
}

impl Avatars {
    async fn load(db: &Database) -> Result<Self, RestError> {
        let show = match option(db, "show_avatars").await? {
            None => true,
            Some(Value::Bool(b)) => b,
            Some(Value::Number(n)) => n.as_i64() != Some(0),
            Some(Value::String(s)) => !matches!(s.trim(), "" | "0" | "false"),
            Some(_) => true,
        };
        let default = match option(db, "avatar_default").await? {
            Some(Value::String(s)) if !s.trim().is_empty() => s,
            _ => "mystery".to_owned(),
        };
        let default = match default.as_str() {
            "mm" | "mystery" | "mysteryman" => Some("mm".to_owned()),
            "gravatar_default" => None,
            other => Some(other.to_owned()),
        };
        let rating = match option(db, "avatar_rating").await? {
            Some(Value::String(s)) if !s.trim().is_empty() => s.to_lowercase(),
            _ => "g".to_owned(),
        };
        Ok(Self {
            show,
            default,
            rating,
        })
    }

    /// `rest_get_avatar_urls`: Gravatar's address for `email` at each size.
    fn urls(&self, email: &str) -> Value {
        // PHP's `strtolower`: ASCII letters only.
        let hash: String = Sha256::digest(email.trim().to_ascii_lowercase().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut urls = Map::new();
        for size in AVATAR_SIZES {
            let mut url = format!("https://secure.gravatar.com/avatar/{hash}?s={size}");
            if let Some(default) = &self.default {
                url.push_str(&format!("&d={}", url_encode(default)));
            }
            url.push_str(&format!("&r={}", url_encode(&self.rating)));
            urls.insert(size.to_string(), json!(url));
        }
        Value::Object(urls)
    }
}

/// PHP's `rawurlencode`: letters, digits and `-_.~` kept, the rest `%XX`.
fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The fields every context shows: `id`, `name`, `url`, `description`, `link`, `slug`,
/// `avatar_urls` (when avatars are shown) and `meta`.
fn base_item(
    site: &Site,
    row: &UserRow,
    avatars: &Avatars,
    description: &str,
) -> Map<String, Value> {
    let mut item = Map::new();
    item.insert("id".into(), json!(row.id));
    item.insert("name".into(), json!(row.name));
    item.insert("url".into(), json!(""));
    item.insert("description".into(), json!(description));
    item.insert(
        "link".into(),
        json!(format!("{}/?author={}", site.home, row.id)),
    );
    item.insert("slug".into(), json!(make_slug(&row.name)));
    if avatars.show {
        item.insert("avatar_urls".into(), avatars.urls(&row.email));
    }
    item.insert("meta".into(), json!({}));
    item
}

/// A user's `_links`: `self` and `collection`. The `self` link's `targetHints` name `GET` only:
/// Mission Control serves no user writes.
fn links(site: &Site, id: i64) -> Value {
    json!({
        "self": [{
            "href": site.rest_url(&format!("wp/v2/users/{id}")),
            "targetHints": {"allow": ["GET"]},
        }],
        "collection": [{"href": site.rest_url("wp/v2/users")}],
    })
}

/// Which of `ids` have published posts of one of `types`, as `count_user_posts` counts them.
async fn published_authors(
    db: &Database,
    types: &[String],
    ids: &[i64],
) -> Result<Vec<i64>, RestError> {
    let mut found = Vec::new();
    if types.is_empty() {
        return Ok(found);
    }
    for chunk in ids.chunks(IN_CHUNK) {
        let mut binds: Vec<Sql> = vec!["publish".into()];
        binds.extend(types.iter().map(|t| Sql::from(t.clone())));
        binds.extend(chunk.iter().map(|id| Sql::from(*id)));
        let rows: Vec<(i64,)> = raw::fetch_all(
            db,
            &format!(
                "SELECT DISTINCT author_id FROM posts WHERE status = ? AND post_type IN ({}) \
                 AND author_id IN ({})",
                vec!["?"; types.len()].join(", "),
                vec!["?"; chunk.len()].join(", ")
            ),
            &binds,
        )
        .await
        .map_err(|e| database_error(&e))?;
        found.extend(rows.into_iter().map(|(id,)| id));
    }
    Ok(found)
}

/// The embed objects of users `ids`, as a post's `_embedded.author` shows them to `reader`:
/// the embed context of each user, or the error WordPress embeds instead. That is
/// `rest_user_invalid_id` for a user who is gone, and `rest_user_cannot_view` for a user with
/// no published posts of `types` when the reader is someone else without `list_users` or
/// `edit_users`, as `get_item_permissions_check` refuses it.
pub(crate) async fn embed_authors(
    db: &Database,
    site: &Site,
    ids: &[i64],
    reader: &UserCaps,
    types: &[String],
) -> Result<HashMap<i64, Value>, RestError> {
    let sees_everyone = has_cap(reader, "list_users") || has_cap(reader, "edit_users");
    let published = if sees_everyone {
        Vec::new()
    } else {
        published_authors(db, types, ids).await?
    };
    let rows = user_rows(db, ids).await?;
    let meta = user_meta(db, ids).await?;
    let avatars = Avatars::load(db).await?;
    let mut embeds = HashMap::new();
    for row in &rows {
        if !sees_everyone && row.id != reader.id() && !published.contains(&row.id) {
            let refused = RestError::new(
                "rest_user_cannot_view",
                "Sorry, you are not allowed to list users.",
                authorization_status(reader),
            );
            embeds.insert(row.id, refused.to_json());
            continue;
        }
        let description = meta
            .get(&(row.id, "description".to_owned()))
            .cloned()
            .unwrap_or_default();
        let mut item = base_item(site, row, &avatars, &description);
        item.remove("meta");
        item.insert("_links".to_owned(), links(site, row.id));
        embeds.insert(row.id, Value::Object(item));
    }
    for id in ids {
        embeds
            .entry(*id)
            .or_insert_with(|| invalid_user().to_json());
    }
    Ok(embeds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gravatar_addresses_hash_the_trimmed_lowercased_email() {
        let avatars = Avatars {
            show: true,
            default: Some("mm".to_owned()),
            rating: "g".to_owned(),
        };
        // SHA-256 of "myemailaddress@example.com", as WordPress 6.8 hashes it (Ruling R62).
        let urls = avatars.urls("  MyEmailAddress@example.com ");
        assert_eq!(
            urls["96"],
            json!(
                "https://secure.gravatar.com/avatar/84059b07d4be67b806386c0aad8070a23f18836bbaae342275dc0a83414c32ee?s=96&d=mm&r=g"
            )
        );
        assert_eq!(urls.as_object().map(Map::len), Some(3));
        // Only ASCII letters are lowercased, as PHP's `strtolower`: `É` stays.
        let urls = avatars.urls("ÉLODIE@Example.com");
        let expected: String = Sha256::digest("Élodie@example.com".as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            urls["24"]
                .as_str()
                .unwrap()
                .contains(&format!("/avatar/{expected}?")),
            "{urls}"
        );
    }

    #[test]
    fn the_id_breaks_ties_in_every_order() {
        let list = ListParams {
            context: Context::View,
            page: 1,
            per_page: 10,
            offset: None,
            search: None,
            order_desc: true,
            orderby: OrderBy::Name,
            include: Vec::new(),
            exclude: Vec::new(),
            slug: Vec::new(),
            roles: Vec::new(),
            capabilities: Vec::new(),
            authors: false,
            fields: Fields(None),
        };
        assert_eq!(order_clause(&list).0, "u.name DESC, u.id DESC");
        let by_include = ListParams {
            orderby: OrderBy::Include,
            include: vec![3, 1],
            order_desc: false,
            ..list
        };
        let (order, binds) = order_clause(&by_include);
        assert_eq!(
            order,
            "CASE u.id WHEN ? THEN 0 WHEN ? THEN 1 ELSE 2 END, u.id ASC"
        );
        assert_eq!(binds.len(), 2);
    }
}
