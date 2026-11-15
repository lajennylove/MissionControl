//! Navigation menus, stored as WordPress stores them (`wp-includes/nav-menu.php`) and built for
//! templates as `wp_nav_menu` builds them.
//!
//! # Storage
//!
//! - A menu is a term of the flat `nav_menu` taxonomy ([`Taxonomy::NAV_MENU`]).
//! - Each item is a `posts` row of type [`NAV_MENU_ITEM`], `publish`, related to the menu's term
//!   in `term_relationships`. Its `title` is the label (empty means the linked object's title),
//!   `excerpt` the link's title attribute, `content` its description and `menu_order` its
//!   position in the menu.
//! - The rest is post meta, as WordPress writes it: `_menu_item_type` (`custom`, `post_type` or
//!   `taxonomy`), `_menu_item_object` (`custom`, the post type or the taxonomy),
//!   `_menu_item_object_id` (the post id, the term id, or the item's own id for a custom link),
//!   `_menu_item_url` (a custom link's address), `_menu_item_menu_item_parent` (the parent
//!   item's id, `"0"` at the top), `_menu_item_target`, `_menu_item_classes` (a list) and
//!   `_menu_item_xfn`. Ids are stored as strings, as WordPress stores every meta value.
//! - The theme's locations are assigned in the option `theme_mods_{stylesheet}`, under
//!   `nav_menu_locations`: location => the menu's term id.
//!
//! ```ignore
//! let menu = create_nav_menu(&db, "Main menu").await?;
//! let about = add_menu_item(&db, &menu, author_id, NewMenuItem::post(&about_page)).await?;
//! add_menu_item(&db, &menu, author_id, NewMenuItem {
//!     parent: Some(about.id),
//!     ..NewMenuItem::custom("Docs", "https://rocketboost.rs/docs/")
//! })
//! .await?;
//! set_menu_location(&db, &theme, "primary", Some(&menu)).await?;
//! ```
//!
//! # Rendering
//!
//! [`theme_menus`] gives each of the theme's locations its [`NavMenu`] (or `None`), the template
//! context's `menus.{location}`. Items are a tree, in `menu_order`, with their links resolved
//! (4.4's permalinks for posts and pages, archive links for categories and tags), their titles,
//! WordPress's classes (`menu-item`, `menu-item-type-*`, `current-menu-item`,
//! `current-menu-ancestor`, ...) and the current item marked.
//!
//! An item is left out when its object is gone, is not readable by the visitor (a draft, a
//! private or trashed post), or is of a kind Mission Control cannot link yet (a post type
//! archive, a taxonomy without archives); an item that is not itself published (a pending
//! change) is left out too. A child of a left-out item is shown at the top level, after the
//! rest, as WordPress's walker shows orphans. A custom link that is not `http`, `https`,
//! `mailto`, `tel` or relative (`javascript:`, `data:`, ...) gets an empty `url`, so the template
//! leaves the link out; Propel's URL-safe attribute handling would also turn it into `#`.
//! Every value is text: templates escape titles with `{{ }}` (WordPress outputs menu titles as
//! HTML).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use orbit_orm::{Database, Error, Model, Value as Sql, raw};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{PageRequest, Theme};
use crate::content::{
    MetaError, NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, TermError, get_option, now,
    put_post_meta_in, set_option, set_terms_in, terms_by_term_id,
};
use crate::front::{Archive, NON_PUBLIC_TYPES, Resolved, post_links, term_links, visitor_readable};
use crate::text::html_class;

/// The post type of menu items.
pub const NAV_MENU_ITEM: &str = "nav_menu_item";

/// The theme mod that maps locations to menus.
pub const NAV_MENU_LOCATIONS: &str = "nav_menu_locations";

/// How deep a menu's tree goes, so a long parent chain written by an importer cannot nest the
/// context without bound. Deeper items are shown at the top level, as orphans are. Templates
/// show fewer: a Propel partial cannot include itself, so a theme writes one loop per level it
/// shows, as a WordPress theme passes `depth` to `wp_nav_menu` (the default theme shows three).
pub const MAX_MENU_DEPTH: usize = 16;

/// How many ids one `IN (...)` list binds, below every database's limit on bind parameters.
const IN_CHUNK: usize = 500;

