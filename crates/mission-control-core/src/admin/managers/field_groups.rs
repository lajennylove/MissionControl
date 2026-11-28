//! The Field Groups manager at `/mc-admin/field-groups` (Task 5.7, design 2.3 and 5): ACF 6's
//! Field Groups list and its one-page editor.
//!
//! - **The list**: the title, the key, the location (a summary such as "Post Type is Book or
//!   Page Template is wide"), the number of fields and the origin. Groups defined in code or by a
//!   plugin are listed, locked. The search matches the title and the key.
//! - **The editor**, as ACF's, on one page (the owner's decision):
//!   - the title;
//!   - **Fields**, a repeater of field rows (Ignite's repeater, as Filament's `Repeater`): each
//!     row's label, name, type, key (made on the first save, `field_` and 13 hexadecimal
//!     digits, as ACF's `uniqid`), Required, default value, instructions, the choices of a
//!     select, checkbox or radio (`value : label` per line, as ACF writes them) and the
//!     placeholder. Rows are added, removed and moved; the order is the form's.
//!   - **Settings** tabs: Location Rules ("Show this field group if": rule groups joined by
//!     "or", each a repeater of rules joined by "and", each rule's parameter, operator and a
//!     value whose choices depend on the parameter, Ignite's dependent options), Presentation
//!     (position, order, hide on screen) and Group Settings (description).
//! - **A save** stores the group and its `field_group_locations` rows through the registry, and
//!   registers the post type screens again: the group's fields are on the forms its rules match
//!   on the next page load. A field's other ACF settings (an import's `return_format`,
//!   `wrapper`, ...) are kept by its key. **Delete** removes the group; the values stay in the
//!   meta tables, as ACF leaves them.
//!
//! The field types offered are design 5.3's phase 1. A field row shows the settings every type
//! has, and the choices; each type's own settings (ACF's `return_format`, `min`, `max`, ...)
//! wait on Ignite showing a row's sub-field by the row's `type` (see the [managers](super)).

use std::collections::HashMap;

use ignite_core::{
    ActingUser, BadgeColor, ColumnDef, Component, FieldDef as IgniteFieldDef, FieldKindDef,
    FormLayout, ListFilter, ListQuery, Page, RepeaterSettings, Row, RuntimeData, RuntimeFuture,
    RuntimeResource, Section, Tabs, ValidationError, layout::Tab,
};
use orbit_orm::{Database, Error};
use serde_json::{Map, Value, json};

use super::{
    COL_ORIGIN, KEY, LOCKED, LOCKED_NOTICE, MANAGE_OPTIONS, Managers, definition_message,
    is_locked_record, list, locked_notice, locked_record_notice, not_found, number, origin_label,
    page_of, registry_error, strings, text,
};
use crate::admin::navigation;
use crate::types::{
    DefinitionError, DefinitionKind, FieldDef, FieldGroupDef, HideOnScreen, LocationOperator,
    LocationParam, LocationRule, Origin, Position, RegistryError, builtin_post_types,
};
use crate::users::role_choices;

/// The manager's slug.
pub const FIELD_GROUPS: &str = "field-groups";

const TITLE: &str = "title";
const FIELDS: &str = "fields";
const LOCATION: &str = "location";
const RULES: &str = "rules";
const POSITION: &str = "position";
const MENU_ORDER: &str = "menu_order";
const HIDE_ON_SCREEN: &str = "hide_on_screen";
const DESCRIPTION: &str = "description";

// A field row's sub-fields.
const F_LABEL: &str = "label";
const F_NAME: &str = "name";
const F_TYPE: &str = "type";
const F_KEY: &str = "key";
const F_REQUIRED: &str = "required";
const F_DEFAULT: &str = "default_value";
const F_INSTRUCTIONS: &str = "instructions";
const F_CHOICES: &str = "choices";
const F_PLACEHOLDER: &str = "placeholder";

// A location rule's sub-fields.
const R_PARAM: &str = "param";
const R_OPERATOR: &str = "operator";
const R_VALUE: &str = "value";

/// The path of a rule's value, for its dependent choices (`options_with`).
const RULE_VALUE_PATH: &str = "location.rules.value";

const COL_TITLE: &str = "col_title";
const COL_KEY: &str = "col_key";
const COL_LOCATION: &str = "col_location";
const COL_FIELDS: &str = "col_fields";

