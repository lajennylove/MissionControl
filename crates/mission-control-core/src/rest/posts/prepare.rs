//! A post as the REST API shows it: `prepare_item_for_response` and `prepare_links` of
//! `WP_REST_Posts_Controller`, the context filter of `filter_response_by_context`, `_fields`
//! (`rest_filter_response_fields`) and `_embed` (`WP_REST_Server::embed_links`).
//!
//! What every post of a page needs beyond its row (links, terms, meta, revision counts) is read
//! in one query per kind for the whole page.

use std::collections::{BTreeMap, HashMap};

use chrono::{FixedOffset, NaiveDateTime};
use orbit_orm::{Database, Value as Sql, raw};
use serde_json::{Map, Value, json};

use super::PostsController;
use super::query::constant_time_eq;
use crate::content::{Post, PostStatus, make_slug, now};
use crate::front::{post_links, term_links, trim_excerpt};
use crate::hooks::THE_CONTENT;
use crate::rest::meta::{MetaObject, MetaValues};
use crate::rest::shared::{Context, Fields, Site, curies, database_error};
use crate::rest::{RestError, media, users};
use crate::text::{escape_html, html_class};

/// How many ids one `IN (...)` binds.
const IN_CHUNK: usize = 500;

/// The post meta the response reads.
const THUMBNAIL: &str = "_thumbnail_id";
const TEMPLATE: &str = "_wp_page_template";
pub(crate) const ATTACHED_FILE: &str = "_wp_attached_file";
pub(crate) const ATTACHMENT_METADATA: &str = "_wp_attachment_metadata";
pub(crate) const ATTACHMENT_ALT: &str = "_wp_attachment_image_alt";

/// How the posts of a request are shown.
#[derive(Debug, Clone)]
pub(crate) struct ItemOptions {
    /// The request's context.
    pub(crate) context: Context,
    /// The `password` parameter, checked already against a single post.
    pub(crate) password: Option<String>,
    /// `_fields`.
    pub(crate) fields: Fields,
    /// `_embed`: `None` for no embedding, an empty list for every relation.
    pub(crate) embed: Option<Vec<String>>,
}

/// One term of a post.
#[derive(Debug, Clone)]
struct TermRow {
    taxonomy: String,
    term_id: i64,
    name: String,
    slug: String,
}

/// What the posts of a page need beyond their rows.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    /// Each post's link path, in order.
    links: Vec<String>,
    /// Each post's sample link path (`%postname%` in place of the slug), in order; edit context
    /// only.
    samples: Vec<String>,
    /// Each post's terms, by post id, ordered by name.
    terms: HashMap<i64, Vec<TermRow>>,
    /// Post meta by post id and key.
    pub(crate) meta: HashMap<(i64, String), Value>,
    /// Each post's revision count and latest revision id.
    revisions: HashMap<i64, (i64, i64)>,
    /// The registered meta shown under `meta`.
    registered: MetaValues,
}

/// The JSON of `posts` for `options`, in order.
pub(crate) async fn items(
    ctl: &PostsController<'_>,
    site: &Site,
    posts: &[Post],
    options: &ItemOptions,
) -> Result<Vec<Value>, RestError> {
    if posts.is_empty() {
        return Ok(Vec::new());
    }
    let db = ctl.db;
    let ids: Vec<i64> = posts.iter().map(|p| p.id).collect();
    let with_links = options.fields.includes("_links") || options.fields.includes("_embedded");
    let mut batch = Batch {
        links: post_links(db, &site.structure, posts)
            .await
            .map_err(|e| database_error(&e))?,
        terms: post_terms(db, &ids).await?,
        meta: post_meta(db, &ids).await?,
        ..Batch::default()
    };
    if options.context == Context::Edit {
        batch.samples = sample_links(ctl, site, posts).await?;
    }
    if ctl.post_type.supports.custom_fields
        && options.context != Context::Embed
        && options.fields.includes("meta")
    {
        batch.registered =
            MetaValues::load(db, ctl.meta, MetaObject::Post, &ctl.post_type.name, &ids).await?;
    }
    if with_links && ctl.post_type.supports.revisions {
        batch.revisions = revision_counts(db, &ids).await?;
    }
    let embedded = match (&options.embed, with_links) {
        (Some(embed), true) => Some(Embeds::load(ctl, site, posts, &batch, embed).await?),
        _ => None,
    };
    let mut out = Vec::with_capacity(posts.len());
    for (i, post) in posts.iter().enumerate() {
        let trash = trash_status(&batch, post);
        let password_allowed = password_allowed(ctl, site, post, trash, options);
        let item = full_item(ctl, site, post, i, &batch, options, password_allowed);
        let mut item = options.fields.filter(by_context(item, options.context));
        if with_links {
            let links = links(ctl, site, post, trash, &batch, options.context);
            if let Some(embeds) = &embedded
                && let Some(value) = embeds.for_post(post, &links, options.embed.as_deref())
            {
                item.insert("_embedded".to_owned(), value);
            }
            item.insert("_links".to_owned(), Value::Object(links));
        }
        out.push(Value::Object(item));
    }
    Ok(out)
}