/// The option that holds a theme's mods (`get_theme_mods`): `theme_mods_{stylesheet}`.
pub fn theme_mods_option(stylesheet: &str) -> String {
    format!("theme_mods_{stylesheet}")
}

/// What a menu item links to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItemObject {
    /// A custom link (`_menu_item_type` `custom`).
    Custom {
        /// The address.
        url: String,
    },
    /// A post of any type (`post_type`).
    Post {
        /// The post's type, such as `page`.
        post_type: String,
        /// The post's id.
        id: i64,
    },
    /// A term (`taxonomy`).
    Term {
        /// The taxonomy's name, such as `category`.
        taxonomy: String,
        /// The term's `term_id`, as WordPress stores it.
        term_id: i64,
    },
}

/// A menu item to add. Start from [`NewMenuItem::custom`], [`NewMenuItem::post`] or
/// [`NewMenuItem::term`] and set the rest with struct update syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMenuItem {
    /// What the item links to.
    pub object: MenuItemObject,
    /// The label; empty shows the linked object's title.
    pub title: String,
    /// The parent item's id, an item of the same menu; `None` at the top level.
    pub parent: Option<i64>,
    /// The position (`menu_order`); 0 puts the item after the menu's last one.
    pub position: i32,
    /// The link's `target`: empty, or `_blank`.
    pub target: String,
    /// The link's title attribute.
    pub attr_title: String,
    /// The item's description.
    pub description: String,
    /// CSS classes for the item; each entry may hold several, separated by spaces.
    pub classes: Vec<String>,
    /// The link relationship (XFN), such as `me`.
    pub xfn: String,
}

impl NewMenuItem {
    /// A custom link to `url`, labelled `title`.
    pub fn custom(title: &str, url: &str) -> Self {
        Self {
            title: title.to_owned(),
            ..Self::linking(MenuItemObject::Custom {
                url: url.to_owned(),
            })
        }
    }

    /// A link to `post`, labelled with its title.
    pub fn post(post: &Post) -> Self {
        Self::linking(MenuItemObject::Post {
            post_type: post.post_type.clone(),
            id: post.id,
        })
    }

    /// A link to `term`'s archive, labelled with its name.
    pub fn term(term: &Term) -> Self {
        Self::linking(MenuItemObject::Term {
            taxonomy: term.taxonomy.clone(),
            term_id: term.term_id,
        })
    }

    fn linking(object: MenuItemObject) -> Self {
        Self {
            object,
            title: String::new(),
            parent: None,
            position: 0,
            target: String::new(),
            attr_title: String::new(),
            description: String::new(),
            classes: Vec::new(),
            xfn: String::new(),
        }
    }
}

/// Why a menu was not changed.
#[derive(Debug)]
pub enum MenuError {
    /// The term is not a `nav_menu` term, or no longer exists.
    NotAMenu,
    /// The parent is not an item of the same menu.
    InvalidParent,
    /// The theme declares no such menu location.
    UnknownLocation {
        /// The location asked for.
        location: String,
    },
    /// The menu's term was refused, such as a name already taken (`menu_exists`).
    Term(TermError),
    /// The theme mods option could not be written.
    Meta(MetaError),
    /// The database refused a query.
    Database(Error),
}

impl fmt::Display for MenuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAMenu => f.write_str("the term is not a navigation menu"),
            Self::InvalidParent => f.write_str("the parent is not an item of this menu"),
            Self::UnknownLocation { location } => {
                write!(f, "the theme has no menu location `{location}`")
            }
            Self::Term(e) => e.fmt(f),
            Self::Meta(e) => e.fmt(f),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for MenuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Term(e) => Some(e),
            Self::Meta(e) => Some(e),
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Error> for MenuError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

impl From<MetaError> for MenuError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Database(e) => Self::Database(e),
            e @ MetaError::InvalidJson(_) => Self::Meta(e),
        }
    }
}

/// Creates a menu named `name`, as `wp_create_nav_menu`: a `nav_menu` term.
///
/// # Errors
///
/// Fails with [`MenuError::Term`] when the name is empty or another menu has it, or with the
/// database's error.
pub async fn create_nav_menu(db: &Database, name: &str) -> Result<Term, MenuError> {
    Term::create(db, &Taxonomy::NAV_MENU, NewTerm::new(name))
        .await
        .map_err(MenuError::Term)
}

