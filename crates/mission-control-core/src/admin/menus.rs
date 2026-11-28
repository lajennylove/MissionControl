//! The admin's Menus screen (Task 6.5, design 5.10, Ruling R133): WordPress's `nav-menus.php`
//! as the Ignite runtime resource [`NAV_MENUS`], over the `nav_menu` terms and their items
//! through the menus API ([`crate::theme`]'s `save_menu_items` and the others). Nothing here
//! needs Rocket: the `mission-control` crate registers the resource and checks
//! [`authorize_menus_screen`] before Ignite's routes run, and puts WordPress's menu builder on
//! the form (its "Add menu items" column, whose boxes [`menu_source_items`] lists, and the Menu
//! Structure), which writes the item rows described here.
//!
//! - The list: each menu's name, the theme locations that show it, and its item count; the
//!   search matches the name.
//! - New (Ignite's create form) and edit, in `nav-menus.php`'s order: the Menu Name over the
//!   Menu Structure (the items), then the Menu Settings: "Automatically add new top-level pages
//!   to this menu" (`nav_menu_options`' `auto_add`, saved as `wp_nav_menu_update_menu_items`
//!   saves it, and applied by `_wp_auto_add_pages_to_menu`, see [`crate::theme`]) and one
//!   checkbox per menu location the active theme registers. The name is required ("Please
//!   enter a valid menu name.", as `nav-menus.php` says) and unique among menus, as
//!   `wp_update_nav_menu_object` checks it (a taken name shows "Menu Name is already taken.").
//! - The items are a repeater: each row a label, a type (a custom link, a post of a post type, a
//!   term of a taxonomy; text, so a stored item of a type the screen no longer offers still
//!   saves), the object id or the URL, the parent, CSS classes, "Open link in a new tab", and
//!   the Title Attribute, Link Relationship (XFN) and Description of `Walker_Nav_Menu_Edit`
//!   (a row without these keys keeps the item's).
//! - An item whose post or term is gone (trashed, deleted) is kept as it is when its row is
//!   saved unchanged, as WordPress keeps it ("Invalid"); the site leaves it out.
//! - Saving the items is `wp_nav_menu_update_menu_items`: rows are saved in order, a row with
//!   the id of one of the menu's items updates it, and the menu's items no row names are deleted.
//!   A row's type is the screen's (`page`, `category`, `custom`, ...) or WordPress's
//!   (`post_type` or `taxonomy`, with the post type or taxonomy in `object`, or read from the
//!   object id when `object` is missing). The parent is the parent item's id, as WordPress's
//!   `menu-item-parent-id`; a new item, which has no id yet, can name a new parent by its **row
//!   number** (1 for the first row; 0 or blank for the top level), in `parent` or, unambiguously,
//!   in `parent_row` (which the menu builder sends).
//! - Ruling R135: everything is checked before anything is written.
//!   A row that links to nothing (a post that is missing, trashed, an auto-draft or of another
//!   type; a term that is missing; a type the screen does not offer), or more than
//!   [`MAX_MENU_ITEMS`] rows, refuses the whole save: nothing is written, and the form comes
//!   back (422) with the typed rows and the reason under the items (Ignite's `ValidationError`,
//!   from [`RuntimeData::try_create_for`] and [`RuntimeData::try_update_for`]). WordPress's
//!   screen only offers existing objects.
//! - The writes: the items and the locations are saved in one transaction
//!   ([`save_nav_menu`]). Creating the menu's term (New) or renaming it comes first, in its own
//!   transaction, because the term API has no transaction-scoped create or update: if the items'
//!   transaction then fails (the database fails mid-save), a new menu stays with no items, or a
//!   renamed menu keeps its new name with its old items and locations.
//! - Delete is `wp_delete_nav_menu`, in one transaction: the items, the term, and the locations
//!   that showed it.
//!
//! As the post type screens, the data layer is never told who is signed in (owner item O8): a
//! new item's author is the author of the menu's newest item, or the site's first user.

use std::collections::{BTreeMap, HashMap, HashSet};

use ignite_core::{
    ActingUser, ColumnDef, Direction, FieldDef, FieldKindDef, FormLayout, Group, ListQuery, Page,
    Row, RuntimeData, RuntimeError, RuntimeFuture, RuntimeResource, Section, ValidationError,
    Width,
};
use orbit_orm::{Database, Error, Model, Value as Sql, raw};
use serde_json::{Value, json};

use super::posts::{PostScreen, Refusal};
use super::settings::refused_save;
use crate::content::{
    MetaError, Post, PostStatus, Taxonomy, Term, TermError, is_valid_link_url, terms_by_term_id,
};
use crate::front::escape_like;
use crate::theme::{
    MAX_MENU_ITEMS, MenuError, MenuItemInput, MenuItemObject, NAV_MENU_ITEM, StoredMenuItem, Theme,
    auto_add_menus, create_nav_menu, delete_nav_menu, get_nav_menu, menu_items, menu_locations,
    rename_nav_menu, save_nav_menu_with,
};
use crate::users::{UserCaps, can};

/// The slug of the Menus screen.
pub const NAV_MENUS: &str = "nav-menus";

/// The most menus a page of the list shows.
const MAX_PER_PAGE: i64 = 100;

/// The prefix of a location's checkbox field: `loc_primary`.
const LOCATION_PREFIX: &str = "loc_";

/// The repeater field of the items.
const ITEMS: &str = "items";

/// The label of the repeater field of the items, under which a refused save's message shows.
const ITEMS_LABEL: &str = "Menu items (one row per item, in order)";

/// The name field.
const NAME: &str = "name";

/// The "Automatically add new top-level pages to this menu" checkbox (WordPress's
/// `auto-add-pages`).
pub const AUTO_ADD_FIELD: &str = "auto_add";

