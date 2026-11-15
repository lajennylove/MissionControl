//! Writing posts: `create_item`, `update_item` and `delete_item` of `WP_REST_Posts_Controller`,
//! with their permission checks and `prepare_item_for_database`.

use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime};
use orbit_orm::{Model, raw};
use serde_json::{Map, Value, json};

use super::prepare::{self, ItemOptions};
use super::{PostType, PostsController, RestTaxonomy, invalid_id};
use crate::content::{
    MetaError, NewPost, Post, PostFields, PostStatus, SaveError, delete_post_meta, now,
    save_revision, set_option, set_post_meta,
};
use crate::content::{PUBLISH_FUTURE_POST, terms_by_term_id};
use crate::rest::meta::{MetaObject, MetaUpdate};
use crate::rest::params::{Params, Reader, as_integer};
use crate::rest::shared::{Context, Fields, Site, database_error, id_of, option};
use crate::rest::{RestError, RestResponse};
use crate::scheduler;
use crate::theme::page_template_name;

/// The statuses a post may be written with, as the schema's `status` enum.
const WRITABLE_STATUSES: &[&str] = &["publish", "future", "draft", "pending", "private"];

/// WordPress's post formats.
const FORMATS: &[&str] = &[
    "standard", "aside", "chat", "gallery", "link", "image", "quote", "status", "video", "audio",
];

/// How many times an update without `If-Unmodified-Since` is retried when another save lands
/// between its read and its write.
const MAX_ATTEMPTS: usize = 5;

/// The fields a request writes, read and checked from its parameters. `None` leaves a field as
/// it is.
#[derive(Debug, Default)]
struct Changes {
    title: Option<String>,
    content: Option<String>,
    excerpt: Option<String>,
    slug: Option<String>,
    status: Option<PostStatus>,
    /// `Some(None)` resets the date (a `null` date).
    date: Option<Option<NaiveDateTime>>,
    author: Option<i64>,
    password: Option<String>,
    /// `Some(None)` for no parent (0).
    parent: Option<Option<i64>>,
    menu_order: Option<i32>,
    featured_media: Option<i64>,
    template: Option<String>,
    sticky: Option<bool>,
    /// The `term_id`s per taxonomy given.
    terms: Vec<(RestTaxonomy, Vec<i64>)>,
}

/// A `title`, `content` or `excerpt` parameter: a string, or an object with `raw`.
fn raw_text(r: &mut Reader<'_>, name: &str) -> Option<String> {
    match r.raw(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Object(object) => match object.get("raw") {
            Some(Value::String(s)) => Some(s.clone()),
            None => None,
            Some(_) => {
                r.invalid.wrong_type(&format!("{name}[raw]"), "string");
                None
            }
        },
        _ => {
            r.invalid.wrong_type(name, "object");
            None
        }
    }
}

/// A date as `rest_parse_date` reads it: `YYYY-MM-DDTHH:MM:SS`, with optional fractions and an
/// optional zone (`Z`, `+02:00`). Without a zone, `date` is in the site's time zone and
/// `date_gmt` in UTC. Returns the UTC time.
fn parse_date(text: &str, offset: Option<FixedOffset>) -> Option<NaiveDateTime> {
    let text = text.trim();
    if let Ok(with_zone) = DateTime::parse_from_rfc3339(&text.replacen(' ', "T", 1)) {
        return Some(with_zone.naive_utc());
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())?;
    let utc = match offset {
        Some(offset) => naive - Duration::seconds(i64::from(offset.local_minus_utc())),
        None => naive,
    };
    Some(chrono::SubsecRound::trunc_subsecs(utc, 0))
}

