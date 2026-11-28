//! The Post Types manager at `/mc-admin/post-types` (Task 5.7, design 2.1): ACF 6's Post Types
//! list and editor, over `register_post_type`'s options.
//!
//! - **The list**: the plural label, the key, the taxonomies, the number of posts and the
//!   origin, as ACF's columns. WordPress's `post` and `page` and the types defined in code or by
//!   a plugin are listed, locked. The search matches the label and the key.
//! - **The editor**: the basic card (plural and singular labels, the key, the taxonomies,
//!   Public, Hierarchical), then ACF's advanced tabs: General (supports, description),
//!   Visibility (show in the admin menu, menu position, menu icon), Permissions (capability
//!   type) and REST API (show in the REST API). The labels WordPress derives from the two names
//!   (`get_post_type_labels`) are left unset, so WordPress's defaults apply, as ACF's
//!   "Regenerate" gives them.
//! - **The key** follows the singular label until it is edited (Ignite's `slug_from`), and is
//!   locked after the first save (the owner's rule, as WordPress keeps a post type's name): the
//!   edit form shows it disabled, and the record is addressed by it
//!   (`/mc-admin/post-types/book/edit`). It must be `sanitize_key`'s: at most 20 lowercase
//!   letters, digits, `_` and `-`, not a name WordPress keeps, and not a type in use.
//! - **No supports ticked** is WordPress's default, title and editor, as `register_post_type`
//!   reads an empty `supports`.
//! - **A save** stores the definition, grants the type's capabilities to Administrator, and
//!   registers the admin screens again: the type is in the sidebar under Content on the next
//!   page load. **Delete** removes the definition only; its posts stay, as in WordPress and ACF.

use std::collections::HashMap;

use ignite_core::{
    ActingUser, BadgeColor, ColumnDef, Component, FieldDef, FieldKindDef, FormLayout, ListFilter,
    ListQuery, Operation, Page, Row, RuntimeData, RuntimeFuture, RuntimeResource, Section, Tabs,
    ValidationError, layout::Tab,
};
use orbit_orm::{Database, Error, raw};
use serde_json::{Value, json};

use super::{
    ADMINISTRATOR, BUILT_IN, COL_ORIGIN, KEY, LOCKED, LOCKED_NOTICE, MANAGE_OPTIONS, Managers,
    built_in_notice, definition_message, flag, is_locked_record, list, locked_notice,
    locked_record_notice, not_found, number, origin_label, page_of, registry_error, strings, text,
};
use crate::admin::navigation;
use crate::types::{
    CapabilityType, DefinitionError, DefinitionKind, Origin, PostTypeDef, RegistryError,
    ShowInMenu, Support, builtin_post_types,
};

/// The manager's slug.
pub const POST_TYPES: &str = "post-types";

const PLURAL: &str = "plural_label";
const SINGULAR: &str = "singular_label";
const TAXONOMIES: &str = "taxonomies";
const PUBLIC: &str = "public";
const HIERARCHICAL: &str = "hierarchical";
const SUPPORTS: &str = "supports";
const DESCRIPTION: &str = "description";
const SHOW_IN_MENU: &str = "show_in_menu";
const MENU_POSITION: &str = "menu_position";
const MENU_ICON: &str = "menu_icon";
const CAPABILITY_TYPE: &str = "capability_type";
const SHOW_IN_REST: &str = "show_in_rest";

const COL_NAME: &str = "col_name";
const COL_KEY: &str = "col_key";
const COL_TAXONOMIES: &str = "col_taxonomies";
const COL_COUNT: &str = "col_count";

/// The supports a type can have, with ACF's labels, in `add_post_type_support`'s order.
const SUPPORT_CHOICES: [(Support, &str); 11] = [
    (Support::Title, "Title"),
    (Support::Editor, "Editor"),
    (Support::Author, "Author"),
    (Support::Thumbnail, "Featured Image"),
    (Support::Excerpt, "Excerpt"),
    (Support::Comments, "Comments"),
    (Support::Revisions, "Revisions"),
    (Support::PageAttributes, "Page Attributes"),
    (Support::CustomFields, "Custom Fields"),
    (Support::Trackbacks, "Trackbacks"),
    (Support::PostFormats, "Post Formats"),
];