/// What the save answers under the name when it is blank, as `nav-menus.php` does.
pub const MENU_NAME_REQUIRED: &str = "Please enter a valid menu name.";

/// Why `user` may not use the Menus screen, or `None`: it needs `edit_theme_options`, as
/// `nav-menus.php` does.
pub fn authorize_menus_screen(user: &UserCaps, _request: PostScreen) -> Option<Refusal> {
    (!can(user, "edit_theme_options", None)).then_some(Refusal::EditThemeOptions)
}

/// What a menu item can link to, a type the item rows offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuObjectType {
    /// A custom link.
    Custom,
    /// A post of the post type named.
    PostType(String),
    /// A term of the taxonomy named.
    Taxonomy(String),
}

impl MenuObjectType {
    /// The value the item row stores: `custom`, the post type, or the taxonomy.
    pub fn value(&self) -> &str {
        match self {
            Self::Custom => "custom",
            Self::PostType(name) | Self::Taxonomy(name) => name,
        }
    }
}

/// The types an item row offers by default: a custom link, a page, a post, a category and a tag,
/// as WordPress's menu screen offers them.
pub fn default_menu_object_types() -> Vec<(MenuObjectType, String)> {
    vec![
        (MenuObjectType::Custom, "Custom Link".to_owned()),
        (
            MenuObjectType::PostType("page".to_owned()),
            "Page".to_owned(),
        ),
        (
            MenuObjectType::PostType("post".to_owned()),
            "Post".to_owned(),
        ),
        (
            MenuObjectType::Taxonomy("category".to_owned()),
            "Category".to_owned(),
        ),
        (
            MenuObjectType::Taxonomy("post_tag".to_owned()),
            "Tag".to_owned(),
        ),
    ]
}

/// The field name of `location`'s checkbox, when it fits Ignite's 64 bytes.
fn location_field(location: &str) -> Option<String> {
    let name = format!("{LOCATION_PREFIX}{location}");
    (name.len() <= ignite_core::MAX_FIELD_NAME).then_some(name)
}

/// The Menus screen as an Ignite runtime resource, for the locations of `theme` and item rows of
/// `types` (each with its label; see [`default_menu_object_types`]).
pub fn nav_menus_resource(theme: &Theme, types: Vec<(MenuObjectType, String)>) -> RuntimeResource {
    let field = |name: &str, label: &str, kind: FieldKindDef, required: bool| FieldDef {
        name: name.to_owned(),
        label: label.to_owned(),
        kind,
        required,
    };
    let mut locations = Vec::new();
    for location in theme.menu_locations() {
        match location_field(&location.location) {
            Some(name) => locations.push((name, location.location, location.description)),
            None => log::warn!(
                "The menu location `{}` has no checkbox on the Menus screen: its id is too long.",
                location.location
            ),
        }
    }
    // Not `required` for Ignite: the save answers a blank name with WordPress's own message
    // ([`MENU_NAME_REQUIRED`]).
    let mut fields = vec![
        field(NAME, "Menu Name", FieldKindDef::Text, false),
        field(
            AUTO_ADD_FIELD,
            "Automatically add new top-level pages to this menu",
            FieldKindDef::Checkbox,
            false,
        ),
    ];
    for (name, _, description) in &locations {
        fields.push(field(
            name,
            &format!("Display location: {description}"),
            FieldKindDef::Checkbox,
            false,
        ));
    }
    // The items as a JSON list of rows (`id`, `title`, `type`, `object_id`, `url`, `parent`,
    // `parent_row`, `classes`, `new_tab`, `attr_title`, `xfn`, `description`) in a textarea,
    // which the menu builder edits as WordPress's Menu Structure (Ignite 0.6.7 edits a
    // repeater as rows, which the builder does not read). A row's `type` is text, not a
    // choice: a stored item of a type the screen no longer offers (a custom type registered
    // without `show_in_nav_menus`, or removed) must still save unchanged, as WordPress keeps
    // such items. The data layer checks each row (see `item_inputs`).
    fields.push(field(ITEMS, ITEMS_LABEL, FieldKindDef::Textarea, false));
    let column = |key: &str, label: &str, sortable: bool, searchable: bool| ColumnDef {
        key: key.to_owned(),
        label: label.to_owned(),
        sortable,
        searchable,
    };
    let mut resource = RuntimeResource::new(
        NAV_MENUS,
        "Menus",
        "Menu",
        NavMenusData {
            theme: theme.clone(),
            locations,
            types: types.into_iter().map(|(t, _)| t).collect(),
        },
    );
    resource.fields = fields;
    // WordPress's edit column (`nav-menus.php`): the Menu Name over the Menu Structure, then
    // the Menu Settings (auto add pages, display locations). The menu builder's script puts the
    // "Add menu items" column beside it, so the page takes the whole width.
    let settings: Vec<String> = resource
        .fields
        .iter()
        .filter(|f| f.name == AUTO_ADD_FIELD || f.name.starts_with(LOCATION_PREFIX))
        .map(|f| f.name.clone())
        .collect();
    resource.form_layout = FormLayout::new()
        .columns(1)
        .max_content_width(Width::Full)
        .schema([Group::make([
            Section::make("Menu Structure").schema([NAME, ITEMS]),
            Section::make("Menu Settings").schema(settings),
        ])
        .column_span(1)]);
    resource.columns = vec![
        column(COL_NAME, "Name", true, true),
        column(COL_LOCATIONS, "Theme Locations", false, false),
        column(COL_ITEMS, "Items", false, false),
    ];
    // WordPress's Menus screen is in Appearance (60).
    super::navigation::place(
        &mut resource,
        super::navigation::APPEARANCE,
        super::navigation::APPEARANCE_ICON,
    );
    // `nav-menus.php` needs `edit_theme_options` for everything.
    super::ScreenCaps::all("edit_theme_options").apply(resource)
}