impl Changes {
    /// Reads the writable fields of `post_type` from `params`, as the schema validates them.
    fn read(post_type: &PostType, site: &Site, params: &Params) -> Result<Self, RestError> {
        let mut r = Reader::new(params);
        let mut changes = Self::default();
        let supports = post_type.supports;
        if supports.title {
            changes.title = raw_text(&mut r, "title");
        }
        if supports.editor {
            changes.content = raw_text(&mut r, "content");
        }
        if supports.excerpt {
            changes.excerpt = raw_text(&mut r, "excerpt");
        }
        changes.slug = r.string("slug");
        changes.status = r
            .one_of("status", WRITABLE_STATUSES)
            .and_then(|s| s.parse().ok());
        // A null `date` or `date_gmt` resets the date; `date` wins over `date_gmt`.
        for (name, offset) in [("date", Some(site.offset)), ("date_gmt", None)] {
            match r.raw(name) {
                None => {}
                Some(Value::Null) => {
                    changes.date.get_or_insert(None);
                }
                Some(Value::String(text)) => match parse_date(text, offset) {
                    Some(date) => {
                        changes.date.get_or_insert(Some(date));
                    }
                    None => r.invalid.add(
                        name,
                        "rest_invalid_date",
                        "Invalid date.".to_owned(),
                        json!({"param": name}),
                    ),
                },
                Some(_) => r.invalid.wrong_type(name, "string"),
            }
        }
        if supports.author {
            changes.author = r.integer("author", Some(0), None);
        }
        changes.password = r.string("password");
        if post_type.has_parent() {
            changes.parent = r
                .integer("parent", Some(0), None)
                .map(|p| (p > 0).then_some(p));
        }
        if supports.page_attributes {
            changes.menu_order = r
                .integer(
                    "menu_order",
                    Some(i64::from(i32::MIN)),
                    Some(i64::from(i32::MAX)),
                )
                .map(|n| i32::try_from(n).expect("checked against i32's range"));
        }
        if supports.thumbnail {
            changes.featured_media = r.integer("featured_media", Some(0), None);
        }
        changes.template = r.string("template");
        if post_type.has_sticky() {
            changes.sticky = r.boolean("sticky");
        }
        if supports.post_formats {
            // Checked as WordPress checks it; formats are not stored yet.
            r.one_of("format", FORMATS);
        }
        if supports.comments {
            // Checked; there are no comments to open or close yet.
            r.one_of("comment_status", &["open", "closed"]);
            r.one_of("ping_status", &["open", "closed"]);
        }
        if supports.custom_fields
            && let Some(meta) = r.raw("meta")
            && !meta.is_object()
            && !meta.as_array().is_some_and(Vec::is_empty)
        {
            r.invalid.wrong_type("meta", "object");
        }
        for taxonomy in &post_type.taxonomies {
            if r.has(&taxonomy.rest_base) {
                let ids = r.integers(&taxonomy.rest_base, 0);
                changes.terms.push((taxonomy.clone(), ids));
            }
        }
        r.invalid.finish()?;
        Ok(changes)
    }
}

/// The checks of `create_item_permissions_check` and `update_item_permissions_check` that read
/// the parameters: writing as another author, making a post sticky, and assigning terms.
async fn check_fields_permission(
    ctl: &PostsController<'_>,
    changes: &Changes,
    creating: bool,
) -> Result<(), RestError> {
    let caps = ctl.post_type.caps();
    if let Some(author) = changes.author
        && author != 0
        && author != ctl.user.id()
        && !ctl.has(&caps.edit_others_posts)
    {
        return Err(ctl.forbidden(
            "rest_cannot_edit_others",
            if creating {
                "Sorry, you are not allowed to create posts as this user."
            } else {
                "Sorry, you are not allowed to update posts as this user."
            },
        ));
    }
    if changes.sticky == Some(true)
        && !ctl.has(&caps.edit_others_posts)
        && !ctl.has(&caps.publish_posts)
    {
        return Err(ctl.forbidden(
            "rest_cannot_assign_sticky",
            "Sorry, you are not allowed to make posts sticky.",
        ));
    }
    if creating && !ctl.has(&caps.edit_posts) {
        return Err(ctl.forbidden(
            "rest_cannot_create",
            "Sorry, you are not allowed to create posts as this user.",
        ));
    }
    // `assign_term` on categories and tags maps to `edit_posts`; unknown terms are skipped later.
    for (taxonomy, ids) in &changes.terms {
        if ids.is_empty() {
            continue;
        }
        let known = terms_by_term_id(ctl.db, &taxonomy.taxonomy, ids)
            .await
            .map_err(|e| database_error(&e))?;
        if !known.is_empty() && !ctl.has("edit_posts") {
            return Err(ctl.forbidden(
                "rest_cannot_assign_term",
                "Sorry, you are not allowed to assign the provided terms.",
            ));
        }
    }
    Ok(())
}