/// The status a trashed post had, from the batch's meta.
fn trash_status(batch: &Batch, post: &Post) -> Option<PostStatus> {
    if post.status != PostStatus::Trash {
        return None;
    }
    batch
        .meta
        .get(&(post.id, "_wp_trash_meta_status".to_owned()))
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// Whether the request may see `post`'s protected content, as `can_access_password_content`:
/// in the edit context for a user who may edit it, or with the right `password`.
fn password_allowed(
    ctl: &PostsController<'_>,
    site: &Site,
    post: &Post,
    trash: Option<PostStatus>,
    options: &ItemOptions,
) -> bool {
    let Some(stored) = post.password.as_deref().filter(|p| !p.is_empty()) else {
        return false;
    };
    if options.context == Context::Edit && ctl.can_on("edit_post", post, trash, site) {
        return true;
    }
    options
        .password
        .as_deref()
        .is_some_and(|given| constant_time_eq(stored.as_bytes(), given.as_bytes()))
}

/// `Y-m-d\TH:i:s`, WordPress's REST date format, of `utc` in the site's time zone.
fn local_date(utc: NaiveDateTime, offset: FixedOffset) -> String {
    (utc + offset).format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn utc_date(utc: NaiveDateTime) -> String {
    utc.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// The GUID WordPress gives a post when it is first saved as a draft: `?p=ID` for a post,
/// `?page_id=ID` for a page, `?post_type=TYPE&p=ID` for another type. Mission Control stores
/// none, so it is made the same way each time.
fn guid(site: &Site, post: &Post) -> String {
    match post.post_type.as_str() {
        "post" => format!("{}/?p={}", site.home, post.id),
        "page" => format!("{}/?page_id={}", site.home, post.id),
        other => format!("{}/?post_type={other}&p={}", site.home, post.id),
    }
}

/// The title as `get_the_title` gives it outside the admin: `Protected: {title}` for a
/// password-protected post, `Private: {title}` for a private one.
pub(crate) fn rendered_title(post: &Post) -> String {
    if post.password.as_deref().is_some_and(|p| !p.is_empty()) {
        format!("Protected: {}", post.title)
    } else if post.status == PostStatus::Private {
        format!("Private: {}", post.title)
    } else {
        post.title.clone()
    }
}

/// Every field of `post`, as the edit context shows it; [`by_context`] removes what another
/// context leaves out.
fn full_item(
    ctl: &PostsController<'_>,
    site: &Site,
    post: &Post,
    index: usize,
    batch: &Batch,
    options: &ItemOptions,
    password_allowed: bool,
) -> Map<String, Value> {
    let post_type = ctl.post_type;
    let supports = post_type.supports;
    let protected = post.password.as_deref().is_some_and(|p| !p.is_empty());
    let required = protected && !password_allowed;
    let date = post.published_at.unwrap_or(post.modified_at);
    let mut item = Map::new();
    let mut put = |key: &str, value: Value| {
        item.insert(key.to_owned(), value);
    };
    put("id", json!(post.id));
    put("date", json!(local_date(date, site.offset)));
    put("date_gmt", json!(utc_date(date)));
    // An attachment's GUID is its file's address, as `wp_insert_attachment` stores it.
    let guid = if post_type.is_attachment() {
        media::source_url(site, batch, post.id)
    } else {
        guid(site, post)
    };
    put("guid", json!({"rendered": guid, "raw": guid}));
    put("modified", json!(local_date(post.modified_at, site.offset)));
    put("modified_gmt", json!(utc_date(post.modified_at)));
    // The attachments controller removes `password` from its schema.
    if !post_type.is_attachment() {
        put("password", json!(post.password.clone().unwrap_or_default()));
    }
    put("slug", json!(post.slug));
    put("status", json!(post.status.as_str()));
    put("type", json!(post.post_type));
    put(
        "link",
        json!(format!("{}{}", site.home, batch.links[index])),
    );
    if supports.title {
        put(
            "title",
            json!({"raw": post.title, "rendered": rendered_title(post)}),
        );
    }
    // Rendered only when it is shown: `the_content` runs the shortcodes.
    let rendered_content = if required
        || !(supports.editor || supports.excerpt)
        || !(options.fields.includes("content.rendered")
            || options.fields.includes("excerpt.rendered"))
    {
        String::new()
    } else {
        ctl.hooks.apply_filters(THE_CONTENT, post.content.clone())
    };
    if supports.editor {
        put(
            "content",
            json!({
                "raw": post.content,
                "rendered": if required { "" } else { rendered_content.as_str() },
                "protected": protected,
                "block_version": i32::from(post.content.contains("<!-- wp:")),
            }),
        );
    }
    if supports.excerpt {
        let rendered = if required {
            String::new()
        } else {
            // Ruling R34, as the front end: a manual excerpt stays text.
            let text = if post.excerpt.trim().is_empty() {
                trim_excerpt(&rendered_content)
            } else {
                escape_html(&post.excerpt)
            };
            if text.is_empty() {
                String::new()
            } else {
                format!("<p>{text}</p>\n")
            }
        };
        put(
            "excerpt",
            json!({"raw": post.excerpt, "rendered": rendered, "protected": protected}),
        );
    }
    if supports.author {
        put("author", json!(post.author_id));
    }
    let thumbnail = meta_id(batch, post.id, THUMBNAIL);
    if supports.thumbnail {
        put("featured_media", json!(thumbnail.unwrap_or(0)));
    }
    if post_type.has_parent() {
        put("parent", json!(post.parent_id.unwrap_or(0)));
    }
    if supports.page_attributes {
        put("menu_order", json!(post.menu_order));
    }
    if supports.comments {
        // No comments yet: closed, whatever the type's default.
        put("comment_status", json!("closed"));
        put("ping_status", json!("closed"));
    }
    if post_type.has_sticky() {
        put("sticky", json!(site.sticky.contains(&post.id)));
    }
    let template = batch
        .meta
        .get(&(post.id, TEMPLATE.to_owned()))
        .and_then(Value::as_str)
        .filter(|t| *t != "default")
        .unwrap_or("");
    put("template", json!(template));
    if supports.post_formats {
        put("format", json!("standard"));
    }
    if supports.custom_fields {
        put("meta", batch.registered.json(post.id));
    }
    let terms = batch.terms.get(&post.id).map_or(&[][..], Vec::as_slice);
    for taxonomy in &post_type.taxonomies {
        let ids: Vec<i64> = terms
            .iter()
            .filter(|t| t.taxonomy == taxonomy.taxonomy.name)
            .map(|t| t.term_id)
            .collect();
        put(&taxonomy.rest_base, json!(ids));
    }
    if options.context == Context::Edit {
        put(
            "permalink_template",
            json!(format!("{}{}", site.home, batch.samples[index])),
        );
        let generated = if !post.slug.is_empty() {
            post.slug.clone()
        } else {
            Some(make_slug(&post.title))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| post.id.to_string())
        };
        put("generated_slug", json!(generated));
    }
    put(
        "class_list",
        json!(class_list(
            post,
            terms,
            thumbnail.is_some(),
            protected,
            required,
            post_type.supports.post_formats
        )),
    );
    if post_type.is_attachment() {
        media::add_fields(
            &mut item,
            ctl,
            site,
            post,
            batch,
            options.context,
            &options.fields,
        );
    }
    item
}

/// `get_post_class` for the REST API: `post-ID`, the type, `type-TYPE`, `status-STATUS`, the
/// format, the password and thumbnail classes, `hentry`, then a class per term.
fn class_list(
    post: &Post,
    terms: &[TermRow],
    has_thumbnail: bool,
    protected: bool,
    required: bool,
    formats: bool,
) -> Vec<String> {
    let post_type = html_class(&post.post_type);
    let mut classes = vec![
        format!("post-{}", post.id),
        post_type.clone(),
        format!("type-{post_type}"),
        format!("status-{}", post.status.as_str()),
    ];
    if formats {
        classes.push("format-standard".to_owned());
    }
    if required {
        classes.push("post-password-required".to_owned());
    } else if protected {
        classes.push("post-password-protected".to_owned());
    }
    if has_thumbnail && !required {
        classes.push("has-post-thumbnail".to_owned());
    }
    classes.push("hentry".to_owned());
    for term in terms {
        let prefix = match term.taxonomy.as_str() {
            "category" => "category".to_owned(),
            "post_tag" => "tag".to_owned(),
            other => html_class(other),
        };
        let slug = html_class(&term.slug);
        // WordPress uses the id when the slug gives no class, or a number.
        let name = if slug.is_empty() || slug.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
            term.term_id.to_string()
        } else {
            slug
        };
        classes.push(format!("{prefix}-{name}"));
    }
    classes
}

/// A meta value that holds a post id.
fn meta_id(batch: &Batch, post_id: i64, key: &str) -> Option<i64> {
    match batch.meta.get(&(post_id, key.to_owned()))? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|id| *id > 0)
}

