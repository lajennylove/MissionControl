//! Post types, taxonomies and field groups defined in Rust code, as Poet and ACF Composer
//! define them in a theme (design 5.3).
//!
//! The builders make the same definitions ([`PostTypeDef`], [`TaxonomyDef`], [`FieldGroupDef`])
//! the admin's screens and `from_json` make. `build` marks the definition [`Origin::Code`], so
//! the admin shows it as "defined in code", and runs the definition's `validate`: a builder
//! refuses exactly what a JSON definition is refused for, with the same [`DefinitionError`].
//! Register the result with [`Registry::register`](super::Registry::register).
//!
//! What a builder does not set keeps WordPress's or ACF's default, as with `from_json`. Two
//! things are filled in, as Poet and ACF Composer fill them:
//!
//! - **Labels and titles** come from the name when unset. A post type's or taxonomy's labels
//!   are Poet's: `case_study` gives "Case Studies" and "Case Study". The plural adds `s` (`es`
//!   after `s`, `x`, `z`, `ch`, `sh`; `ies` for a consonant and `y`); set the labels for any
//!   other plural. A field's or layout's label and a group's title are ACF Composer's
//!   (`ucwords`): `starts_on` gives "Starts On", `event_details` "Event Details".
//! - **Keys** come from the names when unset, as ACF Composer makes them: the group `event` is
//!   `group_event`, its field `venue` is `field_event_venue`, a repeater's sub-field
//!   `field_event_speakers_name`, a flexible content layout `layout_event_sections_quote` and
//!   its fields `field_event_sections_quote_text`. They are made from the builder's names
//!   whatever the group's key: two groups built with the same name and different keys
//!   (`.key("group_5f1a")`, `.key("group_6a2b")`) give their fields the same keys. The registry keeps
//!   both groups and does not check field keys across groups, so set the field keys to tell
//!   such groups apart, or to keep a key stable when renaming.
//!
//!   Keys made this way can collide, since the names may hold `_`: the field `a_b` and the
//!   sub-field `b` of a field `a` both give `field_{group}_a_b`. Deeply nested fields can also
//!   make a key longer than [`MAX_KEY_LEN`]. `build` refuses both with
//!   [`DefinitionError::GeneratedKey`], which names the key; set the key with `.key()`.
//!
//! The builders are named after what they make, as the plan names them (`PostType::new`), so
//! they live in this module only: [`PostType`] and [`Taxonomy`] would read as
//! [`rest::posts::PostType`](crate::rest::posts::PostType) and
//! [`content::Taxonomy`](crate::content::Taxonomy) if `types` re-exported them. Import them
//! from `types::code`.
//!
//! ```
//! use mission_control_core::types::code::{Field, FieldGroup, PostType};
//! use mission_control_core::types::{LocationOperator, LocationParam, Support};
//!
//! let event = PostType::new("event")
//!     .public(true)
//!     .show_in_rest(true)
//!     .menu_icon("dashicons-calendar")
//!     .support(Support::Thumbnail)
//!     .field_group("group_event")
//!     .build()
//!     .unwrap();
//! assert_eq!(event.labels.name, "Events");
//!
//! let details = FieldGroup::new("event")
//!     .title("Event details")
//!     .field(Field::text("venue").required(true))
//!     .field(Field::date_picker("starts_on"))
//!     .location(LocationParam::PostType, LocationOperator::Is, "event")
//!     .build()
//!     .unwrap();
//! assert_eq!(details.fields[1].key, "field_event_starts_on");
//! ```

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::definition::{
    CapabilityType, DefinitionError, FieldDef, FieldGroupDef, FieldKindDef, HasArchive,
    HideOnScreen, LayoutDef, LocationOperator, LocationParam, LocationRule, MAX_KEY_LEN, Origin,
    Position, PostTypeCapabilities, PostTypeDef, Rewrite, RewriteRules, ShowInMenu, Support,
    TaxonomyCapabilities, TaxonomyDef,
};

// ---------------------------------------------------------------------------------------------
// Names

/// The words of a name: `case_study` and `case-study` are `["case", "study"]`.
fn words(name: &str) -> impl Iterator<Item = &str> {
    name.split(['_', '-', ' ']).filter(|w| !w.is_empty())
}

/// `word` with its first letter in upper case.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A name in title case, as Poet's `Str::title` and ACF Composer's `ucwords`: `case_study` is
/// "Case Study".
fn title_case(name: &str) -> String {
    words(name).map(capitalize).collect::<Vec<_>>().join(" ")
}