/// `handle_status_param`: publishing (or `future`) needs `publish_posts`, as does `private`.
fn check_status(ctl: &PostsController<'_>, status: PostStatus) -> Result<(), RestError> {
    let caps = ctl.post_type.caps();
    let message = match status {
        PostStatus::Private => {
            "Sorry, you are not allowed to create private posts in this post type."
        }
        PostStatus::Publish | PostStatus::Future => {
            "Sorry, you are not allowed to publish posts in this post type."
        }
        _ => return Ok(()),
    };
    if ctl.has(&caps.publish_posts) {
        Ok(())
    } else {
        Err(ctl.forbidden("rest_cannot_publish", message))
    }
}

/// Whether user `id` exists.
async fn user_exists(ctl: &PostsController<'_>, id: i64) -> Result<bool, RestError> {
    let count: i64 = raw::fetch_scalar(
        ctl.db,
        "SELECT COUNT(*) FROM users WHERE id = ?",
        &[id.into()],
    )
    .await
    .map_err(|e| database_error(&e))?;
    Ok(count > 0)
}

/// The checks of `prepare_item_for_database` that read the database: the author exists, the
/// parent exists and is not the post or one of its children, the featured media is an
/// attachment, and the template is one the theme has.
async fn check_references(
    ctl: &PostsController<'_>,
    changes: &Changes,
    current_template: Option<&str>,
) -> Result<(), RestError> {
    if let Some(author) = changes.author
        && author != 0
        && !user_exists(ctl, author).await?
    {
        return Err(RestError::new(
            "rest_invalid_author",
            "Invalid author ID.",
            400,
        ));
    }
    if let Some(Some(parent)) = changes.parent {
        let found = Post::query()
            .find(ctl.db, parent)
            .await
            .map_err(|e| database_error(&e))?;
        if found.is_none() {
            return Err(RestError::new(
                "rest_post_invalid_id",
                "Invalid post parent ID.",
                400,
            ));
        }
    }
    if let Some(media) = changes.featured_media
        && media != 0
    {
        let found = Post::query()
            .find(ctl.db, media)
            .await
            .map_err(|e| database_error(&e))?;
        if !found.is_some_and(|p| p.post_type == "attachment") {
            return Err(RestError::new(
                "rest_invalid_featured_media",
                "Invalid featured media ID.",
                400,
            ));
        }
    }
    if let Some(template) = &changes.template
        && !template.is_empty()
        && Some(template.as_str()) != current_template
    {
        let allowed = page_template_name(template).is_some_and(|name| {
            ctl.theme
                .is_some_and(|t| t.locate_template(&[name]).is_some())
        });
        if !allowed {
            return Err(RestError::new(
                "rest_invalid_param",
                "Invalid parameter(s): template",
                400,
            )
            .with_data(
                "params",
                json!({"template": format!("{template} is not a template of the active theme.")}),
            ));
        }
    }
    Ok(())
}

/// The parent to store for `post_id`: `parent`, or none when `parent` is the post or one of its
/// descendants, as `wp_check_post_hierarchy_for_loops` breaks a loop.
async fn loop_free_parent(
    ctl: &PostsController<'_>,
    post_id: i64,
    parent: Option<i64>,
) -> Result<Option<i64>, RestError> {
    let mut next = parent;
    for _ in 0..64 {
        let Some(id) = next else {
            return Ok(parent);
        };
        if id == post_id {
            return Ok(None);
        }
        next = Post::query()
            .find(ctl.db, id)
            .await
            .map_err(|e| database_error(&e))?
            .and_then(|p| p.parent_id);
    }
    Ok(parent)
}

/// How far ahead a `publish` post's date must be to be saved as `future`, as `wp_insert_post`.
const FUTURE_GRACE: Duration = Duration::seconds(60);

/// The status and date to save, as `wp_insert_post` settles them: a published, scheduled or
/// private post gets a date (now) when it has none; a `publish` post dated a minute or more ahead
/// becomes `future` (its publication is scheduled after the save), and a `future` post whose date
/// is not ahead becomes `publish`.
fn settle_status(
    status: PostStatus,
    date: Option<NaiveDateTime>,
) -> (PostStatus, Option<NaiveDateTime>) {
    let current = now();
    match status {
        PostStatus::Publish | PostStatus::Future | PostStatus::Private => {
            let date = date.unwrap_or(current);
            let status = match status {
                PostStatus::Publish if date - current >= FUTURE_GRACE => PostStatus::Future,
                PostStatus::Future if date - current < FUTURE_GRACE => PostStatus::Publish,
                other => other,
            };
            (status, Some(date))
        }
        other => (other, date),
    }
}

