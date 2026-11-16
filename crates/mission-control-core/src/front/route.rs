//! Request routing: a URL to the content it asks for, the inverse of
//! [`permalink`]. It follows WordPress's two steps:
//!
//! 1. The rewrite rules (`WP_Rewrite`, `WP::parse_request`) read the path into query variables,
//!    in WordPress's order: the category and tag bases, custom post types, the root
//!    (`/page/2/`), `search/`, `author/`, the date archives, then the posts and pages. Variables
//!    in the query string win over the path's, as `$_GET` does.
//! 2. The query (`WP_Query::parse_query`, `get_posts` and `WP::handle_404`) turns the variables
//!    into a [`Resolved`]: one post, an archive, a search, the front page, or not found. Before
//!    a 404, `wp_old_slug_redirect` and a few of `redirect_canonical`'s redirects are tried.
//!
//! The visitor is anonymous: only published content resolves. Private posts, drafts, pending
//! and scheduled posts and the trash are not found.
//!
//! Values are compared exactly, whatever the database's collation: MySQL and MariaDB compare
//! text without case and trailing spaces by default, so every fetched post, page, term and old
//! slug is checked again here, and a variant such as `/Hello-World/` is not found (no canonical
//! redirect to the lower-case slug).
//!
//! Input is bounded: a path longer than [`MAX_PATH_BYTES`] or with more than
//! [`MAX_PATH_SEGMENTS`] segments is not found (WordPress pages rarely nest more than a few
//! levels), and at most [`MAX_TYPE_KEYS`] query keys are tried as custom types, in one query.
//!
//! Left for later, as they need layers that do not exist yet:
//! - Readers who may see private posts (`read_private_posts`): `visitor_can_read` is the one
//!   place to widen once a request carries its user.
//! - `%author%` and `/author/{nicename}/`: users have no nicename until the profile layer.
//!   A `%author%` segment in a post's link is matched but not checked (ruling R29), and no
//!   redirect is made to a post link that needs it (it cannot be rendered yet). The author base
//!   does not resolve; author archives resolve by id, `?author=2`.
//! - The post type registry (design 5.2): until it exists, a custom type resolves when the
//!   database has a post of that type, and WordPress's non-public built-in types
//!   ([`NON_PUBLIC_TYPES`]) never resolve, by any route.
//! - Post type archives at a pretty path (`has_archive`), custom rewrite slugs and hierarchical
//!   custom types need the post type registry; a custom type resolves at `{front}{type}/{slug}`,
//!   `?{type}={slug}` and `?post_type={type}`, when the database has a post of that type.
//! - Feeds, embeds, trackbacks, comment pages, `<!--nextpage-->` pages (`/post/2/`), attachment
//!   pages under a post (`/post/photo/`), `wp_old_date_redirect`, and most canonical redirects
//!   (trailing slashes, `/page/1/`, `?cat=` and `?author=` to their pretty links,
//!   `redirect_guess_404_permalink`).
//! - The `timezone_string` option: dates use `gmt_offset` only.

use std::collections::HashMap;

use chrono::{Datelike, Duration, FixedOffset, NaiveDate, NaiveDateTime, Timelike};
use orbit_orm::{Database, Error, Model, Value as Sql, raw};
use serde_json::Value;

use crate::content::{
    MetaError, PermalinkContext, PermalinkStructure, Post, PostStatus, Segment, Tag, Taxonomy,
    Term, get_option, get_post_meta_values, permalink,
};

/// A request's query string, parsed as PHP fills `$_GET`: `+` and `%XX` decoded, a key with no
/// `=` given the empty value, and the last of repeated keys winning.
///
/// ```ignore
/// let query = Query::parse("s=hello+world&paged=2");
/// assert_eq!(query.get("s"), Some("hello world"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// Parses `query`, the part of the URL after `?` (a leading `?` is skipped).
    pub fn parse(query: &str) -> Self {
        let pairs = query
            .trim_start_matches('?')
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(key, true), decode(value, true))
            })
            .collect();
        Self { pairs }
    }

    /// The value of `key`, the last one when it repeats.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether the query string has no variables.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

/// An archive: a list of posts, as WordPress's `is_archive` queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Archive {
    /// The posts of a custom post type (`is_post_type_archive`), by the type's name.
    PostType(String),
    /// The posts of a year, a month or a day (`is_date`), in the site's time zone.
    Date {
        /// The year.
        year: i32,
        /// The month, 1 to 12, for a month or a day archive.
        month: Option<u32>,
        /// The day of the month, for a day archive.
        day: Option<u32>,
    },
    /// The posts of a category and its children (`is_category`).
    Category(Term),
    /// The posts of a tag (`is_tag`).
    Tag(Term),
    /// The posts of an author (`is_author`), by user id.
    Author(i64),
}

/// What a request asks for: the input of the template hierarchy. Each variant answers
/// WordPress's conditional tags.
///
/// `paged` is the page of a list, from `/page/2/` or `?paged=2`; 1 for the first page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The site's root (`is_front_page`). `page` is the static front page when Settings >
    /// Reading shows one (`show_on_front` is `page`, `page_on_front` set); `None` is "Your
    /// latest posts", which is also `is_home`.
    FrontPage {
        /// The static front page, or `None` for the latest posts.
        page: Option<Post>,
        /// The page of the posts list (or of the static page's own pagination).
        paged: u32,
    },
    /// The posts page of a site with a static front page (`is_home` without `is_front_page`):
    /// the page set as `page_for_posts`, which lists the posts.
    Home {
        /// The posts page.
        page: Post,
        /// The page of the posts list.
        paged: u32,
    },
    /// One post of a type that is not a page (`is_single`): a post or a custom type.
    Single(Post),
    /// A page, found by its full path (`is_page`).
    Page(Post),
    /// An attachment's page (`is_attachment`).
    Attachment(Post),
    /// An archive (`is_archive`).
    Archive {
        /// Which posts it lists.
        archive: Archive,
        /// The page of the list.
        paged: u32,
    },
    /// A search (`is_search`), with the terms as the visitor typed them. An empty search is a
    /// search, as in WordPress.
    Search {
        /// The search string.
        terms: String,
        /// The page of the results.
        paged: u32,
    },
    /// The content lives at another address: an old slug (`wp_old_slug_redirect`) or a
    /// canonical redirect. `location` is a path from the site's root, such as `/new-slug/`;
    /// prefix the site address for an absolute `Location`.
    Redirect {
        /// The path to redirect to.
        location: String,
        /// The HTTP status: 301.
        status: u16,
    },
    /// Nothing is here (`is_404`).
    NotFound,
}

/// Resolves the request for `path` (the URL path from the site's root, still percent-encoded)
/// and `query` under the permalink `structure`, as WordPress's `WP::main` does before it loads a
/// template. Reads the posts, terms, old slugs, users and the options it needs:
/// `show_on_front`, `page_on_front`, `page_for_posts`, `posts_per_page` (10 when unset),
/// `gmt_offset`, `category_base`, `tag_base` and `default_category`.
///
/// It is async and takes the database, unlike [`permalink`], because a URL is read back by
/// looking content up.
///
/// # Errors
///
/// Fails with the database's error. An option whose stored value is not valid JSON counts as
/// unset.
pub async fn resolve(
    db: &Database,
    path: &str,
    query: &Query,
    structure: &PermalinkStructure,
) -> Result<Resolved, Error> {
    let site = Site::load(db).await?;
    let router = Router {
        db,
        structure,
        site: &site,
    };
    let Some(mut vars) = router.match_path(path).await? else {
        return Ok(Resolved::NotFound);
    };
    vars.overlay(query);
    router.query(&vars).await
}

/// The posts a request shows, as WordPress's main `WP_Query` holds them after the request is
/// resolved: the loop's posts, how many posts the whole list has and how many pages it fills.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MainQuery {
    /// The posts of this page, newest first; the one post of a singular request.
    pub posts: Vec<Post>,
    /// How many posts the whole list has (`found_posts`).
    pub found_posts: i64,
    /// How many pages the list fills (`max_num_pages`); 1 for a singular request, 0 for an empty
    /// list or nothing found.
    pub max_num_pages: u32,
}