/// The Dashicons the Menu Icon select offers (ACF's icon picker lists them all; a plain select
/// of the common ones is the design's phase 1, gap I7). A stored icon not listed is offered too,
/// so a save keeps it.
const MENU_ICONS: [(&str, &str); 36] = [
    ("dashicons-admin-post", "Post (pin)"),
    ("dashicons-admin-page", "Page"),
    ("dashicons-admin-media", "Media"),
    ("dashicons-admin-links", "Links"),
    ("dashicons-admin-comments", "Comments"),
    ("dashicons-admin-users", "Users"),
    ("dashicons-admin-home", "Home"),
    ("dashicons-admin-site", "Site"),
    ("dashicons-admin-generic", "Gear"),
    ("dashicons-format-aside", "Aside"),
    ("dashicons-format-gallery", "Gallery"),
    ("dashicons-format-image", "Image"),
    ("dashicons-format-quote", "Quote"),
    ("dashicons-format-video", "Video"),
    ("dashicons-format-audio", "Audio"),
    ("dashicons-format-chat", "Chat"),
    ("dashicons-book", "Book"),
    ("dashicons-book-alt", "Book (open)"),
    ("dashicons-calendar", "Calendar"),
    ("dashicons-calendar-alt", "Calendar (alternative)"),
    ("dashicons-location", "Location"),
    ("dashicons-location-alt", "Map"),
    ("dashicons-cart", "Cart"),
    ("dashicons-products", "Products"),
    ("dashicons-portfolio", "Portfolio"),
    ("dashicons-businessman", "Person"),
    ("dashicons-groups", "Groups"),
    ("dashicons-heart", "Heart"),
    ("dashicons-star-filled", "Star"),
    ("dashicons-megaphone", "Megaphone"),
    ("dashicons-microphone", "Microphone"),
    ("dashicons-tickets-alt", "Tickets"),
    ("dashicons-clipboard", "Clipboard"),
    ("dashicons-lightbulb", "Light bulb"),
    ("dashicons-testimonial", "Testimonial"),
    ("dashicons-welcome-learn-more", "Learn more"),
];

/// The Show In Admin Menu choices: WordPress's default (follow Show UI), yes or no.
const SHOW_IN_MENU_CHOICES: [(&str, &str); 2] = [("1", "Yes"), ("0", "No")];

/// The refusal of a key another post type has, on the key field.
pub const KEY_TAKEN: &str = "This post type key is already in use by another post type.";

/// The refusal of a key another admin screen has (a taxonomy's term screen): the type's
/// screens would be at the same address.
pub const KEY_SCREEN_TAKEN: &str =
    "This post type key is already in use by a taxonomy or another admin screen.";