/// `rest_invalid_field` when a sticky post would have a password, as WordPress refuses it.
fn check_sticky_password(sticky: bool, password: &str) -> Result<(), RestError> {
    if sticky && !password.is_empty() {
        return Err(RestError::new(
            "rest_invalid_field",
            "A post can not be sticky and have a password.",
            400,
        ));
    }
    Ok(())
}

/// The writes after the post row: sticky, featured media, template, terms, and the scheduled
/// publication.
async fn write_extras(
    ctl: &PostsController<'_>,
    site: &Site,
    post: &Post,
    changes: &Changes,
    creating: bool,
) -> Result<(), RestError> {
    let db = ctl.db;
    let meta_error = |e: MetaError| database_error(&e);
    if ctl.post_type.has_sticky() && (creating || changes.sticky.is_some()) {
        let sticky = changes.sticky.unwrap_or(false);
        // Read again just before the write, so a post made sticky since the request began is
        // kept.
        let mut list: Vec<i64> = match option(db, "sticky_posts").await? {
            Some(Value::Array(ids)) => ids.iter().filter_map(id_of).collect(),
            _ => Vec::new(),
        };
        let had = list.contains(&post.id);
        if sticky && !had {
            list.push(post.id);
        } else if !sticky && had {
            list.retain(|id| *id != post.id);
        }
        if sticky != had {
            set_option(db, "sticky_posts", &json!(list))
                .await
                .map_err(meta_error)?;
        }
    }
    if let Some(media) = changes.featured_media {
        if media == 0 {
            delete_post_meta(db, post.id, "_thumbnail_id")
                .await
                .map_err(meta_error)?;
        } else {
            set_post_meta(db, post.id, "_thumbnail_id", &json!(media))
                .await
                .map_err(meta_error)?;
        }
    }
    if let Some(template) = &changes.template {
        if template.is_empty() || template == "default" {
            delete_post_meta(db, post.id, "_wp_page_template")
                .await
                .map_err(meta_error)?;
        } else {
            set_post_meta(db, post.id, "_wp_page_template", &json!(template))
                .await
                .map_err(meta_error)?;
        }
    }
    let mut terms: Vec<(RestTaxonomy, Vec<i64>)> = changes.terms.clone();
    // `wp_insert_post` puts a new post in the default category when it is given none.
    if creating
        && let Some(categories) = ctl
            .post_type
            .taxonomies
            .iter()
            .find(|t| t.taxonomy.name == RestTaxonomy::CATEGORIES.taxonomy.name)
        && !terms
            .iter()
            .any(|(t, ids)| t.rest_base == categories.rest_base && !ids.is_empty())
        && let Some(default) = site.default_category
    {
        terms.retain(|(t, _)| t.rest_base != categories.rest_base);
        terms.push((categories.clone(), vec![default]));
    }
    for (taxonomy, ids) in terms {
        let known = terms_by_term_id(db, &taxonomy.taxonomy, &ids)
            .await
            .map_err(|e| database_error(&e))?;
        let tt_ids: Vec<i64> = known.iter().map(|t| t.term_taxonomy_id).collect();
        post.sync_terms(db, &taxonomy.taxonomy, &tt_ids)
            .await
            .map_err(|e| database_error(&e))?;
    }
    // `_future_post_hook`: one event for a scheduled post, none for any other.
    scheduler::unschedule(db, PUBLISH_FUTURE_POST, &post.id)
        .await
        .map_err(|e| database_error(&e))?;
    if let (PostStatus::Future, Some(date)) = (post.status, post.published_at) {
        scheduler::schedule_single(db, date, PUBLISH_FUTURE_POST, &post.id)
            .await
            .map_err(|e| database_error(&e))?;
    }
    Ok(())
}

/// The post, read again, as the edit context shows it.
pub(super) async fn respond(
    ctl: &PostsController<'_>,
    site: &Site,
    id: i64,
    status: u16,
) -> Result<RestResponse, RestError> {
    let post = ctl.find(id).await?;
    let options = ItemOptions {
        context: Context::Edit,
        password: None,
        fields: Fields(None),
        embed: None,
    };
    let mut items = prepare::items(ctl, site, std::slice::from_ref(&post), &options).await?;
    Ok(RestResponse {
        status,
        body: items.pop().unwrap_or(Value::Null),
        headers: Vec::new(),
    })
}

