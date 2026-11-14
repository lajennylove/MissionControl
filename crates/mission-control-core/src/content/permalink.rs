//! Permalinks: the structures of WordPress's Settings > Permalinks and the links they give a
//! post, after `get_permalink`, `_get_page_link` and `get_post_permalink`.
//!
//! A structure is the `permalink_structure` option: empty for plain links (`?p=42`), or a path
//! of literal text and `%tags%`, such as `/%year%/%monthnum%/%postname%/`. It is kept parsed, as
//! a list of [`Segment`]s, so the same value renders a link and, later, reads one back.
//!
//! ```ignore
//! use mission_control_core::content::{PermalinkContext, PermalinkStructure, permalink};
//!
//! let structure: PermalinkStructure = "/%year%/%postname%/".parse()?;
//! let link = permalink(&structure, &post, &PermalinkContext::new("https://example.com"));
//! // "https://example.com/2026/hello-world/"
//! ```

use std::fmt;
use std::str::FromStr;

use chrono::{FixedOffset, NaiveDateTime};
use orbit_orm::{Database, Error, Model};

use super::post::{Post, PostStatus};

/// A structure tag WordPress replaces in a permalink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tag {
    /// `%year%`: the year of the publish date, four digits.
    Year,
    /// `%monthnum%`: the month, two digits.
    Monthnum,
    /// `%day%`: the day of the month, two digits.
    Day,
    /// `%hour%`: the hour, two digits.
    Hour,
    /// `%minute%`: the minute, two digits.
    Minute,
    /// `%second%`: the second, two digits.
    Second,
    /// `%post_id%`: the post's id.
    PostId,
    /// `%postname%`: the post's slug.
    Postname,
    /// `%category%`: the slug path of the post's category (`news/local`).
    Category,
    /// `%author%`: the author's nicename.
    Author,
}

impl Tag {
    /// Every tag, in the order WordPress lists them.
    pub const ALL: [Self; 10] = [
        Self::Year,
        Self::Monthnum,
        Self::Day,
        Self::Hour,
        Self::Minute,
        Self::Second,
        Self::PostId,
        Self::Postname,
        Self::Category,
        Self::Author,
    ];

    /// The tag as it is written in a structure, such as `%postname%`.
    pub const fn token(self) -> &'static str {
        match self {
            Self::Year => "%year%",
            Self::Monthnum => "%monthnum%",
            Self::Day => "%day%",
            Self::Hour => "%hour%",
            Self::Minute => "%minute%",
            Self::Second => "%second%",
            Self::PostId => "%post_id%",
            Self::Postname => "%postname%",
            Self::Category => "%category%",
            Self::Author => "%author%",
        }
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

impl FromStr for Tag {
    type Err = PermalinkError;

    /// Reads a token such as `%postname%`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|t| t.token() == s)
            .ok_or_else(|| PermalinkError::UnknownTag(s.to_owned()))
    }
}

/// A piece of a permalink structure: literal text or a tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Segment {
    /// Text kept as it is, such as `/archives/`.
    Literal(String),
    /// A tag replaced by the post's value.
    Tag(Tag),
}

/// Why a permalink structure was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermalinkError {
    /// A `%tag%` that is not one of WordPress's structure tags.
    UnknownTag(String),
    /// A `%` with no closing `%`.
    Unclosed,
    /// A structure that is not empty has no tag, which WordPress refuses: "A structure tag is
    /// required when using custom permalinks."
    NoTag,
}

impl fmt::Display for PermalinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTag(tag) => write!(f, "{tag} is not a permalink structure tag"),
            Self::Unclosed => f.write_str("a structure tag is not closed with %"),
            Self::NoTag => f.write_str("a structure tag is required when using custom permalinks"),
        }
    }
}

impl std::error::Error for PermalinkError {}

/// The `permalink_structure` option, parsed. Empty is plain links.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct PermalinkStructure {
    segments: Vec<Segment>,
}

impl PermalinkStructure {
    /// Plain: `/?p=123`.
    pub fn plain() -> Self {
        Self::default()
    }

    /// Day and name: `/%year%/%monthnum%/%day%/%postname%/`.
    pub fn day_and_name() -> Self {
        Self::preset("/%year%/%monthnum%/%day%/%postname%/")
    }

