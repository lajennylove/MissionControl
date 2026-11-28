//! The admin's Categories and Tags screens: WordPress's Posts > Categories and Posts > Tags
//! (`edit-tags.php?taxonomy=category` and `?taxonomy=post_tag`) as the Ignite runtime resources
//! [`CATEGORY_SCREEN`] and [`POST_TAG_SCREEN`], over the terms API ([`Term`]). Nothing here needs
//! Rocket: the `mission-control` crate registers the resources ([`register_term_resources`]) and
//! grants their abilities.
//!
//! - The list: each term's name, description, slug and count, as `WP_Terms_List_Table` shows
//!   them. The search matches the name and the slug, as `get_terms`' `search` does. Categories
//!   are listed as a tree (children under their parent, the name prefixed with one "— " per
//!   level) when the list is neither searched nor sorted, as WordPress shows them; otherwise, and
//!   always for tags, the list is flat, ordered by name unless a column is sorted.
//! - New and edit: the name (required), the slug, the parent (categories only, as tags are flat)
//!   and the description. The rules are the terms API's, as `wp_insert_term` and
//!   `wp_update_term` apply them: an empty slug is made from the name; a new term's taken slug
//!   is made unique (`rust-2`), and a taken slug given on edit is refused; a name that exists
//!   under the same parent (anywhere, for tags) is refused on create. The refusals show on the
//!   form, as Ignite shows a unique violation: "Name is already taken." and "Slug is already
//!   taken."; a rename to a name another term has there is refused the same way. A name longer
//!   than 200 characters (the `terms.name` column) is refused on the field ([`NAME_TOO_LONG`]).
//!   The name passes through `sanitize_text_field` and the description through
//!   `wp_filter_kses`, for every user, as the REST API stores them.
//! - Delete is `wp_delete_term`: the term leaves the posts that had it, its children move to its
//!   parent, and a post left with no category gets the default one. The default category (the
//!   `default_category` option, WordPress's "Uncategorized") is never deleted: its row is marked
//!   [`DEFAULT_TERM_KEY`], so Ignite hides its Delete button and refuses its delete (403) for
//!   every user (`hide_default_delete_when`, which binds a super-admin too, unlike the
//!   policy), and the app's fairing answers a direct request with WordPress's "Sorry, you are not
//!   allowed to delete this item." ([`authorize_term_screen`]).
//! - A category is not its own parent: the edit form's Parent Category leaves the category and
//!   its descendants out (the app's form fairing, as `wp_dropdown_categories`' `exclude_tree`),
//!   and a save that names it is refused on the field ([`OWN_PARENT_CATEGORY`]).
//! - Who may use them ([`TermsPolicy`]): the list needs the taxonomy's `manage_terms`
//!   (`manage_categories` for categories, `manage_post_tags` for tags, which `map_meta_cap` maps
//!   to `manage_categories`); New needs `edit_terms`; editing a term `edit_term` and deleting it
//!   `delete_term`, mapped as `map_meta_cap` maps them. With WordPress's roles, administrators
//!   and editors hold `manage_categories`.
//!
//! - The custom taxonomies in use (stored in the Taxonomies manager, or registered in code or by
//!   a plugin) get the same screen at `/mc-admin/<taxonomy>`, under Posts after Categories and
//!   Tags, with their labels and capabilities ([`TaxonomyScreens`], Task 5.7).
//!
//! Not served yet, because Ignite 0.6.3 has no list bulk actions: WordPress's bulk Delete.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_core::{
    ActingUser, ColumnDef, Direction, FieldDef, FieldKindDef, ListFilter, ListQuery, Page, Policy,
    Row, RuntimeData, RuntimeError, RuntimeFuture, RuntimeResource, ValidationError,
};
use orbit_orm::{Database, Error, Value as Sql, raw};
use serde_json::{Value, json};

use super::navigation;
use super::posts::{PostScreen, Refusal};
use super::settings::refused_save;
use crate::content::{
    MAX_FILTERED_BYTES, MAX_TERM_LENGTH, NewTerm, Taxonomy, Term, TermChanges, TermError,
    default_term_ids, kses_data, sanitize_text_field, terms_by_term_id,
};
use crate::front::escape_like;
use crate::rest::{KsesLimiter, id_of};
use crate::types::{Access, Registry, TaxonomyDef};
use crate::users::{Object, TaxonomyCaps, TermContext, UserCaps, can, map_meta_cap};