/// The request's `meta`, checked against the keys registered for the type: nothing for a type
/// without `custom-fields`, whose schema has no `meta`.
fn read_meta(ctl: &PostsController<'_>, params: &Params) -> Result<MetaUpdate, RestError> {
    if !ctl.post_type.supports.custom_fields {
        return Ok(MetaUpdate::default());
    }
    MetaUpdate::read(ctl.meta, MetaObject::Post, &ctl.post_type.name, params)
}

/// `create_item`.
pub(super) async fn create(
    ctl: &PostsController<'_>,
    params: &Params,
) -> Result<RestResponse, RestError> {
    let site = Site::load(ctl.db).await?;
    let changes = Changes::read(ctl.post_type, &site, params)?;
    let meta = read_meta(ctl, params)?;
    if params
        .get("id")
        .is_some_and(|id| as_integer(id).is_some_and(|id| id != 0))
    {
        return Err(RestError::new(
            "rest_post_exists",
            "Cannot create existing post.",
            400,
        ));
    }
    check_fields_permission(ctl, &changes, true).await?;
    let status = changes.status.unwrap_or(PostStatus::Draft);
    check_status(ctl, status)?;
    check_references(ctl, &changes, None).await?;
    let password = changes.password.clone().unwrap_or_default();
    check_sticky_password(changes.sticky == Some(true), &password)?;
    let fields = ctl
        .kses
        .filter(
            ctl.user,
            PostFields {
                title: changes.title.clone().unwrap_or_default(),
                content: changes.content.clone().unwrap_or_default(),
                excerpt: changes.excerpt.clone().unwrap_or_default(),
            },
        )
        .await?;
    let supports = ctl.post_type.supports;
    if supports.editor
        && supports.title
        && supports.excerpt
        && ctl.post_type.name != "page"
        && fields.title.is_empty()
        && fields.content.is_empty()
        && fields.excerpt.is_empty()
    {
        return Err(RestError::new(
            "empty_content",
            "Content, title, and excerpt are empty.",
            400,
        ));
    }
    let (status, date) = settle_status(status, changes.date.flatten());
    let author = match changes.author {
        Some(author) if author != 0 => author,
        _ => ctl.user.id(),
    };
    let new = NewPost {
        status,
        title: fields.title,
        slug: changes.slug.clone().unwrap_or_default(),
        content: fields.content,
        excerpt: fields.excerpt,
        parent_id: changes.parent.flatten(),
        menu_order: changes.menu_order.unwrap_or(0),
        published_at: date,
        password: (!password.is_empty()).then_some(password),
        ..NewPost::new(author, &ctl.post_type.name)
    };
    let post = Post::create(ctl.db, new)
        .await
        .map_err(|e| database_error(&e))?;
    write_extras(ctl, &site, &post, &changes, true).await?;
    // As `create_item`, the meta is checked on the new post, so a refused key leaves it saved.
    let can_edit = ctl.can_on("edit_post", &post, None, &site);
    meta.authorize(ctl.db, ctl.user, post.id, can_edit)
        .await?
        .apply(ctl.db)
        .await?;
    let response = respond(ctl, &site, post.id, 201).await?;
    let location = site.rest_url(&format!("wp/v2/{}/{}", ctl.post_type.rest_base, post.id));
    Ok(response.with_header("Location", location))
}

/// The time in an `If-Unmodified-Since` header, in UTC, when it is an HTTP-date (RFC 9110,
/// section 5.6.7): the IMF-fixdate `Sun, 06 Nov 1994 08:49:37 GMT`, or one of the obsolete forms
/// HTTP still accepts, RFC 850 (`Sunday, 06-Nov-94 08:49:37 GMT`) and asctime
/// (`Sun Nov  6 08:49:37 1994`). Anything else is `None`: another zone or offset (`-0500`),
/// another case or padding, or a day name that does not match the date. The text must read back
/// exactly as it was parsed, which is how each form's fixed layout is enforced.
fn parse_http_date(text: &str) -> Option<NaiveDateTime> {
    const FORMATS: [&str; 3] = [
        "%a, %d %b %Y %H:%M:%S GMT",
        "%A, %d-%b-%y %H:%M:%S GMT",
        "%a %b %e %H:%M:%S %Y",
    ];
    let text = text.trim_matches([' ', '\t']);
    FORMATS.iter().find_map(|format| {
        NaiveDateTime::parse_from_str(text, format)
            .ok()
            .filter(|date| date.format(format).to_string() == text)
    })
}