/// The contexts a field is shown in: everything in `edit`; `embed` shows only the fields
/// WordPress's schema marks for it.
fn by_context(mut item: Map<String, Value>, context: Context) -> Map<String, Value> {
    if context == Context::Edit {
        return item;
    }
    const EDIT_ONLY: &[&str] = &[
        "password",
        "permalink_template",
        "generated_slug",
        "missing_image_sizes",
    ];
    // The attachment fields after `featured_media` are an attachment's only.
    const EMBED: &[&str] = &[
        "id",
        "date",
        "slug",
        "type",
        "link",
        "title",
        "excerpt",
        "author",
        "featured_media",
        "caption",
        "alt_text",
        "media_type",
        "mime_type",
        "media_details",
        "source_url",
    ];
    item.retain(|key, _| {
        !EDIT_ONLY.contains(&key.as_str())
            && (context != Context::Embed || EMBED.contains(&key.as_str()))
    });
    for (key, value) in &mut item {
        if let Value::Object(object) = value {
            object.remove("raw");
            if key == "content" {
                object.remove("block_version");
            }
        }
    }
    item
}

/// The `_links` of `post`, as `prepare_links` and, in the edit context, `get_available_actions`
/// give them. There are no comments yet, so no `replies` link.
fn links(
    ctl: &PostsController<'_>,
    site: &Site,
    post: &Post,
    trash: Option<PostStatus>,
    batch: &Batch,
    context: Context,
) -> Map<String, Value> {
    let post_type = ctl.post_type;
    let base = format!("wp/v2/{}", post_type.rest_base);
    let this = site.rest_url(&format!("{base}/{}", post.id));
    let mut allow = vec!["GET"];
    if ctl.can_on("edit_post", post, trash, site) {
        allow.extend(["POST", "PUT", "PATCH"]);
    }
    if ctl.can_on("delete_post", post, trash, site) {
        allow.push("DELETE");
    }
    let mut links = Map::new();
    let mut put = |rel: &str, value: Value| {
        links.insert(rel.to_owned(), value);
    };
    put(
        "self",
        json!([{"href": this, "targetHints": {"allow": allow}}]),
    );
    put("collection", json!([{"href": site.rest_url(&base)}]));
    put(
        "about",
        json!([{"href": site.rest_url(&format!("wp/v2/types/{}", post.post_type))}]),
    );
    if post_type.supports.author && post.author_id != 0 {
        put(
            "author",
            json!([{"embeddable": true, "href": site.rest_url(&format!("wp/v2/users/{}", post.author_id))}]),
        );
    }
    if post_type.supports.revisions {
        let (count, latest) = batch.revisions.get(&post.id).copied().unwrap_or((0, 0));
        put(
            "version-history",
            json!([{"count": count, "href": format!("{this}/revisions")}]),
        );
        if count > 0 {
            put(
                "predecessor-version",
                json!([{"id": latest, "href": format!("{this}/revisions/{latest}")}]),
            );
        }
    }
    if let Some(media) = meta_id(batch, post.id, THUMBNAIL) {
        put(
            "wp:featuredmedia",
            json!([{"embeddable": true, "href": site.rest_url(&format!("wp/v2/media/{media}"))}]),
        );
    }
    if !post_type.is_attachment() {
        put(
            "wp:attachment",
            json!([{"href": format!("{}?parent={}", site.rest_url("wp/v2/media"), post.id)}]),
        );
    }
    if !post_type.taxonomies.is_empty() {
        let terms: Vec<Value> = post_type
            .taxonomies
            .iter()
            .map(|t| {
                json!({
                    "taxonomy": t.taxonomy.name,
                    "embeddable": true,
                    "href": format!("{}?post={}", site.rest_url(&format!("wp/v2/{}", t.rest_base)), post.id),
                })
            })
            .collect();
        put("wp:term", Value::Array(terms));
    }
    if context == Context::Edit {
        let caps = post_type.caps();
        let action = json!([{"href": this}]);
        if ctl.has(&caps.publish_posts) {
            put("wp:action-publish", action.clone());
        }
        if ctl.has("unfiltered_html") {
            put("wp:action-unfiltered-html", action.clone());
        }
        if post_type.has_sticky()
            && ctl.has(&caps.edit_others_posts)
            && ctl.has(&caps.publish_posts)
        {
            put("wp:action-sticky", action.clone());
        }
        if post_type.supports.author && ctl.has(&caps.edit_others_posts) {
            put("wp:action-assign-author", action.clone());
        }
        for taxonomy in &post_type.taxonomies {
            // Categories and tags: `manage_categories` creates them, `edit_posts` assigns them.
            if ctl.has("manage_categories") {
                put(
                    &format!("wp:action-create-{}", taxonomy.rest_base),
                    action.clone(),
                );
            }
            if ctl.has("edit_posts") {
                put(
                    &format!("wp:action-assign-{}", taxonomy.rest_base),
                    action.clone(),
                );
            }
        }
    }
    put("curies", curies());
    links
}

