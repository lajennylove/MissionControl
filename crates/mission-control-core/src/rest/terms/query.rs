//! Reading terms: `get_items` and `get_item` of `WP_REST_Terms_Controller`, the collection's
//! parameters and the query they build, as `get_terms` runs it.

use orbit_orm::{Model, raw};
use serde_json::{Map, Value, json};

use super::prepare::{self, ItemOptions};
use super::{TaxonomySpec, TermsController};
use crate::content::{Post, PostStatus, Term, make_slug};
use crate::front::escape_like;
use crate::rest::params::{Params, Reader, parse_form};
use crate::rest::shared::{
    Context, Fields, Site, database_error, page_links, position_case, read_context, read_envelope,
};
use crate::rest::{RestError, RestResponse};
use crate::users::{Object, PostContext, can};

/// The `orderby` values, as WordPress's terms controller lists them.
const ORDERBY: &[&str] = &[
    "id",
    "include",
    "name",
    "slug",
    "include_slugs",
    "term_group",
    "description",
    "count",
];

/// A list request's parameters.
struct ListParams {
    context: Context,
    page: i64,
    per_page: i64,
    offset: Option<i64>,
    search: Option<String>,
    order_desc: bool,
    orderby: String,
    include: Vec<i64>,
    exclude: Vec<i64>,
    hide_empty: bool,
    /// `Some(0)` for top-level terms only.
    parent: Option<i64>,
    post: Option<i64>,
    slug: Vec<String>,
    fields: Fields,
    embed: Option<Vec<String>>,
}

impl ListParams {
    /// Reads and checks the parameters, as WordPress validates and sanitizes them before the
    /// route's permission check.
    fn read(taxonomy: &TaxonomySpec, params: &Params) -> Result<Self, RestError> {
        let hierarchical = taxonomy.taxonomy.hierarchical;
        let mut r = Reader::new(params);
        let context = read_context(&mut r);
        let page = r.integer("page", Some(1), None).unwrap_or(1);
        let per_page = r.integer("per_page", Some(1), Some(100)).unwrap_or(10);
        // `offset` is a parameter of flat taxonomies only, and 0 means none.
        let offset = if hierarchical {
            None
        } else {
            r.integer("offset", None, None).filter(|n| *n != 0)
        };
        let search = r.string("search").filter(|s| !s.is_empty());
        let order_desc = r.one_of("order", &["asc", "desc"]).as_deref() == Some("desc");
        let orderby = r
            .one_of("orderby", ORDERBY)
            .unwrap_or_else(|| "name".to_owned());
        let include = r.integers("include", 0);
        let exclude = r.integers("exclude", 0);
        let hide_empty = r.boolean("hide_empty").unwrap_or(false);
        let parent = if hierarchical {
            r.integer("parent", None, None)
        } else {
            None
        };
        let post = r.integer("post", None, None).filter(|p| *p != 0);
        let slug: Vec<String> = r.strings("slug");
        r.invalid.too_many(&[
            ("include", include.len()),
            ("exclude", exclude.len()),
            ("slug", slug.len()),
        ]);
        r.invalid.finish()?;
        let (fields, embed) = read_envelope(params);
        Ok(Self {
            context,
            page,
            per_page,
            offset,
            search,
            order_desc,
            orderby,
            include,
            exclude,
            hide_empty,
            parent,
            post,
            // `get_terms` sanitizes each slug as a title.
            slug: slug.iter().map(|s| make_slug(s)).collect(),
            fields,
            embed,
        })
    }
}