/// Adds an item to `menu`, as `wp_update_nav_menu_item` with no item id: a published
/// `nav_menu_item` post by `author_id`, its `_menu_item_*` meta and its relationship to the
/// menu, in one transaction. Classes and XFN are cleaned with `sanitize_html_class`, the target
/// with `sanitize_key`, and a custom link that is not a safe address is stored empty, as
/// `esc_url_raw` stores it.
///
/// # Errors
///
/// Fails with [`MenuError::NotAMenu`] when `menu` is not a menu (any more),
/// [`MenuError::InvalidParent`] when the parent is not an item of `menu`, or with the
/// database's error.
pub async fn add_menu_item(
    db: &Database,
    menu: &Term,
    author_id: i64,
    item: NewMenuItem,
) -> Result<Post, MenuError> {
    if menu.taxonomy != Taxonomy::NAV_MENU.name {
        return Err(MenuError::NotAMenu);
    }
    let menu_tt = menu.term_taxonomy_id;
    db.transaction(async |tx| {
        let is_menu: i64 = raw::fetch_scalar(
            &mut *tx,
            "SELECT COUNT(*) FROM term_taxonomy WHERE id = ? AND taxonomy = ?",
            &[menu_tt.into(), Taxonomy::NAV_MENU.name.into()],
        )
        .await?;
        if is_menu == 0 {
            return Err(MenuError::NotAMenu);
        }
        if let Some(parent) = item.parent {
            let found: i64 = raw::fetch_scalar(
                &mut *tx,
                "SELECT COUNT(*) FROM term_relationships r INNER JOIN posts p ON p.id = r.object_id \
                 WHERE r.term_taxonomy_id = ? AND r.object_id = ? AND p.post_type = ?",
                &[menu_tt.into(), parent.into(), NAV_MENU_ITEM.into()],
            )
            .await?;
            if found == 0 {
                return Err(MenuError::InvalidParent);
            }
        }
        let position = if item.position > 0 {
            item.position
        } else {
            let last: Option<i32> = raw::fetch_scalar(
                &mut *tx,
                "SELECT MAX(p.menu_order) FROM term_relationships r \
                 INNER JOIN posts p ON p.id = r.object_id \
                 WHERE r.term_taxonomy_id = ? AND p.post_type = ?",
                &[menu_tt.into(), NAV_MENU_ITEM.into()],
            )
            .await?;
            last.unwrap_or(0).saturating_add(1)
        };
        let post = Post::create_in(
            tx,
            NewPost {
                status: PostStatus::Publish,
                title: item.title.clone(),
                excerpt: item.attr_title.clone(),
                content: item.description.clone(),
                menu_order: position,
                published_at: Some(now()),
                ..NewPost::new(author_id, NAV_MENU_ITEM)
            },
        )
        .await?;
        let (item_type, object, object_id, url) = match &item.object {
            MenuItemObject::Custom { url } => ("custom", "custom", post.id, safe_url(url)),
            MenuItemObject::Post { post_type, id } => {
                ("post_type", post_type.as_str(), *id, String::new())
            }
            MenuItemObject::Term { taxonomy, term_id } => {
                ("taxonomy", taxonomy.as_str(), *term_id, String::new())
            }
        };
        let meta = [
            ("_menu_item_type", json!(item_type)),
            (
                "_menu_item_menu_item_parent",
                json!(item.parent.unwrap_or(0).to_string()),
            ),
            ("_menu_item_object_id", json!(object_id.to_string())),
            ("_menu_item_object", json!(object)),
            ("_menu_item_target", json!(sanitize_key(&item.target))),
            ("_menu_item_classes", json!(clean_classes(&item.classes))),
            ("_menu_item_xfn", json!(clean_classes(std::slice::from_ref(&item.xfn)).join(" "))),
            ("_menu_item_url", json!(url)),
        ];
        for (key, value) in &meta {
            put_post_meta_in(tx, post.id, key, value).await?;
        }
        set_terms_in(tx, post.id, &Taxonomy::NAV_MENU, &[menu_tt], true).await?;
        Ok(post)
    })
    .await
}