/// The posts `resolved` shows, as WordPress's main query fetches them: the post of a singular
/// request, or page `paged` of a list, `posts_per_page` to a page (every post on one page when
/// the option is below 1). Lists hold published posts only, newest first, as [`resolve`] counts
/// them: the latest posts for the front page and the posts page, a category with its children,
/// a tag, an author, a date in the site's time zone, a custom type, and posts and pages
/// matching a search. A redirect and a 404 show no posts.
///
/// Search results are ordered by date, not by WordPress's relevance (title matches first).
///
/// # Errors
///
/// Fails with the database's error.
pub async fn query_posts(db: &Database, resolved: &Resolved) -> Result<MainQuery, Error> {
    let singular = |post: &Post| MainQuery {
        posts: vec![post.clone()],
        found_posts: 1,
        max_num_pages: 1,
    };
    let site = Site::load(db).await?;
    let (listing, paged) = match resolved {
        Resolved::Redirect { .. } | Resolved::NotFound => return Ok(MainQuery::default()),
        Resolved::Single(post) | Resolved::Page(post) | Resolved::Attachment(post) => {
            return Ok(singular(post));
        }
        Resolved::FrontPage {
            page: Some(page), ..
        } => return Ok(singular(page)),
        Resolved::FrontPage { page: None, paged } | Resolved::Home { paged, .. } => {
            (Listing::default(), *paged)
        }
        Resolved::Search { terms, paged } => (
            Listing {
                post_types: &["post", "page"],
                search: search_terms(terms),
                ..Listing::default()
            },
            *paged,
        ),
        Resolved::Archive { archive, paged } => {
            let listing = match archive {
                Archive::PostType(post_type) => {
                    let posts = Listing {
                        post_types: &[post_type.as_str()],
                        ..Listing::default()
                    };
                    return list_page(db, &site, &posts, *paged).await;
                }
                Archive::Category(term) => Listing {
                    term_taxonomy_ids: Some(Categories::load(db).await?.with_descendants(term)),
                    ..Listing::default()
                },
                Archive::Tag(term) => Listing {
                    term_taxonomy_ids: Some(vec![term.term_taxonomy_id]),
                    ..Listing::default()
                },
                Archive::Author(id) => Listing {
                    author: Some(*id),
                    ..Listing::default()
                },
                Archive::Date { year, month, day } => match site.date_range(*year, *month, *day) {
                    Some(range) => Listing {
                        range: Some(range),
                        ..Listing::default()
                    },
                    None => return Ok(MainQuery::default()),
                },
            };
            (listing, *paged)
        }
    };
    list_page(db, &site, &listing, paged).await
}

/// Page `paged` of `listing`, with its totals.
async fn list_page(
    db: &Database,
    site: &Site,
    listing: &Listing<'_>,
    paged: u32,
) -> Result<MainQuery, Error> {
    let found_posts = listing.count(db).await?;
    let max_num_pages = match site.per_page {
        Some(n) => u32::try_from(found_posts.saturating_add(n - 1) / n).unwrap_or(u32::MAX),
        None => u32::from(found_posts > 0),
    };
    Ok(MainQuery {
        posts: listing.fetch(db, site.per_page, paged).await?,
        found_posts,
        max_num_pages,
    })
}

/// The permalinks of `posts`, in order, each a path from the site's root
/// (`/2026/03/hello-world/`) as `get_permalink` gives it, with the values its tags need read
/// from the database: a page's ancestors and a post's first category, or the default category.
/// Prefix the site address for an absolute link.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn post_links(
    db: &Database,
    structure: &PermalinkStructure,
    posts: &[Post],
) -> Result<Vec<String>, Error> {
    let site = Site::load(db).await?;
    let router = Router {
        db,
        structure,
        site: &site,
    };
    router.links(posts).await
}

/// How many `IN (...)` ids one query binds, below every database's limit on bind parameters.
const IN_CHUNK: usize = 500;

/// How many levels of parents [`Router::links`] reads for page ancestors. Pages deeper than
/// [`MAX_PATH_SEGMENTS`] do not resolve anyway; this only bounds a long or broken chain.
const MAX_ANCESTOR_LEVELS: usize = 64;

#[cfg(test)]
thread_local! {
    /// The queries [`Router::links`] has run on this thread beyond the options, for the tests
    /// that check links are read in batches.
    static LINK_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts one query of the link path, in tests.
fn count_link_query() {
    #[cfg(test)]
    LINK_QUERIES.with(|n| n.set(n.get() + 1));
}

/// The archive links of `terms`, in order, each a path from the site's root as `get_term_link`
/// gives it: `/category/news/local/` (the category's ancestors in the path) and `/tag/rust/`
/// under the category and tag bases, or `/?cat=3` and `/?tag=rust` with plain permalinks.
/// `None` for a term of a taxonomy without archives (`nav_menu`, or one not built in). Prefix
/// the site address for an absolute link.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn term_links(
    db: &Database,
    structure: &PermalinkStructure,
    terms: &[Term],
) -> Result<Vec<Option<String>>, Error> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let site = Site::load(db).await?;
    let router = Router {
        db,
        structure,
        site: &site,
    };
    let categories = if terms.iter().any(|t| t.taxonomy == Taxonomy::CATEGORY.name) {
        Some(Categories::load(db).await?)
    } else {
        None
    };
    Ok(terms
        .iter()
        .map(|term| {
            let plain = structure.is_plain();
            if term.taxonomy == Taxonomy::CATEGORY.name {
                let categories = categories.as_ref()?;
                Some(if plain {
                    format!("/?cat={}", term.term_id)
                } else {
                    router.list_link(&router.category_prefix(), &categories.path(term), 1)
                })
            } else if term.taxonomy == Taxonomy::POST_TAG.name {
                Some(if plain {
                    format!("/?tag={}", term.slug)
                } else {
                    router.list_link(&router.tag_prefix(), &term.slug, 1)
                })
            } else {
                None
            }
        })
        .collect())
}

/// Whether the anonymous visitor may read `post`: a published post, or an attachment
/// (`inherit`) whose parent is published or that has none, as WordPress shows them to a visitor
/// who is not logged in. Password-protected posts count as readable; the template asks for the
/// password. The one rule routing and menus share; [`visitor_readable`] applies it to many posts.
// TODO(users, Task 8.x): a logged-in reader with `read_private_posts` also reads private posts,
// and `read_post` decides the rest.
pub(crate) async fn visitor_can_read(db: &Database, post: &Post) -> Result<bool, Error> {
    Ok(!visitor_readable(db, vec![post.clone()]).await?.is_empty())
}

/// The posts of `posts` the anonymous visitor may read ([`visitor_can_read`]), in order, with
/// the attachments' parents read in one query per [`IN_CHUNK`] parents.
pub(crate) async fn visitor_readable(db: &Database, posts: Vec<Post>) -> Result<Vec<Post>, Error> {
    let attached = |post: &Post| {
        (post.status == PostStatus::Inherit && post.post_type == "attachment")
            .then_some(post.parent_id)
            .flatten()
    };
    let mut parents: Vec<i64> = posts.iter().filter_map(attached).collect();
    parents.sort_unstable();
    parents.dedup();
    let mut unpublished = std::collections::HashSet::new();
    for chunk in parents.chunks(IN_CHUNK) {
        for parent in Post::query()
            .where_in(Post::PRIMARY_KEY, chunk.to_vec())
            .get(db)
            .await?
        {
            if parent.status != PostStatus::Publish {
                unpublished.insert(parent.id);
            }
        }
    }
    Ok(posts
        .into_iter()
        .filter(|post| match post.status {
            PostStatus::Publish => true,
            PostStatus::Inherit if post.post_type == "attachment" => {
                attached(post).is_none_or(|parent| !unpublished.contains(&parent))
            }
            _ => false,
        })
        .collect())
}

/// The options routing reads, once per request.
struct Site {
    /// `page_on_front`, when `show_on_front` is `page`.
    front_page: Option<i64>,
    /// `page_for_posts`, when `show_on_front` is `page`.
    posts_page: Option<i64>,
    /// `posts_per_page`; `None` shows every post on one page.
    per_page: Option<i64>,
    /// `gmt_offset`, for the dates in URLs.
    offset: FixedOffset,
    /// `category_base`, empty for `category` under the front.
    category_base: String,
    /// `tag_base`, empty for `tag` under the front.
    tag_base: String,
    /// `default_category`, the category in a post's link when it has none.
    default_category: Option<i64>,
}

impl Site {
    async fn load(db: &Database) -> Result<Self, Error> {
        let static_front =
            option(db, "show_on_front").await?.as_ref().and_then(text) == Some("page".to_owned());
        let page = async |name| -> Result<Option<i64>, Error> {
            Ok(if static_front {
                option(db, name).await?.as_ref().and_then(id)
            } else {
                None
            })
        };
        let front_page = page("page_on_front").await?;
        let posts_page = page("page_for_posts").await?;
        let per_page = match option(db, "posts_per_page")
            .await?
            .as_ref()
            .and_then(number)
        {
            None => Some(10),
            Some(n) if n >= 1.0 => Some(n as i64),
            Some(_) => None,
        };
        let offset = crate::content::gmt_offset(db).await?;
        let base = async |name| -> Result<String, Error> {
            Ok(option(db, name)
                .await?
                .as_ref()
                .and_then(text)
                .map(|s| s.trim_matches('/').to_owned())
                .unwrap_or_default())
        };
        Ok(Self {
            front_page,
            posts_page,
            per_page,
            offset,
            category_base: base("category_base").await?,
            tag_base: base("tag_base").await?,
            default_category: option(db, "default_category").await?.as_ref().and_then(id),
        })
    }

    /// The site's offset as a duration, to turn local dates to UTC.
    fn offset_duration(&self) -> Duration {
        Duration::seconds(i64::from(self.offset.local_minus_utc()))
    }