/// The embedded resources of a page of posts: the authors and the terms.
struct Embeds {
    /// The author's embed object, or WordPress's error, by user id.
    authors: HashMap<i64, Value>,
    /// The term embed objects, by `(taxonomy, term_id)`.
    terms: HashMap<(String, i64), Value>,
    /// Each post's terms, by post id.
    post_terms: HashMap<i64, Vec<(String, i64)>>,
    /// The taxonomies' rest bases, by taxonomy name, in the type's order.
    taxonomies: Vec<(String, String)>,
}

impl Embeds {
    async fn load(
        ctl: &PostsController<'_>,
        site: &Site,
        posts: &[Post],
        batch: &Batch,
        embed: &[String],
    ) -> Result<Self, RestError> {
        let wanted = |rel: &str| embed.is_empty() || embed.iter().any(|e| e == rel);
        let mut authors = HashMap::new();
        if wanted("author") && ctl.post_type.supports.author {
            let mut ids: Vec<i64> = posts
                .iter()
                .map(|p| p.author_id)
                .filter(|id| *id != 0)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            // The types whose published posts make an author public: until the post type
            // registry (Task 5.2) reaches here, the built-in types and this one.
            let mut types: Vec<String> = ["post", "page", "attachment"]
                .iter()
                .map(|t| (*t).to_owned())
                .collect();
            if !types.contains(&ctl.post_type.name) {
                types.push(ctl.post_type.name.clone());
            }
            authors = users::embed_authors(ctl.db, site, &ids, ctl.user, &types).await?;
        }
        let mut terms = HashMap::new();
        let mut post_terms: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
        if wanted("wp:term") && !ctl.post_type.taxonomies.is_empty() {
            let mut unique: BTreeMap<(String, i64), &TermRow> = BTreeMap::new();
            for post in posts {
                for row in batch.terms.get(&post.id).into_iter().flatten() {
                    unique.insert((row.taxonomy.clone(), row.term_id), row);
                    post_terms
                        .entry(post.id)
                        .or_default()
                        .push((row.taxonomy.clone(), row.term_id));
                }
            }
            let as_terms: Vec<crate::content::Term> = unique
                .values()
                .map(|row| crate::content::Term {
                    term_id: row.term_id,
                    term_taxonomy_id: 0,
                    name: row.name.clone(),
                    slug: row.slug.clone(),
                    term_group: 0,
                    taxonomy: row.taxonomy.clone(),
                    description: String::new(),
                    parent_id: None,
                    count: 0,
                })
                .collect();
            let paths = term_links(ctl.db, &site.structure, &as_terms)
                .await
                .map_err(|e| database_error(&e))?;
            for (term, path) in as_terms.iter().zip(paths) {
                let base = ctl
                    .post_type
                    .taxonomies
                    .iter()
                    .find(|t| t.taxonomy.name == term.taxonomy)
                    .map_or("", |t| &t.rest_base);
                terms.insert(
                    (term.taxonomy.clone(), term.term_id),
                    json!({
                        "id": term.term_id,
                        "link": path.map(|p| format!("{}{p}", site.home)).unwrap_or_default(),
                        "name": term.name,
                        "slug": term.slug,
                        "taxonomy": term.taxonomy,
                        "_links": {
                            "self": [{"href": site.rest_url(&format!("wp/v2/{base}/{}", term.term_id))}],
                            "collection": [{"href": site.rest_url(&format!("wp/v2/{base}"))}],
                            "about": [{"href": site.rest_url(&format!("wp/v2/taxonomies/{}", term.taxonomy))}],
                            "wp:post_type": [{"href": format!("{}?{}={}", site.rest_url("wp/v2/posts"), base, term.term_id)}],
                            "curies": [{"name": "wp", "href": "https://api.w.org/{rel}", "templated": true}],
                        },
                    }),
                );
            }
        }
        Ok(Self {
            authors,
            terms,
            post_terms,
            taxonomies: ctl
                .post_type
                .taxonomies
                .iter()
                .map(|t| (t.taxonomy.name.to_string(), t.rest_base.to_string()))
                .collect(),
        })
    }