/// Registers the Menus screen with Ignite; see [`nav_menus_resource`].
///
/// # Errors
///
/// Ignite's refusal, such as the slug taken by another registration.
pub fn register_nav_menus_resource(
    theme: &Theme,
    types: Vec<(MenuObjectType, String)>,
) -> Result<(), RuntimeError> {
    ignite_core::register_runtime(nav_menus_resource(theme, types))
}

const COL_NAME: &str = "col_name";
const COL_LOCATIONS: &str = "col_locations";
const COL_ITEMS: &str = "col_items";

/// Where the Menus screen's records live: the `nav_menu` terms. See the [module](self).
#[derive(Clone)]
pub struct NavMenusData {
    theme: Theme,
    /// (field, location id, description) of each location checkbox.
    locations: Vec<(String, String, String)>,
    types: Vec<MenuObjectType>,
}

/// The error Ignite shows as "Menu Name is already taken.": a violation of the index its rule
/// names (`{slug}_{field}_unique`).
fn name_taken() -> Error {
    Error::UniqueViolation {
        index: Some(format!("{NAV_MENUS}_{NAME}_unique")),
    }
}

fn menu_error(e: MenuError) -> Error {
    match e {
        MenuError::Database(e) => e,
        MenuError::Term(TermError::Exists { .. }) => name_taken(),
        MenuError::Term(TermError::Database(e)) => e,
        MenuError::Meta(MetaError::Database(e)) => e,
        other => Error::Database(sqlx::Error::Protocol(other.to_string())),
    }
}