/// Assigns `menu` to the theme's `location`, or clears the location with `None`, in the option
/// `theme_mods_{stylesheet}` under `nav_menu_locations`, as `set_theme_mod` does. The theme's
/// other mods are kept.
///
/// # Errors
///
/// Fails with [`MenuError::UnknownLocation`] when the theme (or its parent) declares no such
/// location, [`MenuError::NotAMenu`] when `menu` is not a menu, or with the database's error.
pub async fn set_menu_location(
    db: &Database,
    theme: &Theme,
    location: &str,
    menu: Option<&Term>,
) -> Result<(), MenuError> {
    if !theme
        .menu_locations()
        .iter()
        .any(|l| l.location == location)
    {
        return Err(MenuError::UnknownLocation {
            location: location.to_owned(),
        });
    }
    if menu.is_some_and(|m| m.taxonomy != Taxonomy::NAV_MENU.name) {
        return Err(MenuError::NotAMenu);
    }
    let name = theme_mods_option(theme.slug());
    let mut mods = match readable_option(db, &name).await? {
        Some(Value::Object(mods)) => mods,
        _ => Map::new(),
    };
    let mut locations = match mods.remove(NAV_MENU_LOCATIONS) {
        Some(Value::Object(locations)) => locations,
        _ => Map::new(),
    };
    match menu {
        Some(menu) => {
            locations.insert(location.to_owned(), json!(menu.term_id));
        }
        None => {
            locations.remove(location);
        }
    }
    mods.insert(NAV_MENU_LOCATIONS.to_owned(), Value::Object(locations));
    set_option(db, &name, &Value::Object(mods)).await?;
    Ok(())
}

/// The menus assigned to the locations of the theme `stylesheet`: location => the menu's term
/// id, as `get_nav_menu_locations`. A value that is not an id is left out.
///
/// # Errors
///
/// Fails with the database's error. A stored option that is not valid JSON counts as unset.
pub async fn menu_locations(
    db: &Database,
    stylesheet: &str,
) -> Result<BTreeMap<String, i64>, Error> {
    let mods = readable_option(db, &theme_mods_option(stylesheet)).await?;
    let Some(Value::Object(locations)) = mods.as_ref().and_then(|m| m.get(NAV_MENU_LOCATIONS))
    else {
        return Ok(BTreeMap::new());
    };
    Ok(locations
        .iter()
        .filter_map(|(location, menu)| Some((location.clone(), id_of(menu)?)))
        .collect())
}

/// A menu as templates see it: `menus.{location}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavMenu {
    /// The menu's term id.
    pub id: i64,
    /// The menu's name.
    pub name: String,
    /// The menu's slug, for `id="menu-{slug}"`.
    pub slug: String,
    /// The top-level items, in order.
    pub items: Vec<NavMenuItem>,
}

/// A menu item as templates see it. Every field is text, for `{{ }}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavMenuItem {
    /// The item's post id, for `id="menu-item-{id}"`.
    pub id: i64,
    /// The label: the item's own, or the linked object's title.
    pub title: String,
    /// The link, absolute for posts and terms; empty when there is no safe link.
    pub url: String,
    /// The link's `target`, such as `_blank`, or empty.
    pub target: String,
    /// The link's `rel`: the XFN, or `noopener` for a `_blank` target without XFN.
    pub rel: String,
    /// The link's title attribute, or empty.
    pub attr_title: String,
    /// The item's description, or empty.
    pub description: String,
    /// The item's classes, as `wp_nav_menu` gives them, separated by spaces.
    pub classes: String,
    /// Whether the item is the current page.
    pub current: bool,
    /// `page` for the current item, for `aria-current`; empty otherwise.
    pub aria_current: String,
    /// The child items, in order.
    pub children: Vec<NavMenuItem>,
}

/// Each menu location of `theme` (and its parent) with the menu assigned to it for this request,
/// or `None` when no menu is assigned or the menu is gone: the template context's `menus`.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn theme_menus(
    db: &Database,
    theme: &Theme,
    request: &PageRequest<'_>,
) -> Result<BTreeMap<String, Option<NavMenu>>, Error> {
    let assigned = menu_locations(db, theme.slug()).await?;
    let mut menus = BTreeMap::new();
    for location in theme.menu_locations() {
        let menu = match assigned.get(&location.location) {
            Some(&menu) => nav_menu(db, menu, request).await?,
            None => None,
        };
        menus.insert(location.location, menu);
    }
    Ok(menus)
}

