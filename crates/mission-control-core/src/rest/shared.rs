//! What the REST controllers share: the site options a request reads ([`Site`]), the request's
//! `context`, `_fields` and `_embed`, the pagination `Link` URLs, and the [`KsesLimiter`] that
//! runs kses off the async threads.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::FixedOffset;
use orbit_orm::Database;
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;

use super::RestError;
use super::params::{Params, Reader, as_list};
use crate::content::{
    FieldTooLarge, MetaError, PermalinkStructure, PostFields, get_option, kses_post_fields,
};
use crate::front::{decode, site_address};
use crate::users::{SitePages, UserCaps};

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
        let user = user.clone();
        self.run(move || kses_post_fields(&user, fields))
            .await?
            .map_err(|e| too_large(&e))
    }

    /// Runs `filter` (a kses filter, or other work as heavy) on a blocking thread once a permit
    /// is free, and returns what it returns.
    ///
    /// # Errors
    ///
    /// A 500 when the limiter is closed or the filter's thread fails.
    pub async fn run<T, F>(&self, filter: F) -> Result<T, RestError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|e| {
                log::error!("The kses limiter is closed: {e}");
                RestError::internal()
            })?;
        tokio::task::spawn_blocking(move || {
            let result = filter();
            drop(permit);
            result
        })
        .await
        .map_err(|e| {
            log::error!("The kses filter failed: {e}");
            RestError::internal()
        })
    }
}

/// `rest_content_too_large`, 413, for a field kses will not filter.
pub(crate) fn too_large(error: &FieldTooLarge) -> RestError {
    RestError::new(
        "rest_content_too_large",
        format!("The {} is too large to save: {error}.", error.field),
        413,
    )
    .with_data("field", json!(error.field))
}

/// The most bytes a `TEXT` column holds on MySQL and MariaDB, where a post's title, content and
/// excerpt and a term's description are `TEXT` (WordPress uses `LONGTEXT`; Orbit's schema
/// builder has no such type).
pub(crate) const MYSQL_TEXT_BYTES: usize = 65_535;