/// `get_items_permissions_check`: the edit context needs the taxonomy's `edit_terms`, and a
/// `post` filter needs the post to exist (`rest_post_invalid_id`, 400) and to be of one of the
/// taxonomy's types and readable, as `check_read_terms_permission_for_post` decides.
async fn check_list_permission(
    ctl: &TermsController<'_>,
    list: &ListParams,
) -> Result<(), RestError> {
    if list.context == Context::Edit && !ctl.has(&ctl.taxonomy.caps().edit_terms) {
        return Err(ctl.forbidden(
            "rest_forbidden_context",
            "Sorry, you are not allowed to edit terms in this taxonomy.",
        ));
    }
    if let Some(post_id) = list.post {
        let post = Post::query()
            .find(ctl.db, post_id)
            .await
            .map_err(|e| database_error(&e))?;
        let Some(post) = post else {
            return Err(RestError::new(
                "rest_post_invalid_id",
                "Invalid post ID.",
                400,
            ));
        };
        // `read_post` with the post's own type's capability names, as WordPress maps it.
        let post_type = ctl.post_types.registered(&post.post_type);
        let context = PostContext {
            post_type_caps: post_type.as_ref().map(|t| &t.caps),
            ..PostContext::new(&post)
        };
        let allowed = ctl.taxonomy.taxonomy.is_for(&post.post_type)
            && (post.status == PostStatus::Publish
                || can(
                    ctl.user,
                    "read_post",
                    Some(&Object::PostWithContext(context)),
                ));
        if !allowed {
            return Err(ctl.forbidden(
                "rest_forbidden_context",
                "Sorry, you are not allowed to view terms for this post.",
            ));
        }
    }
    Ok(())
}

/// `get_items`.
pub(super) async fn list(
    ctl: &TermsController<'_>,
    query: &str,
) -> Result<RestResponse, RestError> {
    let params = parse_form(query);
    let list = ListParams::read(ctl.taxonomy, &params)?;
    check_list_permission(ctl, &list).await?;
    let site = Site::load(ctl.db).await?;
    let (filter, binds) = filter(ctl, &list);
    let total: i64 = raw::fetch_scalar(ctl.db, &count_sql(&filter), &binds)
        .await
        .map_err(|e| database_error(&e))?;
    let max_pages = (total + list.per_page - 1) / list.per_page;
    let terms = fetch(ctl, &list, &filter, binds).await?;
    let options = ItemOptions {
        context: list.context,
        fields: list.fields.clone(),
        embed: list.embed.clone(),
    };
    let items = prepare::items(ctl, &site, &terms, &options).await?;
    let mut response = RestResponse::ok(Value::Array(items))
        .with_header("X-MC-Total", total.to_string())
        .with_header("X-MC-TotalPages", max_pages.to_string());
    let base = site.rest_url(&format!("mc/v2/{}", ctl.taxonomy.rest_base));
    for link in page_links(&base, query, list.page, max_pages) {
        response = response.with_header("Link", link);
    }
    Ok(response)
}

/// `get_item`.
pub(super) async fn get(
    ctl: &TermsController<'_>,
    id: i64,
    query: &str,
) -> Result<RestResponse, RestError> {
    let params = parse_form(query);
    let mut r = Reader::new(&params);
    let context = read_context(&mut r);
    r.invalid.finish()?;
    let (fields, embed) = read_envelope(&params);
    let term = ctl.find(id).await?;
    let site = Site::load(ctl.db).await?;
    if context == Context::Edit {
        let defaults = ctl.default_terms().await?;
        if !ctl.can_on("edit_term", &term, &defaults) {
            return Err(ctl.forbidden(
                "rest_forbidden_context",
                "Sorry, you are not allowed to edit this term.",
            ));
        }
    }
    let options = ItemOptions {
        context,
        fields,
        embed,
    };
    let mut items = prepare::items(ctl, &site, std::slice::from_ref(&term), &options).await?;
    Ok(RestResponse::ok(items.pop().unwrap_or(Value::Null)))
}

/// A list's SQL: an optional `WITH` clause, then its `FROM` and `WHERE`, with the binds of both
/// in order.
struct Filter {
    with: String,
    from: String,
}

/// `SELECT COUNT(*)` over `filter`.
fn count_sql(filter: &Filter) -> String {
    format!("{}SELECT COUNT(*) {}", filter.with, filter.from)
}