    /// `_embedded` for `post`, given its `links`: each embeddable relation asked for, in the
    /// links' order, or `None` when nothing is embedded. A relation whose resources are all
    /// empty is left out, as `embed_links` leaves it.
    fn for_post(
        &self,
        post: &Post,
        links: &Map<String, Value>,
        embed: Option<&[String]>,
    ) -> Option<Value> {
        let wanted = |rel: &str| embed.is_none_or(|e| e.is_empty() || e.iter().any(|x| x == rel));
        let mut embedded = Map::new();
        if wanted("author")
            && links.contains_key("author")
            && let Some(author) = self.authors.get(&post.author_id)
        {
            embedded.insert("author".to_owned(), json!([author]));
        }
        if wanted("wp:term") && links.contains_key("wp:term") {
            let mine = self.post_terms.get(&post.id).map_or(&[][..], Vec::as_slice);
            let lists: Vec<Value> = self
                .taxonomies
                .iter()
                .map(|(taxonomy, _)| {
                    Value::Array(
                        mine.iter()
                            .filter(|(t, _)| t == taxonomy)
                            .filter_map(|key| self.terms.get(key).cloned())
                            .collect(),
                    )
                })
                .collect();
            if lists
                .iter()
                .any(|l| l.as_array().is_some_and(|a| !a.is_empty()))
            {
                embedded.insert("wp:term".to_owned(), Value::Array(lists));
            }
        }
        (!embedded.is_empty()).then_some(Value::Object(embedded))
    }
}

