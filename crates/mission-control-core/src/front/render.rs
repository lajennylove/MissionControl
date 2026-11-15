//! Rendering a request with the active theme, as WordPress's `template-loader.php` does after
//! `WP::main`: resolve the URL, run the main query, pick the template from the hierarchy, and
//! render it with the request's data.
//!
//! This is the part of the front end that does not depend on the web framework: the app hands
//! [`Frontend::respond`] the request's path and query string and sends back the [`Response`].
//!
//! Templates are rendered through the theme's confined loader ([`Theme::loader`]): `@extends` and
//! `@include` resolve only inside the theme and its parent. `{{ }}` escapes its output; a post's
//! content is passed through `the_content` and meant to be output raw (`{!! !!}`), as WordPress
//! echoes it. Sanitising content (WordPress's kses) belongs to the save path, so the HTML here
//! is what an author saved.
//!
//! # The active theme
//!
//! The active theme is the `stylesheet` option, as WordPress stores it. Its parent comes from
//! the theme's own `theme.toml` (`template = "parent"`), so the `template` option, which
//! WordPress keeps in step with that header, is not read (ruling R36).
//!
//! # The template context
//!
//! Every template gets the same variables (a value that does not apply is empty, `null` or
//! `false`, never missing):
//!
//! | Variable | What it holds | WordPress |
//! | --- | --- | --- |
//! | `site.name`, `site.description`, `site.url`, `site.language` | the site's title, tagline, address and language | `bloginfo` |
//! | `head` | the tags that load the theme's built CSS and JavaScript; output raw | `wp_head` |
//! | `document_title` | the `<title>` text | `wp_get_document_title` |
//! | `body_class` | the classes for `<body>` | `body_class` |
//! | `is.*` | the conditional tags: `front_page`, `home`, `singular`, `single`, `page`, `attachment`, `archive`, `category`, `tag`, `author`, `date`, `post_type_archive`, `search`, `not_found`, `paged` | `is_*` |
//! | `post` | the post of a singular request, or `null` | `$post` |
//! | `posts` | the loop's posts; the one post of a singular request | The Loop |
//! | `archive.title`, `archive.description` | an archive's heading and its term's description | `the_archive_title` |
//! | `search_query` | the search terms as typed | `get_search_query` |
//! | `pagination.*` | `current`, `total`, `has_pages`, and the `previous` (newer) and `next` (older) page links or `null` | `posts_nav_link` |
//! | `theme.slug`, `theme.name`, `theme.version` | the active theme | `wp_get_theme` |
//! | `menus.{location}` | each menu location the theme declares: its assigned [`NavMenu`] (`id`, `name`, `slug`, `items`), or `null` | `wp_nav_menu` |
//! | `sidebars.{id}` | each widget area the theme declares: `id`, `name`, `active` and its rendered `widgets` ([`SidebarView`]) | `dynamic_sidebar` |
//!
//! A post has `id`, `post_type`, `slug`, `title`, `url`, `content` (filtered by `the_content`;
//! output raw), `excerpt` (HTML-safe: an auto excerpt is tag-stripped, a manual one escaped; output raw), `date`, `datetime` (ISO 8601), `author`,
//! `classes` and `password_required`. A manual excerpt stays escaped although kses filters
//! excerpts on save ([`kses_post_fields`](crate::content::kses_post_fields)), because rows can
//! be written without kses, by an import or before it existed (Ruling R34).
//!
//! A menu item ([`NavMenuItem`]) has `id`, `title`, `url` (empty when there is no safe link),
//! `target`, `rel`, `attr_title`, `description`, `classes`, `current`, `aria_current` and
//! `children`; every value is text. A widget has `id`, `id_base`, `classes`, `title` (text) and
//! `content` (HTML with every value escaped; output raw). See [`crate::theme`]'s menus and widgets.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{FixedOffset, NaiveDateTime};
use orbit_orm::{Database, Error, Value as Sql, raw};
use propel_runtime::{AssetSource, Assets, RenderError, render_with_assets};
use serde::Serialize;

use super::route::{number, option, text};
use super::{Archive, MainQuery, Query, Resolved, post_links, query_posts, resolve};
use crate::content::{MetaError, PermalinkStructure, Post, get_post_meta};
use crate::hooks::{Hooks, THE_CONTENT};
use crate::text::{escape_html, html_class, strip_all_tags};
#[cfg(doc)]
use crate::theme::NavMenuItem;
use crate::theme::{
    NavMenu, PageRequest, SidebarView, Theme, ThemeError, ThemeLoader, load_theme, templates_for,
    theme_menus, theme_sidebars,
};

/// The theme used when the active theme cannot be loaded, as WordPress's `WP_DEFAULT_THEME`.
pub const DEFAULT_THEME: &str = "default";