/// The filter of a list, as `get_terms` builds its `WHERE`.
fn filter(ctl: &TermsController<'_>, list: &ListParams) -> (Filter, Vec<orbit_orm::Value>) {
    let taxonomy = &ctl.taxonomy.taxonomy;
    let marks = |n: usize| vec!["?"; n].join(", ");
    let mut with = String::new();
    let mut binds: Vec<orbit_orm::Value> = Vec::new();
    if list.hide_empty && taxonomy.hierarchical {
        // The ancestors of every term with posts: `get_terms` keeps an empty term of a
        // hierarchical taxonomy when a term under it has posts.
        with.push_str(
            "WITH RECURSIVE kept (id) AS (\
             SELECT parent_id FROM term_taxonomy \
             WHERE taxonomy = ? AND count > 0 AND parent_id IS NOT NULL \
             UNION \
             SELECT up.parent_id FROM term_taxonomy up INNER JOIN kept ON up.term_id = kept.id \
             WHERE up.taxonomy = ? AND up.parent_id IS NOT NULL) ",
        );
        binds.push((&*taxonomy.name).into());
        binds.push((&*taxonomy.name).into());
    }
    let mut from = String::from(
        "FROM term_taxonomy tt INNER JOIN terms t ON t.id = tt.term_id WHERE tt.taxonomy = ?",
    );
    binds.push((&*taxonomy.name).into());
    for (values, negate) in [(&list.include, false), (&list.exclude, true)] {
        if values.is_empty() {
            continue;
        }
        let not = if negate { "NOT " } else { "" };
        from.push_str(&format!(
            " AND tt.term_id {not}IN ({})",
            marks(values.len())
        ));
        binds.extend(values.iter().map(|v| orbit_orm::Value::from(*v)));
    }
    if !list.slug.is_empty() {
        from.push_str(&format!(" AND t.slug IN ({})", marks(list.slug.len())));
        binds.extend(list.slug.iter().map(|s| orbit_orm::Value::from(s.clone())));
    }
    if let Some(search) = &list.search {
        // As `get_terms`: the whole search text, in the name or the slug.
        let like = format!("%{}%", escape_like(&search.to_lowercase()));
        from.push_str(" AND (LOWER(t.name) LIKE ? ESCAPE '!' OR LOWER(t.slug) LIKE ? ESCAPE '!')");
        binds.push(like.clone().into());
        binds.push(like.into());
    }
    match list.parent {
        None => {}
        // Parent 0 is a top-level term, stored with no parent (NULL).
        Some(0) => from.push_str(" AND (tt.parent_id IS NULL OR tt.parent_id = 0)"),
        Some(parent) => {
            from.push_str(" AND tt.parent_id = ?");
            binds.push(parent.into());
        }
    }
    if list.hide_empty {
        if taxonomy.hierarchical {
            from.push_str(" AND (tt.count > 0 OR tt.term_id IN (SELECT id FROM kept))");
        } else {
            from.push_str(" AND tt.count > 0");
        }
    }
    if let Some(post) = list.post {
        from.push_str(
            " AND EXISTS (SELECT 1 FROM term_relationships r \
             WHERE r.term_taxonomy_id = tt.id AND r.object_id = ?)",
        );
        binds.push(post.into());
    }
    (Filter { with, from }, binds)
}