/// The terms of posts `ids`, by post, ordered by name as `get_the_terms` orders them.
async fn post_terms(db: &Database, ids: &[i64]) -> Result<HashMap<i64, Vec<TermRow>>, RestError> {
    let mut terms: HashMap<i64, Vec<TermRow>> = HashMap::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
        let rows: Vec<(i64, String, i64, String, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT r.object_id, tt.taxonomy, tt.term_id, t.name, t.slug \
                 FROM term_relationships r \
                 INNER JOIN term_taxonomy tt ON tt.id = r.term_taxonomy_id \
                 INNER JOIN terms t ON t.id = tt.term_id \
                 WHERE r.object_id IN ({marks})"
            ),
            &binds,
        )
        .await
        .map_err(|e| database_error(&e))?;
        for (post_id, taxonomy, term_id, name, slug) in rows {
            terms.entry(post_id).or_default().push(TermRow {
                taxonomy,
                term_id,
                name,
                slug,
            });
        }
    }
    for list in terms.values_mut() {
        list.sort_by(|a, b| a.name.cmp(&b.name).then(a.term_id.cmp(&b.term_id)));
    }
    Ok(terms)
}

/// The meta the response reads, for posts `ids`: the first value of each key. A value that is
/// not JSON is left out.
async fn post_meta(db: &Database, ids: &[i64]) -> Result<HashMap<(i64, String), Value>, RestError> {
    const KEYS: [&str; 6] = [
        THUMBNAIL,
        TEMPLATE,
        "_wp_trash_meta_status",
        ATTACHED_FILE,
        ATTACHMENT_METADATA,
        ATTACHMENT_ALT,
    ];
    let mut meta = HashMap::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let mut binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
        binds.extend(KEYS.iter().map(|k| Sql::from(*k)));
        let rows: Vec<(i64, String, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT post_id, meta_key, meta_value FROM post_meta \
                 WHERE post_id IN ({marks}) AND meta_key IN (?, ?, ?, ?, ?, ?) ORDER BY id"
            ),
            &binds,
        )
        .await
        .map_err(|e| database_error(&e))?;
        for (post_id, key, value) in rows {
            // Exact keys: MySQL compares text without case.
            if !KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Ok(value) = serde_json::from_str(&value) {
                meta.entry((post_id, key)).or_insert(value);
            }
        }
    }
    Ok(meta)
}