/// The slug of the Categories screen: the taxonomy's name, as `edit-tags.php?taxonomy=category`.
pub const CATEGORY_SCREEN: &str = "category";

/// The slug of the Tags screen: the taxonomy's name, as `edit-tags.php?taxonomy=post_tag`.
pub const POST_TAG_SCREEN: &str = "post_tag";

/// Both screens' slugs.
pub const TERM_SCREENS: [&str; 2] = [CATEGORY_SCREEN, POST_TAG_SCREEN];

/// Where Categories is listed in the sidebar: under Posts, in its Content group, as WordPress's
/// Posts submenu (`edit.php`, at 15).
pub const CATEGORIES_SORT: i32 = navigation::POSTS + 1;

/// Where Tags is listed: under Posts, after Categories, as WordPress's Posts submenu (at 16).
pub const TAGS_SORT: i32 = navigation::POSTS + 2;

/// The most terms a page of the list shows.
const MAX_PER_PAGE: i64 = 100;

/// The longest search, in characters.
const MAX_SEARCH: usize = 200;

const NAME: &str = "name";
const SLUG: &str = "slug";
const PARENT: &str = "parent";
const DESCRIPTION: &str = "description";

const COL_NAME: &str = "col_name";
const COL_DESCRIPTION: &str = "col_description";
const COL_SLUG: &str = "col_slug";
const COL_COUNT: &str = "col_count";

/// The row key that marks the taxonomy's default term (the `default_category` option), whose
/// Delete the resource hides and the panel refuses (`hide_default_delete_when`), for every user,
/// a super-admin too, as `wp_delete_term` never deletes it.
pub const DEFAULT_TERM_KEY: &str = "default_term";

/// The refusal of a category chosen as its own parent, shown on the Parent Category field (the
/// edit form's dropdown leaves the category and its descendants out, as `term.php`'s
/// `wp_dropdown_categories` does with `exclude_tree`).
pub const OWN_PARENT_CATEGORY: &str = "A category cannot be its own parent.";

/// The refusal of a name longer than [`MAX_TERM_LENGTH`] characters, on the Name field. WordPress
/// has no message of its own for it: its limit is the size of the `terms.name` column
/// (`varchar(200)`), which the database enforces.
pub const NAME_TOO_LONG: &str = "Name must not exceed 200 characters.";

/// The refusal of a name another term has, on the Name field: Ignite's message for a unique
/// violation, as the create form shows it.
pub const NAME_TAKEN: &str = "Name is already taken.";

/// The taxonomy a term screen's slug serves, if `slug` is one.
pub fn term_screen_taxonomy(slug: &str) -> Option<Taxonomy> {
    match slug {
        CATEGORY_SCREEN => Some(Taxonomy::CATEGORY),
        POST_TAG_SCREEN => Some(Taxonomy::POST_TAG),
        _ => None,
    }
}

/// Why the request `screen` on the term screen `slug` is refused before Ignite's routes run, or
/// `None`: Delete on the default category (the `default_category` option) is refused with
/// [`Refusal::DefaultCategory`], before anything changes, as `wp_delete_term` never deletes it.
/// The other requests are left to the screen's [`TermsPolicy`].
///
/// # Errors
///
/// Fails with the database's error.
pub async fn authorize_term_screen(
    db: &Database,
    slug: &str,
    screen: PostScreen,
) -> Result<Option<Refusal>, Error> {
    let PostScreen::Delete(term_id) = screen else {
        return Ok(None);
    };
    if slug != CATEGORY_SCREEN {
        return Ok(None);
    }
    let defaults = default_term_ids(db, &Taxonomy::CATEGORY.name).await?;
    Ok(defaults
        .contains(&term_id)
        .then_some(Refusal::DefaultCategory))
}