impl NavMenusData {
    /// The menus' item counts: term taxonomy id => count.
    async fn item_counts(
        &self,
        db: &Database,
        menus: &[(i64, i64, String)],
    ) -> Result<HashMap<i64, i64>, Error> {
        if menus.is_empty() {
            return Ok(HashMap::new());
        }
        let marks = vec!["?"; menus.len()].join(", ");
        let mut binds: Vec<Sql> = vec![NAV_MENU_ITEM.into()];
        binds.extend(menus.iter().map(|(_, tt, _)| Sql::from(*tt)));
        let rows: Vec<(i64, i64)> = raw::fetch_all(
            db,
            &format!(
                "SELECT r.term_taxonomy_id, COUNT(*) FROM term_relationships r \
                 INNER JOIN posts p ON p.id = r.object_id \
                 WHERE p.post_type = ? AND r.term_taxonomy_id IN ({marks}) \
                 GROUP BY r.term_taxonomy_id"
            ),
            &binds,
        )
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// The descriptions of the locations that show menu `term_id`, joined.
    fn locations_of(&self, assigned: &BTreeMap<String, i64>, term_id: i64) -> String {
        self.locations
            .iter()
            .filter(|(_, location, _)| assigned.get(location) == Some(&term_id))
            .map(|(_, _, description)| description.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    async fn list_page(&self, db: &Database, query: &ListQuery) -> Result<Page<Row>, Error> {
        let per_page = query.per_page.clamp(1, MAX_PER_PAGE);
        let offset = query.page.max(1).saturating_sub(1).saturating_mul(per_page);
        let mut filter = String::from("tt.taxonomy = ?");
        let mut binds: Vec<Sql> = vec![(&*Taxonomy::NAV_MENU.name).into()];
        if let Some(search) = query.search.as_deref().filter(|s| !s.trim().is_empty()) {
            let search: String = search.trim().chars().take(200).collect();
            filter.push_str(" AND LOWER(t.name) LIKE ? ESCAPE '!'");
            binds.push(format!("%{}%", escape_like(&search.to_lowercase())).into());
        }
        let total: i64 = raw::fetch_scalar(
            db,
            &format!(
                "SELECT COUNT(*) FROM term_taxonomy tt INNER JOIN terms t ON t.id = tt.term_id \
                 WHERE {filter}"
            ),
            &binds,
        )
        .await?;
        let order = match &query.sort {
            Some((_, Direction::Desc)) => "DESC",
            _ => "ASC",
        };
        let mut page_binds = binds.clone();
        page_binds.push(per_page.into());
        page_binds.push(offset.into());
        let menus: Vec<(i64, i64, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT t.id, tt.id, t.name FROM term_taxonomy tt \
                 INNER JOIN terms t ON t.id = tt.term_id WHERE {filter} \
                 ORDER BY t.name {order}, t.id {order} LIMIT ? OFFSET ?"
            ),
            &page_binds,
        )
        .await?;
        let counts = self.item_counts(db, &menus).await?;
        let assigned = menu_locations(db, self.theme.slug()).await?;
        let rows = menus
            .into_iter()
            .map(|(term_id, tt, name)| {
                let mut row = Row::new();
                row.insert("id".into(), json!(term_id));
                row.insert(NAME.into(), json!(name));
                row.insert(COL_NAME.into(), json!(name));
                row.insert(
                    COL_LOCATIONS.into(),
                    json!(self.locations_of(&assigned, term_id)),
                );
                row.insert(
                    COL_ITEMS.into(),
                    json!(counts.get(&tt).copied().unwrap_or(0).to_string()),
                );
                row
            })
            .collect();
        Ok(Page { rows, total })
    }

    async fn row(&self, db: &Database, term_id: i64) -> Result<Option<Row>, Error> {
        let Some(menu) = get_nav_menu(db, term_id).await? else {
            return Ok(None);
        };
        let items = menu_items(db, &menu).await?;
        let assigned = menu_locations(db, self.theme.slug()).await?;
        let mut row = Row::new();
        row.insert("id".into(), json!(menu.term_id));
        row.insert(NAME.into(), json!(menu.name));
        row.insert(COL_NAME.into(), json!(menu.name));
        row.insert(
            COL_LOCATIONS.into(),
            json!(self.locations_of(&assigned, menu.term_id)),
        );
        row.insert(COL_ITEMS.into(), json!(items.len().to_string()));
        for (field, location, _) in &self.locations {
            row.insert(
                field.clone(),
                json!(assigned.get(location) == Some(&menu.term_id)),
            );
        }
        row.insert(ITEMS.into(), item_rows(&items));
        row.insert(
            AUTO_ADD_FIELD.into(),
            json!(auto_add_menus(db).await?.contains(&menu.term_id)),
        );
        Ok(Some(row))
    }

    /// What a save of the form changes, checked before anything is written (Ruling R135): the
    /// location changes, and the items when the form sent them.
    async fn plan(
        &self,
        db: &Database,
        menu: Option<&Term>,
        input: &Row,
    ) -> Result<MenuPlan, Refused> {
        let locations = self
            .locations
            .iter()
            .filter_map(|(field, location, _)| match input.get(field) {
                Some(Value::Bool(show)) => Some((location.clone(), *show)),
                _ => None,
            })
            .collect();
        let items = match input.get(ITEMS) {
            None => None,
            Some(items) => {
                let rows: Vec<Value> = match items {
                    Value::String(text) if !text.trim().is_empty() => {
                        serde_json::from_str::<Vec<Value>>(text).unwrap_or_default()
                    }
                    Value::Array(rows) => rows.clone(),
                    _ => Vec::new(),
                };
                let stored = match menu {
                    Some(menu) => menu_items(db, menu).await?,
                    None => Vec::new(),
                };
                Some(self.item_inputs(db, &rows, &stored).await?)
            }
        };
        let auto_add = match input.get(AUTO_ADD_FIELD) {
            Some(Value::Bool(add)) => Some(*add),
            _ => None,
        };
        Ok(MenuPlan {
            locations,
            items,
            auto_add,
        })
    }

    /// Saves `plan` on `menu`: its items and its locations, in one transaction
    /// ([`save_nav_menu`]).
    async fn apply(&self, db: &Database, menu: &Term, plan: &MenuPlan) -> Result<(), Error> {
        let author = item_author(db, menu).await?;
        save_nav_menu_with(
            db,
            menu,
            author,
            plan.items.as_deref(),
            Some((&self.theme, &plan.locations)),
            plan.auto_add,
        )
        .await
        .map_err(menu_error)?;
        Ok(())
    }

    /// The items to save from the repeater's `rows`, each row read and checked against the
    /// menu's `stored` items.
    ///
    /// A row that names one of the menu's items (its `id`) with the item's own type and object
    /// id keeps the item's link as stored, whether or not it still links to something (its page
    /// trashed, its term deleted, its type no longer offered): WordPress keeps such items (it
    /// shows them as "Invalid") and saves the menu. Ruling R135: any other row (new, or with
    /// its type or object id changed) that links to nothing or names a type the screen does not
    /// offer, or more rows than [`MAX_MENU_ITEMS`], refuses the whole save, with the reason
    /// logged and shown under the items.
    async fn item_inputs(
        &self,
        db: &Database,
        rows: &[Value],
        stored: &[StoredMenuItem],
    ) -> Result<Vec<MenuItemInput>, Refused> {
        if rows.len() > MAX_MENU_ITEMS {
            let reason = format!("A menu holds at most {MAX_MENU_ITEMS} items.");
            log::warn!("The menu was not saved: {reason}");
            return Err(Refused::Items(reason));
        }
        let mut read: Vec<RowItem> = rows.iter().map(RowItem::read).collect();
        resolve_wordpress_types(db, &mut read).await?;
        let by_id: HashMap<i64, &StoredMenuItem> = stored.iter().map(|s| (s.id, s)).collect();
        // The stored link of a row that names an item with its own type and object id.
        let unchanged: Vec<Option<MenuItemObject>> = read
            .iter()
            .map(|item| {
                let stored = item.id.and_then(|id| by_id.get(&id))?;
                let (kind, object_id) = match &stored.object {
                    MenuItemObject::Custom { .. } => return None,
                    MenuItemObject::Post { post_type, id } => (post_type.as_str(), *id),
                    MenuItemObject::Term { taxonomy, term_id } => (taxonomy.as_str(), *term_id),
                };
                (item.kind == kind && item.object_id == Some(object_id))
                    .then(|| stored.object.clone())
            })
            .collect();
        let objects: Vec<Option<MenuItemObject>> = read
            .iter()
            .zip(&unchanged)
            .map(|(item, kept)| {
                kept.clone()
                    .or_else(|| self.object_of(&item.kind, item.object_id, &item.url))
            })
            .collect();
        // A custom link's URL must be a link (the editor's URL field, `<input type="url">`):
        // checked for a new link, or one whose URL changed; a stored URL left as it is stays.
        let bad_urls: Vec<String> =
            read.iter()
                .zip(&objects)
                .enumerate()
                .filter(|(_, (item, object))| {
                    let Some(MenuItemObject::Custom { url }) = object else {
                        return false;
                    };
                    let stored_url = item.id.and_then(|id| by_id.get(&id)).and_then(|stored| {
                        match &stored.object {
                            MenuItemObject::Custom { url } => Some(url),
                            _ => None,
                        }
                    });
                    let kept = stored_url == Some(url);
                    !kept && !is_valid_link_url(url)
                })
                .map(|(index, _)| (index + 1).to_string())
                .collect();
        if !bad_urls.is_empty() {
            let reason = format!(
                "{} {}: {INVALID_MENU_URL}",
                if bad_urls.len() == 1 {
                    "Item row"
                } else {
                    "Item rows"
                },
                bad_urls.join(", ")
            );
            log::warn!("The menu was not saved: {reason}");
            return Err(Refused::Items(reason));
        }
        let valid = linked(db, &objects).await?;
        let refused: Vec<String> = (0..read.len())
            .filter(|index| unchanged[*index].is_none() && !valid.contains(index))
            .map(|index| (index + 1).to_string())
            .collect();
        if !refused.is_empty() {
            let rows = refused.join(", ");
            let offered: Vec<&str> = self.types.iter().map(MenuObjectType::value).collect();
            let reason = format!(
                "{} {rows} {} to nothing. Give each item a type ({}), and the ID of a post or \
                 term of that type that is not in the Trash, or the URL of a custom link.",
                if refused.len() == 1 {
                    "Item row"
                } else {
                    "Item rows"
                },
                if refused.len() == 1 { "links" } else { "link" },
                offered.join(", "),
            );
            log::warn!("The menu was not saved: {reason}");
            return Err(Refused::Items(reason));
        }
        let parents = parent_rows(&read, stored);
        Ok(read
            .iter()
            .zip(objects)
            .zip(parents)
            .map(|((item, object), parent)| MenuItemInput {
                id: item.id,
                object: object.expect("checked above"),
                title: item.title.clone(),
                // `save_nav_menu` clears a parent that is the item itself, or on a loop.
                parent,
                target: if item.new_tab { "_blank" } else { "" }.to_owned(),
                classes: vec![item.classes.clone()],
                attr_title: item.attr_title.clone(),
                description: item.description.clone(),
                xfn: item.xfn.clone(),
            })
            .collect())
    }

    /// The object a row's `type` names, with its id or URL.
    fn object_of(&self, kind: &str, object_id: Option<i64>, url: &str) -> Option<MenuItemObject> {
        let found = self.types.iter().find(|t| t.value() == kind)?;
        match found {
            MenuObjectType::Custom => Some(MenuItemObject::Custom {
                url: url.to_owned(),
            }),
            MenuObjectType::PostType(post_type) => Some(MenuItemObject::Post {
                post_type: post_type.clone(),
                id: object_id.filter(|id| *id > 0)?,
            }),
            MenuObjectType::Taxonomy(taxonomy) => Some(MenuItemObject::Term {
                taxonomy: taxonomy.clone(),
                term_id: object_id.filter(|id| *id > 0)?,
            }),
        }
    }
}

/// The row index of each row's parent. A `parent_row` (the parent's row number, from 1), which
/// the menu builder sends for a parent that is a new item, comes first. Otherwise the parent is
/// as WordPress's `menu-item-parent-id` names it: the `id` of the parent item. A parent that is the `id` of another row is that row; one that is an item
/// of the menu no row keeps (deleted with this save) is none, so the item goes to the top level,
/// as WordPress moves the children of a deleted item; otherwise a number from 1 to the number of
/// rows is the parent's row number, so a new item can be the child of another new item, which
/// has no id yet. 0, blank or anything else is the top level.
fn parent_rows(rows: &[RowItem], stored: &[StoredMenuItem]) -> Vec<Option<usize>> {
    rows.iter()
        .map(|row| {
            if let Some(at) = row.parent_row.filter(|n| *n > 0) {
                return usize::try_from(at)
                    .ok()
                    .filter(|n| *n <= rows.len())
                    .map(|n| n - 1);
            }
            let parent = row.parent.filter(|p| *p > 0)?;
            if let Some(at) = rows.iter().position(|r| r.id == Some(parent)) {
                return Some(at);
            }
            if stored.iter().any(|s| s.id == parent) {
                return None;
            }
            usize::try_from(parent)
                .ok()
                .filter(|n| *n <= rows.len())
                .map(|n| n - 1)
        })
        .collect()
}

/// Reads the rows that use WordPress's names for an item's kind, as `wp_update_nav_menu_item`
/// takes them: `type` `post_type` or `taxonomy` with the post type or taxonomy in `object`
/// (`{"type": "post_type", "object": "page", "object_id": 16}`), and `custom` with `object`
/// `custom`. Each row then names its post type or taxonomy in `kind`, as the screen's own rows
/// do. When `object` is missing, it is the type of the post, or the taxonomy of the term, that
/// `object_id` names; a row whose object is not found keeps `post_type` or `taxonomy`, which
/// links to nothing.
async fn resolve_wordpress_types(db: &Database, rows: &mut [RowItem]) -> Result<(), Error> {
    for row in rows.iter_mut() {
        let wordpress_kind = match row.kind.as_str() {
            kind @ ("post_type" | "taxonomy") => kind,
            _ => continue,
        };
        if !row.object.is_empty() {
            row.kind = row.object.clone();
            continue;
        }
        let Some(id) = row.object_id.filter(|id| *id > 0) else {
            continue;
        };
        let found: Option<String> = if wordpress_kind == "post_type" {
            Post::query().find(db, id).await?.map(|p| p.post_type)
        } else {
            let rows: Vec<(String,)> = raw::fetch_all(
                db,
                "SELECT taxonomy FROM term_taxonomy WHERE term_id = ? ORDER BY id LIMIT 1",
                &[id.into()],
            )
            .await?;
            rows.into_iter().next().map(|(taxonomy,)| taxonomy)
        };
        if let Some(kind) = found {
            row.kind = kind;
        }
    }
    Ok(())
}

/// The indexes of `items` whose object exists: a post of the row's post type (not trashed or
/// an auto-draft), or a term of the row's taxonomy; every custom link.
async fn linked(
    db: &Database,
    objects: &[Option<MenuItemObject>],
) -> Result<HashSet<usize>, Error> {
    let mut valid = HashSet::new();
    let post_ids: Vec<i64> = objects
        .iter()
        .filter_map(|o| match o {
            Some(MenuItemObject::Post { id, .. }) => Some(*id),
            _ => None,
        })
        .collect();
    let posts: HashMap<i64, Post> = if post_ids.is_empty() {
        HashMap::new()
    } else {
        Post::query()
            .where_in(Post::PRIMARY_KEY, post_ids)
            .get(db)
            .await?
            .into_iter()
            .map(|p| (p.id, p))
            .collect()
    };
    let mut terms: HashSet<(String, i64)> = HashSet::new();
    let mut by_taxonomy: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for object in objects {
        if let Some(MenuItemObject::Term { taxonomy, term_id }) = object {
            by_taxonomy
                .entry(taxonomy.clone())
                .or_default()
                .push(*term_id);
        }
    }
    for (taxonomy, ids) in by_taxonomy {
        let tax = Taxonomy::builtin(&taxonomy)
            .unwrap_or_else(|| Taxonomy::new(taxonomy.clone(), false, Vec::new()));
        for term in terms_by_term_id(db, &tax, &ids).await? {
            terms.insert((term.taxonomy, term.term_id));
        }
    }
    for (index, object) in objects.iter().enumerate() {
        let ok = match object {
            Some(MenuItemObject::Custom { .. }) => true,
            Some(MenuItemObject::Post { post_type, id }) => posts.get(id).is_some_and(|p| {
                p.post_type == *post_type
                    && !matches!(p.status, PostStatus::Trash | PostStatus::AutoDraft)
            }),
            Some(MenuItemObject::Term { taxonomy, term_id }) => {
                terms.contains(&(taxonomy.clone(), *term_id))
            }
            None => false,
        };
        if ok {
            valid.insert(index);
        }
    }
    Ok(valid)
}

/// What a save of the Menus form changes; see [`NavMenusData::plan`].
struct MenuPlan {
    /// (location, show) for each location checkbox the form sent.
    locations: Vec<(String, bool)>,
    /// The items, when the form sent them.
    items: Option<Vec<MenuItemInput>>,
    /// Whether the menu adds new top-level pages, when the form sent the checkbox.
    auto_add: Option<bool>,
}

/// The message of a custom link whose URL is not a link, shown by the menu builder under the
/// item's URL and, when a save is refused, under the items.
pub const INVALID_MENU_URL: &str = "Please enter a valid URL.";

/// One repeater row, read.
struct RowItem {
    id: Option<i64>,
    kind: String,
    /// WordPress's `object` (`menu-item-object`): the post type or taxonomy of a row whose
    /// `kind` is `post_type` or `taxonomy`; see [`resolve_wordpress_types`].
    object: String,
    object_id: Option<i64>,
    url: String,
    title: String,
    parent: Option<i64>,
    /// The parent's row number (from 1), which the menu builder sends for a new parent.
    parent_row: Option<i64>,
    classes: String,
    new_tab: bool,
    /// The title attribute, description and XFN, when the row has them (an older row without
    /// them keeps the item's).
    attr_title: Option<String>,
    description: Option<String>,
    xfn: Option<String>,
}

impl RowItem {
    fn read(row: &Value) -> Self {
        let text = |key: &str| match row.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let number = |key: &str| match row.get(key) {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        };
        Self {
            id: number("id").filter(|id| *id > 0),
            kind: text("type").trim().to_owned(),
            object: text("object").trim().to_owned(),
            object_id: number("object_id"),
            url: text("url"),
            title: text("title"),
            parent: number("parent"),
            parent_row: number("parent_row"),
            classes: text("classes"),
            new_tab: matches!(row.get("new_tab"), Some(Value::Bool(true))),
            attr_title: row.get("attr_title").map(|_| text("attr_title")),
            description: row.get("description").map(|_| text("description")),
            xfn: row.get("xfn").map(|_| text("xfn")),
        }
    }
}

/// The repeater rows of `items`: each item's id, label, type, object id or URL, parent item id
/// (0 at the top level, or when the parent is not an item of the menu), classes and new-tab flag.
fn item_rows(items: &[StoredMenuItem]) -> Value {
    let ids: HashSet<i64> = items.iter().map(|item| item.id).collect();
    Value::Array(
        items
            .iter()
            .map(|item| {
                let (kind, object_id, url) = match &item.object {
                    MenuItemObject::Custom { url } => {
                        ("custom".to_owned(), Value::Null, url.clone())
                    }
                    MenuItemObject::Post { post_type, id } => {
                        (post_type.clone(), json!(id), String::new())
                    }
                    MenuItemObject::Term { taxonomy, term_id } => {
                        (taxonomy.clone(), json!(term_id), String::new())
                    }
                };
                json!({
                    "id": item.id,
                    "title": item.title,
                    "type": kind,
                    "object_id": object_id,
                    "url": url,
                    "parent": item.parent.filter(|p| ids.contains(p)).unwrap_or(0),
                    "classes": item.classes.join(" "),
                    "new_tab": item.target == "_blank",
                    "attr_title": item.attr_title,
                    "xfn": item.xfn,
                    "description": item.description,
                })
            })
            .collect(),
    )
}

/// The author of a new item of `menu`: the author of its newest item, or the site's first user
/// (the data layer does not know who is signed in, owner item O8).
async fn item_author(db: &Database, menu: &Term) -> Result<i64, Error> {
    let newest: Vec<(i64,)> = raw::fetch_all(
        db,
        "SELECT p.author_id FROM term_relationships r INNER JOIN posts p ON p.id = r.object_id \
         WHERE r.term_taxonomy_id = ? AND p.post_type = ? ORDER BY p.id DESC LIMIT 1",
        &[menu.term_taxonomy_id.into(), NAV_MENU_ITEM.into()],
    )
    .await?;
    if let Some((author,)) = newest.into_iter().next() {
        return Ok(author);
    }
    let first: Option<i64> = raw::fetch_scalar(db, "SELECT MIN(id) FROM users", &[]).await?;
    first.ok_or(Error::NotFound { table: "users" })
}

/// Why a save of the form is refused before anything is written (Ruling R135): a row of the
/// items (the reason shows under the items), or the database's error.
enum Refused {
    Items(String),
    Database(Error),
}

impl From<Error> for Refused {
    fn from(e: Error) -> Self {
        Refused::Database(e)
    }
}

/// `plan`, with a refused row as the items field's message.
fn checked(plan: Result<MenuPlan, Refused>) -> Result<Result<MenuPlan, ValidationError>, Error> {
    match plan {
        Ok(plan) => Ok(Ok(plan)),
        Err(Refused::Items(reason)) => Ok(Err(ValidationError::with_messages([(ITEMS, reason)]))),
        Err(Refused::Database(e)) => Err(e),
    }
}

/// `plan`, refused with [`MENU_NAME_REQUIRED`] under the name when `name` is blank, as
/// `nav-menus.php` answers "Please enter a valid menu name." (the items' own refusal, if any,
/// shows too). Ruling R135: nothing is written, the edit of a menu included, where WordPress
/// would keep the old name and save the rest.
fn named(name: &str, plan: Result<MenuPlan, ValidationError>) -> Result<MenuPlan, ValidationError> {
    if !name.trim().is_empty() {
        return plan;
    }
    let mut messages = vec![(NAME.to_owned(), MENU_NAME_REQUIRED.to_owned())];
    if let Err(refusal) = plan {
        messages.extend(refusal.into_errors());
    }
    Err(ValidationError::with_messages(messages))
}

/// A save's row, or its refusal as a server error, for the data layer's calls without the panel.
fn refused_as_error(saved: Result<Row, ValidationError>) -> Result<Row, Error> {
    saved.map_err(|refusal| refused_save(&refusal.to_string()))
}

impl NavMenusData {
    /// `wp_create_nav_menu`, then the locations and items of the form; a refused row writes
    /// nothing and answers its message under the items.
    async fn create_menu(
        &self,
        db: &Database,
        input: Row,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let name = input
            .get(NAME)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let plan = match named(&name, checked(self.plan(db, None, &input).await)?) {
            Ok(plan) => plan,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let menu = create_nav_menu(db, &name).await.map_err(menu_error)?;
        self.apply(db, &menu, &plan).await?;
        self.row(db, menu.term_id)
            .await?
            .ok_or(Error::NotFound { table: "terms" })
            .map(Ok)
    }

    /// `wp_update_nav_menu_object` for the name, then the locations and items; a refused row
    /// writes nothing and answers its message under the items.
    async fn update_menu(
        &self,
        db: &Database,
        id: i64,
        input: Row,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let Some(mut menu) = get_nav_menu(db, id).await? else {
            return Err(Error::NotFound { table: "terms" });
        };
        // A row without the name keeps it; a blank one (Ignite sends it as `null`) is refused.
        let name = match input.get(NAME) {
            None => menu.name.clone(),
            Some(Value::String(name)) => name.trim().to_owned(),
            Some(_) => String::new(),
        };
        let plan = match named(&name, checked(self.plan(db, Some(&menu), &input).await)?) {
            Ok(plan) => plan,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if let Some(name) = input.get(NAME).and_then(Value::as_str)
            && name.trim() != menu.name
        {
            menu = rename_nav_menu(db, &menu, name.trim())
                .await
                .map_err(menu_error)?;
        }
        self.apply(db, &menu, &plan).await?;
        self.row(db, menu.term_id)
            .await?
            .ok_or(Error::NotFound { table: "terms" })
            .map(Ok)
    }
}

impl RuntimeData for NavMenusData {
    fn list<'a>(&'a self, db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move { self.list_page(db, query).await })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move { self.row(db, id).await })
    }

    /// [`create_menu`](NavMenusData::create_menu); a refused row is a server error. The panel
    /// calls [`try_create_for`](RuntimeData::try_create_for), which shows it on the form.
    fn create<'a>(&'a self, db: &'a Database, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move { refused_as_error(self.create_menu(db, input).await?) })
    }