/// The field types the editor offers, with ACF's labels: design 5.3's phase 1.
pub const PHASE_ONE_TYPES: [(&str, &str); 21] = [
    ("text", "Text"),
    ("textarea", "Text Area"),
    ("number", "Number"),
    ("email", "Email"),
    ("url", "URL"),
    ("password", "Password"),
    ("select", "Select"),
    ("checkbox", "Checkbox"),
    ("radio", "Radio Button"),
    ("true_false", "True / False"),
    ("date_picker", "Date Picker"),
    ("date_time_picker", "Date Time Picker"),
    ("color_picker", "Color Picker"),
    ("image", "Image"),
    ("file", "File"),
    ("link", "Link"),
    ("wysiwyg", "WYSIWYG Editor"),
    ("post_object", "Post Object"),
    ("relationship", "Relationship"),
    ("taxonomy", "Taxonomy"),
    ("user", "User"),
];

/// The location parameters the editor offers, with ACF's labels: those the matcher evaluates
/// for the screens of phase 1 (design 2.3). The options page is phase 2: no options page
/// exists yet to choose.
const PARAMS: [(LocationParam, &str); 6] = [
    (LocationParam::PostType, "Post Type"),
    (LocationParam::PageTemplate, "Page Template"),
    (LocationParam::PostTemplate, "Post Template"),
    (LocationParam::Taxonomy, "Taxonomy"),
    (LocationParam::UserRole, "User Role"),
    (LocationParam::CurrentUserRole, "Current User Role"),
];

/// ACF's operators: "is equal to" and "is not equal to".
const OPERATORS: [(LocationOperator, &str); 2] = [
    (LocationOperator::Is, "is equal to"),
    (LocationOperator::IsNot, "is not equal to"),
];

/// ACF's positions.
const POSITIONS: [(Position, &str); 3] = [
    (Position::AfterTitle, "High (after title)"),
    (Position::Normal, "Normal (after content)"),
    (Position::Side, "Side"),
];

/// ACF's Hide on Screen items, with its labels.
const HIDE_ITEMS: [(HideOnScreen, &str); 14] = [
    (HideOnScreen::Permalink, "Permalink"),
    (HideOnScreen::Content, "Content Editor"),
    (HideOnScreen::Excerpt, "Excerpt"),
    (HideOnScreen::Discussion, "Discussion"),
    (HideOnScreen::Comments, "Comments"),
    (HideOnScreen::Revisions, "Revisions"),
    (HideOnScreen::Slug, "Slug"),
    (HideOnScreen::Author, "Author"),
    (HideOnScreen::Format, "Format"),
    (HideOnScreen::PageAttributes, "Page Attributes"),
    (HideOnScreen::FeaturedImage, "Featured Image"),
    (HideOnScreen::Categories, "Categories"),
    (HideOnScreen::Tags, "Tags"),
    (HideOnScreen::SendTrackbacks, "Send Trackbacks"),
];