/// The screen of `taxonomy` (categories or tags) as an Ignite runtime resource, its
/// description filtered under `kses`.
pub fn terms_resource(taxonomy: &Taxonomy, kses: KsesLimiter) -> RuntimeResource {
    let category = taxonomy.name == Taxonomy::CATEGORY.name;
    let screen = if category {
        TermScreen {
            slug: CATEGORY_SCREEN.to_owned(),
            title: "Categories".to_owned(),
            singular: "Category".to_owned(),
            parent_label: "Parent Category".to_owned(),
            sort: CATEGORIES_SORT,
            caps: TaxonomyCaps::for_taxonomy(&taxonomy.name),
        }
    } else {
        TermScreen {
            slug: POST_TAG_SCREEN.to_owned(),
            title: "Tags".to_owned(),
            singular: "Tag".to_owned(),
            parent_label: String::new(),
            sort: TAGS_SORT,
            caps: TaxonomyCaps::for_taxonomy(&taxonomy.name),
        }
    };
    screen_resource(taxonomy, screen, kses)
}

/// What a term screen shows besides its taxonomy: its slug, its names, its place in the
/// sidebar and its taxonomy's capabilities.
#[derive(Debug, Clone)]
struct TermScreen {
    slug: String,
    title: String,
    singular: String,
    /// The Parent field's label, for a hierarchical taxonomy.
    parent_label: String,
    sort: i32,
    caps: TaxonomyCaps,
}

/// The term screen of a stored or registered custom taxonomy `def` (Task 5.7, design gap M5),
/// as `edit-tags.php?taxonomy=genre`: at `/mc-admin/<taxonomy>`, under Posts after Categories
/// and Tags (at `sort`), with the taxonomy's labels and capabilities.
pub fn custom_terms_resource(def: &TaxonomyDef, kses: KsesLimiter, sort: i32) -> RuntimeResource {
    let title = if def.labels.name.is_empty() {
        def.slug.clone()
    } else {
        def.labels.name.clone()
    };
    let singular = if def.labels.singular_name.is_empty() {
        title.clone()
    } else {
        def.labels.singular_name.clone()
    };
    let parent_label = def
        .labels
        .parent_item
        .clone()
        .unwrap_or_else(|| format!("Parent {singular}"));
    let screen = TermScreen {
        slug: def.slug.clone(),
        title,
        singular,
        parent_label,
        sort,
        caps: def.caps(),
    };
    screen_resource(&def.to_taxonomy(), screen, kses)
}

/// The term screen `screen` of `taxonomy`.
fn screen_resource(taxonomy: &Taxonomy, screen: TermScreen, kses: KsesLimiter) -> RuntimeResource {
    let field = |name: &str, label: &str, kind: FieldKindDef, required: bool| FieldDef {
        name: name.to_owned(),
        label: label.to_owned(),
        kind,
        required,
    };
    let column = |key: &str, label: &str, sortable: bool, searchable: bool| ColumnDef {
        key: key.to_owned(),
        label: label.to_owned(),
        sortable,
        searchable,
    };
    let mut fields = vec![
        field(NAME, "Name", FieldKindDef::Text, true),
        field(SLUG, "Slug", FieldKindDef::Text, false),
    ];
    if taxonomy.hierarchical {
        fields.push(field(
            PARENT,
            &screen.parent_label,
            FieldKindDef::Lookup,
            false,
        ));
    }
    fields.push(field(
        DESCRIPTION,
        "Description",
        FieldKindDef::Textarea,
        false,
    ));
    let mut resource = super::stay_on_edit(RuntimeResource::new(
        screen.slug.clone(),
        screen.title.clone(),
        screen.singular.clone(),
        TermsData {
            taxonomy: taxonomy.clone(),
            screen: screen.slug.clone(),
            kses,
        },
    ));
    resource.fields = fields;
    resource.columns = vec![
        column(COL_NAME, "Name", true, true),
        column(COL_DESCRIPTION, "Description", true, false),
        column(COL_SLUG, "Slug", true, true),
        column(COL_COUNT, "Count", true, false),
    ];
    // The default term has no Delete, and the panel refuses its delete (403) before the data
    // layer runs, whoever asks: the policy alone does not stop a super-admin.
    resource.hide_default_delete_when =
        Some(ListFilter::new().where_eq(DEFAULT_TERM_KEY, Value::Bool(true)));
    navigation::place(&mut resource, screen.sort, navigation::POST_ICON);
    // Categories, Tags and the custom taxonomies are in Posts' submenu (WordPress's
    // `edit.php`), in the Content group with it.
    resource.navigation_parent_item = Some(navigation::POSTS_PARENT.to_owned());
    // `edit-tags.php` needs the taxonomy's `manage_terms`, a new term and an edit its
    // `edit_terms`, and Delete its `delete_terms`, each as `map_meta_cap` maps it to a
    // primitive capability (`manage_categories` for categories and tags).
    let caps = &screen.caps;
    let nobody = UserCaps::from_roles(0, &[], &Default::default());
    let primitive = |cap: &str| {
        map_meta_cap(cap, &nobody, None)
            .into_iter()
            .next()
            .unwrap_or_else(|| cap.to_owned())
    };
    let (manage, edit, delete) = (
        primitive(&caps.manage_terms),
        primitive(&caps.edit_terms),
        primitive(&caps.delete_terms),
    );
    super::ScreenCaps {
        view: &manage,
        create: &edit,
        update: &edit,
        delete: &delete,
    }
    .apply(resource)
}