/// `update_item`.
pub(super) async fn update(
    ctl: &PostsController<'_>,
    id: i64,
    params: &Params,
    if_unmodified_since: Option<&str>,
) -> Result<RestResponse, RestError> {
    let mut stored = ctl.find(id).await?;
    let site = Site::load(ctl.db).await?;
    let changes = Changes::read(ctl.post_type, &site, params)?;
    let mut meta = None;
    let mut authorized = None;
    // RFC 9110 section 13.1.4: a recipient ignores an If-Unmodified-Since that is not a valid
    // HTTP date, so the update goes ahead without the check, and the last write wins.
    let unmodified_since = if_unmodified_since.and_then(parse_http_date);
    for attempt in 1..=MAX_ATTEMPTS {
        let trash = ctl.trash_status(&stored).await?;
        if !ctl.can_on("edit_post", &stored, trash, &site) {
            return Err(ctl.forbidden(
                "rest_cannot_edit",
                "Sorry, you are not allowed to edit this post.",
            ));
        }
        if attempt == 1 {
            check_fields_permission(ctl, &changes, false).await?;
            // As `update_item`, the meta's values are checked once the user may edit the post.
            meta = Some(read_meta(ctl, params)?);
        }
        if let Some(status) = changes.status {
            check_status(ctl, status)?;
        }
        let current_template =
            match crate::content::get_post_meta(ctl.db, id, "_wp_page_template").await {
                Ok(value) => value.and_then(|v| v.as_str().map(str::to_owned)),
                // A stored value that is not JSON is no template.
                Err(MetaError::InvalidJson(_)) => None,
                Err(MetaError::Database(e)) => return Err(database_error(&e)),
            };
        check_references(ctl, &changes, current_template.as_deref()).await?;
        // The meta's permissions are checked before the post is written, so a refused key
        // leaves the post as it was. The user may edit the post: checked above.
        if let Some(meta) = meta.take() {
            authorized = Some(meta.authorize(ctl.db, ctl.user, id, true).await?);
        }
        let mut post = stored.clone();
        if let Some(author) = changes.author
            && author != 0
        {
            post.author_id = author;
        }
        if let Some(slug) = &changes.slug {
            post.slug = slug.clone();
        }
        if let Some(password) = &changes.password {
            post.password = (!password.is_empty()).then(|| password.clone());
        }
        let sticky = changes
            .sticky
            .unwrap_or_else(|| site.sticky.contains(&post.id));
        check_sticky_password(sticky, post.password.as_deref().unwrap_or(""))?;
        if let Some(parent) = changes.parent {
            post.parent_id = loop_free_parent(ctl, id, parent).await?;
        }
        if let Some(order) = changes.menu_order {
            post.menu_order = order;
        }
        if let Some(date) = changes.date {
            post.published_at = date;
        }
        // kses runs on the whole post, as `wp_update_post` filters the merged fields.
        let fields = ctl
            .kses
            .filter(
                ctl.user,
                PostFields {
                    title: changes.title.clone().unwrap_or_else(|| post.title.clone()),
                    content: changes
                        .content
                        .clone()
                        .unwrap_or_else(|| post.content.clone()),
                    excerpt: changes
                        .excerpt
                        .clone()
                        .unwrap_or_else(|| post.excerpt.clone()),
                },
            )
            .await?;
        post.title = fields.title;
        post.content = fields.content;
        post.excerpt = fields.excerpt;
        let status = changes.status.unwrap_or(post.status);
        let (status, date) = if changes.status.is_some() || changes.date.is_some() {
            settle_status(status, post.published_at)
        } else {
            (status, post.published_at)
        };
        post.status = status;
        post.published_at = date;
        if let Some(since) = unmodified_since
            && stored.modified_at > since
        {
            return Err(conflict());
        }
        match save_revision(ctl.db, &post, stored.modified_at).await {
            Ok(_) => {
                let saved = ctl.find(id).await?;
                write_extras(ctl, &site, &saved, &changes, false).await?;
                if let Some(authorized) = authorized.take() {
                    authorized.apply(ctl.db).await?;
                }
                return respond(ctl, &site, id, 200).await;
            }
            Err(SaveError::Conflict { .. }) if unmodified_since.is_some() => return Err(conflict()),
            Err(SaveError::Conflict { .. }) => stored = ctl.find(id).await?,
            Err(SaveError::NotFound { .. } | SaveError::IsRevision { .. }) => {
                return Err(invalid_id());
            }
            Err(SaveError::Database(e)) => return Err(database_error(&e)),
        }
    }
    log::error!("Post {id} changed under {MAX_ATTEMPTS} updates in a row; the update gave up.");
    Err(RestError::internal())
}