/// The Field Groups manager as an Ignite runtime resource. See the [module](self).
pub fn field_groups_resource(managers: Managers) -> RuntimeResource {
    let field = |name: &str, label: &str, kind: FieldKindDef, required: bool| IgniteFieldDef {
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
    let pairs = |items: &[(&str, &str)]| {
        items
            .iter()
            .map(|(v, l)| ((*v).to_owned(), (*l).to_owned()))
            .collect::<Vec<_>>()
    };
    let field_row = vec![
        field(F_LABEL, "Field Label", FieldKindDef::Text, true),
        field(F_NAME, "Field Name", FieldKindDef::Text, true),
        field(
            F_TYPE,
            "Field Type",
            FieldKindDef::Select(pairs(&PHASE_ONE_TYPES)),
            true,
        ),
        field(F_KEY, "Field Key", FieldKindDef::Text, false),
        field(F_REQUIRED, "Required", FieldKindDef::Checkbox, false),
        field(F_DEFAULT, "Default Value", FieldKindDef::Text, false),
        field(
            F_INSTRUCTIONS,
            "Instructions",
            FieldKindDef::Textarea,
            false,
        ),
        field(F_CHOICES, "Choices", FieldKindDef::Textarea, false),
        field(F_PLACEHOLDER, "Placeholder Text", FieldKindDef::Text, false),
    ];
    let rule = vec![
        field(
            R_PARAM,
            "Rule",
            FieldKindDef::Select(
                PARAMS
                    .iter()
                    .map(|(p, l)| (p.as_str().to_owned(), (*l).to_owned()))
                    .collect(),
            ),
            true,
        ),
        field(
            R_OPERATOR,
            "Operator",
            FieldKindDef::Select(
                OPERATORS
                    .iter()
                    .map(|(o, l)| (o.as_str().to_owned(), (*l).to_owned()))
                    .collect(),
            ),
            true,
        ),
        // Its choices depend on the rule's parameter (`options_with`).
        field(R_VALUE, "Value", FieldKindDef::Lookup, true),
    ];
    let or_group = vec![field(
        RULES,
        "Show this field group if",
        FieldKindDef::Repeater(rule),
        false,
    )];
    let mut resource = super::super::stay_on_edit(RuntimeResource::new(
        FIELD_GROUPS,
        "Field Groups",
        "Field Group",
        FieldGroupsData { managers },
    ));
    resource.fields = vec![
        field(TITLE, "Field Group Title", FieldKindDef::Text, true),
        field(FIELDS, "Fields", FieldKindDef::Repeater(field_row), false),
        field(
            LOCATION,
            "Location Rules",
            FieldKindDef::Repeater(or_group),
            false,
        ),
        field(
            POSITION,
            "Position",
            FieldKindDef::Select(
                POSITIONS
                    .iter()
                    .map(|(p, l)| (p.as_str().to_owned(), (*l).to_owned()))
                    .collect(),
            ),
            false,
        ),
        field(MENU_ORDER, "Order No.", FieldKindDef::Number, false),
        field(
            HIDE_ON_SCREEN,
            "Hide on Screen",
            FieldKindDef::CheckboxList(
                HIDE_ITEMS
                    .iter()
                    .map(|(h, l)| (h.as_str().to_owned(), (*l).to_owned()))
                    .collect(),
            ),
            false,
        ),
        field(DESCRIPTION, "Description", FieldKindDef::Textarea, false),
    ];
    resource.columns = vec![
        column(COL_TITLE, "Title", true, true),
        column(COL_KEY, "Key", true, true),
        column(COL_LOCATION, "Location", false, false),
        column(COL_FIELDS, "Fields", true, false),
        column(COL_ORIGIN, "Origin", false, false),
    ];
    resource.form_layout = FormLayout::new().schema([
        Component::from(Section::make("").schema([TITLE])),
        Component::from(Section::make("Fields").schema([FIELDS])),
        Tabs::make("Settings")
            .tabs([
                Tab::make("Location Rules").schema([LOCATION]),
                Tab::make("Presentation").schema([POSITION, MENU_ORDER, HIDE_ON_SCREEN]),
                Tab::make("Group Settings").schema([DESCRIPTION]),
            ])
            .into(),
    ]);
    navigation::place(
        &mut resource,
        navigation::FIELD_GROUPS_MANAGER,
        navigation::FIELD_GROUPS_ICON,
    );
    let resource = resource
        .record_route_key_name(KEY)
        .repeater(
            FIELDS,
            RepeaterSettings::new()
                .item_label(F_LABEL)
                .add_action_label("Add Field"),
        )
        .repeater(
            LOCATION,
            RepeaterSettings::new().add_action_label("Add rule group"),
        )
        .repeater(
            "location.rules",
            RepeaterSettings::new().min_items(1).add_action_label("and"),
        )
        .live("location.rules.param")
        .read_only_when(is_locked_record)
        .read_only_notice(locked_record_notice)
        .hide_default_delete_when(ListFilter::new().where_eq(LOCKED, true))
        .badge_color(COL_ORIGIN, |_| BadgeColor::Gray);
    crate::admin::ScreenCaps::all(MANAGE_OPTIONS).apply(resource)
}

/// The rows of the Field Groups manager: the field groups in use. See the [module](self).
#[derive(Clone)]
pub struct FieldGroupsData {
    managers: Managers,
}

/// The choices of a select, checkbox or radio as ACF's Choices box writes them: `value : label`
/// per line, or the value alone when the label is the same.
fn choices_text(choices: &[(String, String)]) -> String {
    choices
        .iter()
        .map(|(value, label)| {
            if value == label {
                value.clone()
            } else {
                format!("{value} : {label}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The choices of ACF's Choices box: one per line, `value : label` or a value that is its own
/// label; blank lines left out.
fn parse_choices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once(':') {
            Some((value, label)) if !value.trim().is_empty() => {
                (value.trim().to_owned(), label.trim().to_owned())
            }
            _ => (line.to_owned(), line.to_owned()),
        })
        .collect()
}

/// The choices of a field's kind, if it has some.
fn kind_choices(kind: &FieldKindDef) -> Option<&[(String, String)]> {
    match kind {
        FieldKindDef::Select(c) | FieldKindDef::Radio(c) | FieldKindDef::CheckboxList(c) => Some(c),
        _ => None,
    }
}

/// The kind of the ACF field type `field_type`, with `choices`; `None` for a type the editor
/// does not offer.
fn kind_of(field_type: &str, choices: Vec<(String, String)>) -> Option<FieldKindDef> {
    Some(match field_type {
        "text" => FieldKindDef::Text,
        "textarea" => FieldKindDef::Textarea,
        "number" => FieldKindDef::Number,
        "email" => FieldKindDef::Email,
        "url" => FieldKindDef::Url,
        "password" => FieldKindDef::Password,
        "select" => FieldKindDef::Select(choices),
        "checkbox" => FieldKindDef::CheckboxList(choices),
        "radio" => FieldKindDef::Radio(choices),
        "true_false" => FieldKindDef::Checkbox,
        "date_picker" => FieldKindDef::Date,
        "date_time_picker" => FieldKindDef::DateTime,
        "color_picker" => FieldKindDef::Color,
        "image" => FieldKindDef::Image,
        "file" => FieldKindDef::File,
        "link" => FieldKindDef::Link,
        "wysiwyg" => FieldKindDef::Wysiwyg,
        "post_object" => FieldKindDef::PostObject,
        "relationship" => FieldKindDef::Relationship,
        "taxonomy" => FieldKindDef::Taxonomy,
        "user" => FieldKindDef::User,
        _ => return None,
    })
}

/// The ACF type of `field`, as its JSON writes it.
fn acf_type(field: &FieldDef) -> String {
    serde_json::to_value(field)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

/// A summary of `group`'s location, as ACF's list shows it: "Post Type is Book or Page
/// Template is wide".
fn location_summary(group: &FieldGroupDef) -> String {
    let param = |p: LocationParam| {
        PARAMS
            .iter()
            .find(|(q, _)| *q == p)
            .map_or_else(|| p.as_str().to_owned(), |(_, l)| (*l).to_owned())
    };
    group
        .location
        .iter()
        .filter(|rules| !rules.is_empty())
        .map(|rules| {
            rules
                .iter()
                .map(|r| {
                    let op = match r.operator {
                        LocationOperator::Is => "is",
                        LocationOperator::IsNot => "is not",
                    };
                    format!("{} {op} {}", param(r.param), r.value)
                })
                .collect::<Vec<_>>()
                .join(" and ")
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

impl FieldGroupsData {
    /// The row of `group`.
    fn row_of(group: &FieldGroupDef) -> Row {
        let fields: Vec<Value> = group
            .fields
            .iter()
            .map(|f| {
                let default = match &f.default_value {
                    Value::Null => Value::Null,
                    Value::String(s) => Value::String(s.clone()),
                    other => Value::String(other.to_string()),
                };
                let placeholder = f
                    .settings
                    .get(F_PLACEHOLDER)
                    .cloned()
                    .unwrap_or(Value::Null);
                json!({
                    F_LABEL: f.label,
                    F_NAME: f.name,
                    F_TYPE: acf_type(f),
                    F_KEY: f.key,
                    F_REQUIRED: f.required,
                    F_DEFAULT: default,
                    F_INSTRUCTIONS: f.instructions,
                    F_CHOICES: kind_choices(&f.kind).map(choices_text),
                    F_PLACEHOLDER: placeholder,
                })
            })
            .collect();
        let location: Vec<Value> = group
            .location
            .iter()
            .map(|rules| {
                json!({
                    RULES: rules
                        .iter()
                        .map(|r| json!({
                            R_PARAM: r.param.as_str(),
                            R_OPERATOR: r.operator.as_str(),
                            R_VALUE: r.value,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let hide: Vec<&str> = group.hide_on_screen.iter().map(|h| h.as_str()).collect();
        let mut row = Row::new();
        row.insert(KEY.into(), json!(group.key));
        row.insert(LOCKED.into(), json!(group.is_locked()));
        row.insert(
            LOCKED_NOTICE.into(),
            json!(locked_notice("field group", &group.origin)),
        );
        row.insert(TITLE.into(), json!(group.title));
        row.insert(FIELDS.into(), Value::Array(fields));
        row.insert(LOCATION.into(), Value::Array(location));
        row.insert(POSITION.into(), json!(group.position.as_str()));
        row.insert(MENU_ORDER.into(), json!(group.menu_order));
        row.insert(HIDE_ON_SCREEN.into(), json!(hide));
        row.insert(DESCRIPTION.into(), json!(group.description));
        row.insert(COL_TITLE.into(), json!(group.title));
        row.insert(COL_KEY.into(), json!(group.key));
        row.insert(COL_LOCATION.into(), json!(location_summary(group)));
        row.insert(COL_FIELDS.into(), json!(group.fields.len()));
        row.insert(COL_ORIGIN.into(), json!(origin_label(&group.origin)));
        row
    }

    /// The field group with `key` as the manager shows it: the stored group's own rules, not
    /// the ones a post type's `field_groups` adds in the registry's view.
    fn group(&self, key: &str) -> Option<FieldGroupDef> {
        let mut group = self.managers.registry.field_group(key)?;
        if !group.is_locked()
            && let Some(own) = self.managers.registry.admin_field_group(key)
        {
            group.location = own.location;
        }
        Some(group)
    }

    fn rows(&self) -> Vec<Row> {
        self.managers
            .registry
            .all_field_groups()
            .iter()
            .filter_map(|g| self.group(&g.key))
            .map(|g| Self::row_of(&g))
            .collect()
    }

    /// `base` with the form's values in `input` (a value the form did not send is kept), or
    /// the form's refusal.
    fn apply(base: FieldGroupDef, input: &Row) -> Result<FieldGroupDef, ValidationError> {
        let mut group = base;
        if let Some(title) = text(input, TITLE) {
            group.title = title;
        }
        if let Some(rows) = list(input, FIELDS) {
            let previous: HashMap<String, FieldDef> = group
                .fields
                .iter()
                .map(|f| (f.key.clone(), f.clone()))
                .collect();
            let mut fields = Vec::with_capacity(rows.len());
            for (index, row) in rows.iter().enumerate() {
                let empty = Map::new();
                let row = row.as_object().unwrap_or(&empty);
                fields.push(field_of(row, &previous).map_err(|message| {
                    ValidationError::with_messages([(
                        FIELDS,
                        format!("Field {}: {message}", index + 1),
                    )])
                })?);
            }
            group.fields = fields;
        }
        if let Some(groups) = list(input, LOCATION) {
            group.location = groups
                .iter()
                .filter_map(|g| g.get(RULES))
                .map(|rules| {
                    let rules = match rules {
                        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
                        other => other.clone(),
                    };
                    rules
                        .as_array()
                        .map(|rules| rules.iter().filter_map(rule_of).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .filter(|rules| !rules.is_empty())
                .collect();
        }
        if let Some(position) = text(input, POSITION) {
            group.position = position.parse().unwrap_or_default();
        }
        if input.contains_key(MENU_ORDER) {
            group.menu_order = number(input, MENU_ORDER)
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0);
        }
        if let Some(items) = list(input, HIDE_ON_SCREEN) {
            group.hide_on_screen = strings(&items)
                .iter()
                .filter_map(|h| h.parse().ok())
                .collect();
        }
        if let Some(description) = text(input, DESCRIPTION) {
            group.description = description;
        }
        Ok(group)
    }

    /// Stores `group` and serves it: the post type screens again, whose forms show its fields.
    async fn save(
        &self,
        db: &Database,
        group: FieldGroupDef,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let key = group.key.clone();
        match self
            .managers
            .registry
            .save_admin_definition(db, group)
            .await
        {
            Ok(()) => {}
            Err(RegistryError::Definition(e)) => return Ok(Err(refusal(&e))),
            Err(e) => return Err(registry_error(e)),
        }
        self.managers.sync();
        self.group(&key)
            .map(|g| Self::row_of(&g))
            .ok_or_else(not_found)
            .map(Ok)
    }

    async fn create_checked(
        &self,
        db: &Database,
        input: Row,
    ) -> Result<Result<Row, ValidationError>, Error> {
        let mut base = FieldGroupDef::new(&format!("group_{}", super::unique_id()), "");
        base.origin = Origin::Admin;
        match Self::apply(base, &input) {
            Ok(group) => self.save(db, group).await,
            Err(refused) => Ok(Err(refused)),
        }
    }

    /// The choices of a rule's value for the parameter `param`.
    async fn values_for(&self, db: &Database, param: &str) -> Result<Vec<(String, String)>, Error> {
        let label = |name: &str, slug: &str| {
            if name.is_empty() {
                slug.to_owned()
            } else {
                name.to_owned()
            }
        };
        Ok(match param.parse::<LocationParam>() {
            Ok(LocationParam::PostType) => builtin_post_types()
                .into_iter()
                .chain(self.managers.registry.all_post_types())
                .map(|d| (d.slug.clone(), label(&d.labels.singular_name, &d.slug)))
                .collect(),
            Ok(LocationParam::PageTemplate | LocationParam::PostTemplate) => {
                std::iter::once(("default".to_owned(), "Default Template".to_owned()))
                    .chain(
                        self.managers
                            .resources
                            .page_templates()
                            .iter()
                            .map(|t| (t.name.clone(), t.title.clone())),
                    )
                    .collect()
            }
            Ok(LocationParam::Taxonomy) => {
                let mut values = vec![
                    ("all".to_owned(), "All".to_owned()),
                    ("category".to_owned(), "Categories".to_owned()),
                    ("post_tag".to_owned(), "Tags".to_owned()),
                ];
                values.extend(
                    self.managers
                        .registry
                        .all_taxonomies()
                        .into_iter()
                        .map(|d| (d.slug.clone(), label(&d.labels.name, &d.slug))),
                );
                values
            }
            Ok(LocationParam::UserRole | LocationParam::CurrentUserRole) => {
                role_choices(db).await?
            }
            _ => Vec::new(),
        })
    }
}

/// The field of a field row, keeping the settings of the field with the same key in
/// `previous` (an import's `return_format`, ...), or why it is refused.
fn field_of(
    row: &Map<String, Value>,
    previous: &HashMap<String, FieldDef>,
) -> Result<FieldDef, String> {
    let get = |key: &str| match row.get(key) {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let field_type = get(F_TYPE);
    let kind = kind_of(&field_type, parse_choices(&get(F_CHOICES)))
        .ok_or_else(|| format!("the field type \u{201c}{field_type}\u{201d} is not offered."))?;
    let mut key = get(F_KEY);
    if key.is_empty() {
        key = format!("field_{}", super::unique_id());
    }
    let mut field = FieldDef::new(&key, &get(F_NAME), &get(F_LABEL), kind);
    field.required = matches!(row.get(F_REQUIRED), Some(Value::Bool(true)));
    field.instructions = get(F_INSTRUCTIONS);
    let default = get(F_DEFAULT);
    field.default_value = if default.is_empty() {
        Value::Null
    } else {
        Value::String(default)
    };
    if let Some(old) = previous.get(&key) {
        field.settings = old.settings.clone();
    }
    match get(F_PLACEHOLDER) {
        placeholder if placeholder.is_empty() => {
            field.settings.remove(F_PLACEHOLDER);
        }
        placeholder => {
            field
                .settings
                .insert(F_PLACEHOLDER.to_owned(), Value::String(placeholder));
        }
    }
    Ok(field)
}

/// The location rule of a rule row, or `None` when it is incomplete.
fn rule_of(row: &Value) -> Option<LocationRule> {
    let get = |key: &str| row.get(key).and_then(Value::as_str).map(str::trim);
    Some(LocationRule {
        param: get(R_PARAM)?.parse().ok()?,
        operator: get(R_OPERATOR)
            .and_then(|o| o.parse().ok())
            .unwrap_or(LocationOperator::Is),
        value: get(R_VALUE).filter(|v| !v.is_empty())?.to_owned(),
    })
}

/// The form's message for a refused group, under the field it is about.
fn refusal(e: &DefinitionError) -> ValidationError {
    let field = match e {
        DefinitionError::MissingTitle(_) => TITLE,
        DefinitionError::TooLarge { what, .. } if what.contains("title") => TITLE,
        DefinitionError::TooLarge { what, .. } if what.contains("description") => DESCRIPTION,
        DefinitionError::TooLarge { what, .. } if what.contains("location") => LOCATION,
        _ => FIELDS,
    };
    ValidationError::with_messages([(field, definition_message(e))])
}

impl RuntimeData for FieldGroupsData {
    fn list<'a>(&'a self, _db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move {
            Ok(page_of(
                self.rows(),
                query,
                &[COL_TITLE, COL_KEY],
                COL_TITLE,
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
        _db: &'a Database,
        _user: &'a ActingUser,
        key: &'a str,
    ) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move { Ok(self.group(key).map(|g| Self::row_of(&g))) })
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
            let Some(group) = self.group(key) else {
                return Err(not_found());
            };
            if group.is_locked() {
                return Ok(Err(refusal(&DefinitionError::Locked(group.origin.clone()))));
            }
            match Self::apply(group, &input) {
                Ok(group) => self.save(db, group).await,
                Err(refused) => Ok(Err(refused)),
            }
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
                .delete_admin_definition(db, DefinitionKind::FieldGroup, key)
                .await
                .map_err(registry_error)?;
            self.managers.sync();
            Ok(u64::from(deleted))
        })
    }

    fn options_with<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
        field: &'a str,
        state: &'a Row,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move {
            if field != RULE_VALUE_PATH {
                return Ok(Vec::new());
            }
            let param = state.get(R_PARAM).and_then(Value::as_str).unwrap_or("");
            self.values_for(db, param).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_read_and_write_as_acfs_choices_box() {
        let choices = parse_choices("red : Red\n\n green \nblue:Blue sky");
        assert_eq!(
            choices,
            vec![
                ("red".to_owned(), "Red".to_owned()),
                ("green".to_owned(), "green".to_owned()),
                ("blue".to_owned(), "Blue sky".to_owned()),
            ]
        );
        assert_eq!(choices_text(&choices), "red : Red\ngreen\nblue : Blue sky");
    }

    #[test]
    fn a_field_row_makes_a_field_and_keeps_its_other_settings() {
        let mut old = FieldDef::new("field_abc", "colour", "Colour", FieldKindDef::Text);
        old.settings
            .insert("return_format".to_owned(), Value::from("value"));
        let previous = HashMap::from([("field_abc".to_owned(), old)]);
        let row = json!({
            "label": "Colour", "name": "colour", "type": "select", "key": "field_abc",
            "required": true, "choices": "red : Red\nblue : Blue", "placeholder": "Pick",
        });
        let field = field_of(row.as_object().unwrap(), &previous).unwrap();
        assert_eq!(
            field.kind,
            FieldKindDef::Select(vec![
                ("red".to_owned(), "Red".to_owned()),
                ("blue".to_owned(), "Blue".to_owned()),
            ])
        );
        assert!(field.required);
        assert_eq!(field.settings["return_format"], "value");
        assert_eq!(field.settings["placeholder"], "Pick");

        let new = json!({"label": "Venue", "name": "venue", "type": "text"});
        let field = field_of(new.as_object().unwrap(), &HashMap::new()).unwrap();
        assert!(
            field.key.starts_with("field_") && field.key.len() == 19,
            "{}",
            field.key
        );
        let bad = json!({"label": "X", "name": "x", "type": "repeater"});
        assert!(field_of(bad.as_object().unwrap(), &HashMap::new()).is_err());
    }

    #[test]
    fn the_location_summary_reads_as_acfs() {
        let mut group = FieldGroupDef::new("group_a", "A");
        let rule = |param, value: &str| LocationRule {
            param,
            operator: LocationOperator::Is,
            value: value.to_owned(),
        };
        group.location = vec![
            vec![rule(LocationParam::PostType, "book")],
            vec![rule(LocationParam::PageTemplate, "wide")],
        ];
        assert_eq!(
            location_summary(&group),
            "Post Type is book or Page Template is wide"
        );
    }
}