/// Registers the Categories and Tags screens with Ignite, each with its [`TermsPolicy`] reading
/// users' capabilities through `access`. Returns the slugs registered, and Ignite's refusal of
/// each other one (such as its slug taken by another registration, whose policy is then left
/// alone).
pub fn register_term_resources(access: &Access) -> (Vec<&'static str>, Vec<RuntimeError>) {
    let mut registered = Vec::new();
    let mut refused = Vec::new();
    for (slug, taxonomy) in [
        (CATEGORY_SCREEN, Taxonomy::CATEGORY),
        (POST_TAG_SCREEN, Taxonomy::POST_TAG),
    ] {
        match ignite_core::register_runtime(terms_resource(&taxonomy, access.kses.clone())) {
            Ok(()) => {
                ignite_core::register_policy(slug, TermsPolicy::new(taxonomy, access.clone()));
                registered.push(slug);
            }
            Err(e) => refused.push(e),
        }
    }
    (registered, refused)
}

/// The term screens of the custom taxonomies in use (Task 5.7, design gap M5): each one of the
/// [`Registry`] whose `show_ui` is on gets its screen at `/mc-admin/<taxonomy>`, under Posts
/// after Categories and Tags, with its [`TermsPolicy`]. Call [`sync`](Self::sync) at start and
/// after every change to a taxonomy; it registers them again, as
/// [`PostTypeResources::sync`](crate::types::PostTypeResources::sync) does the post types'.
///
/// WordPress lists a taxonomy's screen under each post type it is attached to; the owner's rule
/// for Task 5.7 lists them under Posts, as Categories and Tags are.
pub struct TaxonomyScreens {
    registry: Arc<Registry>,
    access: Access,
    /// The slugs this value registered. Held only by synchronous code.
    registered: Mutex<Vec<String>>,
}

impl std::fmt::Debug for TaxonomyScreens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaxonomyScreens")
            .field("registered", &self.registered())
            .finish_non_exhaustive()
    }
}

impl TaxonomyScreens {
    /// The screens of the custom taxonomies of `registry`, their users' capabilities read
    /// through `access`; nothing is registered until [`sync`](Self::sync).
    pub fn new(registry: Arc<Registry>, access: Access) -> Self {
        Self {
            registry,
            access,
            registered: Mutex::new(Vec::new()),
        }
    }

    /// Registers the screen of every custom taxonomy in use whose `show_ui` is on, replacing
    /// those this value registered before, and unregisters the screens of the taxonomies that
    /// went. Returns Ignite's refusals (a slug another screen holds, such as a post type's).
    pub fn sync(&self) -> Vec<RuntimeError> {
        let mut registered = self
            .registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for slug in registered.iter() {
            let _ = ignite_core::unregister_runtime(slug);
            ignite_core::unregister_policy(slug);
        }
        registered.clear();
        let mut refused = Vec::new();
        let taxonomies = self
            .registry
            .all_taxonomies()
            .into_iter()
            .filter(TaxonomyDef::effective_show_ui);
        for (sort, def) in (TAGS_SORT + 1..).zip(taxonomies) {
            if ignite_core::runtime_resource(&def.slug).is_some()
                || ignite_core::policy(&def.slug).is_some()
            {
                refused.push(RuntimeError::SlugTaken(def.slug.clone()));
                continue;
            }
            ignite_core::register_policy(
                def.slug.clone(),
                TermsPolicy::with_caps(def.to_taxonomy(), def.caps(), self.access.clone()),
            );
            match ignite_core::register_runtime(custom_terms_resource(
                &def,
                self.access.kses.clone(),
                sort,
            )) {
                Ok(()) => registered.push(def.slug.clone()),
                Err(e) => {
                    ignite_core::unregister_policy(&def.slug);
                    refused.push(e);
                }
            }
        }
        refused
    }