    /// The UTC range, end excluded, of a year, a month or a day in the site's time zone, or
    /// `None` when the date is not valid (a day without a month, a year outside 1 to 9999).
    fn date_range(
        &self,
        year: i32,
        month: Option<u32>,
        day: Option<u32>,
    ) -> Option<(NaiveDateTime, NaiveDateTime)> {
        if !(1..=9999).contains(&year) || (day.is_some() && month.is_none()) {
            return None;
        }
        let start = NaiveDate::from_ymd_opt(year, month.unwrap_or(1), day.unwrap_or(1))?;
        let end = match (month, day) {
            (_, Some(_)) => start.succ_opt(),
            (Some(_), None) => start.checked_add_months(chrono::Months::new(1)),
            (None, None) => NaiveDate::from_ymd_opt(year + 1, 1, 1),
        }?;
        let utc = |d: NaiveDate| {
            d.and_time(chrono::NaiveTime::MIN)
                .checked_sub_signed(self.offset_duration())
                .map(storable)
        };
        Some((utc(start)?, utc(end)?))
    }

    /// Whether page `paged` of a list of `count` posts is empty, so the request is not found, as
    /// `handle_404` decides for a paged query that found nothing.
    fn past_the_end(&self, count: i64, paged: u32) -> bool {
        paged > 1
            && match self.per_page {
                Some(n) => i64::from(paged - 1).saturating_mul(n) >= count,
                None => count == 0,
            }
    }
}

/// Option `name`, with a value that is not valid JSON counted as unset.
pub(super) async fn option(db: &Database, name: &str) -> Result<Option<Value>, Error> {
    match get_option(db, name).await {
        Ok(value) => Ok(value),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e),
    }
}

/// An option's text, WordPress storing strings and numbers alike.
pub(super) fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// An option's number, from a JSON number or a numeric string.
pub(super) fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// An option that holds a post or term id; 0 (WordPress's "none") is `None`.
fn id(value: &Value) -> Option<i64> {
    number(value)
        .filter(|n| *n >= 1.0 && n.fract() == 0.0)
        .map(|n| n as i64)
}

/// WordPress's public query variables that routing reads, filled from the path and then the
/// query string. Numbers that do not parse are left unset, as `absint` gives 0.
#[derive(Debug, Default)]
struct Vars {
    p: Option<i64>,
    page_id: Option<i64>,
    attachment_id: Option<i64>,
    name: Option<String>,
    pagename: Option<String>,
    post_type: Option<String>,
    year: Option<i32>,
    monthnum: Option<u32>,
    day: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<u32>,
    m: Option<String>,
    category_name: Option<String>,
    cat: Option<i64>,
    tag: Option<String>,
    author: Option<i64>,
    author_name: Option<String>,
    s: Option<String>,
    paged: Option<u32>,
    /// `?{post_type}={slug}`: a custom type's query variable, for each key that is not one of
    /// the above and could be a public custom type; at most [`MAX_TYPE_KEYS`], without repeats.
    custom: Vec<(String, String)>,
    /// Whether `p` or `page_id` came from the query string, which `redirect_canonical` sends
    /// to the pretty link.
    id_in_query: bool,
}

impl Vars {
    /// Sets the variables the query string gives, over the path's.
    fn overlay(&mut self, query: &Query) {
        for (key, value) in &query.pairs {
            let v = value.as_str();
            match key.as_str() {
                "p" => {
                    self.p = v.parse().ok();
                    self.id_in_query = true;
                }
                "page_id" => {
                    self.page_id = v.parse().ok();
                    self.id_in_query = true;
                }
                "attachment_id" => self.attachment_id = v.parse().ok(),
                "name" => self.name = non_empty(v),
                "pagename" => self.pagename = non_empty(v),
                // A value that is not a public type's exact name is dropped, as WordPress drops
                // an unknown type: no case or padding variant reaches the database.
                "post_type" => {
                    self.post_type =
                        Some(v.to_owned()).filter(|t| is_type_name(t) && is_public_type(t));
                }
                "year" => self.year = v.parse().ok(),
                "monthnum" => self.monthnum = v.parse().ok(),
                "day" => self.day = v.parse().ok(),
                "hour" => self.hour = v.parse().ok(),
                "minute" => self.minute = v.parse().ok(),
                "second" => self.second = v.parse().ok(),
                "m" => self.m = non_empty(v),
                "category_name" => self.category_name = non_empty(v),
                "cat" => self.cat = v.parse().ok(),
                "tag" => self.tag = non_empty(v),
                "author" => self.author = v.parse().ok(),
                "author_name" => self.author_name = non_empty(v),
                "s" => self.s = Some(v.to_owned()),
                "paged" => self.paged = v.parse().ok(),
                _ if !v.is_empty() && is_custom_type(key) => {
                    let full = self.custom.len() >= MAX_TYPE_KEYS;
                    match self.custom.iter_mut().find(|(k, _)| k == key) {
                        Some(pair) => pair.1 = v.to_owned(),
                        None if !full => self.custom.push((key.clone(), v.to_owned())),
                        None => {}
                    }
                }
                _ => {}
            }
        }
        // WordPress unsets a `post_type` that is not publicly queryable (`parse_request`).
        if self
            .post_type
            .as_deref()
            .is_some_and(|t| !is_public_type(t))
        {
            self.post_type = None;
        }
    }

    /// Whether a date archive is asked for.
    fn has_date(&self) -> bool {
        self.year.is_some() || self.monthnum.is_some() || self.day.is_some() || self.m.is_some()
    }
}

fn non_empty(v: &str) -> Option<String> {
    (!v.is_empty()).then(|| v.to_owned())
}

/// Whether `key` could be a post type's name: lower-case letters, digits, `_` and `-`, at most
/// 20 characters, as `register_post_type` allows.
fn is_type_name(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 20
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The public built-in post types, which have rules of their own rather than the custom type
/// rule.
const BUILTIN_TYPES: [&str; 3] = ["post", "page", "attachment"];

/// WordPress's built-in post types that are not publicly queryable: they never resolve on the
/// site, by path, id or query variable. The one list to check until the post type registry
/// (design 5.2) says which types are public.
pub(crate) const NON_PUBLIC_TYPES: [&str; 13] = [
    "revision",
    "nav_menu_item",
    "custom_css",
    "customize_changeset",
    "oembed_cache",
    "user_request",
    "wp_block",
    "wp_template",
    "wp_template_part",
    "wp_global_styles",
    "wp_navigation",
    "wp_font_family",
    "wp_font_face",
];

/// Whether posts of `post_type` may be shown on the site.
fn is_public_type(post_type: &str) -> bool {
    !NON_PUBLIC_TYPES.contains(&post_type)
}

/// Whether `post_type` may resolve through the custom type rule or query variable.
fn is_custom_type(post_type: &str) -> bool {
    is_type_name(post_type) && !BUILTIN_TYPES.contains(&post_type) && is_public_type(post_type)
}

/// The most query string keys tried as custom post types (`?{type}={slug}`).
pub(crate) const MAX_TYPE_KEYS: usize = 8;

/// The longest path, in bytes, that routing reads; a longer one is not found.
pub(crate) const MAX_PATH_BYTES: usize = 1024;

/// The most segments a path may have; a deeper one is not found.
pub(crate) const MAX_PATH_SEGMENTS: usize = 32;

/// Whether `path` is within [`MAX_PATH_BYTES`] and [`MAX_PATH_SEGMENTS`].
fn within_limits(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES && path.split('/').count() <= MAX_PATH_SEGMENTS
}

/// One request's lookups.
struct Router<'a> {
    db: &'a Database,
    structure: &'a PermalinkStructure,
    site: &'a Site,
}

