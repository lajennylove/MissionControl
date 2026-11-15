//! Reading posts: `get_items` and `get_item` of `WP_REST_Posts_Controller`, the collection's
//! parameters and the query they build, as `WP_Query` runs it.

use orbit_orm::{Model, raw};
use serde_json::{Map, Value, json};

use super::prepare::{self, ItemOptions};
use super::{PostType, PostsController};
use crate::content::{Post, PostStatus};
use crate::front::{SearchTerm, escape_like, search_terms};
use crate::rest::params::{Params, Reader, parse_form};
use crate::rest::shared::{
    Context, Fields, Site, database_error, page_links, position_case, read_context, read_envelope,
};
use crate::rest::{RestError, RestResponse};

/// The statuses a list may ask for, as `get_post_stati( array( 'internal' => false ) )` and
/// `any`.
const STATUSES: &[&str] = &[
    "publish", "future", "draft", "pending", "private", "trash", "any",
];

/// The statuses an attachment list may ask for, as `WP_REST_Attachments_Controller` lists them.
const ATTACHMENT_STATUSES: &[&str] = &["inherit", "private", "trash"];

/// The `media_type` values: the first part of the allowed MIME types, as `get_media_types` groups
/// them.
pub(crate) const MEDIA_TYPES: &[&str] = &["image", "video", "text", "application", "audio"];

/// The statuses `post_type`'s lists take, and the default.
fn statuses_of(post_type: &PostType) -> (&'static [&'static str], &'static str) {
    if post_type.is_attachment() {
        (ATTACHMENT_STATUSES, "inherit")
    } else {
        (STATUSES, "publish")
    }
}

/// The most ids `include`, `exclude` and the other id lists take, so one query stays bounded.
const MAX_LIST_IDS: usize = 500;

/// How a list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderBy {
    Author,
    Date,
    Id,
    Include,
    Modified,
    Parent,
    Relevance,
    Slug,
    IncludeSlugs,
    Title,
    MenuOrder,
}

/// The `orderby` values of `post_type`: WordPress's list, and `menu_order` for a type with page
/// attributes.
fn orderby_values(post_type: &PostType) -> Vec<&'static str> {
    let mut values = vec![
        "author",
        "date",
        "id",
        "include",
        "modified",
        "parent",
        "relevance",
        "slug",
        "include_slugs",
        "title",
    ];
    if post_type.supports.page_attributes {
        values.push("menu_order");
    }
    values
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
    author: Vec<i64>,
    author_exclude: Vec<i64>,
    slug: Vec<String>,
    /// `None` for `any`.
    statuses: Option<Vec<PostStatus>>,
    parent: Vec<i64>,
    parent_exclude: Vec<i64>,
    menu_order: Option<i64>,
    sticky: Option<bool>,
    /// An attachment list's `media_type`: the first part of the MIME type.
    media_type: Option<String>,
    /// An attachment list's `mime_type`, when it is one of the media types'.
    mime_type: Option<String>,
    /// Per taxonomy: the `term_id`s a post must have one of, and those it must have none of.
    terms: Vec<(&'static str, Vec<i64>, Vec<i64>)>,
    fields: Fields,
    embed: Option<Vec<String>>,
}

