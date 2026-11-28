//! The Taxonomies manager at `/mc-admin/taxonomies` (Task 5.7, design 2.2): ACF 6's
//! Taxonomies list and editor, over `register_taxonomy`'s options.
//!
//! - **The list**: the plural label, the key, the post types, the number of terms and the
//!   origin. WordPress's `category` and `post_tag` and the taxonomies defined in code or by a
//!   plugin are listed, locked. The search matches the label and the key.
//! - **The editor**: the basic card (plural and singular labels, the key, the post types it is
//!   attached to, Public, Hierarchical), then ACF's advanced tabs: General (description),
//!   Visibility (the admin column) and REST API. The other labels are WordPress's defaults
//!   (`get_taxonomy_labels`).
//! - **The key** follows the singular label until it is edited, and is locked after the first
//!   save, as a post type's. It must be `sanitize_key`'s, at most 32 characters, not a name
//!   WordPress keeps, and not a taxonomy or another admin screen in use (the term screen is at
//!   `/mc-admin/<key>`).
//! - **A save** stores the definition and registers the screens again: the taxonomy's term
//!   screen is under Posts, and its box is on the form of each post type it is attached to, on
//!   the next page load. **Delete** removes the definition only; its terms stay.

use std::collections::HashMap;

use ignite_core::{
    ActingUser, BadgeColor, ColumnDef, Component, FieldDef, FieldKindDef, FormLayout, ListFilter,
    ListQuery, Operation, Page, Row, RuntimeData, RuntimeFuture, RuntimeResource, Section, Tabs,
    ValidationError, layout::Tab,
};
use orbit_orm::{Database, Error, raw};
use serde_json::json;

use super::{
    BUILT_IN, COL_ORIGIN, KEY, LOCKED, LOCKED_NOTICE, MANAGE_OPTIONS, Managers, built_in_notice,
    definition_message, flag, is_locked_record, list, locked_notice, locked_record_notice,
    not_found, origin_label, page_of, registry_error, strings, text,
};
use crate::admin::navigation;
use crate::types::definition::RESERVED_TAXONOMIES;
use crate::types::{
    DefinitionError, DefinitionKind, Origin, RegistryError, TaxonomyDef, builtin_post_types,
};

/// The manager's slug.
pub const TAXONOMIES: &str = "taxonomies";

const PLURAL: &str = "plural_label";
const SINGULAR: &str = "singular_label";
const POST_TYPES: &str = "post_types";
const PUBLIC: &str = "public";
const HIERARCHICAL: &str = "hierarchical";
const DESCRIPTION: &str = "description";
const SHOW_ADMIN_COLUMN: &str = "show_admin_column";
const SHOW_IN_REST: &str = "show_in_rest";

const COL_NAME: &str = "col_name";
const COL_KEY: &str = "col_key";
const COL_POST_TYPES: &str = "col_post_types";
const COL_COUNT: &str = "col_count";

/// The refusal of a key another taxonomy, or another admin screen, has.
pub const KEY_TAKEN: &str = "This taxonomy key is already in use by another taxonomy.";

/// The refusal of a key another admin screen (a post type's) has: the term screen would be at
/// the same address.
pub const KEY_SCREEN_TAKEN: &str =
    "This taxonomy key is already in use by a post type or another admin screen.";