impl Router<'_> {
    /// The query variables of `path`, as the first matching rewrite rule gives them, or `None`
    /// when no rule matches (a 404). The root gives no variables.
    async fn match_path(&self, path: &str) -> Result<Option<Vars>, Error> {
        let rel = path.trim_matches('/');
        if !within_limits(rel) {
            return Ok(None);
        }
        if rel.is_empty() {
            return Ok(Some(Vars::default()));
        }
        // Plain permalinks have no rewrite rules.
        if self.structure.is_plain() {
            return Ok(None);
        }
        let (base, paged) = split_paged(rel);
        let mut vars = Vars {
            paged,
            ..Vars::default()
        };
        // The root's `page/2`.
        if base.is_empty() {
            return Ok(Some(vars));
        }
        // The taxonomies' rules come first (`extra_rules_top`).
        if let Some(rest) = strip_dir(base, &self.category_prefix()) {
            vars.category_name = Some(decode(rest, false));
            return Ok(Some(vars));
        }
        if let Some(rest) = strip_dir(base, &self.tag_prefix()) {
            vars.tag = Some(decode(rest, false));
            return Ok(Some(vars));
        }
        if paged.is_none()
            && let Some((post_type, slug)) = self.custom_type_path(base).await?
        {
            vars.post_type = Some(post_type);
            vars.name = Some(slug);
            return Ok(Some(vars));
        }
        if let Some(rest) = strip_dir(base, "search") {
            vars.s = Some(decode(rest, true));
            return Ok(Some(vars));
        }
        if let Some(rest) = strip_dir(base, &self.with_front("author"))
            && !rest.contains('/')
        {
            vars.author_name = Some(decode(rest, false));
            return Ok(Some(vars));
        }
        if let Some((year, month, day)) = self.match_date(base) {
            vars.year = Some(year);
            vars.monthnum = month;
            vars.day = day;
            return Ok(Some(vars));
        }
        // A structure that starts with a name puts the page rules first and checks that the
        // page exists before taking the rule (`use_verbose_page_rules`); otherwise the post
        // rule comes first and the page rule catches the rest.
        let verbose = matches!(
            self.structure.tags().first(),
            Some(Tag::Postname | Tag::Category | Tag::Author)
        );
        let pagename = decode(base, false);
        if verbose && self.page_by_path(&pagename).await?.is_some() {
            vars.pagename = Some(pagename);
            return Ok(Some(vars));
        }
        if paged.is_none()
            && let Some(captures) = match_structure(self.structure.segments(), base)
        {
            return Ok(post_vars(vars, captures));
        }
        if !verbose {
            vars.pagename = Some(pagename);
            return Ok(Some(vars));
        }
        Ok(None)
    }

    /// `name` under the structure's front (`$wp_rewrite->front`), without slashes at the ends.
    fn with_front(&self, name: &str) -> String {
        let front = self.structure.front().trim_matches('/');
        if front.is_empty() {
            name.to_owned()
        } else {
            format!("{front}/{name}")
        }
    }

    /// Where category archives live: `category_base` from the root, or `category` under the
    /// front, as `register_taxonomy` sets `with_front`.
    fn category_prefix(&self) -> String {
        if self.site.category_base.is_empty() {
            self.with_front("category")
        } else {
            self.site.category_base.clone()
        }
    }

    /// Where tag archives live, as [`category_prefix`](Self::category_prefix).
    fn tag_prefix(&self) -> String {
        if self.site.tag_base.is_empty() {
            self.with_front("tag")
        } else {
            self.site.tag_base.clone()
        }
    }

    /// A custom type's single path, `{front}{type}/{slug}`, when the database has posts of that
    /// type (the stand-in for a registered type).
    async fn custom_type_path(&self, base: &str) -> Result<Option<(String, String)>, Error> {
        let front = self.with_front("");
        let rest = if front.is_empty() {
            Some(base)
        } else {
            strip_dir(base, front.trim_end_matches('/'))
        };
        let Some((post_type, slug)) = rest.and_then(|r| r.split_once('/')) else {
            return Ok(None);
        };
        if slug.contains('/') || !is_custom_type(post_type) {
            return Ok(None);
        }
        if !Post::of_type(post_type).exists(self.db).await? {
            return Ok(None);
        }
        Ok(Some((post_type.to_owned(), decode(slug, false))))
    }

    /// The date archive rules: `year`, `year/month`, `year/month/day` after the date front,
    /// which is the structure's front, plus `date/` when `%post_id%` is among its first three
    /// tags (`get_date_permastruct`).
    fn match_date(&self, base: &str) -> Option<(i32, Option<u32>, Option<u32>)> {
        let id_first = self
            .structure
            .tags()
            .iter()
            .take(3)
            .any(|t| *t == Tag::PostId);
        let prefix = self.with_front(if id_first { "date" } else { "" });
        let prefix = prefix.trim_end_matches('/');
        let rest = if prefix.is_empty() {
            base
        } else {
            strip_dir(base, prefix)?
        };
        let parts: Vec<&str> = rest.split('/').collect();
        let digits = |s: &str, min, max| {
            (s.len() >= min && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u32>().ok())
                .flatten()
        };
        let year = digits(parts.first()?, 4, 4)? as i32;
        match parts[1..] {
            [] => Some((year, None, None)),
            [m] => Some((year, Some(digits(m, 1, 2)?), None)),
            [m, d] => Some((year, Some(digits(m, 1, 2)?), Some(digits(d, 1, 2)?))),
            _ => None,
        }
    }

    /// Turns the variables into the request's content, as `WP_Query` and `handle_404` do.
    async fn query(&self, vars: &Vars) -> Result<Resolved, Error> {
        let paged = vars.paged.filter(|n| *n > 1).unwrap_or(1);
        // Singular queries.
        if let Some(id) = vars.attachment_id {
            return self.attachment(id).await;
        }
        if vars.name.is_some() || vars.p.is_some() {
            let post_type = vars.post_type.as_deref().unwrap_or("post");
            return self.single(vars, post_type).await;
        }
        if vars.page_id.is_some() || vars.pagename.is_some() {
            return self.page(vars, paged).await;
        }
        if let Some((post_type, slug)) = self.custom_query_var(&vars.custom).await? {
            let vars = Vars {
                name: Some(slug.clone()),
                ..Vars::default()
            };
            return self.single(&vars, post_type).await;
        }
        // Lists, in the order the template loader checks them.
        if let Some(terms) = &vars.s {
            let listing = Listing {
                post_types: &["post", "page"],
                search: search_terms(terms),
                ..Listing::default()
            };
            if paged > 1 && self.site.past_the_end(listing.count(self.db).await?, paged) {
                return Ok(Resolved::NotFound);
            }
            return Ok(Resolved::Search {
                terms: terms.clone(),
                paged,
            });
        }
        if let Some(post_type) = vars.post_type.as_deref().filter(|t| *t != "post") {
            if !is_custom_type(post_type) || !Post::of_type(post_type).exists(self.db).await? {
                return Ok(Resolved::NotFound);
            }
            let listing = Listing {
                post_types: &[post_type],
                ..Listing::default()
            };
            let count = listing.count(self.db).await?;
            return Ok(self.list(Archive::PostType(post_type.to_owned()), count, paged, true));
        }
        if vars.cat.is_some() || vars.category_name.is_some() {
            return self.category(vars, paged).await;
        }
        if let Some(slug) = &vars.tag {
            let tag = Term::find_by_slug(self.db, &Taxonomy::POST_TAG, slug).await?;
            let Some(tag) = tag.filter(|t| t.slug == *slug) else {
                return Ok(Resolved::NotFound);
            };
            let listing = Listing {
                term_taxonomy_ids: Some(vec![tag.term_taxonomy_id]),
                ..Listing::default()
            };
            let count = listing.count(self.db).await?;
            return Ok(self.list(Archive::Tag(tag), count, paged, true));
        }
        if vars.author_name.is_some() {
            // TODO(profiles, Task 8.1): resolve the nicename to a user.
            return Ok(Resolved::NotFound);
        }
        if let Some(author) = vars.author {
            let exists: i64 = raw::fetch_scalar(
                self.db,
                "SELECT COUNT(*) FROM users WHERE id = ?",
                &[author.into()],
            )
            .await?;
            let listing = Listing {
                author: Some(author),
                ..Listing::default()
            };
            let count = listing.count(self.db).await?;
            return Ok(self.list(Archive::Author(author), count, paged, exists > 0));
        }
        if vars.has_date() {
            return self.date(vars, paged).await;
        }
        self.home(paged).await
    }

    /// The first `?{type}={slug}` pair whose type has posts, found with one query.
    async fn custom_query_var<'v>(
        &self,
        custom: &'v [(String, String)],
    ) -> Result<Option<&'v (String, String)>, Error> {
        if custom.is_empty() {
            return Ok(None);
        }
        let marks = vec!["?"; custom.len()].join(", ");
        let binds: Vec<Sql> = custom.iter().map(|(t, _)| Sql::from(t.as_str())).collect();
        let existing: Vec<(String,)> = raw::fetch_all(
            self.db,
            &format!("SELECT DISTINCT post_type FROM posts WHERE post_type IN ({marks})"),
            &binds,
        )
        .await?;
        Ok(custom
            .iter()
            .find(|(t, _)| existing.iter().any(|(e,)| e == t)))
    }

    /// A list of `count` posts: not found when the page is past the end, or when it is empty
    /// and nothing named it (`found` is false; a date archive is never found empty).
    fn list(&self, archive: Archive, count: i64, paged: u32, found: bool) -> Resolved {
        if self.site.past_the_end(count, paged) || (count == 0 && !found) {
            Resolved::NotFound
        } else {
            Resolved::Archive { archive, paged }
        }
    }

    /// The root: the static front page, or the latest posts.
    async fn home(&self, paged: u32) -> Result<Resolved, Error> {
        if let Some(id) = self.site.front_page {
            return Ok(match self.visible_of_type(id, "page").await? {
                Some(page) => Resolved::FrontPage {
                    page: Some(page),
                    paged,
                },
                None => Resolved::NotFound,
            });
        }
        let count = Listing::default().count(self.db).await?;
        Ok(if self.site.past_the_end(count, paged) {
            Resolved::NotFound
        } else {
            Resolved::FrontPage { page: None, paged }
        })
    }

    /// A post of `post_type` by `name` or `p`, which must also match the date, id and category
    /// the URL gives.
    async fn single(&self, vars: &Vars, post_type: &str) -> Result<Resolved, Error> {
        // TODO(profiles, Task 8.1): check a `%author%` segment (`author_name`) against the
        // post's author's nicename. Until then it is matched but not checked (R29).
        if !is_public_type(post_type) {
            return Ok(Resolved::NotFound);
        }
        let candidates = if let Some(name) = &vars.name {
            Post::of_type(post_type)
                .where_eq("slug", name.as_str())
                .get(self.db)
                .await?
        } else {
            let id = vars.p.expect("a name or an id");
            Post::of_type(post_type)
                .where_eq(Post::PRIMARY_KEY, id)
                .get(self.db)
                .await?
        };
        let mut found = None;
        for post in candidates {
            // Exact matches only: MySQL's default collations compare without case and trailing
            // spaces, so `/Hello/` or `hello%20` would otherwise find `hello`.
            if post.post_type == post_type
                && vars.name.as_ref().is_none_or(|name| post.slug == *name)
                && vars.p.is_none_or(|id| id == post.id)
                && self.dates_match(vars, &post)
                && self.can_read(&post).await?
            {
                found = Some(post);
                break;
            }
        }
        let Some(post) = found else {
            return self.not_found(vars, post_type).await;
        };
        if self.can_link(&post) {
            if let Some(path) = &vars.category_name
                && !self.in_category(&post, path).await?
            {
                return self.redirect_to(&post).await;
            }
            if vars.id_in_query && !self.structure.is_plain() {
                return self.redirect_to(&post).await;
            }
        }
        Ok(match post.post_type.as_str() {
            "attachment" => Resolved::Attachment(post),
            "page" => Resolved::Page(post),
            _ => Resolved::Single(post),
        })
    }

    /// Before a 404 on a single: the post that had the slug (`wp_old_slug_redirect`), then the
    /// post of another type with the id (`redirect_canonical` on a 404).
    async fn not_found(&self, vars: &Vars, post_type: &str) -> Result<Resolved, Error> {
        if let Some(name) = &vars.name
            && let Some(post) = crate::content::find_by_old_slug(self.db, post_type, name).await?
            && post.post_type == post_type
            && is_public_type(&post.post_type)
            && self.had_slug(&post, name).await?
            && self.dates_match(vars, &post)
            && self.can_link(&post)
            && self.can_read(&post).await?
        {
            return self.redirect_to(&post).await;
        }
        let id = vars.p.or(vars.page_id).or(vars.attachment_id);
        if vars.id_in_query
            && let Some(id) = id
            && let Some(post) = Post::query().find(self.db, id).await?
            && is_public_type(&post.post_type)
            && self.can_link(&post)
            && self.can_read(&post).await?
        {
            return self.redirect_to(&post).await;
        }
        Ok(Resolved::NotFound)
    }

    /// `?attachment_id=`.
    async fn attachment(&self, id: i64) -> Result<Resolved, Error> {
        if let Some(post) = self.visible_of_type(id, "attachment").await? {
            return Ok(Resolved::Attachment(post));
        }
        let vars = Vars {
            attachment_id: Some(id),
            id_in_query: true,
            ..Vars::default()
        };
        self.not_found(&vars, "attachment").await
    }

    /// A page by `page_id` or by its path, which may be the front page or the posts page.
    async fn page(&self, vars: &Vars, paged: u32) -> Result<Resolved, Error> {
        let page = match (vars.page_id, &vars.pagename) {
            (Some(id), _) => self.visible_of_type(id, "page").await?,
            (None, Some(path)) => match self.page_by_path(path).await? {
                Some(page) if self.can_read(&page).await? => Some(page),
                _ => None,
            },
            (None, None) => None,
        };
        let Some(page) = page else {
            return self.not_found(vars, "page").await;
        };
        if self.site.front_page == Some(page.id) {
            return Ok(Resolved::Redirect {
                location: "/".to_owned(),
                status: 301,
            });
        }
        if vars.id_in_query && !self.structure.is_plain() {
            return self.redirect_to(&page).await;
        }
        if self.site.posts_page == Some(page.id) {
            let count = Listing::default().count(self.db).await?;
            if self.site.past_the_end(count, paged) {
                return Ok(Resolved::NotFound);
            }
            return Ok(Resolved::Home { page, paged });
        }
        Ok(Resolved::Page(page))
    }

    /// A category archive by `cat` (a term id) or `category_name` (a slug path).
    async fn category(&self, vars: &Vars, paged: u32) -> Result<Resolved, Error> {
        let categories = Categories::load(self.db).await?;
        let term = match (vars.cat, &vars.category_name) {
            (Some(id), _) => categories.by_term_id(id).cloned(),
            (None, Some(path)) => {
                let path = path.trim_matches('/');
                match categories.by_path(path) {
                    Some(term) => Some(term.clone()),
                    None => {
                        // The leaf's slug alone, or under the wrong parents: WordPress queries
                        // the leaf, then `redirect_canonical` sends it to its link.
                        let leaf = path.rsplit('/').next().unwrap_or(path);
                        return Ok(match categories.by_slug(leaf) {
                            Some(term) => Resolved::Redirect {
                                location: self.list_link(
                                    &self.category_prefix(),
                                    &categories.path(term),
                                    paged,
                                ),
                                status: 301,
                            },
                            None => Resolved::NotFound,
                        });
                    }
                }
            }
            (None, None) => None,
        };
        let Some(term) = term else {
            return Ok(Resolved::NotFound);
        };
        let listing = Listing {
            term_taxonomy_ids: Some(categories.with_descendants(&term)),
            ..Listing::default()
        };
        let count = listing.count(self.db).await?;
        Ok(self.list(Archive::Category(term), count, paged, true))
    }

    /// A date archive from `year`, `monthnum`, `day` or `m` (`YYYY[MM[DD]]`), in the site's time
    /// zone. Not found when the date is not valid or has no posts.
    async fn date(&self, vars: &Vars, paged: u32) -> Result<Resolved, Error> {
        let (mut year, mut month, mut day) = (vars.year, vars.monthnum, vars.day);
        if let Some(m) = &vars.m {
            let digits: String = m.chars().filter(char::is_ascii_digit).collect();
            let part = |from: usize, to: usize| digits.get(from..to).and_then(|s| s.parse().ok());
            year = part(0, 4).map(|y: u32| y as i32).or(year);
            month = part(4, 6).or(month);
            day = part(6, 8).or(day);
        }
        let Some(year) = year else {
            return Ok(Resolved::NotFound);
        };
        let Some(range) = self.site.date_range(year, month, day) else {
            return Ok(Resolved::NotFound);
        };
        let listing = Listing {
            range: Some(range),
            ..Listing::default()
        };
        let count = listing.count(self.db).await?;
        Ok(self.list(Archive::Date { year, month, day }, count, paged, false))
    }

    /// The site's offset as a duration, to turn local dates to UTC.
    fn offset(&self) -> Duration {
        self.site.offset_duration()
    }

    /// Whether `post`'s local publish date matches the date parts in the URL.
    fn dates_match(&self, vars: &Vars, post: &Post) -> bool {
        let Some(date) = post
            .published_at
            .unwrap_or(post.modified_at)
            .checked_add_signed(self.offset())
        else {
            return false;
        };
        vars.year.is_none_or(|y| y == date.year())
            && vars.monthnum.is_none_or(|m| m == date.month())
            && vars.day.is_none_or(|d| d == date.day())
            && vars.hour.is_none_or(|h| h == date.hour())
            && vars.minute.is_none_or(|m| m == date.minute())
            && vars.second.is_none_or(|s| s == date.second())
    }

    /// Whether the anonymous visitor may read `post`; see [`visitor_can_read`].
    async fn can_read(&self, post: &Post) -> Result<bool, Error> {
        visitor_can_read(self.db, post).await
    }

    /// Post `id` of `post_type`, when the visitor may read it.
    async fn visible_of_type(&self, id: i64, post_type: &str) -> Result<Option<Post>, Error> {
        match Post::of_type(post_type)
            .where_eq(Post::PRIMARY_KEY, id)
            .first(self.db)
            .await?
        {
            Some(post) if post.post_type == post_type && self.can_read(&post).await? => {
                Ok(Some(post))
            }
            _ => Ok(None),
        }
    }

    /// Whether `post` once had exactly `slug` as an old slug (`_wp_old_slug`), compared here, not
    /// by the database's collation.
    async fn had_slug(&self, post: &Post, slug: &str) -> Result<bool, Error> {
        let values = match get_post_meta_values(self.db, post.id, "_wp_old_slug").await {
            Ok(values) => values,
            Err(MetaError::InvalidJson(_)) => return Ok(false),
            Err(MetaError::Database(e)) => return Err(e),
        };
        Ok(values.iter().any(|v| v.as_str() == Some(slug)))
    }

    /// The page at `path` (`about/team`, decoded), as `get_page_by_path`: the page with the
    /// last slug whose parents have the slugs before it, the first one a top-level page. A
    /// published page wins over another status at the same path. Trashed pages and auto-drafts
    /// are left out. Slugs are compared exactly, whatever the database's collation.
    async fn page_by_path(&self, path: &str) -> Result<Option<Post>, Error> {
        let parts: Vec<String> = path
            .split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        let Some(last) = parts.last() else {
            return Ok(None);
        };
        if !within_limits(path) {
            return Ok(None);
        }
        let mut slugs = parts.clone();
        slugs.sort();
        slugs.dedup();
        let pages: Vec<Post> = Post::of_type("page")
            .where_in("slug", slugs)
            .get(self.db)
            .await?
            .into_iter()
            .filter(|p| {
                p.post_type == "page"
                    && !matches!(p.status, PostStatus::Trash | PostStatus::AutoDraft)
            })
            .collect();
        let by_id: HashMap<i64, &Post> = pages.iter().map(|p| (p.id, p)).collect();
        let matches_path = |page: &Post| {
            let mut current = page;
            for (i, slug) in parts.iter().enumerate().rev() {
                if current.slug != *slug {
                    return false;
                }
                match (i, current.parent_id) {
                    (0, None) => return true,
                    (0, Some(_)) | (_, None) => return false,
                    (_, Some(parent)) => match by_id.get(&parent) {
                        Some(p) => current = p,
                        None => return false,
                    },
                }
            }
            false
        };
        let mut found: Vec<&Post> = pages
            .iter()
            .filter(|p| p.slug == *last && matches_path(p))
            .collect();
        found.sort_by_key(|p| (p.status != PostStatus::Publish, p.id));
        Ok(found.first().map(|p| (*p).clone()))
    }

    /// Whether `post` has the category at `path` (`news/local`), as `redirect_canonical`
    /// checks a `%category%` link with `get_category_by_path` and `has_term`.
    async fn in_category(&self, post: &Post, path: &str) -> Result<bool, Error> {
        let categories = Categories::load(self.db).await?;
        let Some(term) = categories.by_path(path.trim_matches('/')) else {
            return Ok(false);
        };
        Ok(post
            .terms(self.db, &Taxonomy::CATEGORY)
            .await?
            .iter()
            .any(|t| t.term_taxonomy_id == term.term_taxonomy_id))
    }

    /// Whether `post`'s permalink can be rendered: not a post under a structure with
    /// `%author%`, whose nicename is not known yet (Task 8.1).
    fn can_link(&self, post: &Post) -> bool {
        post.post_type != "post" || !self.structure.tags().contains(&Tag::Author)
    }

    /// A 301 to `post`'s permalink.
    async fn redirect_to(&self, post: &Post) -> Result<Resolved, Error> {
        Ok(Resolved::Redirect {
            location: self.link(post).await?,
            status: 301,
        })
    }

    /// `post`'s permalink as a path from the root; see [`links`](Self::links).
    async fn link(&self, post: &Post) -> Result<String, Error> {
        Ok(self
            .links(std::slice::from_ref(post))
            .await?
            .pop()
            .expect("one link for one post"))
    }

    /// The permalinks of `posts` as paths from the root, with the values their tags need: each
    /// page's ancestors and each post's first category (by id, or the default category), as
    /// `get_permalink` takes them. The author's nicename is not known yet.
    ///
    /// Read in batches, whatever the number of posts: the categories once, the posts'
    /// categories in one query, and the ancestors one level of parents per query (at most
    /// [`MAX_ANCESTOR_LEVELS`]), each per [`IN_CHUNK`] ids.
    async fn links(&self, posts: &[Post]) -> Result<Vec<String>, Error> {
        let ancestors = ancestors_of(self.db, posts).await?;
        let with_category: Vec<i64> = if self.structure.tags().contains(&Tag::Category) {
            posts
                .iter()
                .filter(|p| p.post_type == "post")
                .map(|p| p.id)
                .collect()
        } else {
            Vec::new()
        };
        let (categories, first) = if with_category.is_empty() {
            (None, HashMap::new())
        } else {
            count_link_query();
            (
                Some(Categories::load(self.db).await?),
                first_categories(self.db, &with_category).await?,
            )
        };
        Ok(posts
            .iter()
            .map(|post| {
                let mut ctx = PermalinkContext {
                    utc_offset: self.site.offset,
                    ancestors: ancestors.get(&post.id).cloned().unwrap_or_default(),
                    ..PermalinkContext::new("")
                };
                if let Some(categories) = &categories
                    && post.post_type == "post"
                {
                    let term = first
                        .get(&post.id)
                        .and_then(|&id| categories.by_term_id(id))
                        .or_else(|| {
                            self.site
                                .default_category
                                .and_then(|id| categories.by_term_id(id))
                        });
                    ctx.category = term.map(|t| categories.path(t));
                }
                permalink(self.structure, post, &ctx)
            })
            .collect())
    }

    /// The link of a list at `prefix/path`, with `page/N` for a later page, ending with a slash
    /// when the structure does.
    fn list_link(&self, prefix: &str, path: &str, paged: u32) -> String {
        let mut link = format!("/{prefix}/{path}");
        if paged > 1 {
            link.push_str(&format!("/page/{paged}"));
        }
        if self.structure.uses_trailing_slash() {
            link.push('/');
        }
        link
    }
}