impl ListParams {
    /// Reads and checks the parameters, as WordPress validates and sanitizes them before the
    /// route's permission check.
    fn read(ctl: &PostsController<'_>, params: &Params) -> Result<Self, RestError> {
        let post_type = ctl.post_type;
        let mut r = Reader::new(params);
        let context = read_context(&mut r);
        let page = r.integer("page", Some(1), None).unwrap_or(1);
        let per_page = r.integer("per_page", Some(1), Some(100)).unwrap_or(10);
        let offset = r.integer("offset", None, None).map(|n| n.max(0));
        let search = r.string("search").filter(|s| !s.trim().is_empty());
        let order_desc = r.one_of("order", &["asc", "desc"]).as_deref() != Some("asc");
        let orderby = match r.one_of("orderby", &orderby_values(post_type)).as_deref() {
            Some("author") => OrderBy::Author,
            Some("id") => OrderBy::Id,
            Some("include") => OrderBy::Include,
            Some("modified") => OrderBy::Modified,
            Some("parent") => OrderBy::Parent,
            Some("relevance") => OrderBy::Relevance,
            Some("slug") => OrderBy::Slug,
            Some("include_slugs") => OrderBy::IncludeSlugs,
            Some("title") => OrderBy::Title,
            Some("menu_order") => OrderBy::MenuOrder,
            _ => OrderBy::Date,
        };
        let include = r.integers("include", 0);
        let exclude = r.integers("exclude", 0);
        let author = r.integers("author", 0);
        let author_exclude = r.integers("author_exclude", 0);
        let slug = r.strings("slug");
        let (parent, parent_exclude) = if post_type.filters_by_parent() {
            (r.integers("parent", 0), r.integers("parent_exclude", 0))
        } else {
            (Vec::new(), Vec::new())
        };
        let menu_order = if post_type.has_parent() {
            r.integer("menu_order", None, None)
        } else {
            None
        };
        let (media_type, mime_type) = if post_type.is_attachment() {
            let media_type = r.one_of("media_type", MEDIA_TYPES);
            // `prepare_items_query` ignores a MIME type outside the media types.
            let mime_type = r.string("mime_type").filter(|m| {
                m.split_once('/')
                    .is_some_and(|(kind, rest)| MEDIA_TYPES.contains(&kind) && !rest.is_empty())
            });
            (media_type, mime_type)
        } else {
            (None, None)
        };
        let sticky = if post_type.has_sticky() {
            r.boolean("sticky")
        } else {
            None
        };
        let mut sizes: Vec<(String, usize)> = [
            ("include", include.len()),
            ("exclude", exclude.len()),
            ("author", author.len()),
            ("author_exclude", author_exclude.len()),
            ("slug", slug.len()),
            ("parent", parent.len()),
            ("parent_exclude", parent_exclude.len()),
        ]
        .into_iter()
        .map(|(name, len)| (name.to_owned(), len))
        .collect();
        let mut terms = Vec::new();
        for taxonomy in &post_type.taxonomies {
            let base = taxonomy.rest_base;
            let exclude_name = format!("{base}_exclude");
            let include = term_list(&mut r, base);
            let exclude = term_list(&mut r, &exclude_name);
            sizes.push((base.to_owned(), include.len()));
            sizes.push((exclude_name, exclude.len()));
            if !include.is_empty() || !exclude.is_empty() {
                terms.push((taxonomy.taxonomy.name, include, exclude));
            }
        }
        let statuses = read_statuses(ctl, &mut r);
        for (name, list) in &sizes {
            let (name, list) = (name.as_str(), *list);
            if list > MAX_LIST_IDS {
                r.invalid.add(
                    name,
                    "rest_too_many_items",
                    format!("{name} must contain at most {MAX_LIST_IDS} items."),
                    json!({"param": name}),
                );
            }
        }
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
            author,
            author_exclude,
            slug,
            statuses,
            parent,
            parent_exclude,
            menu_order,
            sticky,
            media_type,
            mime_type,
            terms,
            fields,
            embed,
        })
    }
}

/// A taxonomy filter: a list of term ids, or WordPress's object form `{"terms": [...]}`.
fn term_list(r: &mut Reader<'_>, name: &str) -> Vec<i64> {
    match r.raw(name) {
        Some(Value::Object(object)) => {
            let terms = object.get("terms").cloned().unwrap_or(Value::Null);
            let mut wrapped = Params::new();
            wrapped.insert(name.to_owned(), terms);
            let mut inner = Reader::new(&wrapped);
            let ids = inner.integers(name, 0);
            if !inner.invalid.is_empty() {
                r.invalid.wrong_type(name, "array");
            }
            ids
        }
        _ => r.integers(name, 0),
    }
}