/// The Vite entries the front end loads for the active theme (and its parent first), when the
/// theme's build has them: the family's `resources/css/app.css` and `resources/js/app.js`.
pub const THEME_ENTRIES: [&str; 2] = ["resources/css/app.css", "resources/js/app.js"];

/// How many words an excerpt made from the content keeps, as `excerpt_length`.
const EXCERPT_WORDS: usize = 55;

/// The site's language for `<html lang>`, until the `WPLANG` option is read.
const SITE_LANGUAGE: &str = "en-US";

/// The separator between the parts of the document title (`document_title_separator`).
const TITLE_SEPARATOR: &str = " – ";

/// What to send back for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// A rendered page: 200, or 404 for the not-found template.
    Page {
        /// The HTTP status.
        status: u16,
        /// The HTML.
        html: String,
    },
    /// A redirect, such as an old slug's 301.
    Redirect {
        /// The `Location`: the site address (the `home` option) and the path, or the path from
        /// the root when `home` is not set.
        location: String,
        /// The HTTP status.
        status: u16,
    },
}

/// Why a request could not be answered. The app answers with a 500 and logs it.
#[derive(Debug)]
pub enum FrontError {
    /// The database refused a query.
    Database(Error),
    /// The template could not be rendered: a syntax error, a refused `@include`, an undefined
    /// variable, or a render limit.
    Render(RenderError),
    /// No template of the hierarchy exists, not even `index` (it was removed after the theme was
    /// loaded).
    MissingTemplate {
        /// The active theme's slug.
        theme: String,
    },
}

impl fmt::Display for FrontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrontError::Database(e) => write!(f, "the database refused a query: {e}"),
            FrontError::Render(e) => write!(f, "the template could not be rendered: {e}"),
            FrontError::MissingTemplate { theme } => write!(
                f,
                "the theme `{theme}` has no template for this request, not even index.propel"
            ),
        }
    }
}

impl std::error::Error for FrontError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrontError::Database(e) => Some(e),
            FrontError::Render(e) => Some(e),
            FrontError::MissingTemplate { .. } => None,
        }
    }
}

impl From<Error> for FrontError {
    fn from(e: Error) -> Self {
        FrontError::Database(e)
    }
}

impl From<RenderError> for FrontError {
    fn from(e: RenderError) -> Self {
        FrontError::Render(e)
    }
}

/// Loads the active theme from `themes_dir`: the theme named by the `stylesheet` option (the
/// child theme's slug, as WordPress stores it; its parent comes from its manifest, so the
/// `template` option is not read), or [`DEFAULT_THEME`] when the option is unset.
///
/// A `stylesheet` that is not a theme slug, or names a theme that cannot be loaded, is logged
/// and the default theme is loaded instead, as WordPress's `validate_current_theme` falls back
/// to `WP_DEFAULT_THEME` (the option is not rewritten here).
///
/// # Errors
///
/// Fails with the database's error, or the default theme's error when it cannot be loaded
/// either.
pub async fn load_active_theme(
    db: &Database,
    themes_dir: &Path,
) -> Result<Theme, ActiveThemeError> {
    let stylesheet = option(db, "stylesheet")
        .await
        .map_err(ActiveThemeError::Database)?
        .as_ref()
        .and_then(text)
        .filter(|slug| !slug.is_empty());
    if let Some(slug) = stylesheet.filter(|slug| slug != DEFAULT_THEME) {
        if is_theme_slug(&slug) {
            match load_theme(&themes_dir.join(&slug)) {
                Ok(theme) => return Ok(theme),
                Err(e) => log::error!(
                    "The active theme `{slug}` could not be loaded, so the `{DEFAULT_THEME}` \
                     theme is used: {e}"
                ),
            }
        } else {
            log::error!(
                "The `stylesheet` option is not a theme slug, so the `{DEFAULT_THEME}` theme is \
                 used."
            );
        }
    }
    load_theme(&themes_dir.join(DEFAULT_THEME)).map_err(ActiveThemeError::Theme)
}

/// Why [`load_active_theme`] failed.
#[derive(Debug)]
pub enum ActiveThemeError {
    /// The database refused a query.
    Database(Error),
    /// The default theme could not be loaded.
    Theme(ThemeError),
}

impl fmt::Display for ActiveThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActiveThemeError::Database(e) => write!(f, "the database refused a query: {e}"),
            ActiveThemeError::Theme(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ActiveThemeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ActiveThemeError::Database(e) => Some(e),
            ActiveThemeError::Theme(e) => Some(e),
        }
    }
}