/// The site's categories, for paths and children.
struct Categories {
    terms: Vec<Term>,
}

impl Categories {
    async fn load(db: &Database) -> Result<Self, Error> {
        Ok(Self {
            terms: Term::all(db, &Taxonomy::CATEGORY).await?,
        })
    }

    fn by_term_id(&self, term_id: i64) -> Option<&Term> {
        self.terms.iter().find(|t| t.term_id == term_id)
    }

    fn by_slug(&self, slug: &str) -> Option<&Term> {
        self.terms.iter().find(|t| t.slug == slug)
    }

    /// The slugs of `term`'s ancestors and its own, joined by `/`, as `get_category_parents`.
    fn path(&self, term: &Term) -> String {
        let mut slugs = vec![term.slug.as_str()];
        let mut seen = vec![term.term_id];
        let mut next = term.parent_id;
        while let Some(id) = next {
            let Some(parent) = self.by_term_id(id).filter(|_| !seen.contains(&id)) else {
                break;
            };
            seen.push(id);
            slugs.push(&parent.slug);
            next = parent.parent_id;
        }
        slugs.reverse();
        slugs.join("/")
    }

    /// The category whose full path is `path` (decoded), as `get_category_by_path` with
    /// `full_match`.
    fn by_path(&self, path: &str) -> Option<&Term> {
        let leaf = path.rsplit('/').next()?;
        self.terms
            .iter()
            .find(|t| t.slug == leaf && self.path(t) == path)
    }