/// The terms of the requested page, in order.
async fn fetch(
    ctl: &TermsController<'_>,
    list: &ListParams,
    filter: &Filter,
    mut binds: Vec<orbit_orm::Value>,
) -> Result<Vec<Term>, RestError> {
    let direction = if list.order_desc { "DESC" } else { "ASC" };
    let name = format!("LOWER(t.name) {direction}");
    let order = match list.orderby.as_str() {
        "id" => String::new(),
        "slug" => format!("t.slug {direction}"),
        "term_group" => format!("t.term_group {direction}"),
        "description" => format!("tt.description {direction}"),
        "count" => format!("tt.count {direction}"),
        "include" if !list.include.is_empty() => {
            binds.extend(list.include.iter().map(|v| orbit_orm::Value::from(*v)));
            position_case("tt.term_id", list.include.len())
        }
        "include_slugs" if !list.slug.is_empty() => {
            binds.extend(list.slug.iter().map(|v| orbit_orm::Value::from(v.clone())));
            position_case("t.slug", list.slug.len())
        }
        // `name`, and `include` or `include_slugs` with nothing to order by.
        _ => name,
    };
    let mut order: Vec<String> = [order].into_iter().filter(|o| !o.is_empty()).collect();
    // The id breaks ties, so terms with the same name keep one order.
    order.push(format!("tt.term_id {direction}"));
    let offset = match list.offset {
        Some(offset) => offset.max(0),
        None => (list.page - 1).saturating_mul(list.per_page),
    };
    let sql = format!(
        "{}SELECT tt.term_id, tt.id, t.name, t.slug, t.term_group, tt.taxonomy, tt.description, \
         tt.parent_id, tt.count {} ORDER BY {} LIMIT ? OFFSET ?",
        filter.with,
        filter.from,
        order.join(", ")
    );
    binds.push(list.per_page.into());
    binds.push(offset.into());
    type Row = (
        i64,
        i64,
        String,
        String,
        i64,
        String,
        String,
        Option<i64>,
        i64,
    );
    let rows: Vec<Row> = raw::fetch_all(ctl.db, &sql, &binds)
        .await
        .map_err(|e| database_error(&e))?;
    Ok(rows
        .into_iter()
        .map(
            |(
                term_id,
                term_taxonomy_id,
                name,
                slug,
                term_group,
                taxonomy,
                description,
                parent_id,
                count,
            )| {
                Term {
                    term_id,
                    term_taxonomy_id,
                    name,
                    slug,
                    term_group,
                    taxonomy,
                    description,
                    parent_id,
                    count,
                }
            },
        )
        .collect())
}

/// The `context` argument.
pub(super) fn context_arg() -> Value {
    json!({
        "description": "Scope under which the request is made; determines fields present in response.",
        "type": "string",
        "enum": ["view", "embed", "edit"],
        "default": "view",
        "required": false,
    })
}

/// The arguments of the collection's `GET`, as WordPress's `get_collection_params` lists them.
pub(super) fn collection_args(taxonomy: &TaxonomySpec) -> Map<String, Value> {
    let ids = |description: &str| json!({"description": description, "type": "array", "items": {"type": "integer"}, "default": [], "required": false});
    let mut args = Map::new();
    let mut put = |name: &str, value: Value| {
        args.insert(name.to_owned(), value);
    };
    put("context", context_arg());
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
    if !taxonomy.taxonomy.hierarchical {
        put(
            "offset",
            json!({"description": "Offset the result set by a specific number of items.", "type": "integer", "required": false}),
        );
    }
    put(
        "order",
        json!({"description": "Order sort attribute ascending or descending.", "type": "string", "default": "asc", "enum": ["asc", "desc"], "required": false}),
    );
    put(
        "orderby",
        json!({"description": "Sort collection by term attribute.", "type": "string", "default": "name", "enum": ORDERBY, "required": false}),
    );
    put(
        "hide_empty",
        json!({"description": "Whether to hide terms not assigned to any posts.", "type": "boolean", "default": false, "required": false}),
    );
    if taxonomy.taxonomy.hierarchical {
        put(
            "parent",
            json!({"description": "Limit result set to terms assigned to a specific parent.", "type": "integer", "required": false}),
        );
    }
    put(
        "post",
        json!({"description": "Limit result set to terms assigned to a specific post.", "type": "integer", "default": null, "required": false}),
    );
    put(
        "slug",
        json!({"description": "Limit result set to terms with one or more specific slugs.", "type": "array", "items": {"type": "string"}, "required": false}),
    );
    args
}