/// True for a non-empty run of ASCII letters, digits, `-` and `_`, so a slug read from the
/// database names a folder directly inside the themes folder.
fn is_theme_slug(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The public site with one theme: answers front-end requests.
///
/// ```ignore
/// let theme = load_active_theme(&db, Path::new("themes")).await?;
/// let frontend = Frontend::new(db, hooks, theme);
/// match frontend.respond("/hello-world/", "").await? {
///     Response::Page { status, html } => { /* send it */ }
///     Response::Redirect { location, status } => { /* redirect */ }
/// }
/// ```
pub struct Frontend {
    db: Arc<Database>,
    hooks: Arc<Hooks>,
    theme: Theme,
    loader: ThemeLoader,
    assets: Assets,
}

impl fmt::Debug for Frontend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frontend")
            .field("theme", &self.theme.slug())
            .finish_non_exhaustive()
    }
}

impl Frontend {
    /// The front end for `theme`, reading content from `db` and filtering it through `hooks`.
    ///
    /// The theme's loader is built once, so its parsed templates stay cached (Propel checks each
    /// file's modification time and length, so an edited template shows on the next request).
    /// Each theme's built assets are its `public/build` folder, served at
    /// [`asset_dirs`](Self::asset_dirs).
    pub fn new(db: Arc<Database>, hooks: Arc<Hooks>, theme: Theme) -> Self {
        let mut assets = Assets::empty();
        for theme in themes_of(&theme) {
            assets = assets.namespace(
                theme.slug(),
                AssetSource::directory(theme.dir().join("public"))
                    .url_prefix(asset_prefix(theme.slug())),
            );
        }
        Self {
            loader: theme.loader(),
            db,
            hooks,
            theme,
            assets,
        }
    }

    /// The active theme.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// The folders of built assets to serve, as (URL prefix, folder): each theme's
    /// `public/build`, at `/themes/{slug}/build`. A folder may not exist until the theme is built.
    pub fn asset_dirs(&self) -> Vec<(String, PathBuf)> {
        themes_of(&self.theme)
            .into_iter()
            .map(|theme| {
                (
                    asset_prefix(theme.slug()),
                    theme.dir().join("public").join("build"),
                )
            })
            .collect()
    }

    /// Answers the request for `path` (from the site's root, still percent-encoded) and `query`
    /// (the query string, without `?`).
    ///
    /// # Errors
    ///
    /// Fails with the database's error, or the theme's render error.
    pub async fn respond(&self, path: &str, query: &str) -> Result<Response, FrontError> {
        let db = &*self.db;
        let options = Options::load(db).await?;
        let resolved = resolve(db, path, &Query::parse(query), &options.structure).await?;
        if let Resolved::Redirect { location, status } = resolved {
            return Ok(Response::Redirect {
                location: format!("{}{location}", options.home),
                status,
            });
        }
        let main = query_posts(db, &resolved).await?;
        let page_template = match singular_post(&resolved) {
            Some(post) => page_template(db, post.id).await?,
            None => None,
        };
        let names = templates_for(&resolved, page_template.as_deref());
        let located =
            self.theme
                .locate_template(&names)
                .ok_or_else(|| FrontError::MissingTemplate {
                    theme: self.theme.slug().to_owned(),
                })?;
        let context = self
            .context(
                &options,
                &resolved,
                main,
                page_template.as_deref(),
                path,
                query,
            )
            .await?;
        let html = render_with_assets(&self.loader, &self.assets, &located.view, &context)?;
        let status = if resolved == Resolved::NotFound {
            404
        } else {
            200
        };
        Ok(Response::Page { status, html })
    }

    /// The template context for a request.
    async fn context(
        &self,
        options: &Options,
        resolved: &Resolved,
        main: MainQuery,
        page_template: Option<&str>,
        path: &str,
        query: &str,
    ) -> Result<Context, FrontError> {
        let posts = self.post_views(options, &main.posts).await?;
        let post = singular_post(resolved).and_then(|_| posts.first().cloned());
        let is = Conditionals::of(resolved);
        let archive = self.archive_view(resolved).await?;
        let search_query = match resolved {
            Resolved::Search { terms, .. } => terms.clone(),
            _ => String::new(),
        };
        let paged = paged_of(resolved);
        let pagination = if is.singular || is.not_found {
            Pagination::default()
        } else {
            let link =
                |page: u32| pagenum_link(&options.home, path, query, page, &options.structure);
            Pagination {
                current: paged,
                total: main.max_num_pages,
                has_pages: main.max_num_pages > 1,
                previous: (paged > 1).then(|| link(paged - 1)),
                next: (paged < main.max_num_pages).then(|| link(paged + 1)),
            }
        };
        let request = PageRequest {
            resolved,
            home: &options.home,
            path,
            query,
            structure: &options.structure,
            utc_offset: options.offset,
            search_query: &search_query,
        };
        let menus = theme_menus(&self.db, &self.theme, &request).await?;
        let sidebars = theme_sidebars(&self.db, &self.theme, &request)
            .await?
            .into_iter()
            .map(|sidebar| (sidebar.id.clone(), sidebar))
            .collect();
        let manifest = self.theme.manifest();
        let document_title = document_title(options, resolved, &archive);
        let body_class = body_class(resolved, main.found_posts, page_template);
        Ok(Context {
            site: SiteView {
                name: options.name.clone(),
                description: options.description.clone(),
                url: if options.home.is_empty() {
                    "/".to_owned()
                } else {
                    format!("{}/", options.home)
                },
                language: SITE_LANGUAGE.to_owned(),
            },
            head: self.head(),
            document_title,
            body_class,
            is,
            post,
            posts,
            archive,
            search_query,
            pagination,
            theme: ThemeView {
                slug: self.theme.slug().to_owned(),
                name: manifest.name.clone(),
                version: manifest.version.clone().unwrap_or_default(),
            },
            menus,
            sidebars,
        })
    }