/// The English plural of `singular`'s last word, for the common cases: `ies` for a consonant
/// and `y`, `es` after `s`, `x`, `z`, `ch` and `sh`, and `s` otherwise.
fn plural(singular: &str) -> String {
    let lower = singular.to_ascii_lowercase();
    if let Some(stem) = singular.strip_suffix(['y', 'Y'])
        && stem
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphabetic() && !"aeiouAEIOU".contains(c))
    {
        return format!("{stem}ies");
    }
    if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|end| lower.ends_with(end))
    {
        format!("{singular}es")
    } else {
        format!("{singular}s")
    }
}

/// Sets the label `key` of a label set (`menu_name`, `add_new_item`, ...) to `text`, through
/// the set's JSON form, so every label WordPress names can be set by its name. An empty text
/// unsets the label. A name the set does not have is refused, whatever the text.
fn set_label<L: Serialize + DeserializeOwned>(
    labels: &mut L,
    what: &'static str,
    key: &str,
    text: &str,
) -> Result<(), DefinitionError> {
    let unknown = || DefinitionError::Unknown {
        what,
        value: key.to_owned(),
    };
    let with = |text: &str| -> Option<L> {
        let Ok(Value::Object(mut map)) = serde_json::to_value(&*labels) else {
            return None;
        };
        map.insert(key.to_owned(), Value::String(text.to_owned()));
        serde_json::from_value(Value::Object(map)).ok()
    };
    // The name is known when a non-empty text set under it is kept. Checked first, since an
    // empty text unsets the label and so leaves no trace.
    let known = with("-").is_some_and(
        |probe| matches!(serde_json::to_value(&probe), Ok(Value::Object(m)) if m.contains_key(key)),
    );
    if !known {
        return Err(unknown());
    }
    *labels = with(text).ok_or_else(unknown)?;
    Ok(())
}

/// `rewrite` with its slug set, keeping the other rules.
fn with_rewrite_slug(rewrite: &Rewrite, slug: &str) -> Rewrite {
    let rules = match rewrite {
        Rewrite::Rules(rules) => rules.clone(),
        Rewrite::Enabled(_) => RewriteRules::default(),
    };
    Rewrite::Rules(RewriteRules {
        slug: Some(slug.to_owned()),
        ..rules
    })
}

// ---------------------------------------------------------------------------------------------
// Post types

/// A post type defined in code, as Poet's `post` config and `register_post_type` define one.
///
/// Each method sets the `register_post_type` option of its name. [`build`](Self::build) gives
/// the [`PostTypeDef`].
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct PostType {
    def: PostTypeDef,
    error: Option<DefinitionError>,
}

impl PostType {
    /// The post type `slug`, with `register_post_type`'s defaults, and the labels made from
    /// the slug: `case_study` is "Case Studies", "Case Study".
    pub fn new(slug: &str) -> Self {
        let singular = title_case(slug);
        Self {
            def: PostTypeDef::new(slug, &plural(&singular), &singular),
            error: None,
        }
    }

    /// Sets the general name, plural ("Events"), and the name of one item ("Event").
    pub fn labels(mut self, name: &str, singular_name: &str) -> Self {
        self.def.labels.name = name.to_owned();
        self.def.labels.singular_name = singular_name.to_owned();
        self
    }

    /// Sets one label by the name `get_post_type_labels` gives it, such as `menu_name` or
    /// `add_new_item`. An unknown name makes `build` fail with [`DefinitionError::Unknown`].
    pub fn label(mut self, key: &str, text: &str) -> Self {
        if let Err(e) = set_label(&mut self.def.labels, "post type label", key, text) {
            self.error.get_or_insert(e);
        }
        self
    }

    /// Sets the description.
    pub fn description(mut self, description: &str) -> Self {
        self.def.description = description.to_owned();
        self
    }

    /// Sets whether the type is public. WordPress's default: false.
    pub fn public(mut self, public: bool) -> Self {
        self.def.public = public;
        self
    }

    /// Sets whether posts have parents, as pages do.
    pub fn hierarchical(mut self, hierarchical: bool) -> Self {
        self.def.hierarchical = hierarchical;
        self
    }

    /// Sets whether search leaves the type out; by default the opposite of `public`.
    pub fn exclude_from_search(mut self, exclude: bool) -> Self {
        self.def.exclude_from_search = Some(exclude);
        self
    }

    /// Sets whether the front end serves the type's queries; by default `public`.
    pub fn publicly_queryable(mut self, queryable: bool) -> Self {
        self.def.publicly_queryable = Some(queryable);
        self
    }

    /// Sets whether the admin has screens for the type; by default `public`.
    pub fn show_ui(mut self, show: bool) -> Self {
        self.def.show_ui = Some(show);
        self
    }