    /// The taxonomies whose screen this value has registered, in order.
    pub fn registered(&self) -> Vec<String> {
        self.registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Where a term screen's records live: the terms of one taxonomy, by `term_id`. See the
/// [module](self).
#[derive(Clone)]
pub struct TermsData {
    taxonomy: Taxonomy,
    /// The screen's slug: the taxonomy's name.
    screen: String,
    kses: KsesLimiter,
}

/// The error Ignite shows on the form as "<label> is already taken.": a violation of the index
/// its rule names (`{slug}_{field}_unique`).
fn taken(screen: &str, field: &str) -> Error {
    Error::UniqueViolation {
        index: Some(format!("{screen}_{field}_unique")),
    }
}

/// A text value of the form's row: `None` when the key is missing, `Some("")` when it is null.
fn text(input: &Row, key: &str) -> Option<String> {
    match input.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => Some(String::new()),
        other => Some(other.to_string()),
    }
}

impl TermsData {
    /// The screen's slug.
    fn screen(&self) -> &str {
        &self.screen
    }

    /// The Ignite error for a refusal of the terms API: the name or slug taken show on the form;
    /// the other refusals are logged and answered with Ignite's error page.
    fn term_error(&self, e: TermError) -> Error {
        match e {
            TermError::Exists { .. } => taken(self.screen(), NAME),
            TermError::DuplicateSlug { .. } => taken(self.screen(), SLUG),
            TermError::Database(e) => e,
            other => {
                log::warn!("The {} term was not saved: {other}", self.taxonomy.name);
                refused_save(&other.to_string())
            }
        }
    }

    /// The term with this `term_id`, if it is one of the taxonomy's.
    async fn term(&self, db: &Database, term_id: i64) -> Result<Option<Term>, Error> {
        Ok(terms_by_term_id(db, &self.taxonomy, &[term_id])
            .await?
            .pop())
    }

    /// The name and description as they are stored: `sanitize_text_field` and `wp_filter_kses`,
    /// as the REST API filters them for every user, on a blocking thread behind the limiter.
    async fn sanitize(
        &self,
        name: Option<String>,
        description: Option<String>,
    ) -> Result<(Option<String>, Option<String>), Error> {
        for (field, value) in [(NAME, &name), (DESCRIPTION, &description)] {
            if value.as_ref().is_some_and(|v| v.len() > MAX_FILTERED_BYTES) {
                return Err(refused_save(&format!("the {field} is too long")));
            }
        }
        self.kses
            .run(move || {
                (
                    name.map(|n| sanitize_text_field(&n)),
                    description.map(|d| kses_data(&d)),
                )
            })
            .await
            .map_err(|e| refused_save(&e.to_string()))
    }

    /// The parent the form asks for: `None` when it sent none (or the taxonomy is flat),
    /// `Some(None)` for "None", and `Some(Some(term_id))`.
    fn parent(&self, input: &Row) -> Option<Option<i64>> {
        if !self.taxonomy.hierarchical {
            return None;
        }
        let value = input.get(PARENT)?;
        Some(id_of(value).filter(|id| *id > 0))
    }

    /// The categories in tree order with their depth: each term followed by its children, by
    /// name, as WordPress lists them. A term whose parent is missing is listed at the top level.
    fn tree(terms: Vec<Term>) -> Vec<(Term, usize)> {
        let ids: BTreeSet<i64> = terms.iter().map(|t| t.term_id).collect();
        let mut children: HashMap<Option<i64>, Vec<Term>> = HashMap::new();
        for term in terms {
            let parent = term.parent_id.filter(|p| ids.contains(p));
            children.entry(parent).or_default().push(term);
        }
        let mut out = Vec::new();
        let mut stack: Vec<(Term, usize)> = children
            .remove(&None)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .map(|t| (t, 0))
            .collect();
        while let Some((term, depth)) = stack.pop() {
            if let Some(kids) = children.remove(&Some(term.term_id)) {
                stack.extend(kids.into_iter().rev().map(|t| (t, depth + 1)));
            }
            out.push((term, depth));
        }
        // Terms on a parent loop never reach the top level: listed last, so none is lost.
        let mut rest: Vec<Term> = children.into_values().flatten().collect();
        rest.sort_by_cached_key(|t| (t.name.to_lowercase(), t.term_id));
        out.extend(rest.into_iter().map(|t| (t, 0)));
        out
    }