    /// The tags that load each theme's built entries, the parent's first, as WordPress prints
    /// enqueued styles in `wp_head`. An entry the build does not have is left out, so a theme
    /// renders before `npm run build` has run.
    fn head(&self) -> String {
        let mut head = String::new();
        for theme in themes_of(&self.theme).into_iter().rev() {
            for entry in THEME_ENTRIES {
                if let Some(tags) = self.assets.tags(&[format!("{}::{entry}", theme.slug())]) {
                    head.push_str(&tags);
                }
            }
        }
        head
    }

    /// The posts as templates see them.
    async fn post_views(
        &self,
        options: &Options,
        posts: &[Post],
    ) -> Result<Vec<PostView>, FrontError> {
        if posts.is_empty() {
            return Ok(Vec::new());
        }
        let db = &*self.db;
        let links = post_links(db, &options.structure, posts).await?;
        let authors = author_names(db, posts).await?;
        Ok(posts
            .iter()
            .zip(links)
            .map(|(post, link)| {
                let password_required = post.password.as_deref().is_some_and(|p| !p.is_empty());
                let (title, content, excerpt) = if password_required {
                    (
                        format!("Protected: {}", post.title),
                        "<p class=\"post-password-required\">This content is password \
                         protected.</p>"
                            .to_owned(),
                        "There is no excerpt because this is a protected post.".to_owned(),
                    )
                } else {
                    let content = self.hooks.apply_filters(THE_CONTENT, post.content.clone());
                    let excerpt = if post.excerpt.trim().is_empty() {
                        trim_excerpt(&content)
                    } else {
                        // Ruling R34, kept now that kses runs on save
                        // (`content::kses_post_fields`): a row can be written without it (an
                        // import, a row saved before kses existed, code that skips the save
                        // helper), so a manual excerpt stays text, escaped here.
                        escape_html(&post.excerpt)
                    };
                    (post.title.clone(), content, excerpt)
                };
                let date = local(
                    post.published_at.unwrap_or(post.modified_at),
                    options.offset,
                );
                PostView {
                    id: post.id,
                    post_type: post.post_type.clone(),
                    slug: post.slug.clone(),
                    title,
                    url: format!("{}{link}", options.home),
                    content,
                    excerpt,
                    date: date.format("%B %-d, %Y").to_string(),
                    datetime: date.to_rfc3339(),
                    author: authors.get(&post.author_id).cloned().unwrap_or_default(),
                    classes: post_class(post),
                    password_required,
                }
            })
            .collect())
    }

    /// The heading and description of an archive, or empty ones.
    async fn archive_view(&self, resolved: &Resolved) -> Result<ArchiveView, FrontError> {
        let Resolved::Archive { archive, .. } = resolved else {
            return Ok(ArchiveView::default());
        };
        let (prefix, name, description) = match archive {
            Archive::Category(term) => ("Category", term.name.clone(), term.description.clone()),
            Archive::Tag(term) => ("Tag", term.name.clone(), term.description.clone()),
            Archive::Author(id) => {
                let names = author_names_by_id(&self.db, &[*id]).await?;
                (
                    "Author",
                    names.get(id).cloned().unwrap_or_default(),
                    String::new(),
                )
            }
            Archive::Date { year, month, day } => {
                let date =
                    chrono::NaiveDate::from_ymd_opt(*year, month.unwrap_or(1), day.unwrap_or(1));
                match (date, month, day) {
                    (Some(date), Some(_), Some(_)) => {
                        ("Day", date.format("%B %-d, %Y").to_string(), String::new())
                    }
                    (Some(date), Some(_), None) => {
                        ("Month", date.format("%B %Y").to_string(), String::new())
                    }
                    _ => ("Year", year.to_string(), String::new()),
                }
            }
            Archive::PostType(post_type) => ("Archives", post_type.clone(), String::new()),
        };
        Ok(ArchiveView {
            title: format!("{prefix}: {name}"),
            name,
            description,
        })
    }
}