/// The Taxonomies manager as an Ignite runtime resource. See the [module](self).
pub fn taxonomies_resource(managers: Managers) -> RuntimeResource {
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
    let mut resource = super::super::stay_on_edit(RuntimeResource::new(
        TAXONOMIES,
        "Taxonomies",
        "Taxonomy",
        TaxonomiesData { managers },
    ));
    resource.fields = vec![
        field(PLURAL, "Plural Label", FieldKindDef::Text, true),
        field(SINGULAR, "Singular Label", FieldKindDef::Text, true),
        field(KEY, "Taxonomy Key", FieldKindDef::Text, true),
        field(
            POST_TYPES,
            "Post Types",
            FieldKindDef::CheckboxList(Vec::new()),
            false,
        ),
        field(PUBLIC, "Public", FieldKindDef::Checkbox, false),
        field(HIERARCHICAL, "Hierarchical", FieldKindDef::Checkbox, false),
        field(DESCRIPTION, "Description", FieldKindDef::Textarea, false),
        field(
            SHOW_ADMIN_COLUMN,
            "Show Admin Column",
            FieldKindDef::Checkbox,
            false,
        ),
        field(
            SHOW_IN_REST,
            "Show In REST API",
            FieldKindDef::Checkbox,
            false,
        ),
    ];
    resource.columns = vec![
        column(COL_NAME, "Taxonomy", true, true),
        column(COL_KEY, "Key", true, true),
        column(COL_POST_TYPES, "Post Types", false, false),
        column(COL_COUNT, "Terms", true, false),
        column(COL_ORIGIN, "Origin", false, false),
    ];
    resource.form_layout = FormLayout::new().schema([
        Component::from(Section::make("").schema([
            PLURAL,
            SINGULAR,
            KEY,
            POST_TYPES,
            PUBLIC,
            HIERARCHICAL,
        ])),
        Tabs::make("Advanced Configuration")
            .tabs([
                Tab::make("General").schema([DESCRIPTION]),
                Tab::make("Visibility").schema([SHOW_ADMIN_COLUMN]),
                Tab::make("REST API").schema([SHOW_IN_REST]),
            ])
            .into(),
    ]);
    navigation::place(
        &mut resource,
        navigation::TAXONOMIES_MANAGER,
        navigation::TAXONOMIES_ICON,
    );
    let resource = resource
        .record_route_key_name(KEY)
        .slug_from(KEY, SINGULAR)
        .disabled_on(KEY, &[Operation::Edit])
        .read_only_when(is_locked_record)
        .read_only_notice(locked_record_notice)
        .hide_default_delete_when(ListFilter::new().where_eq(LOCKED, true))
        .badge_color(COL_ORIGIN, |_| BadgeColor::Gray);
    crate::admin::ScreenCaps::all(MANAGE_OPTIONS).apply(resource)
}

/// WordPress's own taxonomies the manager lists, locked: categories and tags.
fn builtin_taxonomies() -> [TaxonomyDef; 2] {
    let builtin = |slug: &str, hierarchical: bool, name: &str, singular: &str| TaxonomyDef {
        public: true,
        hierarchical,
        show_admin_column: true,
        show_in_rest: true,
        origin: Origin::Code,
        ..TaxonomyDef::new(slug, &["post"], name, singular)
    };
    [
        builtin("category", true, "Categories", "Category"),
        builtin("post_tag", false, "Tags", "Tag"),
    ]
}

/// The rows of the Taxonomies manager: the taxonomies in use, from the registry and
/// WordPress's own. See the [module](self).
#[derive(Clone)]
pub struct TaxonomiesData {
    managers: Managers,
}

impl TaxonomiesData {
    /// Every taxonomy listed: categories and tags, then the registry's in use.
    fn definitions(&self) -> Vec<(TaxonomyDef, bool)> {
        builtin_taxonomies()
            .into_iter()
            .map(|def| (def, true))
            .chain(
                self.managers
                    .registry
                    .all_taxonomies()
                    .into_iter()
                    .map(|def| (def, false)),
            )
            .collect()
    }

    /// The labels of the post types a taxonomy can be attached to.
    fn post_type_labels(&self) -> Vec<(String, String)> {
        builtin_post_types()
            .into_iter()
            .chain(self.managers.registry.all_post_types())
            .map(|def| {
                let label = if def.labels.name.is_empty() {
                    def.slug.clone()
                } else {
                    def.labels.name.clone()
                };
                (def.slug, label)
            })
            .collect()
    }