/// The `status` parameter, as `sanitize_post_statuses` checks it: `publish` (`inherit` for
/// attachments, their default) for anyone, any other status only for a user with the type's
/// `edit_posts` (or `read_private_posts` for `private`); otherwise `rest_forbidden_status` inside
/// `rest_invalid_param`.
fn read_statuses(ctl: &PostsController<'_>, r: &mut Reader<'_>) -> Option<Vec<PostStatus>> {
    let (allowed, default) = statuses_of(ctl.post_type);
    let names = if r.has("status") {
        r.strings("status")
    } else {
        vec![default.to_owned()]
    };
    // WordPress validates the values against the enum before the sanitize callback checks
    // permissions.
    if names.iter().any(|name| !allowed.contains(&name.as_str())) {
        r.invalid.not_in_enum("status", allowed);
        return Some(Vec::new());
    }
    let caps = ctl.post_type.caps();
    let mut statuses = Vec::new();
    let mut any = false;
    for name in &names {
        if name != "publish"
            && name != default
            && !(ctl.has(&caps.edit_posts)
                || (name == "private" && ctl.has(&caps.read_private_posts)))
        {
            r.invalid.add(
                "status",
                "rest_forbidden_status",
                "Status is forbidden.".to_owned(),
                json!({"status": ctl.auth_status()}),
            );
            return Some(Vec::new());
        }
        match name.as_str() {
            "any" => any = true,
            other => statuses.push(other.parse().expect("a listed status")),
        }
    }
    if any { None } else { Some(statuses) }
}

/// `get_items`.
pub(super) async fn list(
    ctl: &PostsController<'_>,
    query: &str,
) -> Result<RestResponse, RestError> {
    let params = parse_form(query);
    let list = ListParams::read(ctl, &params)?;
    if list.context == Context::Edit && !ctl.has(&ctl.post_type.caps().edit_posts) {
        return Err(ctl.forbidden(
            "rest_forbidden_context",
            "Sorry, you are not allowed to edit posts in this post type.",
        ));
    }
    if list.orderby == OrderBy::Relevance && list.search.is_none() {
        return Err(RestError::new(
            "rest_no_search_term_defined",
            "You need to define a search term to order by relevance.",
            400,
        ));
    }
    if list.orderby == OrderBy::Include && list.include.is_empty() {
        return Err(RestError::new(
            "rest_orderby_include_missing_include",
            "You need to define an include parameter to order by include.",
            400,
        ));
    }
    let site = Site::load(ctl.db).await?;
    let sql = Sql::filter(ctl, &list, &site);
    let total = match &sql {
        Some((filter, binds)) => {
            raw::fetch_scalar::<i64>(ctl.db, &format!("SELECT COUNT(*) {filter}"), binds)
                .await
                .map_err(|e| database_error(&e))?
        }
        None => 0,
    };
    let max_pages = (total + list.per_page - 1) / list.per_page;
    if list.page > max_pages && total > 0 {
        return Err(RestError::new(
            "rest_post_invalid_page_number",
            "The page number requested is larger than the number of pages available.",
            400,
        ));
    }
    let posts = match &sql {
        Some((filter, binds)) => fetch(ctl, &list, filter, binds.clone()).await?,
        None => Vec::new(),
    };
    // As `get_items`, posts the user may not read are left out of the page, not the totals.
    let mut readable = Vec::with_capacity(posts.len());
    for post in posts {
        let trash = ctl.trash_status(&post).await?;
        if ctl.can_read(&post, trash, &site).await? {
            readable.push(post);
        }
    }
    let options = ItemOptions {
        context: list.context,
        password: None,
        fields: list.fields.clone(),
        embed: list.embed.clone(),
    };
    let items = prepare::items(ctl, &site, &readable, &options).await?;
    let mut response = RestResponse::ok(Value::Array(items))
        .with_header("X-WP-Total", total.to_string())
        .with_header("X-WP-TotalPages", max_pages.to_string());
    let base = site.rest_url(&format!("wp/v2/{}", ctl.post_type.rest_base));
    for link in page_links(&base, query, list.page, max_pages) {
        response = response.with_header("Link", link);
    }
    Ok(response)
}