/// The theme and its parent, the theme first.
fn themes_of(theme: &Theme) -> Vec<&Theme> {
    std::iter::once(theme).chain(theme.parent()).collect()
}

/// The URL prefix a theme's built assets are served under.
fn asset_prefix(slug: &str) -> String {
    format!("/themes/{slug}/build")
}

/// The post of a singular request.
fn singular_post(resolved: &Resolved) -> Option<&Post> {
    match resolved {
        Resolved::Single(post) | Resolved::Page(post) | Resolved::Attachment(post) => Some(post),
        Resolved::FrontPage {
            page: Some(page), ..
        } => Some(page),
        _ => None,
    }
}

/// The page of a list request; 1 otherwise.
fn paged_of(resolved: &Resolved) -> u32 {
    match resolved {
        Resolved::FrontPage { paged, .. }
        | Resolved::Home { paged, .. }
        | Resolved::Archive { paged, .. }
        | Resolved::Search { paged, .. } => *paged,
        _ => 1,
    }
}

/// The post's `_wp_page_template` meta, when it is text.
async fn page_template(db: &Database, post_id: i64) -> Result<Option<String>, FrontError> {
    match get_post_meta(db, post_id, "_wp_page_template").await {
        Ok(value) => Ok(value.as_ref().and_then(text)),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e.into()),
    }
}

/// The display names of the posts' authors, by user id.
async fn author_names(db: &Database, posts: &[Post]) -> Result<HashMap<i64, String>, FrontError> {
    let mut ids: Vec<i64> = posts.iter().map(|post| post.author_id).collect();
    ids.sort_unstable();
    ids.dedup();
    author_names_by_id(db, &ids).await
}

/// The names of users `ids`, from Ignite's `users` table.
async fn author_names_by_id(
    db: &Database,
    ids: &[i64],
) -> Result<HashMap<i64, String>, FrontError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let marks = vec!["?"; ids.len()].join(", ");
    let binds: Vec<Sql> = ids.iter().map(|id| Sql::from(*id)).collect();
    let rows: Vec<(i64, String)> = raw::fetch_all(
        db,
        &format!("SELECT id, name FROM users WHERE id IN ({marks})"),
        &binds,
    )
    .await?;
    Ok(rows.into_iter().collect())
}

/// A UTC date in the site's time zone.
fn local(utc: NaiveDateTime, offset: FixedOffset) -> chrono::DateTime<FixedOffset> {
    utc.and_utc().with_timezone(&offset)
}

/// An excerpt made from filtered content, as `wp_trim_excerpt`: the tags stripped and the first
/// [`EXCERPT_WORDS`] words kept, with ` [&hellip;]` when words were cut. The text is HTML-safe
/// (a stray `<` is `&lt;`).
fn trim_excerpt(content: &str) -> String {
    let text = strip_all_tags(content);
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() > EXCERPT_WORDS {
        format!("{} [&hellip;]", words[..EXCERPT_WORDS].join(" "))
    } else {
        words.join(" ")
    }
}

/// `post_class`: `post-{id}`, the type, `type-{type}`, `status-{status}` and `hentry`.
fn post_class(post: &Post) -> String {
    let post_type = html_class(&post.post_type);
    let mut classes = vec![
        format!("post-{}", post.id),
        post_type.clone(),
        format!("type-{post_type}"),
        format!("status-{}", post.status.as_str()),
    ];
    if post.password.as_deref().is_some_and(|p| !p.is_empty()) {
        classes.push("post-password-required".to_owned());
    }
    classes.push("hentry".to_owned());
    classes.join(" ")
}

/// The `{type}-template...` classes of a singular post, as `get_body_class`: for a chosen
/// template (`templates/full-width.php`), `{type}-template`, one class per path part without
/// `.php`, and one for the whole name with `.` as `-`; otherwise `{type}-template-default`.
fn template_classes(post_type: &str, page_template: Option<&str>) -> Vec<String> {
    let post_type = html_class(post_type);
    match page_template.filter(|t| !t.is_empty() && *t != "default") {
        Some(template) => {
            let mut classes = vec![format!("{post_type}-template")];
            for part in template.split('/') {
                let base = part.strip_suffix(".php").unwrap_or(part);
                classes.push(format!(
                    "{post_type}-template-{}",
                    html_class(&base.replace(['.', '/'], "-"))
                ));
            }
            classes.push(format!(
                "{post_type}-template-{}",
                html_class(&template.replace('.', "-"))
            ));
            classes
        }
        None => vec![format!("{post_type}-template-default")],
    }
}