    /// The list's row of `term`, its name prefixed with one "— " per level of `depth`, marked
    /// [`DEFAULT_TERM_KEY`] when its id is one of `defaults`.
    fn row_of(term: &Term, depth: usize, defaults: &[i64]) -> Row {
        let mut row = Row::new();
        row.insert("id".into(), json!(term.term_id));
        row.insert(
            DEFAULT_TERM_KEY.into(),
            json!(defaults.contains(&term.term_id)),
        );
        row.insert(NAME.into(), json!(term.name));
        row.insert(SLUG.into(), json!(term.slug));
        row.insert(DESCRIPTION.into(), json!(term.description));
        row.insert(
            PARENT.into(),
            json!(term.parent_id.map(|p| p.to_string()).unwrap_or_default()),
        );
        row.insert(
            COL_NAME.into(),
            json!(format!("{}{}", "— ".repeat(depth), term.name)),
        );
        row.insert(COL_DESCRIPTION.into(), json!(term.description));
        row.insert(COL_SLUG.into(), json!(term.slug));
        row.insert(COL_COUNT.into(), json!(term.count.to_string()));
        row
    }

    async fn list_page(&self, db: &Database, query: &ListQuery) -> Result<Page<Row>, Error> {
        let per_page = query.per_page.clamp(1, MAX_PER_PAGE);
        let page = query.page.max(1);
        let offset = page.saturating_sub(1).saturating_mul(per_page);
        let search = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(MAX_SEARCH).collect::<String>());
        let defaults = default_term_ids(db, &self.taxonomy.name).await?;
        if self.taxonomy.hierarchical && search.is_none() && query.sort.is_none() {
            let tree = Self::tree(Term::all(db, &self.taxonomy).await?);
            let total = i64::try_from(tree.len()).unwrap_or(i64::MAX);
            let rows = tree
                .iter()
                .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                .take(usize::try_from(per_page).unwrap_or(0))
                .map(|(term, depth)| Self::row_of(term, *depth, &defaults))
                .collect();
            return Ok(Page { rows, total });
        }
        let mut filter = String::from("tt.taxonomy = ?");
        let mut binds: Vec<Sql> = vec![(&*self.taxonomy.name).into()];
        if let Some(search) = search {
            let like = format!("%{}%", escape_like(&search.to_lowercase()));
            filter.push_str(
                " AND (LOWER(t.name) LIKE ? ESCAPE '!' OR LOWER(t.slug) LIKE ? ESCAPE '!')",
            );
            binds.push(like.clone().into());
            binds.push(like.into());
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
        let (column, order) = match &query.sort {
            Some((key, direction)) => (
                match key.as_str() {
                    COL_SLUG => "t.slug",
                    COL_COUNT => "tt.count",
                    COL_DESCRIPTION => "tt.description",
                    _ => "t.name",
                },
                match direction {
                    Direction::Desc => "DESC",
                    Direction::Asc => "ASC",
                },
            ),
            None => ("t.name", "ASC"),
        };
        binds.push(per_page.into());
        binds.push(offset.into());
        let ids: Vec<(i64,)> = raw::fetch_all(
            db,
            &format!(
                "SELECT t.id FROM term_taxonomy tt INNER JOIN terms t ON t.id = tt.term_id \
                 WHERE {filter} ORDER BY {column} {order}, t.id {order} LIMIT ? OFFSET ?"
            ),
            &binds,
        )
        .await?;
        let ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
        let by_id: HashMap<i64, Term> = terms_by_term_id(db, &self.taxonomy, &ids)
            .await?
            .into_iter()
            .map(|t| (t.term_id, t))
            .collect();
        let rows = ids
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|term| Self::row_of(term, 0, &defaults))
            .collect();
        Ok(Page { rows, total })
    }