/// Each post's revision count and latest revision id, autosaves included, as
/// `wp_get_latest_revision_id_and_total_count` counts them.
async fn revision_counts(
    db: &Database,
    ids: &[i64],
) -> Result<HashMap<i64, (i64, i64)>, RestError> {
    let mut counts = HashMap::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let mut binds: Vec<Sql> = vec![Sql::from("revision")];
        binds.extend(chunk.iter().map(|id| Sql::from(*id)));
        let rows: Vec<(i64, i64, i64)> = raw::fetch_all(
            db,
            &format!(
                "SELECT parent_id, COUNT(*), MAX(id) FROM posts \
                 WHERE post_type = ? AND parent_id IN ({marks}) GROUP BY parent_id"
            ),
            &binds,
        )
        .await
        .map_err(|e| database_error(&e))?;
        for (parent, count, latest) in rows {
            counts.insert(parent, (count, latest));
        }
    }
    Ok(counts)
}

/// Each post's sample link, as `get_sample_permalink` makes it: the post as if published, with
/// `%postname%` (or `%pagename%` for a hierarchical type) in place of its slug. Under plain
/// permalinks, the link itself.
async fn sample_links(
    ctl: &PostsController<'_>,
    site: &Site,
    posts: &[Post],
) -> Result<Vec<String>, RestError> {
    let placeholder = if ctl.post_type.hierarchical {
        "%pagename%"
    } else {
        "%postname%"
    };
    let samples: Vec<Post> = posts
        .iter()
        .map(|post| {
            if site.structure.is_plain() {
                post.clone()
            } else {
                Post {
                    status: PostStatus::Publish,
                    slug: placeholder.to_owned(),
                    published_at: Some(post.published_at.unwrap_or_else(now)),
                    ..post.clone()
                }
            }
        })
        .collect();
    post_links(ctl.db, &site.structure, &samples)
        .await
        .map_err(|e| database_error(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_context_hides_raw_fields() {
        let Value::Object(item) = json!({
            "id": 1,
            "password": "",
            "title": {"raw": "a", "rendered": "a"},
            "content": {"raw": "c", "rendered": "c", "protected": false, "block_version": 0},
            "status": "publish",
        }) else {
            unreachable!()
        };
        assert_eq!(
            Value::Object(by_context(item.clone(), Context::View)),
            json!({"id": 1, "title": {"rendered": "a"}, "content": {"rendered": "c", "protected": false}, "status": "publish"})
        );
        assert_eq!(
            Value::Object(by_context(item, Context::Embed)),
            json!({"id": 1, "title": {"rendered": "a"}})
        );
    }
}