    /// Sets whether the type has a top-level admin menu item; by default `show_ui`.
    pub fn show_in_menu(mut self, show: bool) -> Self {
        self.def.show_in_menu = Some(ShowInMenu::Show(show));
        self
    }

    /// Shows the type as a submenu of the menu `parent`, such as `"tools.php"`.
    pub fn show_under_menu(mut self, parent: &str) -> Self {
        self.def.show_in_menu = Some(ShowInMenu::Under(parent.to_owned()));
        self
    }

    /// Sets whether the type can be added to navigation menus; by default `public`.
    pub fn show_in_nav_menus(mut self, show: bool) -> Self {
        self.def.show_in_nav_menus = Some(show);
        self
    }

    /// Sets the menu icon: a Dashicons class, an image URL or a `data:image/svg+xml` URI.
    pub fn menu_icon(mut self, icon: &str) -> Self {
        self.def.menu_icon = Some(icon.to_owned());
        self
    }

    /// Sets the menu position; unset puts the type after Comments.
    pub fn menu_position(mut self, position: i32) -> Self {
        self.def.menu_position = Some(position);
        self
    }

    /// Sets whether the REST API serves the type. WordPress's default: false.
    pub fn show_in_rest(mut self, show: bool) -> Self {
        self.def.show_in_rest = show;
        self
    }

    /// Sets the type's route under `wp/v2`; by default the slug.
    pub fn rest_base(mut self, base: &str) -> Self {
        self.def.rest_base = Some(base.to_owned());
        self
    }

    /// Sets whether the type has an archive, at its rewrite slug. WordPress's default: false.
    pub fn has_archive(mut self, enabled: bool) -> Self {
        self.def.has_archive = HasArchive::Enabled(enabled);
        self
    }

    /// Gives the type an archive at `slug`.
    pub fn archive_slug(mut self, slug: &str) -> Self {
        self.def.has_archive = HasArchive::Slug(slug.to_owned());
        self
    }

    /// Sets the permalinks: `Rewrite::Enabled(false)` for none, or the rules.
    pub fn rewrite(mut self, rewrite: Rewrite) -> Self {
        self.def.rewrite = rewrite;
        self
    }

    /// Sets the permalink base, keeping the other rewrite rules.
    pub fn rewrite_slug(mut self, slug: &str) -> Self {
        self.def.rewrite = with_rewrite_slug(&self.def.rewrite, slug);
        self
    }

    /// Replaces the supported features. As `register_post_type` reads `supports`, an empty
    /// list is WordPress's default, `title` and `editor`; [`no_supports`](Self::no_supports)
    /// is none.
    pub fn supports(mut self, supports: impl IntoIterator<Item = Support>) -> Self {
        self.def.supports = Vec::new();
        for support in supports {
            self = self.support(support);
        }
        if self.def.supports.is_empty() {
            self.def.supports = vec![Support::Title, Support::Editor];
        }
        self
    }

    /// Supports no feature, not even the title and the editor: `register_post_type`'s
    /// `'supports' => false`.
    pub fn no_supports(mut self) -> Self {
        self.def.supports = Vec::new();
        self
    }

    /// Adds a supported feature, as `add_post_type_support`; one already there is kept once.
    pub fn support(mut self, support: Support) -> Self {
        if !self.def.supports.contains(&support) {
            self.def.supports.push(support);
        }
        self
    }

    /// Attaches the taxonomy `slug`.
    pub fn taxonomy(mut self, slug: &str) -> Self {
        self.def.taxonomies.push(slug.to_owned());
        self
    }

    /// Attaches the field group with the key `key` (`group_…`).
    pub fn field_group(mut self, key: &str) -> Self {
        self.def.field_groups.push(key.to_owned());
        self
    }

    /// Sets the base of the capability names; by default `post`.
    pub fn capability_type(mut self, capability_type: CapabilityType) -> Self {
        self.def.capability_type = capability_type;
        self
    }

    /// Sets capability names that replace those the capability type gives.
    pub fn capabilities(mut self, capabilities: PostTypeCapabilities) -> Self {
        self.def.capabilities = capabilities;
        self
    }

    /// Sets another `register_post_type` option, kept as written (`show_in_admin_bar`,
    /// `template`, ...). A modelled option makes `build` fail with
    /// [`DefinitionError::ReservedSetting`]: use its method.
    pub fn setting(mut self, name: &str, value: Value) -> Self {
        self.def.settings.insert(name.to_owned(), value);
        self
    }

