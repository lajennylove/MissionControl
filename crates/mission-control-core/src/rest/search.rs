//! The search controller: `/wp/v2/search`, as WordPress's `WP_REST_Search_Controller` serves it
//! with its three handlers (`WP_REST_Post_Search_Handler`, `WP_REST_Term_Search_Handler` and
//! `WP_REST_Post_Format_Search_Handler`). [`SearchController`] answers one request, with no
//! Rocket types: the `mission-control` crate mounts it.
//!
//! As WordPress:
//!
//! - `type` picks the handler: `post` (the default), `term` or `post-format`. `subtype` limits
//!   the results to some post types (those shown in the REST API, but not `attachment`), some
//!   taxonomies (those shown in the REST API) or some formats; `any`, the default, is all of the
//!   handler's subtypes, and a subtype of another handler is dropped.
//! - Posts are the published posts of those types whose title, excerpt or content matches
//!   `search` (as `WP_Query` searches: each word, `-word` to exclude), by relevance then newest
//!   first; with no `search`, newest first. A visitor's search never matches a password-protected
//!   post, so its hidden words cannot be probed. Terms are those whose name or slug holds
//!   `search`, by name.
//! - `include` and `exclude` limit the ids; `page` and `per_page` (1 to 100) page the results,
//!   with `X-WP-Total`, `X-WP-TotalPages` and `Link` headers, and a page past the end is
//!   `rest_search_invalid_page_number` (400).
//! - A result has `id`, `title`, `url`, `type`, `subtype` and `_links` (`self`, embeddable;
//!   `about`, the type or taxonomy; `collection`). `context` is `view` or `embed`, which show the
//!   same fields. `_fields` keeps only the keys named; `_embed` embeds `self`, the post or term in
//!   the embed context.
//! - A protected post's title reads `Protected: {title}`, as `get_the_title` gives it (and as the
//!   posts controller's `title.rendered` reads).
//! - `search` is sanitized with `sanitize_text_field`. With no `search` and no subtype of the
//!   handler, a post search lists `post` only, as `WP_Query` does with no post type.
//!
//! Post formats: Mission Control's themes declare no `post-formats` support and no post has a
//! format, so `type=post-format` finds nothing, as WordPress finds nothing on such a site.

use orbit_orm::{Database, Model, raw};
use serde_json::{Map, Value, json};

use super::meta::MetaKeys;
use super::params::{Reader, parse_form};
use super::posts::prepare::rendered_title;
use super::posts::{PostType, PostTypes, PostsController, relevance};
use super::shared::{Fields, Site, database_error, page_links, read_envelope};
use super::terms::{Taxonomies, TaxonomySpec, TermsController};
use super::{Endpoint, KsesLimiter, RestError, RestResponse, RestRoutes, RouteSpec};
use crate::content::{Post, PostStatus, Term, sanitize_text_field};
use crate::front::{escape_like, post_links, search_terms, term_links};
use crate::hooks::Hooks;
use crate::theme::Theme;
use crate::users::UserCaps;

/// The handlers' types, in the order WordPress registers them.
const TYPES: &[&str] = &["post", "term", "post-format"];

/// `subtype`'s value for every subtype of the handler.
const ANY: &str = "any";

/// The post formats, as `get_post_format_strings` lists them.
const POST_FORMATS: &[&str] = &[
    "standard", "aside", "chat", "gallery", "link", "image", "quote", "status", "video", "audio",
];