    /// The `term_taxonomy` ids of `term` and every category under it, as `include_children`.
    fn with_descendants(&self, term: &Term) -> Vec<i64> {
        let mut ids = vec![term.term_taxonomy_id];
        let mut parents = vec![term.term_id];
        while let Some(parent) = parents.pop() {
            for child in self.terms.iter().filter(|t| t.parent_id == Some(parent)) {
                if !ids.contains(&child.term_taxonomy_id) {
                    ids.push(child.term_taxonomy_id);
                    parents.push(child.term_id);
                }
            }
        }
        ids
    }
}

/// Each post's category with the lowest term id (the one `get_permalink` takes), by post id,
/// read in one query per [`IN_CHUNK`] posts.
async fn first_categories(db: &Database, post_ids: &[i64]) -> Result<HashMap<i64, i64>, Error> {
    let mut first: HashMap<i64, i64> = HashMap::new();
    for chunk in post_ids.chunks(IN_CHUNK) {
        count_link_query();
        let marks = vec!["?"; chunk.len()].join(", ");
        let mut binds = vec![Sql::from(&*Taxonomy::CATEGORY.name)];
        binds.extend(chunk.iter().map(|&id| Sql::from(id)));
        let rows: Vec<(i64, i64)> = raw::fetch_all(
            db,
            &format!(
                "SELECT r.object_id, tt.term_id FROM term_relationships r \
                 INNER JOIN term_taxonomy tt ON tt.id = r.term_taxonomy_id \
                 WHERE tt.taxonomy = ? AND r.object_id IN ({marks})"
            ),
            &binds,
        )
        .await?;
        for (post, term) in rows {
            let entry = first.entry(post).or_insert(term);
            *entry = (*entry).min(term);
        }
    }
    Ok(first)
}

/// The slugs of each post's ancestors, the top one first, by post id, as
/// [`page_ancestors`](crate::content::page_ancestors) gives them for one post: parents are read
/// one level per query for every post at once, and a cycle or a missing parent ends a chain.
async fn ancestors_of(db: &Database, posts: &[Post]) -> Result<HashMap<i64, Vec<String>>, Error> {
    let mut known: HashMap<i64, (String, Option<i64>)> = posts
        .iter()
        .map(|p| (p.id, (p.slug.clone(), p.parent_id)))
        .collect();
    let mut asked: std::collections::HashSet<i64> = known.keys().copied().collect();
    let mut wanted: Vec<i64> = posts.iter().filter_map(|p| p.parent_id).collect();
    for _ in 0..MAX_ANCESTOR_LEVELS {
        wanted.retain(|id| asked.insert(*id));
        if wanted.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for chunk in wanted.chunks(IN_CHUNK) {
            count_link_query();
            for parent in Post::query()
                .where_in(Post::PRIMARY_KEY, chunk.to_vec())
                .get(db)
                .await?
            {
                next.extend(parent.parent_id);
                known.insert(parent.id, (parent.slug, parent.parent_id));
            }
        }
        wanted = next;
    }
    Ok(posts
        .iter()
        .map(|post| {
            let mut slugs = Vec::new();
            let mut seen = vec![post.id];
            let mut next = post.parent_id;
            while let Some(id) = next {
                if seen.contains(&id) {
                    break;
                }
                seen.push(id);
                let Some((slug, parent)) = known.get(&id) else {
                    break;
                };
                slugs.push(slug.clone());
                next = *parent;
            }
            slugs.reverse();
            (post.id, slugs)
        })
        .collect())
}