    /// The definition, made in code, once it passes [`PostTypeDef::validate`].
    ///
    /// # Errors
    ///
    /// The [`DefinitionError`] validation gives, or the one a method recorded.
    pub fn build(self) -> Result<PostTypeDef, DefinitionError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut def = self.def;
        def.origin = Origin::Code;
        def.validate()?;
        Ok(def)
    }
}

// ---------------------------------------------------------------------------------------------
// Taxonomies

/// A taxonomy defined in code, as Poet's `taxonomy` config and `register_taxonomy` define one.
///
/// Each method sets the `register_taxonomy` option of its name. [`build`](Self::build) gives
/// the [`TaxonomyDef`].
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct Taxonomy {
    def: TaxonomyDef,
    error: Option<DefinitionError>,
}

impl Taxonomy {
    /// The taxonomy `slug`, for no post type yet, with `register_taxonomy`'s defaults, and the
    /// labels made from the slug: `genre` is "Genres", "Genre".
    pub fn new(slug: &str) -> Self {
        let singular = title_case(slug);
        Self {
            def: TaxonomyDef::new(slug, &[], &plural(&singular), &singular),
            error: None,
        }
    }

    /// Adds the post type `slug` the taxonomy is for, as `register_taxonomy_for_object_type`.
    pub fn object_type(mut self, slug: &str) -> Self {
        self.def.object_type.push(slug.to_owned());
        self
    }

    /// Sets the general name, plural ("Genres"), and the name of one term ("Genre").
    pub fn labels(mut self, name: &str, singular_name: &str) -> Self {
        self.def.labels.name = name.to_owned();
        self.def.labels.singular_name = singular_name.to_owned();
        self
    }

    /// Sets one label by the name `get_taxonomy_labels` gives it, such as `menu_name`. An
    /// unknown name makes `build` fail with [`DefinitionError::Unknown`].
    pub fn label(mut self, key: &str, text: &str) -> Self {
        if let Err(e) = set_label(&mut self.def.labels, "taxonomy label", key, text) {
            self.error.get_or_insert(e);
        }
        self
    }

    /// Sets the description.
    pub fn description(mut self, description: &str) -> Self {
        self.def.description = description.to_owned();
        self
    }

    /// Sets whether the taxonomy is public. WordPress's default: true.
    pub fn public(mut self, public: bool) -> Self {
        self.def.public = public;
        self
    }

    /// Sets whether the front end serves the taxonomy's archives; by default `public`.
    pub fn publicly_queryable(mut self, queryable: bool) -> Self {
        self.def.publicly_queryable = Some(queryable);
        self
    }

    /// Sets whether terms have parents, as categories do.
    pub fn hierarchical(mut self, hierarchical: bool) -> Self {
        self.def.hierarchical = hierarchical;
        self
    }

    /// Sets whether the admin has screens for the taxonomy; by default `public`.
    pub fn show_ui(mut self, show: bool) -> Self {
        self.def.show_ui = Some(show);
        self
    }

    /// Sets whether the taxonomy shows in the admin menu; by default `show_ui`.
    pub fn show_in_menu(mut self, show: bool) -> Self {
        self.def.show_in_menu = Some(show);
        self
    }

    /// Sets whether terms can be added to navigation menus; by default `public`.
    pub fn show_in_nav_menus(mut self, show: bool) -> Self {
        self.def.show_in_nav_menus = Some(show);
        self
    }

    /// Sets whether the tag cloud widget lists the taxonomy; by default `show_ui`.
    pub fn show_tagcloud(mut self, show: bool) -> Self {
        self.def.show_tagcloud = Some(show);
        self
    }

    /// Sets whether quick edit shows the taxonomy; by default `show_ui`.
    pub fn show_in_quick_edit(mut self, show: bool) -> Self {
        self.def.show_in_quick_edit = Some(show);
        self
    }

    /// Sets whether the posts list has a column for the taxonomy.
    pub fn show_admin_column(mut self, show: bool) -> Self {
        self.def.show_admin_column = show;
        self
    }

    /// Sets whether the REST API serves the taxonomy. WordPress's default: false.
    pub fn show_in_rest(mut self, show: bool) -> Self {
        self.def.show_in_rest = show;
        self
    }

    /// Sets the taxonomy's route under `wp/v2`; by default the slug.
    pub fn rest_base(mut self, base: &str) -> Self {
        self.def.rest_base = Some(base.to_owned());
        self
    }

    /// Sets the permalinks: `Rewrite::Enabled(false)` for none, or the rules.
    pub fn rewrite(mut self, rewrite: Rewrite) -> Self {
        self.def.rewrite = rewrite;
        self
    }

    /// Sets the permalink base, keeping the other rewrite rules.
    pub fn rewrite_slug(mut self, slug: &str) -> Self {
        self.def.rewrite = with_rewrite_slug(&self.def.rewrite, slug);
        self
    }

