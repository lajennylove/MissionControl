//! The admin's Menus screen (Task 6.5, design 5.10, Ruling R133): WordPress's `nav-menus.php`
//! as the Ignite runtime resource [`NAV_MENUS`], over the `nav_menu` terms and their items
//! through the menus API ([`crate::theme`]'s `save_menu_items` and the others). Nothing here
//! needs Rocket: the `mission-control` crate registers the resource and checks
//! [`authorize_menus_screen`] before Ignite's routes run.
//!
//! - The list: each menu's name, the theme locations that show it, and its item count; the
//!   search matches the name.
//! - New (Ignite's create form) and edit: the menu's name (unique among menus, as
//!   `wp_update_nav_menu_object` checks it; a taken name shows "Menu Name is already taken."),
//!   one checkbox per menu location the active theme registers, and the items as a repeater:
//!   each row a label, a type (a custom link, a post of a post type, a term of a taxonomy; text,
//!   so a stored item of a type the screen no longer offers still saves), the object id or the
//!   URL, the parent, CSS classes and "Open link in a new tab".
//! - An item whose post or term is gone (trashed, deleted) is kept as it is when its row is
//!   saved unchanged, as WordPress keeps it ("Invalid"); the site leaves it out.
//! - Saving the items is `wp_nav_menu_update_menu_items`: rows are saved in order, a row with
//!   the id of one of the menu's items updates it, and the menu's items no row names are deleted.
//!   A repeater row cannot name another row's item before it is saved, so the parent is the
//!   parent's **row number** (1 for the first row; 0 or blank for the top level). Drag-and-drop
//!   ordering is a follow-up.
//! - Ruling R135 (interim for owner item O10): everything is checked before anything is written.
//!   A row that links to nothing (a post that is missing, trashed, an auto-draft or of another
//!   type; a term that is missing), or more than [`MAX_MENU_ITEMS`] rows, refuses the whole save:
//!   nothing is written, the reason is logged, and Ignite shows its error page (it cannot show the
//!   message on the form yet). WordPress's screen only offers existing objects.
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
    ColumnDef, Direction, FieldDef, FieldKindDef, FormLayout, Group, ListQuery, Page, Row,
    RuntimeData, RuntimeError, RuntimeFuture, RuntimeResource, Section,
};
use orbit_orm::{Database, Error, Model, Value as Sql, raw};
use serde_json::{Value, json};