/// `get_item`.
pub(super) async fn get(
    ctl: &PostsController<'_>,
    id: i64,
    query: &str,
) -> Result<RestResponse, RestError> {
    let params = parse_form(query);
    let mut r = Reader::new(&params);
    let context = read_context(&mut r);
    let password = r.string("password").filter(|p| !p.is_empty());
    r.invalid.finish()?;
    let (fields, embed) = read_envelope(&params);
    let post = ctl.find(id).await?;
    let site = Site::load(ctl.db).await?;
    let trash = ctl.trash_status(&post).await?;
    if context == Context::Edit && !ctl.can_on("edit_post", &post, trash, &site) {
        return Err(ctl.forbidden(
            "rest_forbidden_context",
            "Sorry, you are not allowed to edit this post.",
        ));
    }
    if let Some(password) = &password {
        let stored = post.password.as_deref().unwrap_or("");
        if !constant_time_eq(stored.as_bytes(), password.as_bytes()) {
            return Err(RestError::new(
                "rest_post_incorrect_password",
                "Incorrect post password.",
                403,
            ));
        }
    }
    if !ctl.can_read(&post, trash, &site).await? {
        return Err(ctl.forbidden("rest_forbidden", "Sorry, you are not allowed to do that."));
    }
    let options = ItemOptions {
        context,
        password,
        fields,
        embed,
    };
    let mut items = prepare::items(ctl, &site, std::slice::from_ref(&post), &options).await?;
    Ok(RestResponse::ok(items.pop().unwrap_or(Value::Null)))
}

/// Whether `a` and `b` are equal, in time that depends on their lengths only, as `hash_equals`.
pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// The SQL of a list: its `FROM` and `WHERE`, and their binds.
struct Sql;