/// The menu with term id `menu_term_id` as it renders for `request`, or `None` when there is no
/// such menu, as `wp_nav_menu` builds it. See the module documentation.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn nav_menu(
    db: &Database,
    menu_term_id: i64,
    request: &PageRequest<'_>,
) -> Result<Option<NavMenu>, Error> {
    let rows: Vec<(i64, String, String)> = raw::fetch_all(
        db,
        "SELECT tt.id, t.name, t.slug FROM term_taxonomy tt INNER JOIN terms t ON t.id = tt.term_id \
         WHERE tt.term_id = ? AND tt.taxonomy = ?",
        &[menu_term_id.into(), Taxonomy::NAV_MENU.name.into()],
    )
    .await?;
    let Some((menu_tt, name, slug)) = rows.into_iter().next() else {
        return Ok(None);
    };
    let entries = load_entries(db, menu_tt, request).await?;
    Ok(Some(NavMenu {
        id: menu_term_id,
        name,
        slug,
        items: build_tree(entries, request),
    }))
}

/// A menu item read from its row and meta, before its object is resolved.
struct StoredItem {
    post: Post,
    item_type: String,
    object: String,
    object_id: Option<i64>,
    url: String,
    parent: Option<i64>,
    target: String,
    classes: Vec<String>,
    xfn: String,
}

/// A displayable item: its object resolved, before the tree is built.
struct Entry {
    id: i64,
    parent: Option<i64>,
    title: String,
    url: String,
    item_type: String,
    object: String,
    object_id: Option<i64>,
    target: String,
    xfn: String,
    attr_title: String,
    description: String,
    classes: Vec<String>,
}

/// The menu's published items whose objects the visitor can see, in `menu_order`, with their
/// links and titles.
async fn load_entries(
    db: &Database,
    menu_tt: i64,
    request: &PageRequest<'_>,
) -> Result<Vec<Entry>, Error> {
    let ids: Vec<(i64,)> = raw::fetch_all(
        db,
        "SELECT object_id FROM term_relationships WHERE term_taxonomy_id = ?",
        &[menu_tt.into()],
    )
    .await?;
    let ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
    let mut posts = posts_by_id(db, &ids).await?;
    // Exact comparisons: MySQL compares text without case.
    posts.retain(|p| p.post_type == NAV_MENU_ITEM && p.status == PostStatus::Publish);
    posts.sort_by_key(|p| (p.menu_order, p.id));
    let mut meta = item_meta(db, &posts.iter().map(|p| p.id).collect::<Vec<_>>()).await?;
    let stored: Vec<StoredItem> = posts
        .into_iter()
        .map(|post| {
            let mut meta = meta.remove(&post.id).unwrap_or_default();
            let mut text = |key: &str| meta.remove(key).as_ref().map(text_of).unwrap_or_default();
            let item_type = text("_menu_item_type");
            let object = text("_menu_item_object");
            let url = text("_menu_item_url");
            let target = text("_menu_item_target");
            let xfn = text("_menu_item_xfn");
            StoredItem {
                item_type,
                object,
                object_id: meta.get("_menu_item_object_id").and_then(id_of),
                url,
                parent: meta.get("_menu_item_menu_item_parent").and_then(id_of),
                target,
                classes: match meta.get("_menu_item_classes") {
                    Some(Value::Array(list)) => list.iter().map(text_of).collect(),
                    Some(other) => vec![text_of(other)],
                    None => Vec::new(),
                },
                xfn,
                post,
            }
        })
        .collect();

    let linked_posts = linked_posts(db, request, &stored).await?;
    let linked_terms = linked_terms(db, request, &stored).await?;
    let mut entries = Vec::with_capacity(stored.len());
    for item in stored {
        let resolved = match item.item_type.as_str() {
            "custom" => Some((String::new(), safe_url(&item.url))),
            "post_type" => item
                .object_id
                .and_then(|id| linked_posts.get(&id))
                .filter(|(post_type, _, _)| *post_type == item.object)
                .map(|(_, title, url)| (title.clone(), url.clone())),
            "taxonomy" => item
                .object_id
                .and_then(|id| linked_terms.get(&(item.object.clone(), id)))
                .cloned(),
            _ => None,
        };
        let Some((object_title, url)) = resolved else {
            continue;
        };
        let title = if item.post.title.is_empty() {
            object_title
        } else {
            item.post.title.clone()
        };
        entries.push(Entry {
            id: item.post.id,
            parent: item.parent.filter(|&parent| parent != item.post.id),
            title,
            url,
            item_type: item.item_type,
            object: item.object,
            object_id: item.object_id,
            target: sanitize_key(&item.target),
            xfn: clean_classes(&[item.xfn]).join(" "),
            attr_title: item.post.excerpt.clone(),
            description: item.post.content.clone(),
            classes: clean_classes(&item.classes),
        });
    }
    Ok(entries)
}