/// `get_body_class`'s classes for the request.
fn body_class(resolved: &Resolved, found_posts: i64, page_template: Option<&str>) -> String {
    let mut classes: Vec<String> = Vec::new();
    let mut push = |class: String| classes.push(class);
    match resolved {
        Resolved::FrontPage { page, .. } => {
            push("home".to_owned());
            match page {
                Some(page) => {
                    template_classes("page", page_template)
                        .into_iter()
                        .for_each(&mut push);
                    push("page".to_owned());
                    push(format!("page-id-{}", page.id));
                }
                None => push("blog".to_owned()),
            }
        }
        Resolved::Home { .. } => push("blog".to_owned()),
        Resolved::Single(post) => {
            template_classes(&post.post_type, page_template)
                .into_iter()
                .for_each(&mut push);
            push("single".to_owned());
            push(format!("single-{}", html_class(&post.post_type)));
            push(format!("postid-{}", post.id));
        }
        Resolved::Page(post) => {
            template_classes("page", page_template)
                .into_iter()
                .for_each(&mut push);
            push("page".to_owned());
            push(format!("page-id-{}", post.id));
        }
        Resolved::Attachment(post) => {
            template_classes(&post.post_type, page_template)
                .into_iter()
                .for_each(&mut push);
            push("attachment".to_owned());
            push(format!("attachmentid-{}", post.id));
        }
        Resolved::Archive { archive, .. } => {
            push("archive".to_owned());
            match archive {
                Archive::Category(term) => {
                    push("category".to_owned());
                    push(format!("category-{}", html_class(&term.slug)));
                    push(format!("category-{}", term.term_id));
                }
                Archive::Tag(term) => {
                    push("tag".to_owned());
                    push(format!("tag-{}", html_class(&term.slug)));
                    push(format!("tag-{}", term.term_id));
                }
                Archive::Author(id) => {
                    push("author".to_owned());
                    push(format!("author-{id}"));
                }
                Archive::Date { .. } => push("date".to_owned()),
                Archive::PostType(post_type) => {
                    push("post-type-archive".to_owned());
                    push(format!("post-type-archive-{}", html_class(post_type)));
                }
            }
        }
        Resolved::Search { .. } => {
            push("search".to_owned());
            push(if found_posts > 0 {
                "search-results".to_owned()
            } else {
                "search-no-results".to_owned()
            });
        }
        Resolved::NotFound => push("error404".to_owned()),
        Resolved::Redirect { .. } => {}
    }
    let paged = paged_of(resolved);
    if paged > 1 {
        classes.push("paged".to_owned());
        classes.push(format!("paged-{paged}"));
    }
    classes.join(" ")
}

/// `wp_get_document_title`: the request's title, the page number from page 2, and the site's
/// name (or, on the front page, the site's name and tagline), joined by ` – `.
fn document_title(options: &Options, resolved: &Resolved, archive: &ArchiveView) -> String {
    let mut parts: Vec<String> = Vec::new();
    let front_page = matches!(resolved, Resolved::FrontPage { .. });
    match resolved {
        Resolved::NotFound => parts.push("Page not found".to_owned()),
        Resolved::Search { terms, .. } => parts.push(format!("Search Results for “{terms}”")),
        Resolved::FrontPage { .. } => parts.push(options.name.clone()),
        Resolved::Home { page, .. } => parts.push(page.title.clone()),
        Resolved::Archive { .. } => parts.push(archive.name.clone()),
        Resolved::Single(post) | Resolved::Page(post) | Resolved::Attachment(post) => {
            parts.push(post.title.clone());
        }
        Resolved::Redirect { .. } => {}
    }
    let paged = paged_of(resolved);
    if paged > 1 && !matches!(resolved, Resolved::NotFound) {
        parts.push(format!("Page {paged}"));
    }
    if front_page {
        if !options.description.is_empty() {
            parts.push(options.description.clone());
        }
    } else if !options.name.is_empty() {
        parts.push(options.name.clone());
    }
    parts.retain(|part| !part.is_empty());
    parts.join(TITLE_SEPARATOR)
}