    /// Sets capability names that replace the defaults.
    pub fn capabilities(mut self, capabilities: TaxonomyCapabilities) -> Self {
        self.def.capabilities = capabilities;
        self
    }

    /// Sets another `register_taxonomy` option, kept as written (`query_var`, `sort`, ...). A
    /// modelled option makes `build` fail with [`DefinitionError::ReservedSetting`].
    pub fn setting(mut self, name: &str, value: Value) -> Self {
        self.def.settings.insert(name.to_owned(), value);
        self
    }

    /// The definition, made in code, once it passes [`TaxonomyDef::validate`].
    ///
    /// # Errors
    ///
    /// The [`DefinitionError`] validation gives, or the one a method recorded.
    pub fn build(self) -> Result<TaxonomyDef, DefinitionError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut def = self.def;
        def.origin = Origin::Code;
        def.validate()?;
        Ok(def)
    }
}

// ---------------------------------------------------------------------------------------------
// Field groups

/// A field group defined in code, as ACF Composer's field builder defines one: fields, where
/// the group shows (location rules), and the box's options.
///
/// [`build`](Self::build) gives the [`FieldGroupDef`], with the keys left unset made from the
/// names (see the [module](self)).
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct FieldGroup {
    name: String,
    key: Option<String>,
    title: Option<String>,
    fields: Vec<Field>,
    def: FieldGroupDef,
}

impl FieldGroup {
    /// The group `name`, whose key is `group_{name}` and whose title is made from the name,
    /// with ACF's defaults: no fields, no location (shown nowhere), active, normal position.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            key: None,
            title: None,
            fields: Vec::new(),
            def: FieldGroupDef::new("", ""),
        }
    }

    /// Sets the key (`group_…`) in place of `group_{name}`. The fields' keys are still made
    /// from the group's name, as ACF Composer makes them.
    pub fn key(mut self, key: &str) -> Self {
        self.key = Some(key.to_owned());
        self
    }

    /// Sets the title, the heading of the group's box.
    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_owned());
        self
    }

    /// Adds a field.
    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    /// Adds fields.
    pub fn fields(mut self, fields: impl IntoIterator<Item = Field>) -> Self {
        self.fields.extend(fields);
        self
    }

    /// Adds a location or-group with this rule: the group shows where it holds, or where any
    /// other or-group holds. ACF Composer's `setLocation` and `or`.
    pub fn location(
        mut self,
        param: LocationParam,
        operator: LocationOperator,
        value: &str,
    ) -> Self {
        self.def.location.push(vec![rule(param, operator, value)]);
        self
    }

    /// Adds a rule to the last location or-group: the group shows where all of its rules
    /// hold. ACF Composer's `and`. Without an or-group yet, starts one.
    pub fn and_location(
        mut self,
        param: LocationParam,
        operator: LocationOperator,
        value: &str,
    ) -> Self {
        match self.def.location.last_mut() {
            Some(all) => all.push(rule(param, operator, value)),
            None => self.def.location.push(vec![rule(param, operator, value)]),
        }
        self
    }

    /// Sets the order of the group's box among the others, lowest first.
    pub fn menu_order(mut self, order: i32) -> Self {
        self.def.menu_order = order;
        self
    }

    /// Sets where the box shows on the edit screen. ACF's default: normal.
    pub fn position(mut self, position: Position) -> Self {
        self.def.position = position;
        self
    }

    /// Hides an item of the edit screen where the group shows.
    pub fn hide_on_screen(mut self, item: HideOnScreen) -> Self {
        if !self.def.hide_on_screen.contains(&item) {
            self.def.hide_on_screen.push(item);
        }
        self
    }

    /// Sets whether the group is active. ACF's default: true.
    pub fn active(mut self, active: bool) -> Self {
        self.def.active = active;
        self
    }

    /// Sets the description, shown in the admin's list of groups.
    pub fn description(mut self, description: &str) -> Self {
        self.def.description = description.to_owned();
        self
    }

    /// Sets whether the REST API shows the group's fields (the `acf` field).
    pub fn show_in_rest(mut self, show: bool) -> Self {
        self.def.show_in_rest = show;
        self
    }

    /// Sets another ACF group setting, kept as written (`style`, `label_placement`, ...). A
    /// modelled one makes `build` fail with [`DefinitionError::ReservedSetting`].
    pub fn setting(mut self, name: &str, value: Value) -> Self {
        self.def.settings.insert(name.to_owned(), value);
        self
    }

    /// The definition, made in code, once it passes [`FieldGroupDef::validate`].
    ///
    /// # Errors
    ///
    /// The [`DefinitionError`] validation gives;
    /// [`MisplacedOption`](DefinitionError::MisplacedOption) for an option a field's type
    /// does not take (`choices` on a text field, ...); and
    /// [`GeneratedKey`](DefinitionError::GeneratedKey) where validation refuses a key made from
    /// the names (used twice, or too long).
    pub fn build(self) -> Result<FieldGroupDef, DefinitionError> {
        let mut def = self.def;
        // The keys made from the names, in the order they were made.
        let mut generated = Vec::new();
        def.key = match self.key {
            Some(key) => key,
            None => {
                let key = format!("group_{}", self.name);
                generated.push(key.clone());
                key
            }
        };
        def.title = self.title.unwrap_or_else(|| title_case(&self.name));
        def.fields = self
            .fields
            .into_iter()
            .map(|field| field.into_def(&self.name, &mut generated))
            .collect::<Result<_, _>>()?;
        def.origin = Origin::Code;
        // Checked here, so a key set by hand that is too long is left to validation's error.
        if let Some(key) = generated.iter().find(|k| k.len() > MAX_KEY_LEN) {
            return Err(DefinitionError::GeneratedKey {
                key: key.clone(),
                reason: "is longer than the 64-byte limit",
            });
        }
        def.validate().map_err(|e| match e {
            DefinitionError::DuplicateKey(key) if generated.contains(&key) => {
                DefinitionError::GeneratedKey {
                    key,
                    reason: "is used twice",
                }
            }
            e => e,
        })?;
        Ok(def)
    }
}

