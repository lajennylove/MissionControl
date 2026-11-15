//! Classic widgets, stored as WordPress stores them (`WP_Widget`, `wp_get_sidebars_widgets`) and
//! rendered in Rust, as WordPress's core widgets render in PHP.
//!
//! # Storage
//!
//! - A theme declares its widget areas (WordPress's sidebars) in `theme.toml`'s `[sidebars]`.
//! - Each widget type keeps its instances in the option `widget_{id_base}`: an object keyed by
//!   the instance's number (`"2"`, `"3"`, ...; WordPress numbers from 2), with
//!   `"_multiwidget": 1`. A widget's id is `{id_base}-{number}`, such as `text-2`.
//! - The option `sidebars_widgets` maps each area's id to its widget ids, in order, with
//!   `wp_inactive_widgets` for the unused ones and `"array_version": 3`.
//!
//! ```ignore
//! let text = save_widget(&db, "text", None, &json!({"title": "About", "text": "Hello"})).await?;
//! let recent = save_widget(&db, "recent-posts", None, &json!({"number": 5})).await?;
//! set_sidebar_widgets(&db, "footer", &[&text, &recent]).await?;
//! ```
//!
//! # Rendering
//!
//! [`theme_sidebars`] gives each of the theme's areas its widgets, in order: the template
//! context's `sidebars.{id}`. Each widget has a `title` (text: templates escape it with `{{ }}`)
//! and its `content`, HTML built here with every value escaped (output raw with `{!! !!}`).
//! The core widgets Mission Control has ([`WIDGET_TYPES`]):
//!
//! | `id_base` | Instance keys | Renders |
//! | --- | --- | --- |
//! | `text` | `title`, `text`, `filter`/`visual` (paragraphs) | the text, escaped: there is no kses yet |
//! | `recent-posts` | `title` (`Recent Posts`), `number` (5, at most [`MAX_RECENT_POSTS`]), `show_date` | the latest published posts |
//! | `categories` | `title` (`Categories`), `count`, `hierarchical` | the categories with posts, as a list (`dropdown` is not supported) |
//! | `search` | `title` | the search form |
//!
//! A widget of another type, an id that is not a widget id, or an instance that is gone is
//! skipped, as WordPress skips a widget whose callback is not registered.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use chrono::{FixedOffset, NaiveDateTime};
use orbit_orm::{Database, Error, Model, raw};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::manifest::is_area_id;
use super::{PageRequest, Theme};
use crate::content::{MetaError, Post, PostStatus, Taxonomy, Term, get_option, set_option};
use crate::front::{Archive, Resolved, post_links, term_links};
use crate::text::escape_html;

/// The option that maps widget areas to their widgets.
pub const SIDEBARS_WIDGETS: &str = "sidebars_widgets";

/// The widget types Mission Control renders, by `id_base`.
pub const WIDGET_TYPES: [&str; 4] = ["text", "recent-posts", "categories", "search"];

/// The most posts a Recent Posts widget lists.
pub const MAX_RECENT_POSTS: i64 = 100;

/// The longest `id_base`.
const MAX_ID_BASE: usize = 64;

/// Why a widget or an area was not saved.
#[derive(Debug)]
pub enum WidgetError {
    /// The `id_base` is not lowercase ASCII letters, digits, `_` and `-`.
    InvalidIdBase,
    /// The instance is not a JSON object.
    InvalidInstance,
    /// The instance number is 0.
    InvalidNumber,
    /// The area id is not a theme's area id (lowercase letters, digits and `_`).
    InvalidSidebarId,
    /// A widget id is not `{id_base}-{number}`.
    InvalidWidgetId,
    /// An option could not be written.
    Meta(MetaError),
    /// The database refused a query.
    Database(Error),
}

impl fmt::Display for WidgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdBase => {
                f.write_str("a widget type must be lowercase ASCII letters, digits, `_` and `-`")
            }
            Self::InvalidInstance => f.write_str("a widget instance must be a JSON object"),
            Self::InvalidNumber => f.write_str("a widget number must be 1 or more"),
            Self::InvalidSidebarId => f.write_str(
                "a widget area id must be lowercase letters, digits and `_`, starting with a \
                 letter or `_`",
            ),
            Self::InvalidWidgetId => f.write_str("a widget id must be `{id_base}-{number}`"),
            Self::Meta(e) => e.fmt(f),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for WidgetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Meta(e) => Some(e),
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Error> for WidgetError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