    /// The row of `def`, with `counts` of terms by taxonomy.
    fn row_of(
        def: &TaxonomyDef,
        builtin: bool,
        counts: &HashMap<String, i64>,
        post_type_labels: &[(String, String)],
    ) -> Row {
        let names: Vec<String> = def
            .object_type
            .iter()
            .map(|slug| {
                post_type_labels
                    .iter()
                    .find(|(s, _)| s == slug)
                    .map_or_else(|| slug.clone(), |(_, label)| label.clone())
            })
            .collect();
        let (origin, notice) = if builtin {
            (BUILT_IN.to_owned(), Some(built_in_notice("taxonomy")))
        } else {
            (
                origin_label(&def.origin),
                locked_notice("taxonomy", &def.origin),
            )
        };
        let name = if def.labels.name.is_empty() {
            def.slug.clone()
        } else {
            def.labels.name.clone()
        };
        let mut row = Row::new();
        row.insert(KEY.into(), json!(def.slug));
        row.insert(LOCKED.into(), json!(builtin || def.is_locked()));
        row.insert(LOCKED_NOTICE.into(), json!(notice));
        row.insert(PLURAL.into(), json!(def.labels.name));
        row.insert(SINGULAR.into(), json!(def.labels.singular_name));
        row.insert(POST_TYPES.into(), json!(def.object_type));
        row.insert(PUBLIC.into(), json!(def.public));
        row.insert(HIERARCHICAL.into(), json!(def.hierarchical));
        row.insert(DESCRIPTION.into(), json!(def.description));
        row.insert(SHOW_ADMIN_COLUMN.into(), json!(def.show_admin_column));
        row.insert(SHOW_IN_REST.into(), json!(def.show_in_rest));
        row.insert(COL_NAME.into(), json!(name));
        row.insert(COL_KEY.into(), json!(def.slug));
        row.insert(COL_POST_TYPES.into(), json!(names.join(", ")));
        row.insert(
            COL_COUNT.into(),
            json!(counts.get(&def.slug).copied().unwrap_or(0)),
        );
        row.insert(COL_ORIGIN.into(), json!(origin));
        row
    }

    async fn rows(&self, db: &Database) -> Result<Vec<Row>, Error> {
        let counts = term_counts(db).await?;
        let labels = self.post_type_labels();
        Ok(self
            .definitions()
            .iter()
            .map(|(def, builtin)| Self::row_of(def, *builtin, &counts, &labels))
            .collect())
    }

    async fn row(&self, db: &Database, key: &str) -> Result<Option<Row>, Error> {
        let Some((def, builtin)) = self.definitions().into_iter().find(|(d, _)| d.slug == key)
        else {
            return Ok(None);
        };
        let counts = term_counts(db).await?;
        Ok(Some(Self::row_of(
            &def,
            builtin,
            &counts,
            &self.post_type_labels(),
        )))
    }

    /// `base` with the form's values in `input` (a value the form did not send is kept).
    fn apply(base: TaxonomyDef, input: &Row) -> TaxonomyDef {
        let mut def = base;
        if let Some(plural) = text(input, PLURAL) {
            def.labels.name = plural;
        }
        if let Some(singular) = text(input, SINGULAR) {
            def.labels.singular_name = singular;
        }
        if let Some(items) = list(input, POST_TYPES) {
            def.object_type = strings(&items);
        }
        if let Some(public) = flag(input, PUBLIC) {
            def.public = public;
        }
        if let Some(hierarchical) = flag(input, HIERARCHICAL) {
            def.hierarchical = hierarchical;
        }
        if let Some(description) = text(input, DESCRIPTION) {
            def.description = description;
        }
        if let Some(show) = flag(input, SHOW_ADMIN_COLUMN) {
            def.show_admin_column = show;
        }
        if let Some(show) = flag(input, SHOW_IN_REST) {
            def.show_in_rest = show;
        }
        def
    }

    /// Stores `def` and serves it: the term screens and the post type screens again.
    async fn save(
        &self,
        db: &Database,
        def: TaxonomyDef,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let slug = def.slug.clone();
        match self.managers.registry.save_admin_definition(db, def).await {
            Ok(()) => {}
            Err(RegistryError::Definition(e)) => return Ok(Err(refusal(&e))),
            Err(e) => return Err(registry_error(e)),
        }
        self.managers.sync();
        self.row(db, &slug).await?.ok_or_else(not_found).map(Ok)
    }