/// The published posts a list shows, counted to tell an empty page from a page of results.
struct Listing<'a> {
    /// The post types; `post` unless set.
    post_types: &'a [&'a str],
    /// Published between these UTC times, the end excluded.
    range: Option<(NaiveDateTime, NaiveDateTime)>,
    /// In one of these terms.
    term_taxonomy_ids: Option<Vec<i64>>,
    /// By this author.
    author: Option<i64>,
    /// Matching these search terms.
    search: Vec<SearchTerm>,
}

impl Default for Listing<'_> {
    fn default() -> Self {
        Self {
            post_types: &["post"],
            range: None,
            term_taxonomy_ids: None,
            author: None,
            search: Vec::new(),
        }
    }
}

impl Listing<'_> {
    /// The `FROM` and `WHERE` clauses, with their binds, or `None` when the list is empty
    /// whatever the posts (an empty term list).
    fn filter(&self) -> Option<(String, Vec<Sql>)> {
        let mut binds: Vec<Sql> = vec![PostStatus::Publish.into()];
        let types = vec!["?"; self.post_types.len()].join(", ");
        binds.extend(self.post_types.iter().map(|t| Sql::from(*t)));
        let mut sql = format!("FROM posts p WHERE p.status = ? AND p.post_type IN ({types})");
        if let Some((from, to)) = self.range {
            sql.push_str(" AND p.published_at >= ? AND p.published_at < ?");
            binds.push(from.into());
            binds.push(to.into());
        }
        if let Some(ids) = &self.term_taxonomy_ids {
            if ids.is_empty() {
                return None;
            }
            let marks = vec!["?"; ids.len()].join(", ");
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM term_relationships r \
                 WHERE r.object_id = p.id AND r.term_taxonomy_id IN ({marks}))"
            ));
            binds.extend(ids.iter().map(|id| Sql::from(*id)));
        }
        if let Some(author) = self.author {
            sql.push_str(" AND p.author_id = ?");
            binds.push(author.into());
        }
        for term in &self.search {
            let like = format!("%{}%", escape_like(&term.text.to_lowercase()));
            let columns = "(LOWER(p.title) LIKE ? ESCAPE '!' OR LOWER(p.excerpt) LIKE ? \
                 ESCAPE '!' OR LOWER(p.content) LIKE ? ESCAPE '!')";
            sql.push_str(if term.exclude { " AND NOT " } else { " AND " });
            sql.push_str(columns);
            binds.extend(std::iter::repeat_n(Sql::from(like), 3));
        }
        // `WP_Query::parse_search`: the visitor is never logged in here, so a search never
        // matches a password-protected post, whose hidden words could otherwise be probed.
        if !self.search.is_empty() {
            sql.push_str(" AND (p.password IS NULL OR p.password = '')");
        }
        Some((sql, binds))
    }

    /// How many published posts the list has.
    async fn count(&self, db: &Database) -> Result<i64, Error> {
        let Some((filter, binds)) = self.filter() else {
            return Ok(0);
        };
        raw::fetch_scalar(db, &format!("SELECT COUNT(*) {filter}"), &binds).await
    }

    /// The posts of page `paged` of the list, newest first (`post_date DESC`, then the id, so
    /// posts published in the same second keep one order), `per_page` to a page; every post
    /// when `per_page` is `None`.
    async fn fetch(
        &self,
        db: &Database,
        per_page: Option<i64>,
        paged: u32,
    ) -> Result<Vec<Post>, Error> {
        let Some((filter, mut binds)) = self.filter() else {
            return Ok(Vec::new());
        };
        let mut sql = format!("SELECT p.id {filter} ORDER BY p.published_at DESC, p.id DESC");
        if let Some(per_page) = per_page {
            sql.push_str(" LIMIT ? OFFSET ?");
            binds.push(per_page.into());
            binds.push(i64::from(paged.max(1) - 1).saturating_mul(per_page).into());
        }
        let ids: Vec<(i64,)> = raw::fetch_all(db, &sql, &binds).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
        let mut posts = Post::query()
            .where_in(Post::PRIMARY_KEY, ids.clone())
            .get(db)
            .await?;
        posts.sort_by_key(|post| ids.iter().position(|id| *id == post.id));
        Ok(posts)
    }
}

/// `date` within what every database's datetime column holds (MySQL's `DATETIME` is
/// 1000-01-01 to 9999-12-31 23:59:59), so a bind is never out of range. A bound clamped to the
/// top leaves out posts in the last second of 9999.
fn storable(date: NaiveDateTime) -> NaiveDateTime {
    let min = NaiveDate::from_ymd_opt(1000, 1, 1)
        .expect("a valid date")
        .and_time(chrono::NaiveTime::MIN);
    let max = NaiveDate::from_ymd_opt(9999, 12, 31)
        .expect("a valid date")
        .and_hms_opt(23, 59, 59)
        .expect("a valid time");
    date.clamp(min, max)
}

/// `text` with `!`, `%` and `_` escaped by `!`, for `LIKE ... ESCAPE '!'`, which reads alike on
/// every database (a backslash does not).
pub(crate) fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '!' | '%' | '_') {
            escaped.push('!');
        }
        escaped.push(c);
    }
    escaped
}

/// One term of a search.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SearchTerm {
    /// The words to match.
    pub(crate) text: String,
    /// A term that starts with `-`: posts must not match it.
    pub(crate) exclude: bool,
}

/// WordPress's stop words for search (`wp_search_stopwords`, in English).
const STOPWORDS: [&str; 28] = [
    "about", "an", "are", "as", "at", "be", "by", "com", "for", "from", "how", "in", "is", "it",
    "of", "on", "or", "that", "the", "this", "to", "was", "what", "when", "where", "who", "will",
    "with",
];

/// The terms of search `s`, as `WP_Query::parse_search` splits them: quoted phrases and words
/// split on spaces, tabs, commas and `+`; stop words and single letters dropped; the whole
/// string as one term when nothing is left or there are more than nine.
pub(crate) fn search_terms(s: &str) -> Vec<SearchTerm> {
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let mut raw_terms = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        rest = rest.trim_start_matches([' ', '\t', ',', '+']);
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            raw_terms.push(&quoted[..end]);
            rest = quoted.get(end + 1..).unwrap_or("");
        } else if !rest.is_empty() {
            let end = rest.find([' ', '\t', ',', '+', '"']).unwrap_or(rest.len());
            raw_terms.push(&rest[..end]);
            rest = &rest[end..];
        }
    }
    let mut terms: Vec<SearchTerm> = raw_terms
        .into_iter()
        .map(|t| t.trim_matches(['"', '\'']))
        .filter(|t| {
            !t.is_empty()
                && !(t.len() == 1 && t.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-'))
                && !STOPWORDS.contains(&t.to_lowercase().as_str())
        })
        .map(|t| match t.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => SearchTerm {
                text: rest.to_owned(),
                exclude: true,
            },
            _ => SearchTerm {
                text: t.to_owned(),
                exclude: false,
            },
        })
        .collect();
    if terms.is_empty() || terms.len() > 9 {
        terms = vec![SearchTerm {
            text: s.to_owned(),
            exclude: false,
        }];
    }
    terms
}

/// `rel` without a trailing `page/N` (WordPress's paged rules), and `N`.
fn split_paged(rel: &str) -> (&str, Option<u32>) {
    let (head, last) = rel.rsplit_once('/').unwrap_or(("", rel));
    let (base, word) = head.rsplit_once('/').unwrap_or(("", head));
    if word == "page"
        && last.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = last.parse()
    {
        return (base, Some(n));
    }
    (rel, None)
}

/// What follows `prefix/` in `path`, when it is not empty.
fn strip_dir<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
    path.strip_prefix(prefix)?
        .strip_prefix('/')
        .filter(|rest| !rest.is_empty())
}

/// The variables of a post permalink's captures, or `None` when a number does not fit.
fn post_vars(mut vars: Vars, captures: Vec<(Tag, String)>) -> Option<Vars> {
    for (tag, value) in captures {
        match tag {
            Tag::Year => vars.year = Some(value.parse().ok()?),
            Tag::Monthnum => vars.monthnum = Some(value.parse().ok()?),
            Tag::Day => vars.day = Some(value.parse().ok()?),
            Tag::Hour => vars.hour = Some(value.parse().ok()?),
            Tag::Minute => vars.minute = Some(value.parse().ok()?),
            Tag::Second => vars.second = Some(value.parse().ok()?),
            Tag::PostId => vars.p = Some(value.parse().ok()?),
            Tag::Postname => vars.name = Some(decode(&value, false)),
            Tag::Category => vars.category_name = Some(decode(&value, false)),
            Tag::Author => vars.author_name = Some(decode(&value, false)),
        }
    }
    Some(vars)
}

/// Matches `input` (no slashes at the ends) against a post structure, as its rewrite rule does:
/// `%year%` four digits, the other date tags one or two, `%post_id%` digits, `%postname%` and
/// `%author%` a path segment, `%category%` one or more segments. Returns each tag's text.
fn match_structure(segments: &[Segment], input: &str) -> Option<Vec<(Tag, String)>> {
    let mut pattern: Vec<Segment> = segments.to_vec();
    if let Some(Segment::Literal(first)) = pattern.first_mut() {
        *first = first.trim_start_matches('/').to_owned();
    }
    if let Some(Segment::Literal(last)) = pattern.last_mut() {
        *last = last.trim_end_matches('/').to_owned();
    }
    let mut captures = Vec::new();
    match_from(&pattern, input, &mut captures).then_some(captures)
}