impl Sql {
    /// The filter, or `None` when nothing can match (an empty status list).
    fn filter(
        ctl: &PostsController<'_>,
        list: &ListParams,
        site: &Site,
    ) -> Option<(String, Vec<orbit_orm::Value>)> {
        let mut sql = String::from("FROM posts p WHERE p.post_type = ?");
        let mut binds: Vec<orbit_orm::Value> = vec![ctl.post_type.name.clone().into()];
        let marks = |n: usize| vec!["?"; n].join(", ");
        match &list.statuses {
            Some(statuses) if statuses.is_empty() => return None,
            Some(statuses) => {
                sql.push_str(&format!(" AND p.status IN ({})", marks(statuses.len())));
                binds.extend(statuses.iter().map(|s| orbit_orm::Value::from(*s)));
            }
            // `any`: every status but those excluded from search.
            None => {
                sql.push_str(" AND p.status NOT IN (?, ?)");
                binds.push(PostStatus::Trash.into());
                binds.push(PostStatus::AutoDraft.into());
            }
        }
        let mut ids = |sql: &mut String, column: &str, values: &[i64], negate: bool| {
            if values.is_empty() {
                return;
            }
            let not = if negate { "NOT " } else { "" };
            sql.push_str(&format!(
                " AND p.{column} {not}IN ({})",
                marks(values.len())
            ));
            binds.extend(values.iter().map(|v| orbit_orm::Value::from(*v)));
        };
        ids(&mut sql, "id", &list.include, false);
        ids(&mut sql, "id", &list.exclude, true);
        ids(&mut sql, "author_id", &list.author, false);
        ids(&mut sql, "author_id", &list.author_exclude, true);
        if !list.slug.is_empty() {
            sql.push_str(&format!(" AND p.slug IN ({})", marks(list.slug.len())));
            binds.extend(list.slug.iter().map(|s| orbit_orm::Value::from(s.clone())));
        }
        for (values, negate) in [(&list.parent, false), (&list.parent_exclude, true)] {
            if values.is_empty() {
                continue;
            }
            // Parent 0 is a top-level post, stored with no parent (NULL) or 0.
            let top = values.contains(&0);
            let others: Vec<i64> = values.iter().copied().filter(|v| *v != 0).collect();
            let mut parts = Vec::new();
            if top {
                parts.push("p.parent_id IS NULL OR p.parent_id = 0".to_owned());
            }
            if !others.is_empty() {
                parts.push(format!("p.parent_id IN ({})", marks(others.len())));
                binds.extend(others.iter().map(|v| orbit_orm::Value::from(*v)));
            }
            let clause = parts.join(" OR ");
            if !negate {
                sql.push_str(&format!(" AND ({clause})"));
            } else if top {
                // NULL rows match `IS NULL` first, so the NOT is never NULL.
                sql.push_str(&format!(" AND NOT ({clause})"));
            } else {
                sql.push_str(&format!(" AND (p.parent_id IS NULL OR NOT ({clause}))"));
            }
        }
        if let Some(order) = list.menu_order {
            sql.push_str(" AND p.menu_order = ?");
            binds.push(order.into());
        }
        if let Some(media_type) = &list.media_type {
            sql.push_str(" AND p.mime_type LIKE ? ESCAPE '!'");
            binds.push(format!("{}/%", escape_like(media_type)).into());
        }
        if let Some(mime_type) = &list.mime_type {
            sql.push_str(" AND p.mime_type = ?");
            binds.push(mime_type.clone().into());
        }
        if let Some(sticky) = list.sticky {
            match (sticky, site.sticky.is_empty()) {
                (true, true) => return None,
                (false, true) => {}
                (_, false) => {
                    let not = if sticky { "" } else { "NOT " };
                    sql.push_str(&format!(" AND p.id {not}IN ({})", marks(site.sticky.len())));
                    binds.extend(site.sticky.iter().map(|v| orbit_orm::Value::from(*v)));
                }
            }
        }
        for (taxonomy, include, exclude) in &list.terms {
            for (values, negate) in [(include, false), (exclude, true)] {
                if values.is_empty() {
                    continue;
                }
                let not = if negate { "NOT " } else { "" };
                sql.push_str(&format!(
                    " AND {not}EXISTS (SELECT 1 FROM term_relationships r \
                     INNER JOIN term_taxonomy tt ON tt.id = r.term_taxonomy_id \
                     WHERE r.object_id = p.id AND tt.taxonomy = ? AND tt.term_id IN ({}))",
                    marks(values.len())
                ));
                binds.push((*taxonomy).into());
                binds.extend(values.iter().map(|v| orbit_orm::Value::from(*v)));
            }
        }
        if let Some(search) = &list.search {
            for term in search_terms(search) {
                let like = format!("%{}%", escape_like(&term.text.to_lowercase()));
                sql.push_str(if term.exclude { " AND NOT " } else { " AND " });
                if ctl.post_type.is_attachment() {
                    // `wp_allow_query_attachment_by_filename`: an attachment's file name matches
                    // too.
                    sql.push_str(
                        "(LOWER(p.title) LIKE ? ESCAPE '!' OR LOWER(p.excerpt) LIKE ? ESCAPE '!' \
                         OR LOWER(p.content) LIKE ? ESCAPE '!' OR EXISTS (SELECT 1 FROM \
                         post_meta fm WHERE fm.post_id = p.id AND fm.meta_key = '_wp_attached_file' \
                         AND LOWER(fm.meta_value) LIKE ? ESCAPE '!'))",
                    );
                    binds.extend(std::iter::repeat_n(orbit_orm::Value::from(like), 4));
                } else {
                    sql.push_str(
                        "(LOWER(p.title) LIKE ? ESCAPE '!' OR LOWER(p.excerpt) LIKE ? ESCAPE '!' \
                         OR LOWER(p.content) LIKE ? ESCAPE '!')",
                    );
                    binds.extend(std::iter::repeat_n(orbit_orm::Value::from(like), 3));
                }
            }
            // `WP_Query::parse_search`: a visitor's search never matches a protected post, so
            // its hidden words cannot be probed.
            if ctl.user.id() == 0 && !search_terms(search).is_empty() {
                sql.push_str(" AND (p.password IS NULL OR p.password = '')");
            }
        }
        Some((sql, binds))
    }
}