impl From<MetaError> for WidgetError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Database(e) => Self::Database(e),
            e @ MetaError::InvalidJson(_) => Self::Meta(e),
        }
    }
}

/// Saves a widget instance of type `id_base` in the option `widget_{id_base}`, as
/// `WP_Widget::update_callback` does, and returns the widget's id (`text-2`). `number` replaces
/// that instance; `None` adds a new one, numbered after the highest (from 2). The type's other
/// instances are kept.
///
/// # Errors
///
/// Fails with [`WidgetError::InvalidIdBase`], [`WidgetError::InvalidNumber`] or
/// [`WidgetError::InvalidInstance`], or with the database's error.
pub async fn save_widget(
    db: &Database,
    id_base: &str,
    number: Option<u64>,
    instance: &Value,
) -> Result<String, WidgetError> {
    if !is_id_base(id_base) {
        return Err(WidgetError::InvalidIdBase);
    }
    if number == Some(0) {
        return Err(WidgetError::InvalidNumber);
    }
    if !instance.is_object() {
        return Err(WidgetError::InvalidInstance);
    }
    let name = format!("widget_{id_base}");
    let mut instances = match readable_option(db, &name).await? {
        Some(Value::Object(instances)) => instances,
        _ => Map::new(),
    };
    let number = number.unwrap_or_else(|| {
        instances
            .keys()
            .filter_map(|key| key.parse::<u64>().ok())
            .max()
            .map_or(2, |highest| (highest + 1).max(2))
    });
    instances.insert(number.to_string(), instance.clone());
    instances.insert("_multiwidget".to_owned(), json!(1));
    set_option(db, &name, &Value::Object(instances)).await?;
    Ok(format!("{id_base}-{number}"))
}

/// Puts `widget_ids` in the area `sidebar_id`, in this order, in the option `sidebars_widgets`,
/// as `wp_set_sidebars_widgets` does. The other areas are kept.
///
/// # Errors
///
/// Fails with [`WidgetError::InvalidSidebarId`] or [`WidgetError::InvalidWidgetId`], or with the
/// database's error.
pub async fn set_sidebar_widgets(
    db: &Database,
    sidebar_id: &str,
    widget_ids: &[&str],
) -> Result<(), WidgetError> {
    if !is_area_id(sidebar_id) {
        return Err(WidgetError::InvalidSidebarId);
    }
    if widget_ids.iter().any(|id| parse_widget_id(id).is_none()) {
        return Err(WidgetError::InvalidWidgetId);
    }
    let mut areas = match readable_option(db, SIDEBARS_WIDGETS).await? {
        Some(Value::Object(areas)) => areas,
        _ => Map::new(),
    };
    areas.insert(sidebar_id.to_owned(), json!(widget_ids));
    areas.entry("array_version").or_insert_with(|| json!(3));
    set_option(db, SIDEBARS_WIDGETS, &Value::Object(areas)).await?;
    Ok(())
}

/// Each area's widget ids from the option `sidebars_widgets`, as `wp_get_sidebars_widgets`:
/// `array_version` and any value that is not a list are left out, and so is an id that is not
/// text.
///
/// # Errors
///
/// Fails with the database's error. A stored option that is not valid JSON counts as unset.
pub async fn sidebars_widgets(db: &Database) -> Result<BTreeMap<String, Vec<String>>, Error> {
    let Some(Value::Object(areas)) = readable_option(db, SIDEBARS_WIDGETS).await? else {
        return Ok(BTreeMap::new());
    };
    Ok(areas
        .into_iter()
        .filter_map(|(area, ids)| match ids {
            Value::Array(ids) => Some((
                area,
                ids.into_iter()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect(),
            )),
            _ => None,
        })
        .collect())
}

/// A widget area as templates see it: `sidebars.{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SidebarView {
    /// The area's id.
    pub id: String,
    /// The area's name, from the manifest.
    pub name: String,
    /// Whether the area shows any widget (`is_active_sidebar`).
    pub active: bool,
    /// The area's widgets, in order.
    pub widgets: Vec<WidgetView>,
}

/// A rendered widget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WidgetView {
    /// The widget's id, such as `text-2`, for the wrapper's `id`.
    pub id: String,
    /// The widget's type, such as `text`.
    pub id_base: String,
    /// The type's class for the wrapper, such as `widget_text`.
    pub classes: String,
    /// The title, as text; empty for none.
    pub title: String,
    /// The widget's HTML, every value in it escaped; output raw.
    pub content: String,
}