    /// Month and name: `/%year%/%monthnum%/%postname%/`.
    pub fn month_and_name() -> Self {
        Self::preset("/%year%/%monthnum%/%postname%/")
    }

    /// Numeric: `/archives/%post_id%`.
    pub fn numeric() -> Self {
        Self::preset("/archives/%post_id%")
    }

    /// Post name: `/%postname%/`.
    pub fn post_name() -> Self {
        Self::preset("/%postname%/")
    }

    fn preset(text: &str) -> Self {
        text.parse().expect("a preset structure parses")
    }

    /// Whether this is the plain structure, with no path.
    pub fn is_plain(&self) -> bool {
        self.segments.is_empty()
    }

    /// The literal text and tags, in order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The tags, in order.
    pub fn tags(&self) -> Vec<Tag> {
        self.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Tag(t) => Some(*t),
                Segment::Literal(_) => None,
            })
            .collect()
    }

    /// The literal text before the first tag, as `$wp_rewrite->front`: `/archives/` for the
    /// numeric structure, `/` for the others. Custom post types' links start with it.
    pub fn front(&self) -> &str {
        match self.segments.first() {
            Some(Segment::Literal(text)) => text,
            _ => "/",
        }
    }

    /// Whether links end with a slash, as `user_trailingslashit` decides: when the structure
    /// does.
    pub fn uses_trailing_slash(&self) -> bool {
        matches!(self.segments.last(), Some(Segment::Literal(text)) if text.ends_with('/'))
    }
}

impl FromStr for PermalinkStructure {
    type Err = PermalinkError;

    /// Reads a structure as the Permalinks screen saves it: a leading slash is added and
    /// repeated slashes are folded. Empty is plain.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::plain());
        }
        let mut text = String::with_capacity(s.len() + 1);
        for c in format!("/{s}").chars() {
            if !(c == '/' && text.ends_with('/')) {
                text.push(c);
            }
        }
        let mut segments = Vec::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find('%') {
            if start > 0 {
                segments.push(Segment::Literal(rest[..start].to_owned()));
            }
            let end = rest[start + 1..]
                .find('%')
                .ok_or(PermalinkError::Unclosed)?
                + start
                + 2;
            segments.push(Segment::Tag(rest[start..end].parse()?));
            rest = &rest[end..];
        }
        if !rest.is_empty() {
            segments.push(Segment::Literal(rest.to_owned()));
        }
        if !segments.iter().any(|s| matches!(s, Segment::Tag(_))) {
            return Err(PermalinkError::NoTag);
        }
        Ok(Self { segments })
    }
}

impl fmt::Display for PermalinkStructure {
    /// The structure as the option stores it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => f.write_str(text)?,
                Segment::Tag(tag) => f.write_str(tag.token())?,
            }
        }
        Ok(())
    }
}

/// What a permalink needs besides the post: the site's address, its time zone, and the values
/// of tags that come from other rows. The caller reads them, so rendering does not query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermalinkContext {
    /// The site address (`home_url`), such as `https://example.com`. A trailing slash is ignored.
    pub base_url: String,
    /// The site's offset from UTC at the post's date: dates in links are local, as WordPress
    /// uses `post_date`. For a named time zone, pass its offset at the post's date.
    pub utc_offset: FixedOffset,
    /// The slugs of the post's ancestors, root first, for a page's path (`about/team`). See
    /// [`page_ancestors`].
    pub ancestors: Vec<String>,
    /// `%category%`: the slug path of the post's category, its ancestors first (`news/local`).
    /// WordPress uses the category with the lowest id, or the default category when the post
    /// has none. `None` renders nothing.
    pub category: Option<String>,
    /// `%author%`: the author's nicename. `None` renders nothing.
    pub author: Option<String>,
}

impl PermalinkContext {
    /// A context for `base_url` in UTC, with no ancestors, category or author.
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_owned(),
            utc_offset: FixedOffset::east_opt(0).expect("zero is a valid offset"),
            ancestors: Vec::new(),
            category: None,
            author: None,
        }
    }

    /// `path` on the site, as `home_url($path)`.
    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// The page path: the ancestors' slugs and `slug`, joined by `/`, as `get_page_uri`.
    fn path(&self, slug: &str) -> String {
        let mut parts: Vec<&str> = self.ancestors.iter().map(String::as_str).collect();
        parts.push(slug);
        parts.join("/")
    }
}