/// The posts of the requested page, in order.
async fn fetch(
    ctl: &PostsController<'_>,
    list: &ListParams,
    filter: &str,
    mut binds: Vec<orbit_orm::Value>,
) -> Result<Vec<Post>, RestError> {
    let direction = if list.order_desc { "DESC" } else { "ASC" };
    let date = "COALESCE(p.published_at, p.modified_at)";
    let mut order: Vec<String> = Vec::new();
    match list.orderby {
        OrderBy::Date => order.push(format!("{date} {direction}")),
        OrderBy::Id => {}
        OrderBy::Author => order.push(format!("p.author_id {direction}")),
        OrderBy::Modified => order.push(format!("p.modified_at {direction}")),
        OrderBy::Parent => order.push(format!("COALESCE(p.parent_id, 0) {direction}")),
        OrderBy::Slug => order.push(format!("p.slug {direction}")),
        OrderBy::Title => order.push(format!("p.title {direction}")),
        OrderBy::MenuOrder => order.push(format!("p.menu_order {direction}")),
        OrderBy::Include => {
            order.push(position_case("p.id", list.include.len()));
            binds.extend(list.include.iter().map(|v| orbit_orm::Value::from(*v)));
        }
        OrderBy::IncludeSlugs => {
            order.push(position_case("p.slug", list.slug.len()));
            binds.extend(list.slug.iter().map(|v| orbit_orm::Value::from(v.clone())));
        }
        OrderBy::Relevance => {
            let terms = search_terms(list.search.as_deref().unwrap_or_default());
            let (clause, relevance_binds) =
                relevance(list.search.as_deref().unwrap_or_default(), &terms);
            order.push(clause);
            binds.extend(relevance_binds);
            order.push(format!("{date} DESC"));
        }
    }
    // The id breaks ties, so posts saved in the same second keep one order.
    order.push(format!("p.id {direction}"));
    let offset = match list.offset {
        Some(offset) => offset,
        None => (list.page - 1).saturating_mul(list.per_page),
    };
    let sql = format!(
        "SELECT p.id {filter} ORDER BY {} LIMIT ? OFFSET ?",
        order.join(", ")
    );
    binds.push(list.per_page.into());
    binds.push(offset.into());
    let ids: Vec<(i64,)> = raw::fetch_all(ctl.db, &sql, &binds)
        .await
        .map_err(|e| database_error(&e))?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
    let mut posts = Post::query()
        .where_in(Post::PRIMARY_KEY, ids.clone())
        .get(ctl.db)
        .await
        .map_err(|e| database_error(&e))?;
    posts.sort_by_key(|post| ids.iter().position(|id| *id == post.id));
    Ok(posts)
}

/// WordPress's search order (`WP_Query::parse_search_order`): with one term, posts whose title
/// has it first; with more, the whole phrase in the title, then every term in the title, then
/// any term in the title, then the phrase in the excerpt, then in the content.
fn relevance(search: &str, terms: &[SearchTerm]) -> (String, Vec<orbit_orm::Value>) {
    let like =
        |text: &str| orbit_orm::Value::from(format!("%{}%", escape_like(&text.to_lowercase())));
    let included: Vec<&SearchTerm> = terms.iter().filter(|t| !t.exclude).collect();
    let title = "LOWER(p.title) LIKE ? ESCAPE '!'";
    if included.len() <= 1 {
        let text = included.first().map_or(search, |t| t.text.as_str());
        return (
            format!("CASE WHEN {title} THEN 0 ELSE 1 END"),
            vec![like(text)],
        );
    }
    let mut binds = vec![like(search)];
    let all: Vec<&str> = included.iter().map(|_| title).collect();
    binds.extend(included.iter().map(|t| like(&t.text)));
    binds.extend(included.iter().map(|t| like(&t.text)));
    binds.push(like(search));
    binds.push(like(search));
    (
        format!(
            "CASE WHEN {title} THEN 1 WHEN ({}) THEN 2 WHEN ({}) THEN 3 \
             WHEN LOWER(p.excerpt) LIKE ? ESCAPE '!' THEN 4 \
             WHEN LOWER(p.content) LIKE ? ESCAPE '!' THEN 5 ELSE 6 END",
            all.join(" AND "),
            all.join(" OR ")
        ),
        binds,
    )
}