/// Ruling R52: on MySQL and MariaDB, a `field` longer than its `TEXT` column holds is refused
/// with `rest_content_too_large` (413) before the database is touched, rather than fail with a
/// 500 or be cut short by a non-strict server. Other databases have no such limit.
pub(crate) fn check_text_capacity(
    db: &Database,
    field: &str,
    value: &str,
) -> Result<(), RestError> {
    if db.driver() != orbit_orm::Driver::MySql || value.len() <= MYSQL_TEXT_BYTES {
        return Ok(());
    }
    Err(RestError::new(
        "rest_content_too_large",
        format!(
            "The {field} is too large to save: it is {} bytes, and this database holds at most \
             {MYSQL_TEXT_BYTES} bytes.",
            value.len()
        ),
        413,
    )
    .with_data("field", json!(field)))
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
pub(crate) async fn option(db: &Database, name: &str) -> Result<Option<Value>, RestError> {
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
pub(crate) fn id_of(value: &Value) -> Option<i64> {
    let n = match value {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (n > 0).then_some(n)
}

/// A database error, logged, as WordPress's 500.
pub(crate) fn database_error(error: &dyn std::fmt::Display) -> RestError {
    log::error!("A REST request failed: {error}");
    RestError::internal()
}

/// WordPress's `rest_authorization_required_code`: 401 for a visitor, 403 for a user.
pub(crate) fn authorization_status(user: &UserCaps) -> u16 {
    if user.id() == 0 { 401 } else { 403 }
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

/// Reads `context`, defaulting to `view`.
pub(crate) fn read_context(reader: &mut Reader<'_>) -> Context {
    match reader
        .one_of("context", &["view", "embed", "edit"])
        .as_deref()
    {
        Some("edit") => Context::Edit,
        Some("embed") => Context::Embed,
        _ => Context::View,
    }
}

/// Reads `_fields` and `_embed`, which WordPress reads before any route.
pub(crate) fn read_envelope(params: &Params) -> (Fields, Option<Vec<String>>) {
    let fields = params.get("_fields").map(|value| {
        as_list(value)
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
            .filter(|s| !s.is_empty())
            .collect()
    });
    // `_embed`, `_embed=1` and `_embed=true` embed everything; a list embeds those relations.
    let embed = params.get("_embed").map(|value| match value {
        Value::String(s) if s.is_empty() || s == "1" || s == "true" => Vec::new(),
        other => as_list(other)
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
    });
    (Fields(fields), embed)
}

/// The `_fields` of a request: `None` for every field.
#[derive(Debug, Clone, Default)]
pub(crate) struct Fields(pub(crate) Option<Vec<String>>);

impl Fields {
    /// Whether `field` (a dotted path) is in the response, as `rest_is_field_included`: it is
    /// named, a parent of it is named, or a field under it is named.
    pub(crate) fn includes(&self, field: &str) -> bool {
        let Some(fields) = &self.0 else {
            return true;
        };
        fields.iter().any(|f| {
            f == field
                || field
                    .strip_prefix(f.as_str())
                    .is_some_and(|rest| rest.starts_with('.'))
                || f.strip_prefix(field)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    /// `item` with only the named fields, as `rest_filter_response_fields`: `title` keeps the
    /// whole title, `title.rendered` only its rendered text.
    pub(crate) fn filter(&self, item: Map<String, Value>) -> Map<String, Value> {
        let Some(fields) = &self.0 else {
            return item;
        };
        let mut tree = FieldTree::default();
        for field in fields {
            tree.add(field.split('.'));
        }
        tree.keep(item)
    }
}

/// The named fields as a tree: a leaf keeps a whole value.
#[derive(Debug, Default)]
struct FieldTree {
    leaf: bool,
    children: BTreeMap<String, FieldTree>,
}

impl FieldTree {
    fn add<'a>(&mut self, mut path: impl Iterator<Item = &'a str>) {
        match path.next() {
            None => self.leaf = true,
            Some(name) => self.children.entry(name.to_owned()).or_default().add(path),
        }
    }

    fn keep(&self, mut item: Map<String, Value>) -> Map<String, Value> {
        let mut kept = Map::new();
        for (name, child) in &self.children {
            let Some(value) = item.remove(name) else {
                continue;
            };
            if child.leaf {
                kept.insert(name.clone(), value);
            } else if let Value::Object(object) = value {
                kept.insert(name.clone(), Value::Object(child.keep(object)));
            }
        }
        kept
    }
}

/// `base` with the request's query and `page` set to `page`, as `add_query_arg` replaces it in
/// place or adds it at the end.
pub(crate) fn with_page(base: &str, query: &str, page: i64) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key, true), decode(value, true))
        })
        .collect();
    let mut replaced = false;
    for (key, value) in &mut pairs {
        if key == "page" {
            *value = page.to_string();
            replaced = true;
        }
    }
    if !replaced {
        pairs.push(("page".to_owned(), page.to_string()));
    }
    let query: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect();
    format!("{base}?{}", query.join("&"))
}

/// The `Link` headers of page `page` of `max_pages` of a collection at `base`, as `get_items`
/// adds them: `prev` from the second page on (at most the last page), `next` before the last.
pub(crate) fn page_links(base: &str, query: &str, page: i64, max_pages: i64) -> Vec<String> {
    let mut links = Vec::new();
    if page > 1 {
        let prev = (page - 1).min(max_pages.max(1));
        links.push(format!("<{}>; rel=\"prev\"", with_page(base, query, prev)));
    }
    if max_pages > page {
        links.push(format!(
            "<{}>; rel=\"next\"",
            with_page(base, query, page + 1)
        ));
    }
    links
}

/// PHP's `urlencode`: letters, digits and `-_.` kept, a space as `+`, the rest `%XX`.
fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(byte as char),
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `CASE column WHEN ? THEN 0 WHEN ? THEN 1 ... ELSE n END`: the position in a list, as MySQL's
/// `FIELD` orders, on every database.
pub(crate) fn position_case(column: &str, n: usize) -> String {
    let mut case = format!("CASE {column}");
    for i in 0..n {
        case.push_str(&format!(" WHEN ? THEN {i}"));
    }
    case.push_str(&format!(" ELSE {n} END"));
    case
}

/// The `curies` link every response with `wp:` relations carries.
pub(crate) fn curies() -> Value {
    json!([{"name": "wp", "href": "https://api.w.org/{rel}", "templated": true}])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(list: &[&str]) -> Fields {
        Fields(Some(list.iter().map(|s| (*s).to_owned()).collect()))
    }

    #[test]
    fn fields_are_included_by_name_parent_or_child() {
        let f = fields(&["id", "title.rendered", "content"]);
        assert!(f.includes("id"));
        assert!(f.includes("title"));
        assert!(f.includes("title.rendered"));
        assert!(!f.includes("title.raw"));
        assert!(f.includes("content.raw"));
        assert!(!f.includes("_links"));
        assert!(Fields(None).includes("_links"));
    }

    #[test]
    fn fields_filter_nested_values() {
        let item = json!({"id": 1, "title": {"raw": "a", "rendered": "b"}, "slug": "s"});
        let Value::Object(item) = item else {
            unreachable!()
        };
        let kept = fields(&["title.rendered", "id"]).filter(item);
        assert_eq!(
            Value::Object(kept),
            json!({"id": 1, "title": {"rendered": "b"}})
        );
    }

    #[test]
    fn pagination_links_replace_or_add_the_page() {
        assert_eq!(
            with_page("http://x/wp-json/wp/v2/posts", "per_page=2&page=3", 2),
            "http://x/wp-json/wp/v2/posts?per_page=2&page=2"
        );
        assert_eq!(
            with_page("http://x/wp-json/wp/v2/posts", "search=a+b", 2),
            "http://x/wp-json/wp/v2/posts?search=a+b&page=2"
        );
        assert_eq!(
            page_links("http://x/c", "page=2", 2, 3),
            [
                "<http://x/c?page=1>; rel=\"prev\"",
                "<http://x/c?page=3>; rel=\"next\""
            ]
        );
        assert!(page_links("http://x/c", "", 1, 1).is_empty());
    }

    #[test]
    fn include_order_is_a_case() {
        assert_eq!(
            position_case("p.id", 2),
            "CASE p.id WHEN ? THEN 0 WHEN ? THEN 1 ELSE 2 END"
        );
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