    async fn row(&self, db: &Database, term_id: i64) -> Result<Option<Row>, Error> {
        let Some(term) = self.term(db, term_id).await? else {
            return Ok(None);
        };
        let defaults = default_term_ids(db, &self.taxonomy.name).await?;
        Ok(Some(Self::row_of(&term, 0, &defaults)))
    }

    /// The Name field's refusal of `input`, for the term `id` (`None` on the New screen):
    /// [`NAME_TOO_LONG`] for a name, as it is stored, longer than [`MAX_TERM_LENGTH`]
    /// characters; and on edit, [`NAME_TAKEN`] for a name another term has under the same parent
    /// (anywhere, for tags), compared without regard to case as `term_exists` compares names
    /// (the create path refuses it in the terms API).
    async fn name_refusal(
        &self,
        db: &Database,
        id: Option<i64>,
        input: &Row,
    ) -> Result<Option<ValidationError>, Error> {
        let (name, _) = self.sanitize(text(input, NAME), None).await?;
        let Some(name) = name.map(|n| n.trim().to_owned()) else {
            return Ok(None);
        };
        let refuse = |message: &str| Some(ValidationError::with_messages([(NAME, message)]));
        if name.chars().count() > MAX_TERM_LENGTH {
            return Ok(refuse(NAME_TOO_LONG));
        }
        let Some(id) = id else {
            return Ok(None);
        };
        let Some(term) = self.term(db, id).await? else {
            return Ok(None);
        };
        let parent = match self.parent(input) {
            Some(parent) => parent,
            None => term.parent_id,
        };
        let lower = name.to_lowercase();
        let taken = Term::all(db, &self.taxonomy).await?.iter().any(|other| {
            other.term_id != id
                && (!self.taxonomy.hierarchical || other.parent_id == parent)
                && other.name.to_lowercase() == lower
        });
        Ok(if taken { refuse(NAME_TAKEN) } else { None })
    }

    /// The parent choices: "None", then every category in tree order, indented by level.
    async fn parent_choices(&self, db: &Database) -> Result<Vec<(String, String)>, Error> {
        let mut choices = vec![(String::new(), "None".to_owned())];
        for (term, depth) in Self::tree(Term::all(db, &self.taxonomy).await?) {
            choices.push((
                term.term_id.to_string(),
                format!("{}{}", "\u{a0}\u{a0}\u{a0}".repeat(depth), term.name),
            ));
        }
        Ok(choices)
    }
}

impl RuntimeData for TermsData {
    fn list<'a>(&'a self, db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move { self.list_page(db, query).await })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move { self.row(db, id).await })
    }

    /// `wp_insert_term`.
    fn create<'a>(&'a self, db: &'a Database, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            let (name, description) = self
                .sanitize(text(&input, NAME), text(&input, DESCRIPTION))
                .await?;
            let new = NewTerm {
                name: name.unwrap_or_default(),
                slug: text(&input, SLUG).unwrap_or_default(),
                description: description.unwrap_or_default(),
                parent_id: self.parent(&input).flatten(),
            };
            let term = Term::create(db, &self.taxonomy, new)
                .await
                .map_err(|e| self.term_error(e))?;
            Ok(Self::row_of(&term, 0, &[]))
        })
    }

    /// `wp_update_term`: an empty slug is made from the name.
    fn update<'a>(&'a self, db: &'a Database, id: i64, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            let Some(term) = self.term(db, id).await? else {
                return Err(Error::NotFound { table: "terms" });
            };
            let (name, description) = self
                .sanitize(text(&input, NAME), text(&input, DESCRIPTION))
                .await?;
            let changes = TermChanges {
                name,
                slug: text(&input, SLUG),
                description,
                parent_id: self.parent(&input),
            };
            let term = term
                .update(db, &self.taxonomy, changes)
                .await
                .map_err(|e| self.term_error(e))?;
            Ok(Self::row_of(&term, 0, &[]))
        })
    }

    /// [`update`](RuntimeData::update), after the parent is checked: a category chosen as its
    /// own parent is refused on the Parent Category field ([`OWN_PARENT_CATEGORY`]), and nothing
    /// is saved. `wp_update_term` would drop such a parent silently; the form says why instead.
    fn try_update_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        id: i64,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move {
            if self.parent(&input).flatten() == Some(id) {
                return Ok(Err(ValidationError::with_messages([(
                    PARENT,
                    OWN_PARENT_CATEGORY,
                )])));
            }
            if let Some(refused) = self.name_refusal(db, Some(id), &input).await? {
                return Ok(Err(refused));
            }
            self.update(db, id, input).await.map(Ok)
        })
    }

    /// [`create`](RuntimeData::create), after the name's length is checked on the field
    /// ([`NAME_TOO_LONG`]).
    fn try_create_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move {
            if let Some(refused) = self.name_refusal(db, None, &input).await? {
                return Ok(Err(refused));
            }
            self.create(db, input).await.map(Ok)
        })
    }

    /// `wp_delete_term`. The panel refuses the default category first (its row is marked
    /// [`DEFAULT_TERM_KEY`]); the terms API refuses it again.
    fn delete<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, u64> {
        Box::pin(async move {
            let Some(term) = self.term(db, id).await? else {
                return Ok(0);
            };
            let deleted = term.delete(db).await.map_err(|e| self.term_error(e))?;
            Ok(u64::from(deleted))
        })
    }

    fn options<'a>(
        &'a self,
        db: &'a Database,
        field: &'a str,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move {
            if field == PARENT && self.taxonomy.hierarchical {
                self.parent_choices(db).await
            } else {
                Ok(Vec::new())
            }
        })
    }
}