/// The arguments of the collection's `GET`, as WordPress's `get_collection_params` lists them.
pub(crate) fn collection_args(post_type: &PostType) -> Map<String, Value> {
    let ids = |description: &str| json!({"description": description, "type": "array", "items": {"type": "integer"}, "default": [], "required": false});
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
    put(
        "author",
        ids("Limit result set to posts assigned to specific authors."),
    );
    put(
        "author_exclude",
        ids("Ensure result set excludes posts assigned to specific authors."),
    );
    put("exclude", ids("Ensure result set excludes specific IDs."));
    put("include", ids("Limit result set to specific IDs."));
    if post_type.has_parent() {
        put(
            "menu_order",
            json!({"description": "Limit result set to posts with a specific menu_order value.", "type": "integer", "required": false}),
        );
    }
    if post_type.filters_by_parent() {
        put(
            "parent",
            ids("Limit result set to items with particular parent IDs."),
        );
        put(
            "parent_exclude",
            ids("Limit result set to all items except those of a particular parent ID."),
        );
    }
    put(
        "offset",
        json!({"description": "Offset the result set by a specific number of items.", "type": "integer", "required": false}),
    );
    put(
        "order",
        json!({"description": "Order sort attribute ascending or descending.", "type": "string", "default": "desc", "enum": ["asc", "desc"], "required": false}),
    );
    put(
        "orderby",
        json!({"description": "Sort collection by post attribute.", "type": "string", "default": "date", "enum": orderby_values(post_type), "required": false}),
    );
    put(
        "slug",
        json!({"description": "Limit result set to posts with one or more specific slugs.", "type": "array", "items": {"type": "string"}, "required": false}),
    );
    let (statuses, default) = statuses_of(post_type);
    put(
        "status",
        json!({"default": default, "description": "Limit result set to posts assigned one or more statuses.", "type": "array", "items": {"enum": statuses, "type": "string"}, "required": false}),
    );
    if post_type.is_attachment() {
        put(
            "media_type",
            json!({"default": null, "description": "Limit result set to attachments of a particular media type.", "type": "string", "enum": MEDIA_TYPES, "required": false}),
        );
        put(
            "mime_type",
            json!({"default": null, "description": "Limit result set to attachments of a particular MIME type.", "type": "string", "required": false}),
        );
    }
    for taxonomy in &post_type.taxonomies {
        let base = taxonomy.rest_base;
        put(
            base,
            json!({"description": format!("Limit result set to items with specific terms assigned in the {base} taxonomy."), "type": "array", "items": {"type": "integer"}, "default": [], "required": false}),
        );
        put(
            &format!("{base}_exclude"),
            json!({"description": format!("Limit result set to items except those with specific terms assigned in the {base} taxonomy."), "type": "array", "items": {"type": "integer"}, "default": [], "required": false}),
        );
    }
    if post_type.has_sticky() {
        put(
            "sticky",
            json!({"description": "Limit result set to items that are sticky.", "type": "boolean", "required": false}),
        );
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_compare_whole() {
        assert!(constant_time_eq(b"sesame", b"sesame"));
        assert!(!constant_time_eq(b"sesame", b"sesam"));
        assert!(!constant_time_eq(b"sesame", b"Sesame"));
    }
}