/// The link to `post` under `structure`, as WordPress's `get_permalink` gives it.
///
/// - A post (`post`) follows the structure, its tags replaced by the post's values. Dates are
///   the publish date in the context's time zone (the modification date when there is none).
/// - A page follows its path (`/about/team/`), not the structure, as `_get_page_link` does.
/// - Another type gets `{front}{post_type}/{path}`, as a post type registered with the default
///   rewrite (`with_front`) does.
/// - An attachment gets `?attachment_id=ID`.
///
/// Links end with a slash when the structure does. Under the plain structure, or when the post
/// is not public (anything but `publish` and `private`), the link is a query: `?p=ID`,
/// `?page_id=ID`, `?{post_type}={slug}` for a published custom type and `?post_type=...&p=ID`
/// otherwise. A private post gets its pretty link, as WordPress gives it to users who may read
/// it.
pub fn permalink(
    structure: &PermalinkStructure,
    post: &Post,
    context: &PermalinkContext,
) -> String {
    let public = matches!(post.status, PostStatus::Publish | PostStatus::Private);
    let pretty = public && !structure.is_plain();
    let slash = |path: String| trailing_slash(path, structure.uses_trailing_slash());
    match post.post_type.as_str() {
        "attachment" => context.url(&format!("?attachment_id={}", post.id)),
        "post" if pretty => context.url(&slash(render(structure, post, context))),
        "post" => context.url(&format!("?p={}", post.id)),
        "page" if pretty => context.url(&slash(context.path(&post.slug))),
        "page" => context.url(&format!("?page_id={}", post.id)),
        post_type if pretty => context.url(&slash(format!(
            "{}{post_type}/{}",
            structure.front(),
            context.path(&post.slug)
        ))),
        post_type if public => context.url(&format!("?{post_type}={}", context.path(&post.slug))),
        post_type => context.url(&format!("?post_type={post_type}&p={}", post.id)),
    }
}

/// `structure` with its tags replaced by `post`'s values.
fn render(structure: &PermalinkStructure, post: &Post, context: &PermalinkContext) -> String {
    let date = local_date(
        post.published_at.unwrap_or(post.modified_at),
        context.utc_offset,
    );
    let mut path = String::new();
    for segment in structure.segments() {
        match segment {
            Segment::Literal(text) => path.push_str(text),
            Segment::Tag(tag) => path.push_str(&match tag {
                Tag::Year => date.format("%Y").to_string(),
                Tag::Monthnum => date.format("%m").to_string(),
                Tag::Day => date.format("%d").to_string(),
                Tag::Hour => date.format("%H").to_string(),
                Tag::Minute => date.format("%M").to_string(),
                Tag::Second => date.format("%S").to_string(),
                Tag::PostId => post.id.to_string(),
                Tag::Postname => post.slug.clone(),
                Tag::Category => context.category.clone().unwrap_or_default(),
                Tag::Author => context.author.clone().unwrap_or_default(),
            }),
        }
    }
    path
}

/// `utc` in the time zone at `offset`.
fn local_date(utc: NaiveDateTime, offset: FixedOffset) -> NaiveDateTime {
    utc + offset
}

/// `path` with one trailing slash when `slash`, and none otherwise, as `user_trailingslashit`.
fn trailing_slash(path: String, slash: bool) -> String {
    let trimmed = path.trim_end_matches('/');
    if slash {
        format!("{trimmed}/")
    } else {
        trimmed.to_owned()
    }
}

/// The slugs of `post`'s ancestors, root first, read by following `parent_id`, as WordPress's
/// `get_post_ancestors` (reversed) for `get_page_uri`. Stops at a missing parent, and at a loop.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn page_ancestors(db: &Database, post: &Post) -> Result<Vec<String>, Error> {
    let mut slugs = Vec::new();
    let mut seen = vec![post.id];
    let mut next = post.parent_id;
    while let Some(id) = next {
        if seen.contains(&id) {
            break;
        }
        seen.push(id);
        let Some(parent) = Post::query().find(db, id).await? else {
            break;
        };
        slugs.push(parent.slug);
        next = parent.parent_id;
    }
    slugs.reverse();
    Ok(slugs)
}