/// Each widget area of `theme` (and its parent), in the manifest's order, with its widgets
/// rendered for `request`: the template context's `sidebars`.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn theme_sidebars(
    db: &Database,
    theme: &Theme,
    request: &PageRequest<'_>,
) -> Result<Vec<SidebarView>, Error> {
    let assigned = sidebars_widgets(db).await?;
    let mut instances: HashMap<String, Option<Map<String, Value>>> = HashMap::new();
    let mut areas = Vec::new();
    for sidebar in theme.sidebars() {
        let mut widgets = Vec::new();
        for widget_id in assigned.get(&sidebar.id).map(Vec::as_slice).unwrap_or(&[]) {
            let Some((id_base, number)) = parse_widget_id(widget_id) else {
                continue;
            };
            if !WIDGET_TYPES.contains(&id_base) {
                continue;
            }
            if !instances.contains_key(id_base) {
                let stored = match readable_option(db, &format!("widget_{id_base}")).await? {
                    Some(Value::Object(stored)) => Some(stored),
                    _ => None,
                };
                instances.insert(id_base.to_owned(), stored);
            }
            let Some(Value::Object(instance)) = instances[id_base]
                .as_ref()
                .and_then(|stored| stored.get(&number.to_string()))
            else {
                continue;
            };
            if let Some(view) = render_widget(db, request, widget_id, id_base, instance).await? {
                widgets.push(view);
            }
        }
        areas.push(SidebarView {
            id: sidebar.id,
            name: sidebar.name,
            active: !widgets.is_empty(),
            widgets,
        });
    }
    Ok(areas)
}

/// Renders one widget of a known type; `None` when it shows nothing (no posts).
async fn render_widget(
    db: &Database,
    request: &PageRequest<'_>,
    widget_id: &str,
    id_base: &str,
    instance: &Map<String, Value>,
) -> Result<Option<WidgetView>, Error> {
    let title = instance.get("title").map(text_of).unwrap_or_default();
    let (class, title, content) = match id_base {
        "text" => ("widget_text", title, text_widget(instance)),
        "recent-posts" => {
            let Some(content) = recent_posts_widget(db, request, instance).await? else {
                return Ok(None);
            };
            ("widget_recent_entries", or(title, "Recent Posts"), content)
        }
        "categories" => (
            "widget_categories",
            or(title, "Categories"),
            categories_widget(db, request, instance).await?,
        ),
        "search" => ("widget_search", title, search_form(request)),
        _ => return Ok(None),
    };
    Ok(Some(WidgetView {
        id: widget_id.to_owned(),
        id_base: id_base.to_owned(),
        classes: class.to_owned(),
        title,
        content,
    }))
}

/// The Text widget: the text escaped (Mission Control has no kses yet, so HTML is not kept),
/// in paragraphs when `filter` or `visual` is set, as WordPress runs `wpautop` then.
fn text_widget(instance: &Map<String, Value>) -> String {
    let text = escape_html(&instance.get("text").map(text_of).unwrap_or_default());
    let paragraphs = [instance.get("filter"), instance.get("visual")]
        .into_iter()
        .flatten()
        .any(truthy);
    let body = if paragraphs { autop(&text) } else { text };
    format!("<div class=\"textwidget\">{body}</div>")
}