    /// A new taxonomy from the form: its key free and valid, then stored and served.
    async fn create_checked(
        &self,
        db: &Database,
        input: Row,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let key = text(&input, KEY).unwrap_or_default();
        let taken = RESERVED_TAXONOMIES.contains(&key.as_str())
            || self.managers.registry.taxonomy(&key).is_some()
            || stored(db, &key).await?;
        if taken {
            return Ok(Err(ValidationError::with_messages([(KEY, KEY_TAKEN)])));
        }
        if ignite_core::runtime_resource(&key).is_some()
            || self.managers.registry.post_type(&key).is_some()
        {
            return Ok(Err(ValidationError::with_messages([(
                KEY,
                KEY_SCREEN_TAKEN,
            )])));
        }
        let plural = text(&input, PLURAL).unwrap_or_default();
        let singular = text(&input, SINGULAR).unwrap_or_default();
        let base = TaxonomyDef::new(&key, &[], &plural, &singular);
        self.save(db, Self::apply(base, &input)).await
    }
}

/// The form's message for a refused definition, under the field it is about.
fn refusal(e: &DefinitionError) -> ValidationError {
    let field = match e {
        DefinitionError::InvalidSlug { what, .. } if *what == "post type" => POST_TYPES,
        DefinitionError::TooLarge { what, .. } if what.contains("label") => PLURAL,
        DefinitionError::TooLarge { what, .. } if what.contains("description") => DESCRIPTION,
        _ => KEY,
    };
    ValidationError::with_messages([(field, definition_message(e))])
}

/// The number of terms of each taxonomy.
async fn term_counts(db: &Database) -> Result<HashMap<String, i64>, Error> {
    let rows: Vec<(String, i64)> = raw::fetch_all(
        db,
        "SELECT taxonomy, COUNT(*) FROM term_taxonomy GROUP BY taxonomy",
        &[],
    )
    .await?;
    Ok(rows.into_iter().collect())
}

/// Whether a taxonomy with `key` is stored, though not in use (overridden by code).
async fn stored(db: &Database, key: &str) -> Result<bool, Error> {
    let found: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM taxonomies WHERE slug = ?",
        &[key.into()],
    )
    .await?;
    Ok(found > 0)
}

impl RuntimeData for TaxonomiesData {
    fn list<'a>(&'a self, db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move {
            Ok(page_of(
                self.rows(db).await?,
                query,
                &[COL_NAME, COL_KEY],
                COL_NAME,
            ))
        })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        // The records are addressed by their key, never by a number.
        let _ = (db, id);
        Box::pin(async { Ok(None) })
    }

    fn find_by_key_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        key: &'a str,
    ) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(self.row(db, key))
    }

    fn create<'a>(&'a self, db: &'a Database, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            match self.create_checked(db, input).await? {
                Ok(row) => Ok(row),
                Err(e) => Err(super::super::settings::refused_save(&format!("{e:?}"))),
            }
        })
    }

    fn try_create_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(self.create_checked(db, input))
    }

    fn update<'a>(&'a self, db: &'a Database, id: i64, input: Row) -> RuntimeFuture<'a, Row> {
        let _ = (db, id, input);
        Box::pin(async { Err(not_found()) })
    }

    fn try_update_by_key_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        key: &'a str,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move {
            let Some(def) = self.managers.registry.taxonomy(key) else {
                return Err(not_found());
            };
            if def.is_locked() {
                return Ok(Err(refusal(&DefinitionError::Locked(def.origin.clone()))));
            }
            self.save(db, Self::apply(def, &input)).await
        })
    }

    fn delete<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, u64> {
        let _ = (db, id);
        Box::pin(async { Ok(0) })
    }

    fn delete_by_key_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        key: &'a str,
    ) -> RuntimeFuture<'a, u64> {
        Box::pin(async move {
            let deleted = self
                .managers
                .registry
                .delete_admin_definition(db, DefinitionKind::Taxonomy, key)
                .await
                .map_err(registry_error)?;
            self.managers.sync();
            Ok(u64::from(deleted))
        })
    }

    fn options<'a>(
        &'a self,
        _db: &'a Database,
        field: &'a str,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move {
            Ok(match field {
                POST_TYPES => self.post_type_labels(),
                _ => Vec::new(),
            })
        })
    }
}