/// `n` bind markers: `?, ?, ?`.
fn marks(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// The post types a post search covers: those shown in the REST API, but not `attachment`, as
/// `WP_REST_Post_Search_Handler` lists them.
fn post_subtypes(post_types: &PostTypes) -> Vec<String> {
    post_types
        .shown_in_rest()
        .into_iter()
        .filter(|t| t.name != "attachment")
        .map(|t| t.name)
        .collect()
}

/// The taxonomies a term search covers: those shown in the REST API.
fn term_subtypes(taxonomies: &Taxonomies) -> Vec<String> {
    taxonomies
        .shown_in_rest()
        .into_iter()
        .map(|t| t.taxonomy.name.into_owned())
        .collect()
}

/// Every handler's subtypes, then `any`: `subtype`'s allowed values.
fn all_subtypes(post_types: &PostTypes, taxonomies: &Taxonomies) -> Vec<String> {
    let mut subtypes = post_subtypes(post_types);
    subtypes.extend(term_subtypes(taxonomies));
    subtypes.extend(POST_FORMATS.iter().map(|f| (*f).to_owned()));
    subtypes.push(ANY.to_owned());
    subtypes
}

/// Registers `/wp/v2/search` on `routes`, so discovery lists it, with the subtypes of
/// `post_types` and `taxonomies`. Register it again after registering a post type or taxonomy.
pub fn register_routes(routes: &RestRoutes, post_types: &PostTypes, taxonomies: &Taxonomies) {
    let ids = |description: &str| json!({"description": description, "type": "array", "items": {"type": "integer"}, "default": [], "required": false});
    let args = json!({
        "context": {
            "description": "Scope under which the request is made; determines fields present in response.",
            "type": "string",
            "enum": ["view", "embed"],
            "default": "view",
            "required": false,
        },
        "page": {"description": "Current page of the collection.", "type": "integer", "default": 1, "minimum": 1, "required": false},
        "per_page": {"description": "Maximum number of items to be returned in result set.", "type": "integer", "default": 10, "minimum": 1, "maximum": 100, "required": false},
        "search": {"description": "Limit results to those matching a string.", "type": "string", "required": false},
        "type": {"default": "post", "description": "Limit results to items of an object type.", "type": "string", "enum": TYPES, "required": false},
        "subtype": {
            "default": ANY,
            "description": "Limit results to items of one or more object subtypes.",
            "type": "array",
            "items": {"enum": all_subtypes(post_types, taxonomies), "type": "string"},
            "required": false,
        },
        "exclude": ids("Ensure result set excludes specific IDs."),
        "include": ids("Limit result set to specific IDs."),
    });
    routes.register(RouteSpec::new(
        "/wp/v2/search",
        vec![Endpoint::new(["GET"], args)],
    ));
}

/// One request to the search controller.
///
/// ```ignore
/// let controller = SearchController {
///     db: &db,
///     hooks: &hooks,
///     post_types: &post_types,
///     taxonomies: &taxonomies,
///     user: &caps,
///     kses: &kses,
///     theme: Some(&theme),
///     meta: &meta_keys,
/// };
/// let response = controller.search("search=rocket&type=post").await?;
/// ```
pub struct SearchController<'a> {
    /// The database.
    pub db: &'a Database,
    /// The hooks, for the posts embedded with `_embed`.
    pub hooks: &'a Hooks,
    /// The post types the REST API serves.
    pub post_types: &'a PostTypes,
    /// The taxonomies the REST API serves.
    pub taxonomies: &'a Taxonomies,
    /// The current user's capabilities: a visitor's (id 0, no role) when nobody signed in.
    pub user: &'a UserCaps,
    /// The kses limiter the embedded posts' and terms' controllers take.
    pub kses: &'a KsesLimiter,
    /// The active theme, for the embedded posts' controller.
    pub theme: Option<&'a Theme>,
    /// The registered meta keys, for the embedded posts and terms.
    pub meta: &'a MetaKeys,
}

/// A search request's parameters.
struct SearchParams {
    page: i64,
    per_page: i64,
    search: Option<String>,
    kind: String,
    /// The requested subtypes; `any` stays in the list.
    subtypes: Vec<String>,
    include: Vec<i64>,
    exclude: Vec<i64>,
    fields: Fields,
    embed: Option<Vec<String>>,
}