    /// [`update_menu`](NavMenusData::update_menu); a refused row is a server error. The panel
    /// calls [`try_update_for`](RuntimeData::try_update_for), which shows it on the form.
    fn update<'a>(&'a self, db: &'a Database, id: i64, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move { refused_as_error(self.update_menu(db, id, input).await?) })
    }

    fn try_create_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move { self.create_menu(db, input).await })
    }

    fn try_update_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        id: i64,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move { self.update_menu(db, id, input).await })
    }

    /// `wp_delete_nav_menu`.
    fn delete<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, u64> {
        Box::pin(async move {
            let Some(menu) = get_nav_menu(db, id).await? else {
                return Ok(0);
            };
            let deleted = delete_nav_menu(db, self.theme.slug(), &menu)
                .await
                .map_err(menu_error)?;
            Ok(u64::from(deleted))
        })
    }
}

/// How many items a box of the menu builder's "Add menu items" column lists at a time, as
/// `wp_nav_menu_item_post_type_meta_box` and `wp_nav_menu_item_taxonomy_meta_box` page them
/// (`$per_page = 50`).
pub const MENU_SOURCE_PER_PAGE: i64 = 50;

/// One post or term a box of the "Add menu items" column lists.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MenuSourceItem {
    /// The post id, or the term's `term_id` (the item's `object_id`).
    pub id: i64,
    /// The post's title or the term's name; a post without a title is `#<id> (no title)`, as
    /// WordPress lists it.
    pub title: String,
    /// The post's status (`publish`, `trash`, ...); empty for a term.
    pub status: String,
}