/// `get_pagenum_link`: the link to page `page` of the list at `path`. A `page/N` at the end of
/// the path is replaced; with pretty permalinks the page is a path segment
/// (`/category/news/page/2/`), with plain ones the `paged` query variable. The query string is
/// kept, without its `paged`. The link always starts with one `/` after the site address, so a
/// path such as `//example.org` cannot make a link to another host.
fn pagenum_link(
    home: &str,
    path: &str,
    query: &str,
    page: u32,
    structure: &PermalinkStructure,
) -> String {
    let trimmed = path.trim_matches('/');
    let base = match trimmed.rsplit_once("page/") {
        Some((before, number))
            if (before.is_empty() || before.ends_with('/'))
                && !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit()) =>
        {
            before.trim_end_matches('/')
        }
        _ => trimmed,
    };
    let base = base.trim_start_matches('/');
    let mut pairs: Vec<String> = query
        .trim_start_matches('?')
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| pair.split_once('=').map_or(*pair, |(key, _)| key) != "paged")
        .map(str::to_owned)
        .collect();
    let mut link = format!("{home}/{base}");
    if structure.is_plain() {
        if page > 1 {
            pairs.push(format!("paged={page}"));
        }
    } else if page > 1 {
        if !base.is_empty() {
            link.push('/');
        }
        link.push_str(&format!("page/{page}"));
        if structure.uses_trailing_slash() {
            link.push('/');
        }
    } else if !base.is_empty() && structure.uses_trailing_slash() {
        link.push('/');
    }
    if !pairs.is_empty() {
        link.push('?');
        link.push_str(&pairs.join("&"));
    }
    link
}