/// The Post Types manager as an Ignite runtime resource. See the [module](self).
pub fn post_types_resource(managers: Managers) -> RuntimeResource {
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
    let choices = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(v, l)| ((*v).to_owned(), (*l).to_owned()))
            .collect::<Vec<_>>()
    };
    let supports = SUPPORT_CHOICES
        .iter()
        .map(|(s, l)| (s.as_str().to_owned(), (*l).to_owned()))
        .collect();
    let mut resource = super::super::stay_on_edit(RuntimeResource::new(
        POST_TYPES,
        "Post Types",
        "Post Type",
        PostTypesData { managers },
    ));
    resource.fields = vec![
        field(PLURAL, "Plural Label", FieldKindDef::Text, true),
        field(SINGULAR, "Singular Label", FieldKindDef::Text, true),
        field(KEY, "Post Type Key", FieldKindDef::Text, true),
        // Loaded: the taxonomies in use, `category` and `post_tag` first.
        field(
            TAXONOMIES,
            "Taxonomies",
            FieldKindDef::CheckboxList(Vec::new()),
            false,
        ),
        field(PUBLIC, "Public", FieldKindDef::Checkbox, false),
        field(HIERARCHICAL, "Hierarchical", FieldKindDef::Checkbox, false),
        field(
            SUPPORTS,
            "Supports",
            FieldKindDef::CheckboxList(supports),
            false,
        ),
        field(DESCRIPTION, "Description", FieldKindDef::Textarea, false),
        field(
            SHOW_IN_MENU,
            "Show In Admin Menu",
            FieldKindDef::Select(choices(&SHOW_IN_MENU_CHOICES)),
            false,
        ),
        field(MENU_POSITION, "Menu Position", FieldKindDef::Number, false),
        field(MENU_ICON, "Menu Icon", FieldKindDef::Lookup, false),
        field(
            CAPABILITY_TYPE,
            "Capability Type",
            FieldKindDef::Text,
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
        column(COL_NAME, "Post Type", true, true),
        column(COL_KEY, "Key", true, true),
        column(COL_TAXONOMIES, "Taxonomies", false, false),
        column(COL_COUNT, "Posts", true, false),
        column(COL_ORIGIN, "Origin", false, false),
    ];
    resource.form_layout = FormLayout::new().schema([
        Component::from(Section::make("").schema([
            PLURAL,
            SINGULAR,
            KEY,
            TAXONOMIES,
            PUBLIC,
            HIERARCHICAL,
        ])),
        Tabs::make("Advanced Configuration")
            .tabs([
                Tab::make("General").schema([SUPPORTS, DESCRIPTION]),
                Tab::make("Visibility").schema([SHOW_IN_MENU, MENU_POSITION, MENU_ICON]),
                Tab::make("Permissions").schema([CAPABILITY_TYPE]),
                Tab::make("REST API").schema([SHOW_IN_REST]),
            ])
            .into(),
    ]);
    navigation::place(
        &mut resource,
        navigation::POST_TYPES_MANAGER,
        navigation::POST_TYPES_ICON,
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

/// The rows of the Post Types manager: the post types in use, from the [`Registry`] and
/// WordPress's own. See the [module](self).
///
/// [`Registry`]: crate::types::Registry
#[derive(Clone)]
pub struct PostTypesData {
    managers: Managers,
}

impl PostTypesData {
    /// Every post type listed: `post` and `page`, then the registry's in use.
    fn definitions(&self) -> Vec<(PostTypeDef, bool)> {
        builtin_post_types()
            .into_iter()
            .map(|def| (def, true))
            .chain(
                self.managers
                    .registry
                    .all_post_types()
                    .into_iter()
                    .map(|def| (def, false)),
            )
            .collect()
    }

    /// The labels of the taxonomies a post type can use: categories, tags, then the registry's.
    fn taxonomy_labels(&self) -> Vec<(String, String)> {
        let mut labels = vec![
            ("category".to_owned(), "Categories".to_owned()),
            ("post_tag".to_owned(), "Tags".to_owned()),
        ];
        for def in self.managers.registry.all_taxonomies() {
            let label = if def.labels.name.is_empty() {
                def.slug.clone()
            } else {
                def.labels.name.clone()
            };
            labels.push((def.slug, label));
        }
        labels
    }

    /// The row of `def`, with `counts` of posts by type.
    fn row_of(
        &self,
        def: &PostTypeDef,
        builtin: bool,
        counts: &HashMap<String, i64>,
        taxonomy_labels: &[(String, String)],
    ) -> Row {
        let mut taxonomies = def.taxonomies.clone();
        for taxonomy in self.managers.registry.all_taxonomies() {
            if taxonomy.object_type.contains(&def.slug) && !taxonomies.contains(&taxonomy.slug) {
                taxonomies.push(taxonomy.slug);
            }
        }
        let taxonomy_names: Vec<String> = taxonomies
            .iter()
            .map(|slug| {
                taxonomy_labels
                    .iter()
                    .find(|(s, _)| s == slug)
                    .map_or_else(|| slug.clone(), |(_, label)| label.clone())
            })
            .collect();
        let (origin, notice) = if builtin {
            (BUILT_IN.to_owned(), Some(built_in_notice("post type")))
        } else {
            (
                origin_label(&def.origin),
                locked_notice("post type", &def.origin),
            )
        };
        let supports: Vec<&str> = def.supports.iter().map(|s| s.as_str()).collect();
        let show_in_menu = match &def.show_in_menu {
            None => "",
            Some(ShowInMenu::Show(true)) => "1",
            Some(_) => "0",
        };
        let capability_type =
            if def.capability_type == CapabilityType::new(&def.capability_type.singular) {
                def.capability_type.singular.clone()
            } else {
                format!(
                    "{}, {}",
                    def.capability_type.singular, def.capability_type.plural
                )
            };
        let mut row = Row::new();
        row.insert(KEY.into(), json!(def.slug));
        row.insert(LOCKED.into(), json!(builtin || def.is_locked()));
        row.insert(LOCKED_NOTICE.into(), json!(notice));
        row.insert(PLURAL.into(), json!(def.labels.name));
        row.insert(SINGULAR.into(), json!(def.labels.singular_name));
        row.insert(TAXONOMIES.into(), json!(def.taxonomies));
        row.insert(PUBLIC.into(), json!(def.public));
        row.insert(HIERARCHICAL.into(), json!(def.hierarchical));
        row.insert(SUPPORTS.into(), json!(supports));
        row.insert(DESCRIPTION.into(), json!(def.description));
        row.insert(SHOW_IN_MENU.into(), json!(show_in_menu));
        row.insert(MENU_POSITION.into(), json!(def.menu_position));
        row.insert(MENU_ICON.into(), json!(def.menu_icon));
        row.insert(CAPABILITY_TYPE.into(), json!(capability_type));
        row.insert(SHOW_IN_REST.into(), json!(def.show_in_rest));
        let name = if def.labels.name.is_empty() {
            def.slug.clone()
        } else {
            def.labels.name.clone()
        };
        row.insert(COL_NAME.into(), json!(name));
        row.insert(COL_KEY.into(), json!(def.slug));
        row.insert(COL_TAXONOMIES.into(), json!(taxonomy_names.join(", ")));
        row.insert(
            COL_COUNT.into(),
            json!(counts.get(&def.slug).copied().unwrap_or(0)),
        );
        row.insert(COL_ORIGIN.into(), json!(origin));
        row
    }

    /// The rows of every post type listed.
    async fn rows(&self, db: &Database) -> Result<Vec<Row>, Error> {
        let counts = post_counts(db).await?;
        let labels = self.taxonomy_labels();
        Ok(self
            .definitions()
            .iter()
            .map(|(def, builtin)| self.row_of(def, *builtin, &counts, &labels))
            .collect())
    }

    /// The row of the post type `key`.
    async fn row(&self, db: &Database, key: &str) -> Result<Option<Row>, Error> {
        let Some((def, builtin)) = self.definitions().into_iter().find(|(d, _)| d.slug == key)
        else {
            return Ok(None);
        };
        let counts = post_counts(db).await?;
        Ok(Some(self.row_of(
            &def,
            builtin,
            &counts,
            &self.taxonomy_labels(),
        )))
    }

    /// `base` with the form's values in `input` (a value the form did not send is kept).
    fn apply(base: PostTypeDef, input: &Row) -> PostTypeDef {
        let mut def = base;
        if let Some(plural) = text(input, PLURAL) {
            def.labels.name = plural;
        }
        if let Some(singular) = text(input, SINGULAR) {
            def.labels.singular_name = singular;
        }
        if let Some(items) = list(input, TAXONOMIES) {
            def.taxonomies = strings(&items);
        }
        if let Some(public) = flag(input, PUBLIC) {
            def.public = public;
        }
        if let Some(hierarchical) = flag(input, HIERARCHICAL) {
            def.hierarchical = hierarchical;
        }
        if let Some(items) = list(input, SUPPORTS) {
            let supports: Vec<Support> = strings(&items)
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect();
            // `register_post_type` reads no supports as its defaults, title and editor.
            def.supports = if supports.is_empty() {
                vec![Support::Title, Support::Editor]
            } else {
                supports
            };
        }
        if let Some(description) = text(input, DESCRIPTION) {
            def.description = description;
        }
        if let Some(show) = text(input, SHOW_IN_MENU) {
            def.show_in_menu = match show.as_str() {
                "1" => Some(ShowInMenu::Show(true)),
                "0" => Some(ShowInMenu::Show(false)),
                _ => None,
            };
        }
        if input.contains_key(MENU_POSITION) {
            def.menu_position = number(input, MENU_POSITION).and_then(|n| i32::try_from(n).ok());
        }
        if let Some(icon) = text(input, MENU_ICON) {
            def.menu_icon = (!icon.is_empty()).then_some(icon);
        }
        if let Some(capability) = text(input, CAPABILITY_TYPE) {
            def.capability_type = capability_type(&capability);
        }
        if let Some(show) = flag(input, SHOW_IN_REST) {
            def.show_in_rest = show;
        }
        def
    }

    /// Stores `def` and serves it: the admin screens again, and its capabilities granted to
    /// Administrator. A refusal of the definition shows on the form.
    async fn save(
        &self,
        db: &Database,
        def: PostTypeDef,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let caps = def.caps();
        match self
            .managers
            .registry
            .save_admin_definition(db, def.clone())
            .await
        {
            Ok(()) => {}
            Err(RegistryError::Definition(e)) => return Ok(Err(refusal(&e))),
            Err(e) => return Err(registry_error(e)),
        }
        self.managers.sync();
        let granted = vec![
            caps.read,
            caps.edit_posts,
            caps.edit_others_posts,
            caps.edit_published_posts,
            caps.edit_private_posts,
            caps.delete_posts,
            caps.delete_others_posts,
            caps.delete_published_posts,
            caps.delete_private_posts,
            caps.publish_posts,
            caps.read_private_posts,
        ];
        if let Err(e) = self.managers.grants.grant(db, ADMINISTRATOR, granted).await {
            log::error!(
                "The capabilities of the post type `{}` were not granted to Administrator: {e}",
                def.slug
            );
        }
        self.row(db, &def.slug).await?.ok_or_else(not_found).map(Ok)
    }
}

/// The capability type the field holds: `book`, or `story, stories` for an irregular plural.
fn capability_type(text: &str) -> CapabilityType {
    let mut parts = text.split(',').map(str::trim).filter(|p| !p.is_empty());
    match (parts.next(), parts.next()) {
        (Some(singular), Some(plural)) => CapabilityType {
            singular: singular.to_owned(),
            plural: plural.to_owned(),
        },
        (Some(singular), None) => CapabilityType::new(singular),
        _ => CapabilityType::default(),
    }
}

/// The form's message for a refused definition, under the field it is about.
fn refusal(e: &DefinitionError) -> ValidationError {
    let field = match e {
        DefinitionError::InvalidCapabilityType => CAPABILITY_TYPE,
        DefinitionError::TooLarge { what, .. } if what.contains("label") => PLURAL,
        DefinitionError::TooLarge { what, .. } if what.contains("description") => DESCRIPTION,
        DefinitionError::TooLarge { what, .. } if what.contains("capability") => CAPABILITY_TYPE,
        DefinitionError::TooLarge { what, .. } if what.contains("icon") => MENU_ICON,
        _ => KEY,
    };
    ValidationError::with_messages([(field, definition_message(e))])
}

/// The number of posts of each type, the auto-drafts left out, as ACF's Posts column counts.
async fn post_counts(db: &Database) -> Result<HashMap<String, i64>, Error> {
    let rows: Vec<(String, i64)> = raw::fetch_all(
        db,
        "SELECT post_type, COUNT(*) FROM posts WHERE status <> ? GROUP BY post_type",
        &["auto-draft".into()],
    )
    .await?;
    Ok(rows.into_iter().collect())
}

impl RuntimeData for PostTypesData {
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
            let Some(def) = self.managers.registry.post_type(key) else {
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
                .delete_admin_definition(db, DefinitionKind::PostType, key)
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
                TAXONOMIES => self.taxonomy_labels(),
                MENU_ICON => icon_choices(None),
                _ => Vec::new(),
            })
        })
    }

    fn options_with<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        field: &'a str,
        state: &'a Row,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        if field == MENU_ICON {
            let current = state
                .get(MENU_ICON)
                .and_then(Value::as_str)
                .map(str::to_owned);
            return Box::pin(async move { Ok(icon_choices(current.as_deref())) });
        }
        self.options_for(db, user, field)
    }
}