/// `rest_post_conflict`, 412: the post changed after the client's `If-Unmodified-Since`.
fn conflict() -> RestError {
    RestError::new(
        "rest_post_conflict",
        "The post has been changed since the If-Unmodified-Since time. Read it again before \
         saving.",
        412,
    )
}

/// `delete_item`.
pub(super) async fn delete(
    ctl: &PostsController<'_>,
    id: i64,
    params: &Params,
) -> Result<RestResponse, RestError> {
    let mut r = Reader::new(params);
    let force = r.boolean("force").unwrap_or(false);
    r.invalid.finish()?;
    let mut post = ctl.find(id).await?;
    let site = Site::load(ctl.db).await?;
    let trash = ctl.trash_status(&post).await?;
    if !ctl.can_on("delete_post", &post, trash, &site) {
        return Err(ctl.forbidden(
            "rest_cannot_delete",
            "Sorry, you are not allowed to delete this post.",
        ));
    }
    if force {
        let previous = respond(ctl, &site, id, 200).await?.body;
        let deleted = post.delete(ctl.db).await.map_err(|e| database_error(&e))?;
        if !deleted {
            return Err(RestError::new(
                "rest_cannot_delete",
                "The post cannot be deleted.",
                500,
            ));
        }
        return Ok(RestResponse::ok(
            json!({"deleted": true, "previous": previous}),
        ));
    }
    if post.status == PostStatus::Trash {
        return Err(RestError::new(
            "rest_already_trashed",
            "The post has already been deleted.",
            410,
        ));
    }
    post.trash(ctl.db).await.map_err(|e| database_error(&e))?;
    scheduler::unschedule(ctl.db, PUBLISH_FUTURE_POST, &id)
        .await
        .map_err(|e| database_error(&e))?;
    respond(ctl, &site, id, 200).await
}