/// The posts that `post_type` items link to and the visitor can read: id => (type, title,
/// absolute link).
async fn linked_posts(
    db: &Database,
    request: &PageRequest<'_>,
    items: &[StoredItem],
) -> Result<HashMap<i64, (String, String, String)>, Error> {
    let ids: Vec<i64> = items
        .iter()
        .filter(|i| i.item_type == "post_type")
        .filter_map(|i| i.object_id)
        .collect();
    let candidates: Vec<Post> = posts_by_id(db, &ids)
        .await?
        .into_iter()
        .filter(|post| !NON_PUBLIC_TYPES.contains(&post.post_type.as_str()))
        .collect();
    let readable = visitor_readable(db, candidates).await?;
    let links = post_links(db, request.structure, &readable).await?;
    Ok(readable
        .into_iter()
        .zip(links)
        .map(|(post, link)| {
            (
                post.id,
                (
                    post.post_type,
                    post.title,
                    format!("{}{link}", request.home),
                ),
            )
        })
        .collect())
}

/// The terms that `taxonomy` items link to, of a taxonomy with archives: (taxonomy, term id) =>
/// (name, absolute link).
async fn linked_terms(
    db: &Database,
    request: &PageRequest<'_>,
    items: &[StoredItem],
) -> Result<HashMap<(String, i64), (String, String)>, Error> {
    let mut terms = Vec::new();
    for taxonomy in [Taxonomy::CATEGORY, Taxonomy::POST_TAG] {
        let mut ids: Vec<i64> = items
            .iter()
            .filter(|i| i.item_type == "taxonomy" && i.object == taxonomy.name)
            .filter_map(|i| i.object_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if !ids.is_empty() {
            terms.extend(terms_by_term_id(db, &taxonomy, &ids).await?);
        }
    }
    let links = term_links(db, request.structure, &terms).await?;
    Ok(terms
        .into_iter()
        .zip(links)
        .filter_map(|(term, link)| {
            Some((
                (term.taxonomy, term.term_id),
                (term.name, format!("{}{}", request.home, link?)),
            ))
        })
        .collect())
}

/// The posts with these ids, in no order.
async fn posts_by_id(db: &Database, ids: &[i64]) -> Result<Vec<Post>, Error> {
    let mut posts = Vec::new();
    for chunk in ids.chunks(IN_CHUNK) {
        posts.extend(
            Post::query()
                .where_in(Post::PRIMARY_KEY, chunk.to_vec())
                .get(db)
                .await?,
        );
    }
    Ok(posts)
}

/// The `_menu_item_*` meta of the posts `ids`: post id => key => value. A value that is not
/// valid JSON is left out; a repeated key keeps its first value.
async fn item_meta(
    db: &Database,
    ids: &[i64],
) -> Result<HashMap<i64, HashMap<String, Value>>, Error> {
    let mut meta: HashMap<i64, HashMap<String, Value>> = HashMap::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let binds: Vec<Sql> = chunk.iter().map(|&id| Sql::from(id)).collect();
        let rows: Vec<(i64, String, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT post_id, meta_key, meta_value FROM post_meta \
                 WHERE post_id IN ({marks}) ORDER BY id"
            ),
            &binds,
        )
        .await?;
        for (post_id, key, value) in rows {
            if !key.starts_with("_menu_item_") {
                continue;
            }
            if let Ok(value) = serde_json::from_str(&value) {
                meta.entry(post_id).or_default().entry(key).or_insert(value);
            }
        }
    }
    Ok(meta)
}

/// What the request shows, to mark the current item: a post's id, or a term.
enum Queried<'a> {
    Post(i64),
    Term(&'a Term),
    Nothing,
}