/// A page of a box's items, and whether there are more ("Load more").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MenuSourcePage {
    /// The items, in the box's order.
    pub items: Vec<MenuSourceItem>,
    /// Whether another page follows.
    pub more: bool,
}

/// What to list in a box of the "Add menu items" column.
#[derive(Debug, Clone, Default)]
pub struct MenuSourceQuery<'a> {
    /// Words the title (or the term's name) must contain, as `_wp_ajax_menu_quick_search`
    /// searches the title only.
    pub search: Option<&'a str>,
    /// The page, from 1.
    pub page: i64,
    /// Only these ids, whatever their status (the builder reads the titles of a menu's items,
    /// and marks those whose post is gone or trashed "Invalid"). Empty lists the box.
    pub include: &'a [i64],
}

/// The items of `kind` for the menu builder's boxes, as `wp_nav_menu_item_post_type_meta_box`
/// and `wp_nav_menu_item_taxonomy_meta_box` list them: the published posts of a post type
/// (pages by title, as "View All" orders them; other types newest first, as "Most Recent"), or
/// every term of a taxonomy by name (`hide_empty` false), [`MENU_SOURCE_PER_PAGE`] at a time.
/// A custom link lists nothing.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn menu_source_items(
    db: &Database,
    kind: &MenuObjectType,
    query: &MenuSourceQuery<'_>,
) -> Result<MenuSourcePage, Error> {
    let per_page = MENU_SOURCE_PER_PAGE;
    let offset = query.page.max(1).saturating_sub(1).saturating_mul(per_page);
    let search = query
        .search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let s: String = s.chars().take(200).collect();
            format!("%{}%", escape_like(&s.to_lowercase()))
        });
    let include: Vec<i64> = query
        .include
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .take(MAX_MENU_ITEMS)
        .collect();
    let (sql, mut binds): (String, Vec<Sql>) = match kind {
        MenuObjectType::Custom => {
            return Ok(MenuSourcePage {
                items: Vec::new(),
                more: false,
            });
        }
        MenuObjectType::PostType(post_type) => {
            let mut filter = String::from("post_type = ?");
            let mut binds: Vec<Sql> = vec![post_type.as_str().into()];
            if include.is_empty() {
                filter.push_str(" AND status = ?");
                binds.push(PostStatus::Publish.as_str().into());
            }
            if let Some(search) = &search {
                filter.push_str(" AND LOWER(title) LIKE ? ESCAPE '!'");
                binds.push(search.clone().into());
            }
            if !include.is_empty() {
                filter.push_str(&format!(
                    " AND id IN ({})",
                    vec!["?"; include.len()].join(", ")
                ));
                binds.extend(include.iter().map(|id| Sql::from(*id)));
            }
            let order = if post_type == "page" {
                "title ASC, id ASC"
            } else {
                "published_at DESC, id DESC"
            };
            (
                format!(
                    "SELECT id, title, status FROM posts WHERE {filter} ORDER BY {order} \
                     LIMIT ? OFFSET ?"
                ),
                binds,
            )
        }
        MenuObjectType::Taxonomy(taxonomy) => {
            let mut filter = String::from("tt.taxonomy = ?");
            let mut binds: Vec<Sql> = vec![taxonomy.as_str().into()];
            if let Some(search) = &search {
                filter.push_str(" AND LOWER(t.name) LIKE ? ESCAPE '!'");
                binds.push(search.clone().into());
            }
            if !include.is_empty() {
                filter.push_str(&format!(
                    " AND t.id IN ({})",
                    vec!["?"; include.len()].join(", ")
                ));
                binds.extend(include.iter().map(|id| Sql::from(*id)));
            }
            (
                format!(
                    "SELECT t.id, t.name, '' FROM term_taxonomy tt \
                     INNER JOIN terms t ON t.id = tt.term_id WHERE {filter} \
                     ORDER BY t.name ASC, t.id ASC LIMIT ? OFFSET ?"
                ),
                binds,
            )
        }
    };
    let limit = if include.is_empty() {
        per_page + 1
    } else {
        i64::try_from(include.len()).unwrap_or(i64::MAX)
    };
    binds.push(limit.into());
    binds.push(if include.is_empty() { offset } else { 0 }.into());
    let mut rows: Vec<(i64, String, String)> = raw::fetch_all(db, &sql, &binds).await?;
    let more = include.is_empty() && rows.len() > usize::try_from(per_page).unwrap_or(usize::MAX);
    if more {
        rows.truncate(usize::try_from(per_page).unwrap_or(usize::MAX));
    }
    Ok(MenuSourcePage {
        items: rows
            .into_iter()
            .map(|(id, title, status)| MenuSourceItem {
                id,
                title: if title.trim().is_empty() {
                    format!("#{id} (no title)")
                } else {
                    title
                },
                status,
            })
            .collect(),
        more,
    })
}

/// The most menus the "Select a menu to edit" dropdown lists.
pub const MAX_MENU_CHOICES: i64 = 1000;

/// Every menu (term id and name), by name, as `nav-menus.php`'s "Select a menu to edit"
/// dropdown lists them (`wp_get_nav_menus`), at most [`MAX_MENU_CHOICES`].
///
/// # Errors
///
/// Fails with the database's error.
pub async fn nav_menu_choices(db: &Database) -> Result<Vec<(i64, String)>, Error> {
    raw::fetch_all(
        db,
        "SELECT t.id, t.name FROM term_taxonomy tt INNER JOIN terms t ON t.id = tt.term_id \
         WHERE tt.taxonomy = ? ORDER BY t.name ASC, t.id ASC LIMIT ?",
        &[(&*Taxonomy::NAV_MENU.name).into(), MAX_MENU_CHOICES.into()],
    )
    .await
}