impl SearchParams {
    fn read(ctl: &SearchController<'_>, query: &str) -> Result<Self, RestError> {
        let params = parse_form(query);
        let mut r = Reader::new(&params);
        r.one_of("context", &["view", "embed"]);
        let page = r.integer("page", Some(1), None).unwrap_or(1);
        let per_page = r.integer("per_page", Some(1), Some(100)).unwrap_or(10);
        // The collection's `search` argument is sanitized with `sanitize_text_field`.
        let search = r
            .string("search")
            .map(|s| sanitize_text_field(&s))
            .filter(|s| !s.is_empty());
        let kind = r.one_of("type", TYPES).unwrap_or_else(|| "post".to_owned());
        let allowed = all_subtypes(ctl.post_types, ctl.taxonomies);
        let allowed: Vec<&str> = allowed.iter().map(String::as_str).collect();
        let subtypes = if r.has("subtype") {
            let subtypes = r.strings("subtype");
            if let Some(i) = subtypes.iter().position(|s| !allowed.contains(&s.as_str())) {
                r.invalid.not_in_enum(&format!("subtype[{i}]"), &allowed);
            }
            subtypes
        } else {
            vec![ANY.to_owned()]
        };
        let include = r.integers("include", i64::MIN);
        let exclude = r.integers("exclude", i64::MIN);
        r.invalid
            .too_many(&[("include", include.len()), ("exclude", exclude.len())]);
        r.invalid.finish()?;
        let (fields, embed) = read_envelope(&params);
        Ok(Self {
            page,
            per_page,
            search,
            kind,
            subtypes,
            include,
            exclude,
            fields,
            embed,
        })
    }

    /// The subtypes to search among `handler`'s, as `sanitize_subtypes` and the handlers read
    /// them: all of them for `any`, else those asked for. When none asked for is the handler's,
    /// `fallback`: what the handler's query does with no subtype.
    fn chosen(&self, handler: Vec<String>, fallback: Vec<String>) -> Vec<String> {
        if self.subtypes.iter().any(|s| s == ANY) {
            return handler;
        }
        let chosen: Vec<String> = handler
            .iter()
            .filter(|s| self.subtypes.contains(s))
            .cloned()
            .collect();
        if chosen.is_empty() { fallback } else { chosen }
    }

    /// `include` and `exclude` on `column`, added to `filter` and `binds`.
    fn filter_ids(&self, column: &str, filter: &mut String, binds: &mut Vec<orbit_orm::Value>) {
        for (ids, not) in [(&self.include, ""), (&self.exclude, "NOT ")] {
            if !ids.is_empty() {
                filter.push_str(&format!(" AND {column} {not}IN ({})", marks(ids.len())));
                binds.extend(ids.iter().map(|id| orbit_orm::Value::from(*id)));
            }
        }
    }

    /// The offset of the requested page.
    fn offset(&self) -> i64 {
        (self.page - 1).saturating_mul(self.per_page)
    }

    /// Whether `_embed` asks for `self`.
    fn embeds_self(&self) -> bool {
        self.embed
            .as_ref()
            .is_some_and(|e| e.is_empty() || e.iter().any(|rel| rel == "self"))
    }
}

/// One result before it is shaped: its fields, and where its `self` and `about` links point.
struct Found {
    id: i64,
    title: String,
    url: String,
    subtype: String,
    /// The route of the post or term, such as `wp/v2/posts/7`.
    route: String,
    /// The route of its type or taxonomy, such as `wp/v2/types/post`.
    about: String,
}