/// The arguments of a create or update, as WordPress's discovery lists the item schema's
/// writable fields.
pub(super) fn schema_args(post_type: &PostType) -> Map<String, Value> {
    let supports = post_type.supports;
    let mut args = Map::new();
    let mut put = |name: &str, value: Value| {
        args.insert(name.to_owned(), value);
    };
    put(
        "date",
        json!({"description": "The date the post was published, in the site's timezone.", "type": ["string", "null"], "format": "date-time", "required": false}),
    );
    put(
        "date_gmt",
        json!({"description": "The date the post was published, as GMT.", "type": ["string", "null"], "format": "date-time", "required": false}),
    );
    put(
        "slug",
        json!({"description": "An alphanumeric identifier for the post unique to its type.", "type": "string", "required": false}),
    );
    put(
        "status",
        json!({"description": "A named status for the post.", "type": "string", "enum": WRITABLE_STATUSES, "required": false}),
    );
    put(
        "password",
        json!({"description": "A password to protect access to the content and excerpt.", "type": "string", "required": false}),
    );
    if post_type.has_parent() {
        put(
            "parent",
            json!({"description": "The ID for the parent of the post.", "type": "integer", "required": false}),
        );
    }
    let raw_object = |description: &str| json!({"description": description, "type": "object", "properties": {"raw": {"type": "string"}}, "required": false});
    if supports.title {
        put("title", raw_object("The title for the post."));
    }
    if supports.editor {
        put("content", raw_object("The content for the post."));
    }
    if supports.author {
        put(
            "author",
            json!({"description": "The ID for the author of the post.", "type": "integer", "required": false}),
        );
    }
    if supports.excerpt {
        put("excerpt", raw_object("The excerpt for the post."));
    }
    if supports.thumbnail {
        put(
            "featured_media",
            json!({"description": "The ID of the featured media for the post.", "type": "integer", "required": false}),
        );
    }
    if supports.comments {
        put(
            "comment_status",
            json!({"description": "Whether or not comments are open on the post.", "type": "string", "enum": ["open", "closed"], "required": false}),
        );
        put(
            "ping_status",
            json!({"description": "Whether or not the post can be pinged.", "type": "string", "enum": ["open", "closed"], "required": false}),
        );
    }
    if supports.page_attributes {
        put(
            "menu_order",
            json!({"description": "The order of the post in relation to other posts.", "type": "integer", "required": false}),
        );
    }
    if supports.post_formats {
        put(
            "format",
            json!({"description": "The format for the post.", "type": "string", "enum": FORMATS, "required": false}),
        );
    }
    if supports.custom_fields {
        put(
            "meta",
            json!({"description": "Meta fields.", "type": "object", "properties": [], "required": false}),
        );
    }
    if post_type.has_sticky() {
        put(
            "sticky",
            json!({"description": "Whether or not the post should be treated as sticky.", "type": "boolean", "required": false}),
        );
    }
    put(
        "template",
        json!({"description": "The theme file to use to display the post.", "type": "string", "required": false}),
    );
    for taxonomy in &post_type.taxonomies {
        put(
            &taxonomy.rest_base,
            json!({"description": format!("The terms assigned to the post in the {} taxonomy.", taxonomy.taxonomy.name), "type": "array", "items": {"type": "integer"}, "required": false}),
        );
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_read_as_rest_parse_date() {
        let plus_two = FixedOffset::east_opt(7200);
        let utc = |s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(
            parse_date("2026-03-01T12:00:00", plus_two),
            Some(utc("2026-03-01 10:00:00"))
        );
        assert_eq!(
            parse_date("2026-03-01T12:00:00", None),
            Some(utc("2026-03-01 12:00:00"))
        );
        assert_eq!(
            parse_date("2026-03-01T12:00:00Z", plus_two),
            Some(utc("2026-03-01 12:00:00"))
        );
        assert_eq!(
            parse_date("2026-03-01T12:00:00+01:00", None),
            Some(utc("2026-03-01 11:00:00"))
        );
        assert_eq!(
            parse_date("2026-03-01 12:00:00.5", None),
            Some(utc("2026-03-01 12:00:00"))
        );
        assert_eq!(parse_date("yesterday", None), None);
    }

    #[test]
    fn http_dates_parse() {
        let expected =
            NaiveDateTime::parse_from_str("1994-11-06 08:49:37", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(expected)
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(expected)
        );
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(expected));
        assert_eq!(parse_http_date("not a date"), None);
    }

    #[test]
    fn statuses_settle_as_wp_insert_post() {
        let soon = now() + Duration::seconds(30);
        let later = now() + Duration::hours(2);
        assert_eq!(
            settle_status(PostStatus::Publish, Some(later)).0,
            PostStatus::Future
        );
        assert_eq!(
            settle_status(PostStatus::Publish, Some(soon)).0,
            PostStatus::Publish
        );
        assert_eq!(
            settle_status(PostStatus::Future, Some(soon)).0,
            PostStatus::Publish
        );
        assert_eq!(
            settle_status(PostStatus::Future, None).0,
            PostStatus::Publish
        );
        assert!(settle_status(PostStatus::Publish, None).1.is_some());
        assert_eq!(
            settle_status(PostStatus::Draft, None),
            (PostStatus::Draft, None)
        );
    }

    #[test]
    fn http_dates_are_the_three_forms_of_rfc_9110() {
        let expected =
            NaiveDateTime::parse_from_str("1994-11-06 08:49:37", "%Y-%m-%d %H:%M:%S").unwrap();
        for valid in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            " Sun, 06 Nov 1994 08:49:37 GMT ",
        ] {
            assert_eq!(parse_http_date(valid), Some(expected), "{valid:?}");
        }
        for invalid in [
            "Sun, 06 Nov 1994 08:49:37 -0500",
            "Sun, 06 Nov 1994 03:49:37 -0500",
            "Sun, 06 Nov 1994 08:49:37 +0000",
            "Sun, 06 Nov 1994 08:49:37 UT",
            "Sun, 06 Nov 1994 08:49:37 EST",
            "06 Nov 1994 08:49:37 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "sun, 06 nov 1994 08:49:37 GMT",
            "Mon, 06 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 gmt",
            "Sun, 99 Nov 1994 08:49:37 GMT",
            "Sun Nov 6 08:49:37 1994",
            "1994-11-06T08:49:37Z",
            "not a date",
            "",
        ] {
            assert_eq!(parse_http_date(invalid), None, "{invalid:?}");
        }
    }
}