/// The `home` option as a link prefix: an `http` or `https` address without its trailing
/// slash, as `esc_url` keeps it. Anything else (a `javascript:` URL, a relative path) is logged
/// and ignored, so links and redirects start from the root instead.
fn site_address(home: &str) -> String {
    let home = home.trim().trim_end_matches('/');
    let lower = home.to_ascii_lowercase();
    let is_web = ["http://", "https://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len());
    if home.is_empty() || (is_web && !home.contains(['"', '<', '>', ' ', '\\'])) {
        home.to_owned()
    } else {
        log::warn!("The home option is not an http or https address; links start from the root.");
        String::new()
    }
}

/// The options a page reads, once per request.
struct Options {
    /// `permalink_structure`; plain when unset or not valid.
    structure: PermalinkStructure,
    /// `home`, without a trailing slash; empty when unset, for links from the root.
    home: String,
    /// `blogname`.
    name: String,
    /// `blogdescription`.
    description: String,
    /// `gmt_offset`, for dates.
    offset: FixedOffset,
}

impl Options {
    async fn load(db: &Database) -> Result<Self, FrontError> {
        let text_option = async |name| -> Result<String, FrontError> {
            Ok(option(db, name)
                .await?
                .as_ref()
                .and_then(text)
                .unwrap_or_default())
        };
        let structure_text = text_option("permalink_structure").await?;
        let structure = structure_text.parse().unwrap_or_else(|e| {
            log::warn!("The permalink_structure option is not valid ({e}); plain links are used.");
            PermalinkStructure::plain()
        });
        let hours = option(db, "gmt_offset")
            .await?
            .as_ref()
            .and_then(number)
            .unwrap_or(0.0);
        let offset = FixedOffset::east_opt((hours * 3600.0).round() as i32)
            .unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero is a valid offset"));
        Ok(Self {
            structure,
            home: site_address(&text_option("home").await?),
            name: text_option("blogname").await?,
            description: text_option("blogdescription").await?,
            offset,
        })
    }
}

/// The data every template gets. See the module documentation.
#[derive(Debug, Serialize)]
struct Context {
    site: SiteView,
    head: String,
    document_title: String,
    body_class: String,
    is: Conditionals,
    post: Option<PostView>,
    posts: Vec<PostView>,
    archive: ArchiveView,
    search_query: String,
    pagination: Pagination,
    theme: ThemeView,
    menus: BTreeMap<String, Option<NavMenu>>,
    sidebars: BTreeMap<String, SidebarView>,
}

#[derive(Debug, Serialize)]
struct SiteView {
    name: String,
    description: String,
    url: String,
    language: String,
}

#[derive(Debug, Serialize)]
struct ThemeView {
    slug: String,
    name: String,
    version: String,
}

/// A post as templates see it.
#[derive(Debug, Clone, Serialize)]
struct PostView {
    id: i64,
    post_type: String,
    slug: String,
    title: String,
    url: String,
    content: String,
    excerpt: String,
    date: String,
    datetime: String,
    author: String,
    classes: String,
    password_required: bool,
}

#[derive(Debug, Default, Serialize)]
struct ArchiveView {
    /// `the_archive_title`, such as `Category: News`.
    title: String,
    /// The archive's name without the prefix, such as `News`.
    name: String,
    description: String,
}

#[derive(Debug, Default, Serialize)]
struct Pagination {
    current: u32,
    total: u32,
    has_pages: bool,
    previous: Option<String>,
    next: Option<String>,
}

/// WordPress's conditional tags for the request.
#[derive(Debug, Default, Serialize)]
struct Conditionals {
    front_page: bool,
    home: bool,
    singular: bool,
    single: bool,
    page: bool,
    attachment: bool,
    archive: bool,
    category: bool,
    tag: bool,
    author: bool,
    date: bool,
    post_type_archive: bool,
    search: bool,
    not_found: bool,
    paged: bool,
}

impl Conditionals {
    fn of(resolved: &Resolved) -> Self {
        let mut is = Conditionals {
            paged: paged_of(resolved) > 1,
            ..Conditionals::default()
        };
        match resolved {
            Resolved::FrontPage { page, .. } => {
                is.front_page = true;
                match page {
                    Some(_) => {
                        is.singular = true;
                        is.page = true;
                    }
                    None => is.home = true,
                }
            }
            Resolved::Home { .. } => is.home = true,
            Resolved::Single(_) => {
                is.singular = true;
                is.single = true;
            }
            Resolved::Page(_) => {
                is.singular = true;
                is.page = true;
            }
            Resolved::Attachment(_) => {
                is.singular = true;
                is.single = true;
                is.attachment = true;
            }
            Resolved::Archive { archive, .. } => {
                is.archive = true;
                match archive {
                    Archive::Category(_) => is.category = true,
                    Archive::Tag(_) => is.tag = true,
                    Archive::Author(_) => is.author = true,
                    Archive::Date { .. } => is.date = true,
                    Archive::PostType(_) => is.post_type_archive = true,
                }
            }
            Resolved::Search { .. } => is.search = true,
            Resolved::NotFound => is.not_found = true,
            Resolved::Redirect { .. } => {}
        }
        is
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pretty() -> PermalinkStructure {
        PermalinkStructure::post_name()
    }

    #[test]
    fn pagenum_links_replace_the_page_segment() {
        let s = pretty();
        assert_eq!(pagenum_link("", "/", "", 2, &s), "/page/2/");
        assert_eq!(pagenum_link("", "/page/2/", "", 1, &s), "/");
        assert_eq!(pagenum_link("", "/page/2/", "", 3, &s), "/page/3/");
        assert_eq!(
            pagenum_link("", "/category/news/page/3/", "", 2, &s),
            "/category/news/page/2/"
        );
        assert_eq!(
            pagenum_link("", "/category/news/", "", 1, &s),
            "/category/news/"
        );
        assert_eq!(
            pagenum_link("https://example.com", "/", "s=rocket&paged=2", 3, &s),
            "https://example.com/page/3/?s=rocket"
        );
        // A slug that ends in `page` is not a page segment.
        assert_eq!(
            pagenum_link("", "/frontpage/", "", 2, &s),
            "/frontpage/page/2/"
        );
    }

    #[test]
    fn pagenum_links_use_paged_with_plain_permalinks() {
        let s = PermalinkStructure::plain();
        assert_eq!(pagenum_link("", "/", "cat=3", 2, &s), "/?cat=3&paged=2");
        assert_eq!(pagenum_link("", "/", "cat=3&paged=2", 1, &s), "/?cat=3");
    }

    #[test]
    fn pagenum_links_never_start_with_two_slashes() {
        let s = pretty();
        assert_eq!(
            pagenum_link("", "//evil.example/", "", 2, &s),
            "/evil.example/page/2/"
        );
    }

    #[test]
    fn template_classes_follow_get_body_class() {
        assert_eq!(template_classes("page", None), ["page-template-default"]);
        assert_eq!(
            template_classes("page", Some("default")),
            ["page-template-default"]
        );
        assert_eq!(
            template_classes("page", Some("templates/full-width.php")),
            [
                "page-template",
                "page-template-templates",
                "page-template-full-width",
                "page-template-templatesfull-width-php",
            ]
        );
        assert_eq!(html_class("caf%c3%a9-x y"), "caf-xy");
    }

    #[test]
    fn site_addresses_are_web_urls_only() {
        assert_eq!(site_address("https://example.com/"), "https://example.com");
        assert_eq!(site_address("javascript:alert(1)"), "");
        assert_eq!(site_address("JavaScript:alert(1)"), "");
        assert_eq!(site_address("//evil.example"), "");
        assert_eq!(site_address(""), "");
    }

    #[test]
    fn manual_excerpts_are_escaped() {
        assert_eq!(
            escape_html("<b>\"a\" & 'b'</b>"),
            "&lt;b&gt;&quot;a&quot; &amp; &#039;b&#039;&lt;/b&gt;"
        );
    }

    #[test]
    fn excerpts_strip_tags_and_keep_55_words() {
        assert_eq!(trim_excerpt("<p>Hello <b>world</b></p>"), "Hello world");
        let long = "word ".repeat(60);
        let excerpt = trim_excerpt(&long);
        assert!(excerpt.ends_with(" [&hellip;]"));
        assert_eq!(excerpt.split_whitespace().count(), 56);
    }
}