impl SearchController<'_> {
    /// `GET /wp/v2/search`. `query` is the request's query string.
    ///
    /// # Errors
    ///
    /// `rest_invalid_param` for a bad parameter, and `rest_search_invalid_page_number` (400) for
    /// a page past the end.
    pub async fn search(&self, query: &str) -> Result<RestResponse, RestError> {
        let params = SearchParams::read(self, query)?;
        let site = Site::load(self.db).await?;
        let (total, found) = match params.kind.as_str() {
            "post" => self.posts(&site, &params).await?,
            "term" => self.terms(&site, &params).await?,
            // No post has a format: see the module's notes.
            _ => (0, Vec::new()),
        };
        let max_pages = (total + params.per_page - 1) / params.per_page;
        if params.page > max_pages && total > 0 {
            return Err(RestError::new(
                "rest_search_invalid_page_number",
                "The page number requested is larger than the number of pages available.",
                400,
            ));
        }
        let mut items = Vec::with_capacity(found.len());
        for result in &found {
            items.push(Value::Object(self.item(&site, &params, result).await?));
        }
        let mut response = RestResponse::ok(Value::Array(items))
            .with_header("X-WP-Total", total.to_string())
            .with_header("X-WP-TotalPages", max_pages.to_string());
        let base = site.rest_url("wp/v2/search");
        for link in page_links(&base, query, params.page, max_pages) {
            response = response.with_header("Link", link);
        }
        Ok(response)
    }

    /// The JSON of one result.
    async fn item(
        &self,
        site: &Site,
        params: &SearchParams,
        found: &Found,
    ) -> Result<Map<String, Value>, RestError> {
        let mut item = Map::new();
        for (key, value) in [
            ("id", json!(found.id)),
            ("title", json!(found.title)),
            ("url", json!(found.url)),
            ("type", json!(params.kind)),
            ("subtype", json!(found.subtype)),
        ] {
            if params.fields.includes(key) {
                item.insert(key.to_owned(), value);
            }
        }
        if params.fields.includes("_links") || params.fields.includes("_embedded") {
            if params.embeds_self() {
                let embedded = self.embedded(params, found).await?;
                item.insert("_embedded".to_owned(), json!({"self": [embedded]}));
            }
            item.insert(
                "_links".to_owned(),
                json!({
                    "self": [{"embeddable": true, "href": site.rest_url(&found.route)}],
                    "about": [{"href": site.rest_url(&found.about)}],
                    "collection": [{"href": site.rest_url("wp/v2/search")}],
                }),
            );
        }
        Ok(params.fields.filter(item))
    }

    /// The result's post or term in the embed context, or WordPress's error body, as the REST
    /// server embeds a link.
    async fn embedded(&self, params: &SearchParams, found: &Found) -> Result<Value, RestError> {
        let answer = if params.kind == "post" {
            let Some(post_type) = self.post_types.by_name(&found.subtype) else {
                return Ok(Value::Null);
            };
            PostsController {
                db: self.db,
                hooks: self.hooks,
                post_type: &post_type,
                user: self.user,
                kses: self.kses,
                theme: self.theme,
                image_sizes: None,
                meta: self.meta,
                post_types: self.post_types,
            }
            .get(found.id, "context=embed")
            .await
        } else {
            let Some(taxonomy) = self.taxonomies.registered(&found.subtype) else {
                return Ok(Value::Null);
            };
            TermsController {
                db: self.db,
                taxonomy: &taxonomy,
                post_types: self.post_types,
                user: self.user,
                kses: self.kses,
                meta: self.meta,
            }
            .get(found.id, "context=embed")
            .await
        };
        Ok(match answer {
            Ok(response) => response.body,
            Err(error) if error.status >= 500 => return Err(error),
            Err(error) => error.to_json(),
        })
    }

    /// `WP_REST_Post_Search_Handler::search_items`: the total and the page of published posts.
    async fn posts(
        &self,
        site: &Site,
        params: &SearchParams,
    ) -> Result<(i64, Vec<Found>), RestError> {
        let types: Vec<PostType> = params
            .chosen(post_subtypes(self.post_types), {
                // `WP_Query` with no post type searches every searchable type, and lists `post`
                // otherwise.
                if params.search.is_some() {
                    post_subtypes(self.post_types)
                } else {
                    vec!["post".to_owned()]
                }
            })
            .iter()
            .filter_map(|name| self.post_types.by_name(name))
            .collect();
        if types.is_empty() {
            return Ok((0, Vec::new()));
        }
        let mut filter = format!(
            "FROM posts p WHERE p.post_type IN ({}) AND p.status = ?",
            marks(types.len())
        );
        let mut binds: Vec<orbit_orm::Value> =
            types.iter().map(|t| t.name.clone().into()).collect();
        binds.push(PostStatus::Publish.into());
        params.filter_ids("p.id", &mut filter, &mut binds);
        let terms = search_terms(params.search.as_deref().unwrap_or_default());
        for term in &terms {
            let like = format!("%{}%", escape_like(&term.text.to_lowercase()));
            filter.push_str(if term.exclude { " AND NOT " } else { " AND " });
            filter.push_str(
                "(LOWER(p.title) LIKE ? ESCAPE '!' OR LOWER(p.excerpt) LIKE ? ESCAPE '!' \
                 OR LOWER(p.content) LIKE ? ESCAPE '!')",
            );
            binds.extend(std::iter::repeat_n(orbit_orm::Value::from(like), 3));
        }
        // `WP_Query::parse_search`: a visitor's search never matches a protected post.
        if !terms.is_empty() && self.user.id() == 0 {
            filter.push_str(" AND (p.password IS NULL OR p.password = '')");
        }
        let total: i64 = raw::fetch_scalar(self.db, &format!("SELECT COUNT(*) {filter}"), &binds)
            .await
            .map_err(|e| database_error(&e))?;
        let mut order = Vec::new();
        if !terms.is_empty() {
            let (clause, relevance_binds) =
                relevance(params.search.as_deref().unwrap_or_default(), &terms);
            order.push(clause);
            binds.extend(relevance_binds);
        }
        order.push("COALESCE(p.published_at, p.modified_at) DESC".to_owned());
        order.push("p.id DESC".to_owned());
        binds.push(params.per_page.into());
        binds.push(params.offset().into());
        let sql = format!(
            "SELECT p.id {filter} ORDER BY {} LIMIT ? OFFSET ?",
            order.join(", ")
        );
        let ids: Vec<(i64,)> = raw::fetch_all(self.db, &sql, &binds)
            .await
            .map_err(|e| database_error(&e))?;
        if ids.is_empty() {
            return Ok((total, Vec::new()));
        }
        let ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
        let mut posts = Post::query()
            .where_in(Post::PRIMARY_KEY, ids.clone())
            .get(self.db)
            .await
            .map_err(|e| database_error(&e))?;
        posts.sort_by_key(|post| ids.iter().position(|id| *id == post.id));
        let paths = post_links(self.db, &site.structure, &posts)
            .await
            .map_err(|e| database_error(&e))?;
        let found = posts
            .iter()
            .zip(paths)
            .map(|(post, path)| {
                let rest_base = types
                    .iter()
                    .find(|t| t.name == post.post_type)
                    .map_or(post.post_type.as_str(), |t| t.rest_base.as_str());
                Found {
                    id: post.id,
                    title: rendered_title(post),
                    url: format!("{}{path}", site.home),
                    subtype: post.post_type.clone(),
                    route: format!("wp/v2/{rest_base}/{}", post.id),
                    about: format!("wp/v2/types/{}", post.post_type),
                }
            })
            .collect();
        Ok((total, found))
    }

    /// `WP_REST_Term_Search_Handler::search_items`: the total and the page of terms, by name.
    async fn terms(
        &self,
        site: &Site,
        params: &SearchParams,
    ) -> Result<(i64, Vec<Found>), RestError> {
        let taxonomies: Vec<TaxonomySpec> = params
            .chosen(
                term_subtypes(self.taxonomies),
                term_subtypes(self.taxonomies),
            )
            .iter()
            .filter_map(|name| self.taxonomies.registered(name))
            .filter(|t| t.show_in_rest)
            .collect();
        if taxonomies.is_empty() {
            return Ok((0, Vec::new()));
        }
        let mut filter = format!(
            "FROM term_taxonomy tt INNER JOIN terms t ON t.id = tt.term_id \
             WHERE tt.taxonomy IN ({})",
            marks(taxonomies.len())
        );
        let mut binds: Vec<orbit_orm::Value> = taxonomies
            .iter()
            .map(|t| (&*t.taxonomy.name).into())
            .collect();
        params.filter_ids("tt.term_id", &mut filter, &mut binds);
        if let Some(search) = &params.search {
            // As `get_terms`: the whole search text, in the name or the slug.
            let like = format!("%{}%", escape_like(&search.to_lowercase()));
            filter.push_str(
                " AND (LOWER(t.name) LIKE ? ESCAPE '!' OR LOWER(t.slug) LIKE ? ESCAPE '!')",
            );
            binds.push(like.clone().into());
            binds.push(like.into());
        }
        let total: i64 = raw::fetch_scalar(self.db, &format!("SELECT COUNT(*) {filter}"), &binds)
            .await
            .map_err(|e| database_error(&e))?;
        binds.push(params.per_page.into());
        binds.push(params.offset().into());
        let sql = format!(
            "SELECT tt.term_id, tt.id, t.name, t.slug, tt.taxonomy, tt.parent_id {filter} \
             ORDER BY LOWER(t.name) ASC, tt.term_id ASC LIMIT ? OFFSET ?"
        );
        type Row = (i64, i64, String, String, String, Option<i64>);
        let rows: Vec<Row> = raw::fetch_all(self.db, &sql, &binds)
            .await
            .map_err(|e| database_error(&e))?;
        let terms: Vec<Term> = rows
            .into_iter()
            .map(
                |(term_id, term_taxonomy_id, name, slug, taxonomy, parent_id)| Term {
                    term_id,
                    term_taxonomy_id,
                    name,
                    slug,
                    term_group: 0,
                    taxonomy,
                    description: String::new(),
                    parent_id,
                    count: 0,
                },
            )
            .collect();
        let paths = term_links(self.db, &site.structure, &terms)
            .await
            .map_err(|e| database_error(&e))?;
        let found = terms
            .iter()
            .zip(paths)
            .map(|(term, path)| {
                let rest_base = taxonomies
                    .iter()
                    .find(|t| t.taxonomy.name == term.taxonomy)
                    .map_or(term.taxonomy.as_str(), |t| t.rest_base.as_str());
                let url = match path {
                    Some(path) => format!("{}{path}", site.home),
                    None => format!(
                        "{}/?taxonomy={}&term={}",
                        site.home, term.taxonomy, term.slug
                    ),
                };
                Found {
                    id: term.term_id,
                    title: term.name.clone(),
                    url,
                    subtype: term.taxonomy.clone(),
                    route: format!("wp/v2/{rest_base}/{}", term.term_id),
                    about: format!("wp/v2/taxonomies/{}", term.taxonomy),
                }
            })
            .collect();
        Ok((total, found))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtypes_are_every_handlers_then_any() {
        let subtypes = all_subtypes(&PostTypes::new(), &Taxonomies::new());
        assert_eq!(&subtypes[..4], ["post", "page", "category", "post_tag"]);
        assert_eq!(subtypes.last().map(String::as_str), Some(ANY));
        assert!(subtypes.iter().any(|s| s == "aside"));
    }

    #[test]
    fn the_route_lists_wordpresss_arguments() {
        let routes = RestRoutes::new();
        register_routes(&routes, &PostTypes::new(), &Taxonomies::new());
        let route = routes
            .routes()
            .into_iter()
            .find(|r| r.path == "/wp/v2/search")
            .expect("the search route");
        let mut args: Vec<&str> = route.endpoints[0].args.keys().map(String::as_str).collect();
        args.sort_unstable();
        assert_eq!(
            args,
            [
                "context", "exclude", "include", "page", "per_page", "search", "subtype", "type"
            ]
        );
    }
}