/// Backtracking: whether `pattern` matches all of `input`, pushing the captures.
fn match_from(pattern: &[Segment], input: &str, captures: &mut Vec<(Tag, String)>) -> bool {
    let Some((segment, rest)) = pattern.split_first() else {
        return input.is_empty();
    };
    let tag = match segment {
        Segment::Literal(text) => {
            return input
                .strip_prefix(text.as_str())
                .is_some_and(|after| match_from(rest, after, captures));
        }
        Segment::Tag(tag) => *tag,
    };
    // The lengths the tag may take, in the order a regex tries them: greedy for runs of
    // digits and segments, lazy for `%category%` (`(.+?)`).
    let run = run_length(tag, input);
    let mut ends: Vec<usize> = input[..run]
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .collect();
    if tag != Tag::Category {
        ends.reverse();
    }
    for end in ends {
        let value = &input[..end];
        if !complete(tag, value) {
            continue;
        }
        captures.push((tag, value.to_owned()));
        if match_from(rest, &input[end..], captures) {
            return true;
        }
        captures.pop();
    }
    false
}

/// The length in bytes of the longest start of `input` the tag's pattern can take, found in one
/// pass: every shorter start (at a character boundary) fits too.
fn run_length(tag: Tag, input: &str) -> usize {
    let digits = || input.bytes().take_while(u8::is_ascii_digit).count();
    match tag {
        Tag::Year => digits().min(4),
        Tag::Monthnum | Tag::Day | Tag::Hour | Tag::Minute | Tag::Second => digits().min(2),
        Tag::PostId => digits(),
        Tag::Postname | Tag::Author => input.find('/').unwrap_or(input.len()),
        Tag::Category => input.len(),
    }
}

/// Whether `value` is a whole match of the tag.
fn complete(tag: Tag, value: &str) -> bool {
    match tag {
        Tag::Year => value.len() == 4,
        Tag::Category => !value.starts_with('/') && !value.ends_with('/'),
        _ => !value.is_empty(),
    }
}

/// `text` with `%XX` decoded, and `+` as a space when `plus` (query strings and search
/// paths). Bytes that are not UTF-8 become U+FFFD.
pub(crate) fn decode(text: &str, plus: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match bytes[i] {
            b'%' => {
                match (
                    bytes.get(i + 1).copied().and_then(hex),
                    bytes.get(i + 2).copied().and_then(hex),
                ) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            b'+' if plus => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captures(structure: &str, input: &str) -> Option<Vec<(Tag, String)>> {
        let s: PermalinkStructure = structure.parse().unwrap();
        match_structure(s.segments(), input)
    }

    #[test]
    fn structures_match_with_wordpress_rule_patterns() {
        assert_eq!(
            captures("/%year%/%monthnum%/%postname%/", "2026/3/hello"),
            Some(vec![
                (Tag::Year, "2026".into()),
                (Tag::Monthnum, "3".into()),
                (Tag::Postname, "hello".into()),
            ])
        );
        assert_eq!(captures("/%year%/%postname%/", "226/hello"), None);
        assert_eq!(captures("/%postname%/", "a/b"), None);
        assert_eq!(
            captures("/%category%/%postname%/", "news/local/hello"),
            Some(vec![
                (Tag::Category, "news/local".into()),
                (Tag::Postname, "hello".into()),
            ])
        );
        assert_eq!(
            captures("/%post_id%-%postname%/", "42-hello-world"),
            Some(vec![
                (Tag::PostId, "42".into()),
                (Tag::Postname, "hello-world".into()),
            ])
        );
        assert_eq!(
            captures("/%postname%.html", "hello.html"),
            Some(vec![(Tag::Postname, "hello".into())])
        );
        assert_eq!(captures("/archives/%post_id%", "archives/x"), None);
    }

    #[test]
    fn paged_suffixes_split_off() {
        assert_eq!(split_paged("page/2"), ("", Some(2)));
        assert_eq!(
            split_paged("category/news/page/3"),
            ("category/news", Some(3))
        );
        assert_eq!(split_paged("my-page/2"), ("my-page/2", None));
        assert_eq!(split_paged("page"), ("page", None));
    }

    #[test]
    fn search_terms_split_as_parse_search() {
        let texts = |s| -> Vec<(String, bool)> {
            search_terms(s)
                .into_iter()
                .map(|t| (t.text, t.exclude))
                .collect()
        };
        assert_eq!(
            texts("the rust \"web server\" -php a"),
            [
                ("rust".to_owned(), false),
                ("web server".to_owned(), false),
                ("php".to_owned(), true),
            ]
        );
        assert_eq!(texts("the a"), [("the a".to_owned(), false)]);
        assert!(search_terms("  ").is_empty());
    }

    #[test]
    fn decoding_follows_urldecode() {
        assert_eq!(decode("caf%C3%A9+x", true), "café x");
        assert_eq!(decode("a+b", false), "a+b");
        assert_eq!(decode("100%", true), "100%");
        assert_eq!(decode("%zz", true), "%zz");
    }

    #[test]
    fn escape_like_escapes_wildcards() {
        assert_eq!(escape_like("50%_off!"), "50!%!_off!!");
    }

    /// A fresh SQLite database with the core tables, a stand-in `users` table and an author.
    async fn link_site() -> (tempfile::TempDir, Database, i64) {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display());
        let db = Database::connect(&url).await.unwrap();
        let users = orbit_orm::schema::create("users", |t| {
            t.id();
            t.string("name");
            t.string("email");
            t.string("password_hash");
        });
        for sql in users.to_sql(db.driver()) {
            raw::execute(&db, &sql, &[]).await.unwrap();
        }
        crate::migrations::migrator()
            .unwrap()
            .run(&db)
            .await
            .unwrap();
        raw::execute(
            &db,
            "INSERT INTO users (name, email, password_hash) VALUES ('A', 'a@x.test', 'h')",
            &[],
        )
        .await
        .unwrap();
        (dir, db, 1)
    }

    async fn new_post(
        db: &Database,
        author: i64,
        post_type: &str,
        slug: &str,
        parent: Option<i64>,
    ) -> Post {
        Post::create(
            db,
            crate::content::NewPost {
                status: PostStatus::Publish,
                title: slug.to_owned(),
                slug: slug.to_owned(),
                parent_id: parent,
                published_at: Some(
                    NaiveDate::from_ymd_opt(2026, 3, 7)
                        .unwrap()
                        .and_hms_opt(8, 0, 0)
                        .unwrap(),
                ),
                ..crate::content::NewPost::new(author, post_type)
            },
        )
        .await
        .unwrap()
    }

    /// The link-path queries `post_links` runs for `posts`, and the links.
    async fn counted_links(
        db: &Database,
        structure: &PermalinkStructure,
        posts: &[Post],
    ) -> (usize, Vec<String>) {
        LINK_QUERIES.with(|n| n.set(0));
        let links = post_links(db, structure, posts).await.unwrap();
        (LINK_QUERIES.with(std::cell::Cell::get), links)
    }

    #[tokio::test]
    async fn post_links_read_categories_and_ancestors_in_batches() {
        let (_dir, db, author) = link_site().await;
        let news = crate::content::Term::create(
            &db,
            &Taxonomy::CATEGORY,
            crate::content::NewTerm::new("News"),
        )
        .await
        .unwrap();
        let local = crate::content::Term::create(
            &db,
            &Taxonomy::CATEGORY,
            crate::content::NewTerm {
                parent_id: Some(news.term_id),
                ..crate::content::NewTerm::new("Local")
            },
        )
        .await
        .unwrap();
        let mut posts = Vec::new();
        for i in 0..9 {
            let post = new_post(&db, author, "post", &format!("post-{i}"), None).await;
            let term = if i % 2 == 0 { &news } else { &local };
            post.sync_terms(&db, &Taxonomy::CATEGORY, &[term.term_taxonomy_id])
                .await
                .unwrap();
            posts.push(post);
        }
        let top = new_post(&db, author, "page", "top", None).await;
        let mid = new_post(&db, author, "page", "mid", Some(top.id)).await;
        for i in 0..9 {
            posts.push(new_post(&db, author, "page", &format!("leaf-{i}"), Some(mid.id)).await);
        }
        let structure: PermalinkStructure = "/%category%/%postname%/".parse().unwrap();

        let few: Vec<Post> = posts[..3].iter().chain(&posts[9..12]).cloned().collect();
        let (few_queries, _) = counted_links(&db, &structure, &few).await;
        let (all_queries, links) = counted_links(&db, &structure, &posts).await;
        // One categories load, one relationships query, two levels of parents (mid, then top),
        // whatever the number of posts.
        assert_eq!(few_queries, 4);
        assert_eq!(all_queries, 4);

        assert_eq!(links[0], "/news/post-0/");
        assert_eq!(links[1], "/news/local/post-1/");
        assert_eq!(links[9], "/top/mid/leaf-0/");
        // The batched links are the links of each post on its own.
        for (post, link) in posts.iter().zip(&links) {
            let (_, one) = counted_links(&db, &structure, std::slice::from_ref(post)).await;
            assert_eq!(&one[0], link);
        }
    }
}