use super::posts::{PostScreen, Refusal};
use super::settings::refused_save;
use crate::content::{MetaError, Post, PostStatus, Taxonomy, Term, TermError, terms_by_term_id};
use crate::front::escape_like;
use crate::theme::{
    MAX_MENU_ITEMS, MenuError, MenuItemInput, MenuItemObject, NAV_MENU_ITEM, StoredMenuItem, Theme,
    create_nav_menu, delete_nav_menu, get_nav_menu, menu_items, menu_locations, rename_nav_menu,
    save_nav_menu,
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

/// The name field.
const NAME: &str = "name";

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
    let mut fields = vec![field(NAME, "Menu Name", FieldKindDef::Text, true)];
    for (name, _, description) in &locations {
        fields.push(field(
            name,
            &format!("Display location: {description}"),
            FieldKindDef::Checkbox,
            false,
        ));
    }
    // A text field, not a select: a stored item of a type the screen no longer offers (a custom
    // type registered without `show_in_nav_menus`, or removed) must still save unchanged, as
    // WordPress keeps such items. The data layer checks the value (see `item_inputs`).
    let type_label = format!(
        "Type: {}",
        types
            .iter()
            .map(|(t, label)| format!("{} ({label})", t.value()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    fields.push(field(
        ITEMS,
        "Menu items (one row per item, in order)",
        FieldKindDef::Repeater(vec![
            field(
                "id",
                "Item ID (blank for a new item)",
                FieldKindDef::Number,
                false,
            ),
            field("title", "Navigation Label", FieldKindDef::Text, false),
            field("type", &type_label, FieldKindDef::Text, true),
            field(
                "object_id",
                "Post or term ID (not for a custom link)",
                FieldKindDef::Number,
                false,
            ),
            field("url", "URL (custom link)", FieldKindDef::Text, false),
            field(
                "parent",
                "Parent row number (0 for none)",
                FieldKindDef::Number,
                false,
            ),
            field(
                "classes",
                "CSS Classes (optional)",
                FieldKindDef::Text,
                false,
            ),
            field(
                "new_tab",
                "Open link in a new tab",
                FieldKindDef::Checkbox,
                false,
            ),
        ]),
        false,
    ));
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
    // Two equal columns, as the settings screens: the menu's name and display locations
    // (WordPress's Menu Settings) on the left, its items (Menu Structure) on the right.
    let settings: Vec<String> = resource
        .fields
        .iter()
        .filter(|f| f.name == NAME || f.name.starts_with(LOCATION_PREFIX))
        .map(|f| f.name.clone())
        .collect();
    resource.form_layout = FormLayout::new().columns(2).schema([
        Group::make([Section::make("Menu Settings").schema(settings)]).column_span(1),
        Group::make([Section::make("Menu Structure").schema([ITEMS])]).column_span(1),
    ]);
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
    resource
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
        Ok(Some(row))
    }

    /// What a save of the form changes, checked before anything is written (Ruling R135): the
    /// location changes, and the items when the form sent them.
    async fn plan(
        &self,
        db: &Database,
        menu: Option<&Term>,
        input: &Row,
    ) -> Result<MenuPlan, Error> {
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
        Ok(MenuPlan { locations, items })
    }

    /// Saves `plan` on `menu`: its items and its locations, in one transaction
    /// ([`save_nav_menu`]).
    async fn apply(&self, db: &Database, menu: &Term, plan: &MenuPlan) -> Result<(), Error> {
        let author = item_author(db, menu).await?;
        save_nav_menu(
            db,
            menu,
            author,
            plan.items.as_deref(),
            Some((&self.theme, &plan.locations)),
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
    /// offer, or more rows than [`MAX_MENU_ITEMS`], refuses the whole save, with the reason logged.
    async fn item_inputs(
        &self,
        db: &Database,
        rows: &[Value],
        stored: &[StoredMenuItem],
    ) -> Result<Vec<MenuItemInput>, Error> {
        if rows.len() > MAX_MENU_ITEMS {
            let reason = format!("a menu holds at most {MAX_MENU_ITEMS} items");
            log::warn!("The menu was not saved: {reason}");
            return Err(refused_save(&reason));
        }
        let read: Vec<RowItem> = rows.iter().map(RowItem::read).collect();
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
        let valid = linked(db, &objects).await?;
        let refused: Vec<String> = (0..read.len())
            .filter(|index| unchanged[*index].is_none() && !valid.contains(index))
            .map(|index| format!("item row {} links to nothing", index + 1))
            .collect();
        if !refused.is_empty() {
            let reason = refused.join("; ");
            log::warn!("The menu was not saved: {reason}");
            return Err(refused_save(&reason));
        }
        Ok(read
            .iter()
            .zip(objects)
            .map(|(item, object)| MenuItemInput {
                id: item.id,
                object: object.expect("checked above"),
                title: item.title.clone(),
                // The parent's row number, 1-based; `save_nav_menu` clears one that is the item
                // itself, out of range, or on a loop.
                parent: item
                    .parent
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .map(|n| n - 1),
                target: if item.new_tab { "_blank" } else { "" }.to_owned(),
                classes: vec![item.classes.clone()],
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
}

/// One repeater row, read.
struct RowItem {
    id: Option<i64>,
    kind: String,
    object_id: Option<i64>,
    url: String,
    title: String,
    parent: Option<i64>,
    classes: String,
    new_tab: bool,
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
            kind: text("type"),
            object_id: number("object_id"),
            url: text("url"),
            title: text("title"),
            parent: number("parent"),
            classes: text("classes"),
            new_tab: matches!(row.get("new_tab"), Some(Value::Bool(true))),
        }
    }
}

/// The repeater rows of `items`: each item's id, label, type, object id or URL, parent row
/// number (0 at the top level), classes and new-tab flag.
fn item_rows(items: &[StoredMenuItem]) -> Value {
    let row_of: HashMap<i64, usize> = items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id, index + 1))
        .collect();
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
                    "parent": item.parent.and_then(|p| row_of.get(&p)).copied().unwrap_or(0),
                    "classes": item.classes.join(" "),
                    "new_tab": item.target == "_blank",
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

impl RuntimeData for NavMenusData {
    fn list<'a>(&'a self, db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move { self.list_page(db, query).await })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move { self.row(db, id).await })
    }

    /// `wp_create_nav_menu`, then the locations and items of the form.
    fn create<'a>(&'a self, db: &'a Database, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            let name = input
                .get(NAME)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            let plan = self.plan(db, None, &input).await?;
            let menu = create_nav_menu(db, &name).await.map_err(menu_error)?;
            self.apply(db, &menu, &plan).await?;
            self.row(db, menu.term_id)
                .await?
                .ok_or(Error::NotFound { table: "terms" })
        })
    }

    /// `wp_update_nav_menu_object` for the name, then the locations and items.
    fn update<'a>(&'a self, db: &'a Database, id: i64, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            let Some(mut menu) = get_nav_menu(db, id).await? else {
                return Err(Error::NotFound { table: "terms" });
            };
            let plan = self.plan(db, Some(&menu), &input).await?;
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
        })
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