/// The Menu Icon choices: "Default (the post icon)", the listed Dashicons, and `current` when
/// it is a stored icon not listed, so a save keeps it.
fn icon_choices(current: Option<&str>) -> Vec<(String, String)> {
    let mut choices = vec![(String::new(), "Default (the post icon)".to_owned())];
    choices.extend(
        MENU_ICONS
            .iter()
            .map(|(v, l)| ((*v).to_owned(), (*l).to_owned())),
    );
    if let Some(current) = current.map(str::trim).filter(|c| !c.is_empty())
        && !choices.iter().any(|(v, _)| v == current)
    {
        choices.push((current.to_owned(), current.to_owned()));
    }
    choices
}

impl PostTypesData {
    /// A new post type from the form: its key free and valid, then stored and served.
    async fn create_checked(
        &self,
        db: &Database,
        input: Row,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let key = text(&input, KEY).unwrap_or_default();
        let taken = builtin_post_types().iter().any(|d| d.slug == key)
            || self.managers.registry.post_type(&key).is_some()
            || stored(db, &key).await?;
        if taken {
            return Ok(Err(ValidationError::with_messages([(KEY, KEY_TAKEN)])));
        }
        // The type's screens are at `/mc-admin/<key>`: another screen's address (a taxonomy's
        // term screen, the media library) cannot be taken.
        if ignite_core::runtime_resource(&key).is_some() {
            return Ok(Err(ValidationError::with_messages([(
                KEY,
                KEY_SCREEN_TAKEN,
            )])));
        }
        let plural = text(&input, PLURAL).unwrap_or_default();
        let singular = text(&input, SINGULAR).unwrap_or_default();
        let mut base = PostTypeDef::new(&key, &plural, &singular);
        base.origin = Origin::Admin;
        self.save(db, Self::apply(base, &input)).await
    }
}

/// Whether a post type with `key` is stored, though not in use (overridden by code).
async fn stored(db: &Database, key: &str) -> Result<bool, Error> {
    let found: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM post_types WHERE slug = ?",
        &[key.into()],
    )
    .await?;
    Ok(found > 0)
}