fn rule(param: LocationParam, operator: LocationOperator, value: &str) -> LocationRule {
    LocationRule {
        param,
        operator,
        value: value.to_owned(),
    }
}

/// A field of a [`FieldGroup`], named after its ACF type: `Field::text("venue")`,
/// `Field::true_false("free")`, `Field::repeater("speakers").sub_field(...)`.
///
/// The name is the meta key the value is stored under, which
/// [`get_field`](super::get_field) reads. The label is made from the name when unset.
#[derive(Debug, Clone)]
#[must_use = "a field does nothing until it is added to a group"]
pub struct Field {
    name: String,
    key: Option<String>,
    label: Option<String>,
    kind: FieldKindDef,
    required: bool,
    instructions: String,
    default_value: Value,
    sub_fields: Vec<Field>,
    layouts: Vec<Layout>,
    clone: Vec<String>,
    /// An option set that the kind does not take, such as `choices` on a text field.
    misplaced: Option<&'static str>,
    settings: Map<String, Value>,
}

/// Declares a `Field` constructor per ACF field type of a fixed kind.
macro_rules! field_types {
    ($( $(#[doc = $doc:literal])+ $fn:ident => $kind:expr, )+) => {
        $(
            $(#[doc = $doc])+
            pub fn $fn(name: &str) -> Self {
                Self::new(name, $kind)
            }
        )+
    };
}

impl Field {
    /// A field named `name` of `kind`. The constructors named after ACF's types are shorter.
    pub fn new(name: &str, kind: FieldKindDef) -> Self {
        Self {
            name: name.to_owned(),
            key: None,
            label: None,
            kind,
            required: false,
            instructions: String::new(),
            default_value: Value::Null,
            sub_fields: Vec::new(),
            layouts: Vec::new(),
            clone: Vec::new(),
            misplaced: None,
            settings: Map::new(),
        }
    }

    field_types! {
        /// ACF's `text`.
        text => FieldKindDef::Text,
        /// ACF's `textarea`.
        textarea => FieldKindDef::Textarea,
        /// ACF's `number`, whole numbers; [`step`](Self::step) allows decimals.
        number => FieldKindDef::Number,
        /// ACF's `email`.
        email => FieldKindDef::Email,
        /// ACF's `url`.
        url => FieldKindDef::Url,
        /// ACF's `password`.
        password => FieldKindDef::Password,
        /// ACF's `select`; add the options with [`choice`](Self::choice).
        select => FieldKindDef::Select(Vec::new()),
        /// ACF's `radio`; add the options with [`choice`](Self::choice).
        radio => FieldKindDef::Radio(Vec::new()),
        /// ACF's `true_false`: yes or no.
        true_false => FieldKindDef::Checkbox,
        /// ACF's `date_picker`.
        date_picker => FieldKindDef::Date,
        /// ACF's `date_time_picker`.
        date_time_picker => FieldKindDef::DateTime,
        /// ACF's `color_picker`.
        color_picker => FieldKindDef::Color,
        /// ACF's `image`.
        image => FieldKindDef::Image,
        /// ACF's `file`.
        file => FieldKindDef::File,
        /// ACF's `link`.
        link => FieldKindDef::Link,
        /// ACF's `wysiwyg`.
        wysiwyg => FieldKindDef::Wysiwyg,
        /// ACF's `post_object`.
        post_object => FieldKindDef::PostObject,
        /// ACF's `relationship`.
        relationship => FieldKindDef::Relationship,
        /// ACF's `taxonomy`.
        taxonomy => FieldKindDef::Taxonomy,
        /// ACF's `user`.
        user => FieldKindDef::User,
        /// ACF's `repeater`; add its fields with [`sub_field`](Self::sub_field).
        repeater => FieldKindDef::Repeater(Vec::new()),
        /// ACF's `group`; add its fields with [`sub_field`](Self::sub_field).
        group => FieldKindDef::Group(Vec::new()),
        /// ACF's `flexible_content`; add its layouts with [`layout`](Self::layout).
        flexible_content => FieldKindDef::FlexibleContent(Vec::new()),
        /// ACF's `gallery`.
        gallery => FieldKindDef::Gallery,
    }

    /// ACF's `clone`: reuses the fields, or the groups' fields, with the keys `keys`
    /// (`field_…` or `group_…`). Not `clone`, which Rust's `Clone` names.
    pub fn clone_of<'a>(name: &str, keys: impl IntoIterator<Item = &'a str>) -> Self {
        let mut field = Self::new(name, FieldKindDef::Clone(Vec::new()));
        field.clone = keys.into_iter().map(str::to_owned).collect();
        field
    }

    /// Sets the key (`field_…`) in place of the one made from the names.
    pub fn key(mut self, key: &str) -> Self {
        self.key = Some(key.to_owned());
        self
    }

    /// Sets the label shown next to the input.
    pub fn label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// Sets whether a value is required.
    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Sets the instructions shown to the author.
    pub fn instructions(mut self, instructions: &str) -> Self {
        self.instructions = instructions.to_owned();
        self
    }

    /// Sets the value a new post starts with.
    pub fn default_value(mut self, value: Value) -> Self {
        self.default_value = value;
        self
    }

    /// Sets a number field's `step`: a step other than 1 allows decimals. Only a number field
    /// takes it.
    pub fn step(mut self, step: &str) -> Self {
        let step = step.trim();
        match self.kind {
            FieldKindDef::Number | FieldKindDef::Decimal(_) => {
                let whole = step.is_empty() || step.parse::<f64>().is_ok_and(|n| n == 1.0);
                self.kind = if whole {
                    FieldKindDef::Number
                } else {
                    FieldKindDef::Decimal(step.to_owned())
                };
            }
            _ => self.misplaced = self.misplaced.or(Some("step")),
        }
        self
    }

    /// Adds an option to a select or radio field: the stored value and its label. Only a
    /// select or radio field takes it.
    pub fn choice(mut self, value: &str, label: &str) -> Self {
        match &mut self.kind {
            FieldKindDef::Select(choices) | FieldKindDef::Radio(choices) => {
                choices.push((value.to_owned(), label.to_owned()));
            }
            _ => self.misplaced = self.misplaced.or(Some("choices")),
        }
        self
    }

    /// Adds a field to a repeater or group.
    pub fn sub_field(mut self, field: Field) -> Self {
        self.sub_fields.push(field);
        self
    }

    /// Adds a layout to a flexible content field.
    pub fn layout(mut self, layout: Layout) -> Self {
        self.layouts.push(layout);
        self
    }

    /// Sets another ACF setting of the field's type, kept as written (`placeholder`,
    /// `return_format`, `wrapper`, ...). A modelled one makes the group's `build` fail with
    /// [`DefinitionError::ReservedSetting`].
    pub fn setting(mut self, name: &str, value: Value) -> Self {
        self.settings.insert(name.to_owned(), value);
        self
    }

    /// The field's definition, its key made from `prefix` (the group's name and the parents'
    /// names) when unset, and then added to `generated`.
    fn into_def(
        self,
        prefix: &str,
        generated: &mut Vec<String>,
    ) -> Result<FieldDef, DefinitionError> {
        let key = match self.key {
            Some(key) => key,
            None => {
                let key = format!("field_{prefix}_{}", self.name);
                generated.push(key.clone());
                key
            }
        };
        if let Some(option) = self.misplaced {
            return Err(DefinitionError::MisplacedOption { key, option });
        }
        let path = format!("{prefix}_{}", self.name);
        let label = self.label.unwrap_or_else(|| title_case(&self.name));
        let sub_fields = self
            .sub_fields
            .into_iter()
            .map(|f| f.into_def(&path, generated))
            .collect::<Result<Vec<_>, _>>()?;
        let layouts = self
            .layouts
            .into_iter()
            .map(|l| l.into_def(&path, generated))
            .collect::<Result<Vec<_>, _>>()?;
        let mut def = FieldDef::new(&key, &self.name, &label, self.kind)
            .with_sub_fields(sub_fields)
            .with_layouts(layouts);
        def.required = self.required;
        def.instructions = self.instructions;
        def.default_value = self.default_value;
        def.clone = self.clone;
        def.settings = self.settings;
        Ok(def)
    }
}

/// A layout of a flexible content [`Field`]: its name (stored as each row's `type`), label and
/// fields.
#[derive(Debug, Clone)]
#[must_use = "a layout does nothing until it is added to a field"]
pub struct Layout {
    name: String,
    key: Option<String>,
    label: Option<String>,
    fields: Vec<Field>,
    settings: Map<String, Value>,
}

impl Layout {
    /// The layout `name`, whose label is made from the name.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            key: None,
            label: None,
            fields: Vec::new(),
            settings: Map::new(),
        }
    }

    /// Sets the key (`layout_…`) in place of the one made from the names.
    pub fn key(mut self, key: &str) -> Self {
        self.key = Some(key.to_owned());
        self
    }

    /// Sets the label shown when choosing the layout.
    pub fn label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// Adds a field.
    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    /// Sets another ACF layout setting, kept as written (`display`, `min`, `max`).
    pub fn setting(mut self, name: &str, value: Value) -> Self {
        self.settings.insert(name.to_owned(), value);
        self
    }

    /// The layout's definition, its key made from `prefix` (the field's path) when unset, and
    /// added to `generated`.
    fn into_def(
        self,
        prefix: &str,
        generated: &mut Vec<String>,
    ) -> Result<LayoutDef, DefinitionError> {
        let path = format!("{prefix}_{}", self.name);
        let key = match self.key {
            Some(key) => key,
            None => {
                let key = format!("layout_{path}");
                generated.push(key.clone());
                key
            }
        };
        Ok(LayoutDef {
            key,
            label: self.label.unwrap_or_else(|| title_case(&self.name)),
            sub_fields: self
                .fields
                .into_iter()
                .map(|f| f.into_def(&path, generated))
                .collect::<Result<_, _>>()?,
            name: self.name,
            settings: self.settings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_become_labels() {
        assert_eq!(title_case("case_study"), "Case Study");
        assert_eq!(title_case("event-type"), "Event Type");
        assert_eq!(title_case("starts_on"), "Starts On");
        assert_eq!(plural("Event"), "Events");
        assert_eq!(plural("Case Study"), "Case Studies");
        assert_eq!(plural("Day"), "Days");
        assert_eq!(plural("Class"), "Classes");
        assert_eq!(plural("Box"), "Boxes");
        assert_eq!(plural("Branch"), "Branches");
    }

    #[test]
    fn an_unknown_label_is_refused() {
        let unknown = |key: &str| DefinitionError::Unknown {
            what: "post type label",
            value: key.to_owned(),
        };
        assert_eq!(
            PostType::new("event").label("menu_nam", "X").build(),
            Err(unknown("menu_nam"))
        );
        // Refused with an empty text too, which would unset a known label.
        assert_eq!(
            PostType::new("event").label("menu_nam", "").build(),
            Err(unknown("menu_nam"))
        );
        let def = PostType::new("event")
            .label("add_new_item", "Add event")
            .build()
            .unwrap();
        assert_eq!(def.labels.add_new_item.as_deref(), Some("Add event"));
        let def = PostType::new("event")
            .label("add_new_item", "Add event")
            .label("add_new_item", "")
            .build()
            .unwrap();
        assert_eq!(def.labels.add_new_item, None);
        assert_eq!(
            Taxonomy::new("genre").label("nope", "").build(),
            Err(DefinitionError::Unknown {
                what: "taxonomy label",
                value: "nope".into()
            })
        );
    }

    #[test]
    fn an_option_the_type_does_not_take_is_refused() {
        assert_eq!(
            FieldGroup::new("g")
                .field(Field::text("a").choice("x", "X"))
                .build(),
            Err(DefinitionError::MisplacedOption {
                key: "field_g_a".into(),
                option: "choices"
            })
        );
        assert_eq!(
            FieldGroup::new("g")
                .field(Field::text("a").step("2"))
                .build(),
            Err(DefinitionError::MisplacedOption {
                key: "field_g_a".into(),
                option: "step"
            })
        );
    }
}