fn queried(resolved: &Resolved) -> Queried<'_> {
    match resolved {
        Resolved::Single(post) | Resolved::Page(post) | Resolved::Attachment(post) => {
            Queried::Post(post.id)
        }
        Resolved::FrontPage {
            page: Some(page), ..
        }
        | Resolved::Home { page, .. } => Queried::Post(page.id),
        Resolved::Archive {
            archive: Archive::Category(term) | Archive::Tag(term),
            ..
        } => Queried::Term(term),
        _ => Queried::Nothing,
    }
}

/// `url` without a fragment (unless it is only a fragment) and without trailing slashes, as
/// `_wp_menu_item_classes_by_context` compares addresses.
fn comparable(url: &str) -> &str {
    let url = match url.find('#') {
        Some(at) if at > 0 => &url[..at],
        _ => url,
    };
    url.trim_end_matches('/')
}

/// The tree of `entries`, with WordPress's classes and the current item marked.
fn build_tree(entries: Vec<Entry>, request: &PageRequest<'_>) -> Vec<NavMenuItem> {
    let by_id: HashMap<i64, usize> = entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
    // The current item, as `_wp_menu_item_classes_by_context` finds it.
    let queried = queried(request.resolved);
    let mut request_url = request.path.to_owned();
    if !request.query.is_empty() {
        request_url.push('?');
        request_url.push_str(request.query);
    }
    let absolute = format!("{}{request_url}", request.home);
    let here = [comparable(&absolute), comparable(&request_url)];
    let home = comparable(request.home);
    let is_current = |entry: &Entry| match entry.item_type.as_str() {
        "post_type" => matches!(queried, Queried::Post(id) if entry.object_id == Some(id)),
        "taxonomy" => matches!(queried, Queried::Term(term)
            if term.taxonomy == entry.object && entry.object_id == Some(term.term_id)),
        _ => !entry.url.is_empty() && here.contains(&comparable(&entry.url)),
    };
    let current: Vec<bool> = entries.iter().map(is_current).collect();
    // Every ancestor of a current item, and its parent.
    let mut ancestors: HashSet<usize> = HashSet::new();
    let mut parents: HashSet<usize> = HashSet::new();
    for (index, _) in current.iter().enumerate().filter(|(_, c)| **c) {
        let mut seen = HashSet::from([index]);
        let mut next = entries[index].parent.and_then(|p| by_id.get(&p).copied());
        if let Some(parent) = next {
            parents.insert(parent);
        }
        while let Some(at) = next.filter(|at| seen.insert(*at)) {
            ancestors.insert(at);
            next = entries[at].parent.and_then(|p| by_id.get(&p).copied());
        }
    }
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match entry.parent {
            None => roots.push(index),
            Some(parent) => {
                if let Some(&at) = by_id.get(&parent) {
                    children.entry(at).or_default().push(index);
                }
            }
        }
    }

    let view = |index: usize, kids: Vec<NavMenuItem>| -> NavMenuItem {
        let entry = &entries[index];
        let page = entry.item_type == "post_type" && entry.object == "page";
        let mut classes = entry.classes.clone();
        classes.push("menu-item".to_owned());
        classes.push(format!("menu-item-type-{}", html_class(&entry.item_type)));
        classes.push(format!("menu-item-object-{}", html_class(&entry.object)));
        if entry.item_type == "custom" && !entry.url.is_empty() && comparable(&entry.url) == home {
            classes.push("menu-item-home".to_owned());
        }
        if current[index] {
            classes.push("current-menu-item".to_owned());
            if page {
                classes.push("page_item".to_owned());
                classes.push(format!("page-item-{}", entry.object_id.unwrap_or(0)));
                classes.push("current_page_item".to_owned());
            }
        }
        if parents.contains(&index) {
            classes.push("current-menu-parent".to_owned());
            if page {
                classes.push("current_page_parent".to_owned());
            }
        }
        if ancestors.contains(&index) {
            classes.push("current-menu-ancestor".to_owned());
            if page {
                classes.push("current_page_ancestor".to_owned());
            }
        }
        if !kids.is_empty() {
            classes.push("menu-item-has-children".to_owned());
        }
        classes.push(format!("menu-item-{}", entry.id));
        let rel = if entry.target == "_blank" && entry.xfn.is_empty() {
            "noopener".to_owned()
        } else {
            entry.xfn.clone()
        };
        NavMenuItem {
            id: entry.id,
            title: entry.title.clone(),
            url: entry.url.clone(),
            target: entry.target.clone(),
            rel,
            attr_title: entry.attr_title.clone(),
            description: entry.description.clone(),
            classes: classes.join(" "),
            current: current[index],
            aria_current: if current[index] { "page" } else { "" }.to_owned(),
            children: kids,
        }
    };

    /// Builds `index` and its children, depth first, marking what it shows.
    fn walk(
        index: usize,
        depth: usize,
        children: &HashMap<usize, Vec<usize>>,
        shown: &mut HashSet<usize>,
        view: &dyn Fn(usize, Vec<NavMenuItem>) -> NavMenuItem,
    ) -> NavMenuItem {
        shown.insert(index);
        let mut kids = Vec::new();
        if depth + 1 < MAX_MENU_DEPTH {
            for &child in children.get(&index).map(Vec::as_slice).unwrap_or(&[]) {
                if !shown.contains(&child) {
                    kids.push(walk(child, depth + 1, children, shown, view));
                }
            }
        }
        view(index, kids)
    }

    let mut shown = HashSet::new();
    let mut items: Vec<NavMenuItem> = roots
        .iter()
        .map(|&root| walk(root, 0, &children, &mut shown, &view))
        .collect();
    // Orphans (a parent left out, a cycle, too deep), at the top level in order, without their
    // children, as Walker::walk shows them.
    for index in 0..entries.len() {
        if !shown.contains(&index) {
            shown.insert(index);
            items.push(view(index, Vec::new()));
        }
    }
    items
}