/// The Recent Posts widget: the latest published posts, newest first, each linked, with its
/// date when `show_date` is set; `None` when there are none, as WordPress then shows nothing.
async fn recent_posts_widget(
    db: &Database,
    request: &PageRequest<'_>,
    instance: &Map<String, Value>,
) -> Result<Option<String>, Error> {
    let number = instance
        .get("number")
        .and_then(number_of)
        .filter(|n| *n > 0)
        .unwrap_or(5)
        .min(MAX_RECENT_POSTS);
    let show_date = instance.get("show_date").is_some_and(truthy);
    let ids: Vec<(i64,)> = raw::fetch_all(
        db,
        "SELECT id FROM posts WHERE status = ? AND post_type = ? \
         ORDER BY published_at DESC, id DESC LIMIT ?",
        &[PostStatus::Publish.into(), "post".into(), number.into()],
    )
    .await?;
    if ids.is_empty() {
        return Ok(None);
    }
    let ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
    let mut posts = Post::query()
        .where_in(Post::PRIMARY_KEY, ids.clone())
        .get(db)
        .await?;
    posts.sort_by_key(|post| ids.iter().position(|id| *id == post.id));
    let links = post_links(db, request.structure, &posts).await?;
    let current = match request.resolved {
        Resolved::Single(post) => Some(post.id),
        _ => None,
    };
    let mut html = String::from("<ul>\n");
    for (post, link) in posts.iter().zip(links) {
        let mut title = if post.title.is_empty() {
            "(no title)".to_owned()
        } else {
            post.title.clone()
        };
        if post.password.as_deref().is_some_and(|p| !p.is_empty()) {
            title = format!("Protected: {title}");
        }
        let aria = if current == Some(post.id) {
            " aria-current=\"page\""
        } else {
            ""
        };
        html.push_str(&format!(
            "<li><a href=\"{}\"{aria}>{}</a>",
            escape_html(&format!("{}{link}", request.home)),
            escape_html(&title)
        ));
        if show_date {
            let date = local(
                post.published_at.unwrap_or(post.modified_at),
                request.utc_offset,
            );
            html.push_str(&format!(
                " <span class=\"post-date\">{}</span>",
                escape_html(&date)
            ));
        }
        html.push_str("</li>\n");
    }
    html.push_str("</ul>");
    Ok(Some(html))
}

/// The Categories widget: the categories that have posts, by name, each linked, with its count
/// when `count` is set and nested when `hierarchical` is (a parent without posts of its own is
/// shown when a child has some), as `wp_list_categories` lists them.
async fn categories_widget(
    db: &Database,
    request: &PageRequest<'_>,
    instance: &Map<String, Value>,
) -> Result<String, Error> {
    let show_count = instance.get("count").is_some_and(truthy);
    let hierarchical = instance.get("hierarchical").is_some_and(truthy);
    let terms = Term::all(db, &Taxonomy::CATEGORY).await?;
    let has_posts = |term: &Term| -> bool {
        // A term or, when nested, one of its descendants has posts. Ids are checked against a
        // seen list, so a parent cycle cannot loop.
        let mut stack = vec![term.term_id];
        let mut seen = Vec::new();
        while let Some(id) = stack.pop() {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let Some(t) = terms.iter().find(|t| t.term_id == id) else {
                continue;
            };
            if t.count > 0 {
                return true;
            }
            if hierarchical {
                stack.extend(
                    terms
                        .iter()
                        .filter(|c| c.parent_id == Some(id))
                        .map(|c| c.term_id),
                );
            }
        }
        false
    };
    let shown: Vec<&Term> = terms.iter().filter(|t| has_posts(t)).collect();
    if shown.is_empty() {
        return Ok("<ul>\n<li class=\"cat-item-none\">No categories</li>\n</ul>".to_owned());
    }
    let owned: Vec<Term> = shown.iter().map(|t| (*t).clone()).collect();
    let links = term_links(db, request.structure, &owned).await?;
    let link_of: HashMap<i64, String> = owned
        .iter()
        .zip(links)
        .filter_map(|(t, link)| Some((t.term_id, format!("{}{}", request.home, link?))))
        .collect();
    let current = match request.resolved {
        Resolved::Archive {
            archive: Archive::Category(term),
            ..
        } => Some(term.term_id),
        _ => None,
    };
    let item = |term: &Term| -> String {
        let is_current = current == Some(term.term_id);
        let mut li = format!(
            "<li class=\"cat-item cat-item-{}{}\"><a href=\"{}\"{}>{}</a>",
            term.term_id,
            if is_current { " current-cat" } else { "" },
            escape_html(link_of.get(&term.term_id).map_or("", String::as_str)),
            if is_current {
                " aria-current=\"page\""
            } else {
                ""
            },
            escape_html(&term.name)
        );
        if show_count {
            li.push_str(&format!(" ({})", term.count));
        }
        li
    };
    let mut html = String::from("<ul>\n");
    if hierarchical {
        let is_shown = |id: i64| shown.iter().any(|t| t.term_id == id);
        // Top level: no parent, or a parent that is not shown.
        let roots: Vec<&Term> = shown
            .iter()
            .copied()
            .filter(|t| t.parent_id.is_none_or(|p| !is_shown(p)))
            .collect();
        let mut seen = Vec::new();
        for root in roots {
            nested(root, &shown, &item, &mut seen, &mut html);
        }
    } else {
        for term in &shown {
            html.push_str(&item(term));
            html.push_str("</li>\n");
        }
    }
    html.push_str("</ul>");
    Ok(html)
}