/// Who may use a term screen, as WordPress's `edit-tags.php` and `term.php` decide: the list
/// needs the taxonomy's `manage_terms`, New `edit_terms`, a term's form and save `edit_term`, and
/// Delete `delete_term`, which nobody holds on the taxonomy's default term. Registered for the
/// screen's slug by [`register_term_resources`].
pub struct TermsPolicy {
    taxonomy: Taxonomy,
    caps: TaxonomyCaps,
    access: Access,
}

impl TermsPolicy {
    /// The policy of `taxonomy`'s screen, reading users' capabilities through `access`.
    pub fn new(taxonomy: Taxonomy, access: Access) -> Self {
        let caps = TaxonomyCaps::for_taxonomy(&taxonomy.name);
        Self::with_caps(taxonomy, caps, access)
    }

    /// The policy of a custom taxonomy's screen, whose capabilities are `caps` (its
    /// definition's, [`TaxonomyDef::caps`]).
    pub fn with_caps(taxonomy: Taxonomy, caps: TaxonomyCaps, access: Access) -> Self {
        Self {
            taxonomy,
            caps,
            access,
        }
    }

    async fn user_caps(&self, db: &Database, user: &ActingUser) -> Result<UserCaps, Error> {
        self.access.caps(db, user).await
    }

    /// Whether `user` holds the taxonomy capability `cap`.
    async fn has(&self, db: &Database, user: &ActingUser, cap: &str) -> Result<bool, Error> {
        Ok(can(&self.user_caps(db, user).await?, cap, None))
    }

    /// Whether `user` may do the term meta capability `cap` (`edit_term`, `delete_term`) on the
    /// term of `record`. A record without an id is refused.
    async fn on_term(
        &self,
        db: &Database,
        user: &ActingUser,
        cap: &str,
        record: &Row,
    ) -> Result<bool, Error> {
        let Some(term_id) = record.get("id").and_then(id_of) else {
            return Ok(false);
        };
        let defaults = default_term_ids(db, &self.taxonomy.name).await?;
        let context = TermContext {
            term_id,
            taxonomy: &self.taxonomy.name,
            default_terms: &defaults,
            caps: Some(&self.caps),
        };
        Ok(can(
            &self.user_caps(db, user).await?,
            cap,
            Some(&Object::Term(context)),
        ))
    }
}

impl Policy for TermsPolicy {
    fn view_any<'a>(&'a self, db: &'a Database, user: &'a ActingUser) -> RuntimeFuture<'a, bool> {
        Box::pin(self.has(db, user, &self.caps.manage_terms))
    }

    fn view<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(self.on_term(db, user, "edit_term", record))
    }

    fn create<'a>(&'a self, db: &'a Database, user: &'a ActingUser) -> RuntimeFuture<'a, bool> {
        Box::pin(self.has(db, user, &self.caps.edit_terms))
    }

    fn update<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(self.on_term(db, user, "edit_term", record))
    }

    fn delete<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(self.on_term(db, user, "delete_term", record))
    }
}