/// `url` when it is safe in a link: relative, or `http`, `https`, `mailto` or `tel`; empty
/// otherwise, as `esc_url` empties a link with another protocol. Whitespace and control
/// characters are ignored when the scheme is read, as browsers ignore them (`java\tscript:`).
pub(crate) fn safe_url(url: &str) -> String {
    let url = url.trim();
    let cleaned: String = url
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let safe = match cleaned.find(':') {
        Some(colon) if !cleaned[..colon].contains(['/', '?', '#']) => {
            matches!(&cleaned[..colon], "http" | "https" | "mailto" | "tel")
        }
        _ => true,
    };
    if safe { url.to_owned() } else { String::new() }
}

/// Each class of `classes` (an entry may hold several, separated by whitespace) cleaned with
/// `sanitize_html_class`; empty ones dropped.
fn clean_classes(classes: &[String]) -> Vec<String> {
    classes
        .iter()
        .flat_map(|entry| entry.split_whitespace())
        .map(html_class)
        .filter(|class| !class.is_empty())
        .collect()
}

/// `sanitize_key`: lowercase ASCII letters, digits, `_` and `-`.
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// A meta or option value as text: a string, or a number written out.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

/// A post or term id from a meta or option value: a positive whole number, or a string of one.
/// 0 (WordPress's "none") is `None`.
fn id_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|id| *id > 0)
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
    fn unsafe_schemes_are_emptied() {
        for url in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "java\tscript:alert(1)",
            " javascript:alert(1)",
            "data:text/html,x",
            "vbscript:x",
            "file:///etc/passwd",
        ] {
            assert_eq!(safe_url(url), "", "{url:?}");
        }
        for url in [
            "https://example.com/a?b=c#d",
            "http://example.com",
            "mailto:a@example.com",
            "tel:+1555",
            "/about/",
            "about",
            "#top",
            "?s=x",
            "//cdn.example.com/x",
            "/a:b",
        ] {
            assert_eq!(safe_url(url), url, "{url:?}");
        }
    }

    #[test]
    fn addresses_compare_without_fragments_and_trailing_slashes() {
        assert_eq!(
            comparable("https://example.com/about/#team"),
            "https://example.com/about"
        );
        assert_eq!(comparable("#top"), "#top");
        assert_eq!(comparable("/"), "");
    }

    #[test]
    fn classes_and_keys_are_cleaned() {
        assert_eq!(
            clean_classes(&["a b!".to_owned(), "  ".to_owned(), "c%41d".to_owned()]),
            ["a", "b", "cd"]
        );
        assert_eq!(sanitize_key("_Blank\"><x"), "_blankx");
        assert_eq!(id_of(&json!("12")), Some(12));
        assert_eq!(id_of(&json!(0)), None);
        assert_eq!(id_of(&json!("x")), None);
    }
}