/// One category and its shown children, nested in `<ul class="children">`.
fn nested(
    term: &Term,
    shown: &[&Term],
    item: &dyn Fn(&Term) -> String,
    seen: &mut Vec<i64>,
    html: &mut String,
) {
    if seen.contains(&term.term_id) {
        return;
    }
    seen.push(term.term_id);
    html.push_str(&item(term));
    let children: Vec<&Term> = shown
        .iter()
        .copied()
        .filter(|t| t.parent_id == Some(term.term_id))
        .collect();
    if !children.is_empty() {
        html.push_str("\n<ul class=\"children\">\n");
        for child in children {
            nested(child, shown, item, seen, html);
        }
        html.push_str("</ul>\n");
    }
    html.push_str("</li>\n");
}

/// The search form, as `get_search_form` gives it for an HTML5 theme.
fn search_form(request: &PageRequest<'_>) -> String {
    format!(
        "<form role=\"search\" method=\"get\" class=\"search-form\" action=\"{}\">\
         <label><span class=\"screen-reader-text\">Search for:</span>\
         <input type=\"search\" class=\"search-field\" placeholder=\"Search &hellip;\" \
         value=\"{}\" name=\"s\"></label>\
         <button type=\"submit\" class=\"search-submit\">Search</button></form>",
        escape_html(&request.site_url()),
        escape_html(request.search_query)
    )
}

/// `text`, already escaped, in paragraphs as `wpautop` makes them: blank lines separate
/// paragraphs and a single line break becomes `<br />`.
fn autop(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut paragraphs = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join("<br />\n"));
                current.clear();
            }
        } else {
            current.push(line.trim());
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join("<br />\n"));
    }
    paragraphs
        .iter()
        .map(|p| format!("<p>{p}</p>"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `{id_base}-{number}` read back, the number 1 or more.
fn parse_widget_id(id: &str) -> Option<(&str, u64)> {
    let (id_base, number) = id.rsplit_once('-')?;
    let number: u64 = number
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| number.parse().ok())??;
    (is_id_base(id_base) && number > 0).then_some((id_base, number))
}

/// Whether `id_base` names a widget type: lowercase ASCII letters, digits, `_` and `-`, at most
/// 64 characters.
fn is_id_base(id_base: &str) -> bool {
    !id_base.is_empty()
        && id_base.len() <= MAX_ID_BASE
        && id_base
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// `text`, or `default` when it is empty.
fn or(text: String, default: &str) -> String {
    if text.trim().is_empty() {
        default.to_owned()
    } else {
        text
    }
}

/// An instance value as text: a string, or a number written out.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

/// An instance value as a whole number: a number, or a string of one.
fn number_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Whether an instance flag is set, as PHP reads `! empty( $instance['flag'] )`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty() && text != "0",
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// A UTC date in the site's time zone, as `get_the_date` writes it by default.
fn local(utc: NaiveDateTime, offset: FixedOffset) -> String {
    utc.and_utc()
        .with_timezone(&offset)
        .format("%B %-d, %Y")
        .to_string()
}

/// Option `name`, with a value that is not valid JSON counted as unset.
async fn readable_option(db: &Database, name: &str) -> Result<Option<Value>, Error> {
    match get_option(db, name).await {
        Ok(value) => Ok(value),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_ids_are_read_back() {
        assert_eq!(parse_widget_id("text-2"), Some(("text", 2)));
        assert_eq!(
            parse_widget_id("recent-posts-12"),
            Some(("recent-posts", 12))
        );
        for id in [
            "text", "text-", "-2", "text-0", "Text-2", "text-+2", "text-2x",
        ] {
            assert_eq!(parse_widget_id(id), None, "{id}");
        }
    }

    #[test]
    fn paragraphs_follow_wpautop() {
        assert_eq!(autop("a\nb\n\n\nc"), "<p>a<br />\nb</p>\n<p>c</p>");
        assert_eq!(autop("\r\n one \r\n"), "<p>one</p>");
        assert_eq!(autop(""), "");
    }

    #[test]
    fn flags_read_as_php_empty() {
        for value in [json!(true), json!(1), json!("1"), json!("content")] {
            assert!(truthy(&value), "{value}");
        }
        for value in [json!(false), json!(0), json!("0"), json!(""), json!(null)] {
            assert!(!truthy(&value), "{value}");
        }
    }
}
