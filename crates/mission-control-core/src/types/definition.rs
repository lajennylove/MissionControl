//! The definition format of custom post types, taxonomies and field groups (design 5.3).
//!
//! One format, produced three ways: the admin's manager screens, Rust code (as Poet and ACF
//! Composer do in a theme) and plugins. [`Origin`] records which; a definition made in code or
//! by a plugin is locked, so the admin shows it as "defined in code", as ACF shows local JSON.
//!
//! - [`PostTypeDef`] takes `register_post_type`'s options, with WordPress's names and defaults.
//! - [`TaxonomyDef`] takes `register_taxonomy`'s options.
//! - [`FieldGroupDef`] is an ACF field group: its key, title, [`FieldDef`]s, location rules,
//!   position, order and hide-on-screen options. A field's kind is Ignite's [`FieldKindDef`];
//!   in JSON it is ACF's `type` (`text`, `true_false`, `date_picker`, ...) with ACF's
//!   type-specific settings (`choices`, `step`, `sub_fields`, `layouts`, `clone`).
//!
//! The JSON keys are WordPress's and ACF's, so a definition exported from WordPress or ACF maps
//! naturally. As PHP writes them, booleans may be `0`/`1` or `"0"`/`"1"`, and `""` is false or
//! unset; they never become a slug or a menu name. An option Mission Control does not model is
//! kept, as written, in the definition's `settings`, so saving it again loses nothing. A value
//! Mission Control does not know (a support, a field type, a location rule parameter) is
//! refused with an error that names it.
//!
//! # Reading untrusted definitions
//!
//! `from_json` is the way in: it refuses JSON over [`MAX_JSON_BYTES`], parses it, sets the
//! [`Origin`] the caller gives, and validates. The origin is never read from the JSON, so a
//! definition cannot unlock itself or claim another plugin's name. Validation applies the rules
//! WordPress and ACF apply (slugs, reserved names, keys, field names) and the size limits below
//! ([`MAX_FIELD_DEPTH`], [`MAX_FIELDS`], ...), each refused with an error that names the limit.
//!
//! The types also implement serde's `Deserialize`, which the field group's own parsing uses.
//! It checks the shape and the nesting depth of fields, but **not** the other rules, and it
//! marks the definition [`Origin::Code`] (locked). A caller that deserializes a definition by
//! another route must cap the input's size first and call `validate` before using it.
//!
//! ```
//! use mission_control_core::types::{Origin, PostTypeDef, Support};
//!
//! let book = PostTypeDef::from_json(r#"{"slug": "book", "public": true,
//!     "labels": {"name": "Books", "singular_name": "Book"},
//!     "supports": ["title", "editor", "thumbnail"]}"#, Origin::Admin).unwrap();
//! assert_eq!(book.supports, [Support::Title, Support::Editor, Support::Thumbnail]);
//! assert!(book.ensure_editable().is_ok());
//! ```

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use ignite_core::FieldKindDef;
use ignite_core::{BlockDef, FieldDef as IgniteFieldDef};

use crate::content::Taxonomy;
use crate::rest::posts::{PostType, RestTaxonomy, Supports};
use crate::rest::terms::TaxonomySpec;
use crate::users::{PostTypeCaps, TaxonomyCaps};

/// The longest post type slug WordPress registers (`register_post_type`).
pub const POST_TYPE_SLUG_MAX: usize = 20;

/// The longest taxonomy slug WordPress registers (`register_taxonomy`).
pub const TAXONOMY_SLUG_MAX: usize = 32;

/// The largest definition `from_json` reads, in bytes: 1 MiB. ACF's largest real-world groups
/// are a few hundred KiB.
pub const MAX_JSON_BYTES: usize = 1 << 20;

/// How deep fields may nest: a field of the group is at depth 1, a repeater's sub-field at 2,
/// and so on (a flexible content layout's fields are one level below the field). ACF's screens
/// rarely pass 5.
pub const MAX_FIELD_DEPTH: usize = 10;

/// The most fields a group may hold, counting sub-fields at every depth.
pub const MAX_FIELDS: usize = 1000;

/// The most layouts one flexible content field may have.
pub const MAX_LAYOUTS: usize = 100;

/// The most choices one select or radio field may have.
pub const MAX_CHOICES: usize = 1000;

/// The most location rules a group may have, counting every rule of every or-group.
pub const MAX_LOCATION_RULES: usize = 100;

/// The most entries of a list of names: a post type's taxonomies and field groups, a
/// taxonomy's object types, a clone field's keys, a group's hide-on-screen items.
pub const MAX_LIST_ITEMS: usize = 100;

/// The longest key (`field_…`, `group_…`, `layout_…`), field name or layout name, in bytes.
pub const MAX_KEY_LEN: usize = 64;

/// The longest text (a label, title, description, instructions, choice, location value or
/// REST base), in bytes.
pub const MAX_TEXT_LEN: usize = 4096;

/// The longest menu icon, in bytes: room for a `data:image/svg+xml;base64,` icon.
pub const MAX_ICON_LEN: usize = 64 * 1024;

/// Post type names WordPress refuses or reserves: its own types, and the names
/// `register_post_type` and the query vars keep for themselves.
pub const RESERVED_POST_TYPES: &[&str] = &[
    "post",
    "page",
    "attachment",
    "revision",
    "nav_menu_item",
    "custom_css",
    "customize_changeset",
    "oembed_cache",
    "user_request",
    "wp_block",
    "wp_template",
    "wp_template_part",
    "wp_global_styles",
    "wp_navigation",
    "wp_font_family",
    "wp_font_face",
    "action",
    "author",
    "order",
    "theme",
];

/// WordPress's own taxonomies, which a definition may not replace.
pub const RESERVED_TAXONOMIES: &[&str] = &[
    "category",
    "post_tag",
    "nav_menu",
    "link_category",
    "post_format",
    "wp_theme",
    "wp_template_part_area",
    "wp_pattern_category",
];

/// Why a definition was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionError {
    /// The JSON is not a definition. The message is the parser's, and names the bad value.
    Json(String),
    /// A value Mission Control does not know, such as a post type support or a field type.
    Unknown {
        /// What the value is, such as `"post type support"`.
        what: &'static str,
        /// The value, as written.
        value: String,
    },
    /// The definition passes a size limit, such as [`MAX_FIELD_DEPTH`].
    TooLarge {
        /// What is too large, such as `"field nesting depth"`.
        what: &'static str,
        /// The limit.
        limit: usize,
    },
    /// A post type or taxonomy slug that WordPress would not register.
    InvalidSlug {
        /// `"post type"` or `"taxonomy"`.
        what: &'static str,
        /// The slug, as written.
        slug: String,
        /// Why it is refused.
        reason: String,
    },
    /// A slug WordPress keeps for itself.
    ReservedSlug {
        /// `"post type"` or `"taxonomy"`.
        what: &'static str,
        /// The slug.
        slug: String,
    },
    /// A key not formatted as ACF formats it: the prefix (`field_`, `group_` or `layout_`) and
    /// at least one letter, digit, `_` or `-`.
    InvalidKey {
        /// The key, as written.
        key: String,
        /// The prefix it must start with.
        prefix: &'static str,
    },
    /// A field name that is empty, or holds a character other than a letter, digit, `_` or `-`.
    InvalidFieldName(String),
    /// Two fields with the same name under the same parent (the group, a field or a layout).
    DuplicateFieldName(String),
    /// A layout name that is empty, or holds a character other than a letter, digit, `_` or
    /// `-`.
    InvalidLayoutName(String),
    /// Two layouts of one flexible content field with the same name.
    DuplicateLayoutName(String),
    /// Two fields or layouts with the same key in one group.
    DuplicateKey(String),
    /// A field group without a title.
    MissingTitle(String),
    /// A repeater, group or flexible content field whose kind does not hold its own
    /// `sub_fields` or `layouts`, or a field of another kind that has some. The field's key.
    SubFieldsMismatch(String),
    /// A field whose kind has no ACF field type, such as [`FieldKindDef::Lookup`]: it cannot be
    /// written in a definition.
    UnsupportedFieldKind {
        /// The field's key.
        key: String,
        /// The kind, as Rust prints it.
        kind: String,
    },
    /// A `settings` entry that repeats an option the definition has its own place for, such
    /// as a field's `type` or a post type's `public`.
    ReservedSetting {
        /// The field's key, or the post type's or taxonomy's slug.
        key: String,
        /// The setting's name.
        setting: String,
    },
    /// An empty `capability_type`.
    InvalidCapabilityType,
    /// A plugin origin without the plugin's name.
    EmptyPluginName,
    /// The definition was made in code or by a plugin, so it cannot be changed or deleted
    /// from the admin.
    Locked(Origin),
    /// A key a [code builder](super::code) made from the names is used twice (names with `_`
    /// can make the same key) or passes [`MAX_KEY_LEN`]. Setting the key with `.key()` fixes
    /// it.
    GeneratedKey {
        /// The key made.
        key: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// An option set with a [code builder](super::code) on a field whose type does not take
    /// it, such as `choices` on a text field.
    MisplacedOption {
        /// The field's key.
        key: String,
        /// The option, such as `choices` or `step`.
        option: &'static str,
    },
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid definition: {message}"),
            Self::Unknown { what, value } => write!(f, "unknown {what} `{value}`"),
            Self::TooLarge { what, limit } => {
                write!(f, "the {what} passes the limit of {limit}")
            }
            Self::InvalidSlug { what, slug, reason } => {
                write!(f, "invalid {what} slug `{slug}`: {reason}")
            }
            Self::ReservedSlug { what, slug } => {
                write!(f, "the {what} slug `{slug}` is reserved by WordPress")
            }
            Self::InvalidKey { key, prefix } => {
                write!(
                    f,
                    "invalid key `{key}`: it must be `{prefix}` and letters, digits, `_` or `-`"
                )
            }
            Self::InvalidFieldName(name) => write!(
                f,
                "invalid field name `{name}`: use letters, digits, `_` and `-`"
            ),
            Self::DuplicateFieldName(name) => {
                write!(f, "two fields are named `{name}` under the same parent")
            }
            Self::InvalidLayoutName(name) => write!(
                f,
                "invalid layout name `{name}`: use letters, digits, `_` and `-`"
            ),
            Self::DuplicateLayoutName(name) => {
                write!(f, "two layouts of one field are named `{name}`")
            }
            Self::DuplicateKey(key) => write!(f, "the key `{key}` is used twice"),
            Self::MissingTitle(key) => write!(f, "the field group `{key}` has no title"),
            Self::SubFieldsMismatch(key) => write!(
                f,
                "the field `{key}`: its kind does not match its sub-fields or layouts"
            ),
            Self::UnsupportedFieldKind { key, kind } => {
                write!(
                    f,
                    "the field `{key}`: the kind {kind} has no ACF field type"
                )
            }
            Self::ReservedSetting { key, setting } => write!(
                f,
                "`{key}`: the setting `{setting}` has its own place in the definition"
            ),
            Self::InvalidCapabilityType => write!(f, "the capability type is empty"),
            Self::EmptyPluginName => write!(f, "a plugin origin needs the plugin's name"),
            Self::Locked(origin) => write!(
                f,
                "this definition is defined in {}, so it cannot be changed here",
                match origin {
                    Origin::Plugin(name) => format!("the plugin `{name}`"),
                    _ => "code".to_owned(),
                }
            ),
            Self::GeneratedKey { key, reason } => write!(
                f,
                "the key `{key}`, made from the names, {reason}: set the key with `.key()`"
            ),
            Self::MisplacedOption { key, option } => {
                write!(f, "the field `{key}` does not take the option `{option}`")
            }
        }
    }
}

impl std::error::Error for DefinitionError {}

/// Parses `json` (at most [`MAX_JSON_BYTES`]) as a definition, with no validation.
fn parse<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T, DefinitionError> {
    if json.len() > MAX_JSON_BYTES {
        return Err(DefinitionError::TooLarge {
            what: "definition size in bytes",
            limit: MAX_JSON_BYTES,
        });
    }
    serde_json::from_str(json).map_err(|e| DefinitionError::Json(e.to_string()))
}

/// Refuses `text` longer than `limit` bytes.
fn check_len(what: &'static str, text: &str, limit: usize) -> Result<(), DefinitionError> {
    if text.len() > limit {
        Err(DefinitionError::TooLarge { what, limit })
    } else {
        Ok(())
    }
}

/// Refuses a list longer than `limit`.
fn check_count(what: &'static str, count: usize, limit: usize) -> Result<(), DefinitionError> {
    if count > limit {
        Err(DefinitionError::TooLarge { what, limit })
    } else {
        Ok(())
    }
}

/// Refuses a `settings` entry named as one of `modelled`, the options with their own place.
fn check_settings(
    owner: &str,
    settings: &Map<String, Value>,
    modelled: &[&str],
) -> Result<(), DefinitionError> {
    match settings.keys().find(|k| modelled.contains(&k.as_str())) {
        Some(setting) => Err(DefinitionError::ReservedSetting {
            key: owner.to_owned(),
            setting: setting.clone(),
        }),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Origin

/// Who made a definition. Only [`Admin`](Self::Admin) definitions can be edited in the admin;
/// the others show as "defined in code".
///
/// The origin is never written to or read from a definition's JSON: the code that loads a
/// definition knows where it came from, and passes it to `from_json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Made with the admin's manager screens, and stored in the database.
    #[default]
    Admin,
    /// Made in Rust code: a theme's or plugin's WASM module, or a site's own binary.
    Code,
    /// Made by the plugin with this name.
    Plugin(String),
}

impl Origin {
    /// Whether the definition is read-only in the admin: made in code or by a plugin.
    pub fn is_locked(&self) -> bool {
        !matches!(self, Self::Admin)
    }

    /// `Ok` for an admin definition, [`DefinitionError::Locked`] for one made in code or by a
    /// plugin. The registry and the admin call it before they change or delete a definition.
    pub fn ensure_editable(&self) -> Result<(), DefinitionError> {
        if self.is_locked() {
            Err(DefinitionError::Locked(self.clone()))
        } else {
            Ok(())
        }
    }

    fn validate(&self) -> Result<(), DefinitionError> {
        match self {
            Self::Plugin(name) if name.trim().is_empty() => Err(DefinitionError::EmptyPluginName),
            Self::Plugin(name) => check_len("plugin name length", name, MAX_KEY_LEN),
            _ => Ok(()),
        }
    }
}

/// The origin of a definition read through serde, whatever its JSON says: [`Origin::Code`],
/// locked, until its loader sets the real one.
fn loaded_origin() -> Origin {
    Origin::Code
}

/// Reads and drops an `origin` written in the JSON: the loader sets the origin.
fn ignore_origin<'de, D: Deserializer<'de>>(d: D) -> Result<Origin, D::Error> {
    IgnoredAny::deserialize(d)?;
    Ok(loaded_origin())
}

// ---------------------------------------------------------------------------------------------
// String enums

/// Declares an enum of the string values WordPress or ACF stores, with `as_str`, `FromStr`,
/// and serde through the string; an unknown string is refused with
/// [`DefinitionError::Unknown`], which names it.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $what:literal,
        { $( $(#[$vmeta:meta])+ $variant:ident = $value:literal, )+ }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vmeta])+ $variant, )+
        }

        impl $name {
            /// Every value, in the order WordPress or ACF lists them.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The name WordPress or ACF stores.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)+
                }
            }
        }

        impl FromStr for $name {
            type Err = DefinitionError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($value => Ok(Self::$variant),)+
                    other => Err(DefinitionError::Unknown {
                        what: $what,
                        value: other.to_owned(),
                    }),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                s.parse().map_err(de::Error::custom)
            }
        }
    };
}

string_enum! {
    /// A feature a post type supports, as `add_post_type_support` names it.
    Support, "post type support",
    {
        /// `title`.
        Title = "title",
        /// `editor`: the content.
        Editor = "editor",
        /// `author`.
        Author = "author",
        /// `thumbnail`: the featured image.
        Thumbnail = "thumbnail",
        /// `excerpt`.
        Excerpt = "excerpt",
        /// `trackbacks`.
        Trackbacks = "trackbacks",
        /// `custom-fields`: the meta box, and the REST `meta` field.
        CustomFields = "custom-fields",
        /// `comments`.
        Comments = "comments",
        /// `revisions`.
        Revisions = "revisions",
        /// `page-attributes`: the template, and for a hierarchical type the parent and order.
        PageAttributes = "page-attributes",
        /// `post-formats`.
        PostFormats = "post-formats",
    }
}

string_enum! {
    /// Where a field group's box shows on the edit screen, as ACF's `position`.
    #[derive(Default)]
    Position, "field group position",
    {
        /// `acf_after_title`: high, after the title. ACF's third position, kept so an ACF
        /// export maps.
        AfterTitle = "acf_after_title",
        /// `normal`: in the main column, after the content. The default.
        #[default]
        Normal = "normal",
        /// `side`: in the sidebar.
        Side = "side",
    }
}

string_enum! {
    /// An item of the edit screen a field group hides, as ACF's `hide_on_screen`.
    HideOnScreen, "hide-on-screen option",
    {
        /// The permalink.
        Permalink = "permalink",
        /// The content editor.
        Content = "the_content",
        /// The excerpt.
        Excerpt = "excerpt",
        /// The discussion box.
        Discussion = "discussion",
        /// The comments box.
        Comments = "comments",
        /// The revisions box.
        Revisions = "revisions",
        /// The slug.
        Slug = "slug",
        /// The author.
        Author = "author",
        /// The post format.
        Format = "format",
        /// The page attributes.
        PageAttributes = "page_attributes",
        /// The featured image.
        FeaturedImage = "featured_image",
        /// The categories.
        Categories = "categories",
        /// The tags.
        Tags = "tags",
        /// Send trackbacks.
        SendTrackbacks = "send-trackbacks",
    }
}

string_enum! {
    /// What a location rule tests, as ACF's `param`. The matcher is Task 5.4.
    LocationParam, "location rule parameter",
    {
        /// The post type.
        PostType = "post_type",
        /// The post's template.
        PostTemplate = "post_template",
        /// The post's status.
        PostStatus = "post_status",
        /// The post's format.
        PostFormat = "post_format",
        /// The post's category.
        PostCategory = "post_category",
        /// A term of the post, as `taxonomy:slug`.
        PostTaxonomy = "post_taxonomy",
        /// One post.
        Post = "post",
        /// The page's template.
        PageTemplate = "page_template",
        /// The page's type: front page, posts page, top level, parent or child.
        PageType = "page_type",
        /// The page's parent.
        PageParent = "page_parent",
        /// One page.
        Page = "page",
        /// The signed-in user is (or is not) viewing the front end or the admin.
        CurrentUser = "current_user",
        /// The signed-in user's role.
        CurrentUserRole = "current_user_role",
        /// A user form: add, edit or register.
        UserForm = "user_form",
        /// The edited user's role.
        UserRole = "user_role",
        /// A taxonomy's term form.
        Taxonomy = "taxonomy",
        /// An attachment, by mime type.
        Attachment = "attachment",
        /// A comment.
        Comment = "comment",
        /// A widget.
        Widget = "widget",
        /// A navigation menu.
        NavMenu = "nav_menu",
        /// A navigation menu item.
        NavMenuItem = "nav_menu_item",
        /// A block.
        Block = "block",
        /// An options page.
        OptionsPage = "options_page",
    }
}

string_enum! {
    /// How a location rule compares, as ACF's `operator`.
    LocationOperator, "location rule operator",
    {
        /// `==`: is.
        Is = "==",
        /// `!=`: is not.
        IsNot = "!=",
    }
}

// ---------------------------------------------------------------------------------------------
// Lenient values, as WordPress and ACF export them

/// A field's `default_value` as the value its kind stores, for `get_field`: as ACF exports a
/// default, a `true_false`'s may be `0`/`1` or `"0"`/`"1"` and a number's a numeric string.
/// `None` for no default: `null`, an empty string (ACF's empty default), and `false` for a
/// select or radio (ACF's "no choice"), and an empty list for a select or radio (ACF 5 exports
/// `"default_value": []`).
///
/// - `true_false`: read with [`php_bool`]'s rules, a boolean.
/// - `number`: a numeric string is a number, whole when it reads as one.
/// - `number` with a step (decimal): a numeric string is a number with a fraction (`f64`).
/// - `select` or `radio`: a one-item list is that item unless the field's `multiple` setting is
///   true, as ACF's select formatting unwraps it.
///
/// Any other value is kept as written, so a default that does not fit the type asked for is
/// still a type error.
fn typed_default_value(
    kind: &FieldKindDef,
    value: &Value,
    settings: &Map<String, Value>,
) -> Option<Value> {
    let float = |s: &str| {
        s.trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
    };
    match (kind, value) {
        (_, Value::Null) => None,
        (_, Value::String(s)) if s.is_empty() => None,
        (FieldKindDef::Checkbox, v) => Some(php_bool(v).map_or_else(|| v.clone(), Value::Bool)),
        (FieldKindDef::Number, Value::String(s)) => Some(
            s.trim()
                .parse::<i64>()
                .map(Value::from)
                .ok()
                .or_else(|| float(s))
                .unwrap_or_else(|| value.clone()),
        ),
        (FieldKindDef::Decimal(_), Value::String(s)) => {
            Some(float(s).unwrap_or_else(|| value.clone()))
        }
        (FieldKindDef::Select(_) | FieldKindDef::Radio(_), Value::Bool(false)) => None,
        (FieldKindDef::Select(_) | FieldKindDef::Radio(_), Value::Array(items))
            if items.is_empty() =>
        {
            None
        }
        (FieldKindDef::Select(_) | FieldKindDef::Radio(_), Value::Array(items))
            if items.len() == 1
                && !settings.get("multiple").and_then(php_bool).unwrap_or(false) =>
        {
            typed_default_value(kind, &items[0], settings)
        }
        (_, v) => Some(v.clone()),
    }
}

/// A PHP scalar read as a boolean, as PHP's truthiness reads it: `true`/`false`, a number
/// (0 is false), and the strings `"1"`, `"true"`, `"0"`, `"false"` and `""` (false). `None`
/// for any other value, such as `null`, another string, a list or an object.
fn php_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => Some(n.as_f64() != Some(0.0)),
        Value::String(s) => match s.as_str() {
            "1" | "true" => Some(true),
            "0" | "false" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Deserializers that read values as PHP writes them: booleans as `0`/`1` or `"0"`/`"1"`, and
/// an empty string for an unset option.
mod lenient {
    use super::*;

    struct BoolVisitor;

    impl Visitor<'_> for BoolVisitor {
        type Value = Option<bool>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a boolean, 0 or 1")
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            match v {
                0 => Ok(Some(false)),
                1 => Ok(Some(true)),
                _ => Err(E::invalid_value(de::Unexpected::Signed(v), &self)),
            }
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            self.visit_i64(i64::try_from(v).unwrap_or(i64::MAX))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            match v {
                "" => Ok(None),
                "1" | "true" => Ok(Some(true)),
                "0" | "false" => Ok(Some(false)),
                _ => Err(E::invalid_value(de::Unexpected::Str(v), &self)),
            }
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    /// A boolean; an empty value is `false`.
    pub(super) fn bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        Ok(d.deserialize_any(BoolVisitor)?.unwrap_or(false))
    }

    /// A boolean; an empty value is `true`.
    pub(super) fn bool_true<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        Ok(d.deserialize_any(BoolVisitor)?.unwrap_or(true))
    }

    /// An optional boolean; an empty value is unset.
    pub(super) fn opt_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        d.deserialize_any(BoolVisitor)
    }

    /// An optional string; an empty string is unset.
    pub(super) fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.filter(|s| !s.is_empty()))
    }

    /// A string; `null` is empty.
    pub(super) fn string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
    }

    /// A scalar as text, as PHP compares it: a string as written, a number's digits, `true` as
    /// `"1"`, and `false` or `null` as `""`.
    pub(super) fn scalar_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) => Ok(s),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(true) => Ok("1".to_owned()),
            Value::Bool(false) | Value::Null => Ok(String::new()),
            other => Err(de::Error::custom(format!(
                "expected a string or a number, found `{other}`"
            ))),
        }
    }

    /// A whole number, or its digits as a string; an empty value is unset.
    pub(super) fn opt_i32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
        match Value::deserialize(d)? {
            Value::Null => Ok(None),
            Value::String(s) if s.trim().is_empty() => Ok(None),
            Value::String(s) => s.trim().parse().map(Some).map_err(de::Error::custom),
            Value::Number(n) => n
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| de::Error::custom(format!("`{n}` is not a whole number"))),
            other => Err(de::Error::custom(format!(
                "`{other}` is not a whole number"
            ))),
        }
    }

    /// A whole number; an empty value is 0.
    pub(super) fn i32<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
        Ok(opt_i32(d)?.unwrap_or(0))
    }

    /// A list of strings; an empty string (PHP's empty value) is an empty list.
    pub(super) fn strings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        list(d)
    }

    /// A list of values; an empty string or `null` is an empty list. Read in place, not
    /// through a JSON value, so the order of the objects' keys (ACF's `choices`) is kept.
    pub(super) fn list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        d: D,
    ) -> Result<Vec<T>, D::Error> {
        struct ListVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ListVisitor<T> {
            type Value = Vec<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut list = Vec::new();
                while let Some(item) = seq.next_element()? {
                    list.push(item);
                }
                Ok(list)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.is_empty() {
                    Ok(Vec::new())
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }
        }

        d.deserialize_any(ListVisitor(std::marker::PhantomData))
    }
}

// ---------------------------------------------------------------------------------------------
// Post types

/// Declares a label set: `name` and `singular_name`, and optional labels WordPress fills from
/// them when unset.
macro_rules! label_set {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[doc = $doc:literal])+ $field:ident, )+ }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
        pub struct $name {
            /// The general name, plural, such as "Books".
            #[serde(default, deserialize_with = "lenient::string")]
            pub name: String,
            /// The name of one item, such as "Book".
            #[serde(default, deserialize_with = "lenient::string")]
            pub singular_name: String,
            $(
                $(#[doc = $doc])+
                #[serde(
                    default,
                    skip_serializing_if = "Option::is_none",
                    deserialize_with = "lenient::opt_string"
                )]
                pub $field: Option<String>,
            )+
        }

        impl $name {
            /// Every label that is set.
            fn texts(&self) -> impl Iterator<Item = &str> {
                [self.name.as_str(), self.singular_name.as_str()]
                    .into_iter()
                    .chain([$(self.$field.as_deref()),+].into_iter().flatten())
            }
        }
    };
}

label_set! {
    /// A post type's labels, as `get_post_type_labels` names them. An unset label takes
    /// WordPress's default for the type.
    PostTypeLabels {
        /// "Add New Post".
        add_new,
        /// "Add New Post", the screen's title.
        add_new_item,
        /// "Edit Post".
        edit_item,
        /// "New Post".
        new_item,
        /// "View Post".
        view_item,
        /// "View Posts".
        view_items,
        /// "Search Posts".
        search_items,
        /// "No posts found."
        not_found,
        /// "No posts found in Trash."
        not_found_in_trash,
        /// "Parent Page:", for hierarchical types.
        parent_item_colon,
        /// "All Posts", the submenu.
        all_items,
        /// "Post Archives", for navigation menus.
        archives,
        /// "Post Attributes", the attributes box.
        attributes,
        /// "Insert into post", the media button.
        insert_into_item,
        /// "Uploaded to this post", the media filter.
        uploaded_to_this_item,
        /// "Featured image".
        featured_image,
        /// "Set featured image".
        set_featured_image,
        /// "Remove featured image".
        remove_featured_image,
        /// "Use as featured image".
        use_featured_image,
        /// The menu's name; defaults to `name`.
        menu_name,
        /// "Filter posts list".
        filter_items_list,
        /// "Filter by date".
        filter_by_date,
        /// "Posts list navigation".
        items_list_navigation,
        /// "Posts list".
        items_list,
        /// "Post published."
        item_published,
        /// "Post published privately."
        item_published_privately,
        /// "Post reverted to draft."
        item_reverted_to_draft,
        /// "Post trashed."
        item_trashed,
        /// "Post scheduled."
        item_scheduled,
        /// "Post updated."
        item_updated,
        /// "Post Link", the navigation link block.
        item_link,
        /// "A link to a post.", the navigation link block's description.
        item_link_description,
    }
}

/// Checks every label's length.
fn check_labels<'a>(labels: impl Iterator<Item = &'a str>) -> Result<(), DefinitionError> {
    labels
        .into_iter()
        .try_for_each(|label| check_len("label length", label, MAX_TEXT_LEN))
}

/// Whether and where a post type shows in the admin menu, as `show_in_menu`: `true` or
/// `false`, or the slug of the menu it goes under, such as `"tools.php"`.
///
/// Read as PHP writes it: `0`/`1`, `"0"`/`"1"`, `"true"`/`"false"` and `""` (false) are
/// booleans, never a menu named `"1"`; any other string is a menu slug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ShowInMenu {
    /// A top-level menu item, or none.
    Show(bool),
    /// A submenu of the menu with this slug.
    Under(String),
}

impl<'de> Deserialize<'de> for ShowInMenu {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if let Some(show) = php_bool(&value) {
            return Ok(Self::Show(show));
        }
        match value {
            Value::String(slug) => Ok(Self::Under(slug)),
            other => Err(de::Error::custom(format!(
                "`show_in_menu` must be a boolean or a menu slug, not `{other}`"
            ))),
        }
    }
}

/// A post type's archive, as `has_archive`: `true` or `false`, or the archive's slug.
///
/// Read as PHP writes it: `0`/`1`, `"0"`/`"1"`, `"true"`/`"false"`, `""` and `null` (false)
/// are booleans, never an archive at `"1"`; any other string is the slug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum HasArchive {
    /// An archive at the type's rewrite slug, or none.
    Enabled(bool),
    /// An archive at this slug.
    Slug(String),
}

impl Default for HasArchive {
    fn default() -> Self {
        Self::Enabled(false)
    }
}

impl<'de> Deserialize<'de> for HasArchive {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if let Some(enabled) = php_bool(&value) {
            return Ok(Self::Enabled(enabled));
        }
        match value {
            Value::Null => Ok(Self::default()),
            Value::String(slug) => Ok(Self::Slug(slug)),
            other => Err(de::Error::custom(format!(
                "`has_archive` must be a boolean or a slug, not `{other}`"
            ))),
        }
    }
}

/// The permalink options of `rewrite`, when it is an array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteRules {
    /// The permalink base; defaults to the slug.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_string"
    )]
    pub slug: Option<String>,
    /// Whether the permalink starts with the front base; defaults to true.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub with_front: Option<bool>,
    /// Whether feeds have permalinks; defaults to `has_archive` (post types).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub feeds: Option<bool>,
    /// Whether pagination has permalinks; defaults to true (post types).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub pages: Option<bool>,
    /// Whether term permalinks nest their parents; defaults to false (taxonomies).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub hierarchical: Option<bool>,
}

/// A post type's or taxonomy's permalinks, as `rewrite`: `true` or `false`, or the options.
///
/// Read as PHP writes it: `0`/`1`, `"0"`/`"1"`, `"true"`/`"false"` and `""` (false) are
/// booleans; `null` is the default (permalinks on); an object (or PHP's empty array, `[]`) is
/// the options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Rewrite {
    /// Permalinks with the defaults, or none.
    Enabled(bool),
    /// Permalinks with these options.
    Rules(RewriteRules),
}

impl Default for Rewrite {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

impl<'de> Deserialize<'de> for Rewrite {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if let Some(enabled) = php_bool(&value) {
            return Ok(Self::Enabled(enabled));
        }
        match value {
            Value::Null => Ok(Self::default()),
            Value::Array(items) if items.is_empty() => Ok(Self::Rules(RewriteRules::default())),
            Value::Object(_) => serde_json::from_value(value)
                .map(Self::Rules)
                .map_err(de::Error::custom),
            other => Err(de::Error::custom(format!(
                "`rewrite` must be a boolean or an object, not `{other}`"
            ))),
        }
    }
}

impl Rewrite {
    /// The permalink base, or `None` without permalinks: the rules' slug, or `default` (the
    /// post type's or taxonomy's slug).
    pub fn slug_or<'a>(&'a self, default: &'a str) -> Option<&'a str> {
        match self {
            Self::Enabled(false) => None,
            Self::Enabled(true) => Some(default),
            Self::Rules(rules) => Some(rules.slug.as_deref().unwrap_or(default)),
        }
    }

    fn validate(&self) -> Result<(), DefinitionError> {
        match self {
            Self::Rules(RewriteRules {
                slug: Some(slug), ..
            }) => check_len("rewrite slug length", slug, MAX_TEXT_LEN),
            _ => Ok(()),
        }
    }
}

/// The base of a post type's capability names, as `capability_type`: `"book"` gives
/// `edit_books`. In JSON a string (whose plural adds an `s`) or `[singular, plural]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityType {
    /// The singular, such as `book`.
    pub singular: String,
    /// The plural, such as `books`.
    pub plural: String,
}

impl CapabilityType {
    /// The capability type `singular`, whose plural adds an `s`, as WordPress makes it.
    pub fn new(singular: &str) -> Self {
        Self {
            singular: singular.to_owned(),
            plural: format!("{singular}s"),
        }
    }
}

impl Default for CapabilityType {
    /// `post`, as WordPress's default.
    fn default() -> Self {
        Self::new("post")
    }
}

impl Serialize for CapabilityType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.plural == format!("{}s", self.singular) {
            serializer.serialize_str(&self.singular)
        } else {
            [&self.singular, &self.plural].serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for CapabilityType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Two([String; 2]),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::One(singular) => Self::new(&singular),
            Raw::Two([singular, plural]) => Self { singular, plural },
        })
    }
}

/// Declares a set of optional capability-name overrides, as `register_post_type` and
/// `register_taxonomy` take `capabilities`.
macro_rules! capability_set {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[doc = $doc:literal])+ $field:ident, )+ }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
        pub struct $name {
            $(
                $(#[doc = $doc])+
                #[serde(
                    default,
                    skip_serializing_if = "Option::is_none",
                    deserialize_with = "lenient::opt_string"
                )]
                pub $field: Option<String>,
            )+
        }

        impl $name {
            fn is_empty(&self) -> bool {
                *self == Self::default()
            }

            fn validate(&self) -> Result<(), DefinitionError> {
                $(
                    if let Some(cap) = &self.$field {
                        check_len("capability name length", cap, MAX_KEY_LEN)?;
                    }
                )+
                Ok(())
            }
        }
    };
}

capability_set! {
    /// A post type's capability names, as `register_post_type`'s `capabilities`: each one set
    /// replaces the name `capability_type` gives. Meta capabilities (`edit_post`, ...) and
    /// `create_posts` are not modelled; [`map_meta_cap`](crate::users::map_meta_cap) maps them
    /// from these.
    PostTypeCapabilities {
        /// `read`.
        read,
        /// `edit_posts`.
        edit_posts,
        /// `edit_others_posts`.
        edit_others_posts,
        /// `edit_published_posts`.
        edit_published_posts,
        /// `edit_private_posts`.
        edit_private_posts,
        /// `delete_posts`.
        delete_posts,
        /// `delete_others_posts`.
        delete_others_posts,
        /// `delete_published_posts`.
        delete_published_posts,
        /// `delete_private_posts`.
        delete_private_posts,
        /// `publish_posts`.
        publish_posts,
        /// `read_private_posts`.
        read_private_posts,
    }
}

/// `title` and `editor`: the supports WordPress gives a type registered without any.
fn default_supports() -> Vec<Support> {
    vec![Support::Title, Support::Editor]
}

/// `supports` as `register_post_type` reads it: a list of supports; `false` alone is none; an
/// empty list, `null`, `""`, `"0"`, `0` or `true` gets the defaults (`title`, `editor`). A PHP
/// array exported as an object is read too: a string value is a support, and a key whose value
/// is an array (`{"editor": {"notes": true}}`) is one.
fn deserialize_supports<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Support>, D::Error> {
    let parse = |s: &str| s.parse::<Support>().map_err(de::Error::custom);
    match Value::deserialize(d)? {
        Value::Bool(false) => Ok(Vec::new()),
        Value::Array(items) if items.is_empty() => Ok(default_supports()),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => parse(s),
                other => Err(de::Error::custom(format!(
                    "a post type support must be a string, not `{other}`"
                ))),
            })
            .collect(),
        Value::Object(map) if map.is_empty() => Ok(default_supports()),
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| match value {
                Value::String(s) => parse(s),
                _ => parse(key),
            })
            .collect(),
        Value::Null | Value::Bool(true) | Value::Number(_) | Value::String(_) => {
            Ok(default_supports())
        }
    }
}

/// Writes no supports as `false`, since WordPress reads an empty list as the defaults.
fn serialize_supports<S: Serializer>(supports: &[Support], s: S) -> Result<S::Ok, S::Error> {
    if supports.is_empty() {
        s.serialize_bool(false)
    } else {
        supports.serialize(s)
    }
}

/// The JSON keys [`PostTypeDef`] models, so they may not be in its `settings`.
const POST_TYPE_KEYS: &[&str] = &[
    "slug",
    "post_type",
    "labels",
    "description",
    "public",
    "hierarchical",
    "exclude_from_search",
    "publicly_queryable",
    "show_ui",
    "show_in_menu",
    "show_in_nav_menus",
    "menu_icon",
    "menu_position",
    "show_in_rest",
    "rest_base",
    "has_archive",
    "rewrite",
    "supports",
    "taxonomies",
    "field_groups",
    "capability_type",
    "capabilities",
    "origin",
];

/// A custom post type, with `register_post_type`'s options and WordPress's defaults. An option
/// left `None` takes WordPress's default, which the `effective_*` methods give.
///
/// Modelled: `labels`, `description`, `public`, `hierarchical`, `exclude_from_search`,
/// `publicly_queryable`, `show_ui`, `show_in_menu`, `show_in_nav_menus`, `menu_icon`,
/// `menu_position`, `show_in_rest`, `rest_base`, `has_archive`, `rewrite`, `supports`,
/// `taxonomies`, `capability_type` and `capabilities`, plus Mission Control's `field_groups`.
/// Every other option (`show_in_admin_bar`, `map_meta_cap`, `query_var`, `can_export`,
/// `delete_with_user`, `template`, `template_lock`, `rest_namespace`, ...) is kept in
/// [`settings`](Self::settings) as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostTypeDef {
    /// The type's name, stored in `posts.post_type`: at most 20 characters of lowercase
    /// letters, digits, `_` and `-`. ACF exports it as `post_type`.
    #[serde(alias = "post_type")]
    pub slug: String,
    /// The labels.
    #[serde(default)]
    pub labels: PostTypeLabels,
    /// A short description.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "lenient::string"
    )]
    pub description: String,
    /// Whether the type is public, in the admin and on the site. WordPress's default: false.
    #[serde(default, deserialize_with = "lenient::bool")]
    pub public: bool,
    /// Whether posts have parents, as pages do.
    #[serde(default, deserialize_with = "lenient::bool")]
    pub hierarchical: bool,
    /// Whether search leaves the type out; defaults to the opposite of `public`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub exclude_from_search: Option<bool>,
    /// Whether the front end serves the type's queries; defaults to `public`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub publicly_queryable: Option<bool>,
    /// Whether the admin has screens for the type; defaults to `public`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub show_ui: Option<bool>,
    /// Whether and where the type shows in the admin menu; defaults to `show_ui`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_in_menu: Option<ShowInMenu>,
    /// Whether the type can be added to navigation menus; defaults to `public`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub show_in_nav_menus: Option<bool>,
    /// The menu's icon: a Dashicons class, an image URL or a `data:image/svg+xml` URI.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_string"
    )]
    pub menu_icon: Option<String>,
    /// The menu's position; unset puts it after Comments.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_i32"
    )]
    pub menu_position: Option<i32>,
    /// Whether the REST API serves the type. WordPress's default: false.
    #[serde(default, deserialize_with = "lenient::bool")]
    pub show_in_rest: bool,
    /// The type's route under `mc/v2`; defaults to the slug.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_string"
    )]
    pub rest_base: Option<String>,
    /// The type's archive.
    #[serde(default)]
    pub has_archive: HasArchive,
    /// The type's permalinks.
    #[serde(default)]
    pub rewrite: Rewrite,
    /// The features the type supports. Defaults to `title` and `editor`; `false` is none.
    #[serde(
        default = "default_supports",
        deserialize_with = "deserialize_supports",
        serialize_with = "serialize_supports"
    )]
    pub supports: Vec<Support>,
    /// The taxonomies the type uses, by slug.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub taxonomies: Vec<String>,
    /// The field groups attached to the type, by key (`group_…`).
    #[serde(default, deserialize_with = "lenient::strings")]
    pub field_groups: Vec<String>,
    /// The base of the type's capability names; defaults to `post`.
    #[serde(default)]
    pub capability_type: CapabilityType,
    /// Capability names that replace those `capability_type` gives.
    #[serde(default, skip_serializing_if = "PostTypeCapabilities::is_empty")]
    pub capabilities: PostTypeCapabilities,
    /// Who made the definition. Never read from or written to the JSON: `from_json` sets it.
    #[serde(
        skip_serializing,
        default = "loaded_origin",
        deserialize_with = "ignore_origin"
    )]
    pub origin: Origin,
    /// `register_post_type`'s other options, kept as written.
    #[serde(flatten)]
    pub settings: Map<String, Value>,
}

impl PostTypeDef {
    /// A post type named `slug`, with `register_post_type`'s defaults, its labels' `name`
    /// and `singular_name`, and the admin as origin.
    pub fn new(slug: &str, name: &str, singular_name: &str) -> Self {
        Self {
            slug: slug.to_owned(),
            labels: PostTypeLabels {
                name: name.to_owned(),
                singular_name: singular_name.to_owned(),
                ..PostTypeLabels::default()
            },
            description: String::new(),
            public: false,
            hierarchical: false,
            exclude_from_search: None,
            publicly_queryable: None,
            show_ui: None,
            show_in_menu: None,
            show_in_nav_menus: None,
            menu_icon: None,
            menu_position: None,
            show_in_rest: false,
            rest_base: None,
            has_archive: HasArchive::default(),
            rewrite: Rewrite::default(),
            supports: default_supports(),
            taxonomies: Vec::new(),
            field_groups: Vec::new(),
            capability_type: CapabilityType::default(),
            capabilities: PostTypeCapabilities::default(),
            origin: Origin::Admin,
            settings: Map::new(),
        }
    }

    /// Parses `json` (at most [`MAX_JSON_BYTES`]), gives it `origin`, and
    /// [validates](Self::validate) it. Any `origin` in the JSON is ignored.
    pub fn from_json(json: &str, origin: Origin) -> Result<Self, DefinitionError> {
        let mut def: Self = parse(json)?;
        def.origin = origin;
        def.validate()?;
        Ok(def)
    }

    /// Checks the rules WordPress applies, and the size limits: the slug is 1 to 20
    /// characters of lowercase letters, digits, `_` and `-`, and not a name WordPress keeps
    /// ([`RESERVED_POST_TYPES`]); the taxonomies are taxonomy slugs; the field groups are
    /// group keys; the capability type is not empty; `settings` does not repeat a modelled
    /// option.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        check_slug("post type", &self.slug, POST_TYPE_SLUG_MAX)?;
        if RESERVED_POST_TYPES.contains(&self.slug.as_str()) {
            return Err(DefinitionError::ReservedSlug {
                what: "post type",
                slug: self.slug.clone(),
            });
        }
        check_labels(self.labels.texts())?;
        check_len("description length", &self.description, MAX_TEXT_LEN)?;
        if let Some(icon) = &self.menu_icon {
            check_len("menu icon length", icon, MAX_ICON_LEN)?;
        }
        if let Some(base) = &self.rest_base {
            check_len("REST base length", base, MAX_TEXT_LEN)?;
        }
        if let HasArchive::Slug(slug) = &self.has_archive {
            check_len("archive slug length", slug, MAX_TEXT_LEN)?;
        }
        if let Some(ShowInMenu::Under(menu)) = &self.show_in_menu {
            check_len("parent menu length", menu, MAX_TEXT_LEN)?;
        }
        self.rewrite.validate()?;
        check_count(
            "number of taxonomies",
            self.taxonomies.len(),
            MAX_LIST_ITEMS,
        )?;
        for taxonomy in &self.taxonomies {
            check_slug("taxonomy", taxonomy, TAXONOMY_SLUG_MAX)?;
        }
        check_count(
            "number of field groups",
            self.field_groups.len(),
            MAX_LIST_ITEMS,
        )?;
        for group in &self.field_groups {
            check_key(group, GROUP_PREFIX)?;
        }
        let CapabilityType { singular, plural } = &self.capability_type;
        if singular.trim().is_empty() || plural.trim().is_empty() {
            return Err(DefinitionError::InvalidCapabilityType);
        }
        check_len("capability type length", singular, MAX_KEY_LEN)?;
        check_len("capability type length", plural, MAX_KEY_LEN)?;
        self.capabilities.validate()?;
        check_settings(&self.slug, &self.settings, POST_TYPE_KEYS)?;
        self.origin.validate()
    }

    /// Whether the definition is read-only in the admin. See [`Origin::is_locked`].
    pub fn is_locked(&self) -> bool {
        self.origin.is_locked()
    }

    /// See [`Origin::ensure_editable`].
    pub fn ensure_editable(&self) -> Result<(), DefinitionError> {
        self.origin.ensure_editable()
    }

    /// Whether the front end serves the type: `publicly_queryable`, or `public`.
    pub fn effective_publicly_queryable(&self) -> bool {
        self.publicly_queryable.unwrap_or(self.public)
    }

    /// Whether the admin has screens for the type: `show_ui`, or `public`.
    pub fn effective_show_ui(&self) -> bool {
        self.show_ui.unwrap_or(self.public)
    }

    /// Where the type shows in the admin menu: `show_in_menu`, or `show_ui`.
    pub fn effective_show_in_menu(&self) -> ShowInMenu {
        self.show_in_menu
            .clone()
            .unwrap_or(ShowInMenu::Show(self.effective_show_ui()))
    }

    /// The type's route under `mc/v2`: `rest_base`, or the slug.
    pub fn effective_rest_base(&self) -> &str {
        self.rest_base.as_deref().unwrap_or(&self.slug)
    }

    /// The type's permalink base, or `None` without permalinks.
    pub fn effective_rewrite_slug(&self) -> Option<&str> {
        self.rewrite.slug_or(&self.slug)
    }

    /// Whether the type supports `support`.
    pub fn supports(&self, support: Support) -> bool {
        self.supports.contains(&support)
    }

    /// The supports as the REST controller reads them. `trackbacks` has no REST field.
    pub fn rest_supports(&self) -> Supports {
        Supports {
            title: self.supports(Support::Title),
            editor: self.supports(Support::Editor),
            author: self.supports(Support::Author),
            excerpt: self.supports(Support::Excerpt),
            thumbnail: self.supports(Support::Thumbnail),
            comments: self.supports(Support::Comments),
            revisions: self.supports(Support::Revisions),
            page_attributes: self.supports(Support::PageAttributes),
            post_formats: self.supports(Support::PostFormats),
            custom_fields: self.supports(Support::CustomFields),
        }
    }

    /// The type as the REST controller serves it. Its taxonomies are those of `known` (the
    /// taxonomies the REST API serves) named in [`taxonomies`](Self::taxonomies), in the
    /// definition's order; a name not in `known` is left out.
    pub fn to_post_type(&self, known: &[RestTaxonomy]) -> PostType {
        PostType {
            name: self.slug.clone(),
            rest_base: self.effective_rest_base().to_owned(),
            hierarchical: self.hierarchical,
            show_in_rest: self.show_in_rest,
            supports: self.rest_supports(),
            taxonomies: self
                .taxonomies
                .iter()
                .filter_map(|name| known.iter().find(|t| t.taxonomy.name == *name).cloned())
                .collect(),
            caps: self.caps(),
        }
    }

    /// The type's capability names, as `get_post_type_capabilities` derives them with
    /// `map_meta_cap` on: the meta capabilities from the `capability_type`'s singular
    /// (`edit_book`), the primitive ones from its plural, with each name set in `capabilities`
    /// in its place.
    pub fn caps(&self) -> PostTypeCaps {
        let CapabilityType { singular, plural } = &self.capability_type;
        let set = &self.capabilities;
        let cap = |given: &Option<String>, derived: String| given.clone().unwrap_or(derived);
        PostTypeCaps {
            edit_post: format!("edit_{singular}"),
            read_post: format!("read_{singular}"),
            delete_post: format!("delete_{singular}"),
            read: cap(&set.read, "read".to_owned()),
            edit_posts: cap(&set.edit_posts, format!("edit_{plural}")),
            edit_others_posts: cap(&set.edit_others_posts, format!("edit_others_{plural}")),
            edit_published_posts: cap(
                &set.edit_published_posts,
                format!("edit_published_{plural}"),
            ),
            edit_private_posts: cap(&set.edit_private_posts, format!("edit_private_{plural}")),
            delete_posts: cap(&set.delete_posts, format!("delete_{plural}")),
            delete_others_posts: cap(&set.delete_others_posts, format!("delete_others_{plural}")),
            delete_published_posts: cap(
                &set.delete_published_posts,
                format!("delete_published_{plural}"),
            ),
            delete_private_posts: cap(
                &set.delete_private_posts,
                format!("delete_private_{plural}"),
            ),
            publish_posts: cap(&set.publish_posts, format!("publish_{plural}")),
            read_private_posts: cap(&set.read_private_posts, format!("read_private_{plural}")),
        }
    }
}

/// Checks a post type or taxonomy slug as `sanitize_key` and the length limit leave it.
fn check_slug(what: &'static str, slug: &str, max: usize) -> Result<(), DefinitionError> {
    let invalid = |reason: String| DefinitionError::InvalidSlug {
        what,
        slug: slug.chars().take(MAX_KEY_LEN).collect(),
        reason,
    };
    if slug.is_empty() {
        return Err(invalid("it is empty".to_owned()));
    }
    if slug.len() > max {
        return Err(invalid(format!("it is longer than {max} characters")));
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(invalid(
            "use only lowercase letters, digits, `_` and `-`".to_owned(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Taxonomies

label_set! {
    /// A taxonomy's labels, as `get_taxonomy_labels` names them. An unset label takes
    /// WordPress's default for the taxonomy.
    TaxonomyLabels {
        /// "Search Tags".
        search_items,
        /// "Popular Tags", for flat taxonomies.
        popular_items,
        /// "All Tags".
        all_items,
        /// "Parent Category", for hierarchical taxonomies.
        parent_item,
        /// "Parent Category:".
        parent_item_colon,
        /// The description of the name field on the term form.
        name_field_description,
        /// The description of the slug field.
        slug_field_description,
        /// The description of the parent field.
        parent_field_description,
        /// The description of the description field.
        desc_field_description,
        /// "Edit Tag".
        edit_item,
        /// "View Tag".
        view_item,
        /// "Update Tag".
        update_item,
        /// "Add New Tag".
        add_new_item,
        /// "New Tag Name".
        new_item_name,
        /// "Tag templates", the block template's name.
        template_name,
        /// "Separate tags with commas", for flat taxonomies.
        separate_items_with_commas,
        /// "Add or remove tags", for flat taxonomies.
        add_or_remove_items,
        /// "Choose from the most used tags", for flat taxonomies.
        choose_from_most_used,
        /// "No tags found."
        not_found,
        /// "No tags", in the posts list.
        no_terms,
        /// "Filter by category", in the posts list.
        filter_by_item,
        /// "Tags list navigation".
        items_list_navigation,
        /// "Tags list".
        items_list,
        /// "Most Used", the tab.
        most_used,
        /// "← Go to Tags", after a term is updated.
        back_to_items,
        /// "Tag Link", the navigation link block.
        item_link,
        /// "A link to a tag.", the navigation link block's description.
        item_link_description,
        /// The menu's name; defaults to `name`.
        menu_name,
    }
}

capability_set! {
    /// A taxonomy's capability names, as `register_taxonomy`'s `capabilities`. An unset name
    /// takes Mission Control's default for the taxonomy ([`TaxonomyCaps::for_taxonomy`]).
    TaxonomyCapabilities {
        /// `manage_terms`.
        manage_terms,
        /// `edit_terms`.
        edit_terms,
        /// `delete_terms`.
        delete_terms,
        /// `assign_terms`.
        assign_terms,
    }
}

fn true_() -> bool {
    true
}

/// The JSON keys [`TaxonomyDef`] models, so they may not be in its `settings`.
const TAXONOMY_KEYS: &[&str] = &[
    "slug",
    "taxonomy",
    "object_type",
    "labels",
    "description",
    "public",
    "publicly_queryable",
    "hierarchical",
    "show_ui",
    "show_in_menu",
    "show_in_nav_menus",
    "show_tagcloud",
    "show_in_quick_edit",
    "show_admin_column",
    "show_in_rest",
    "rest_base",
    "rewrite",
    "capabilities",
    "origin",
];

/// A custom taxonomy, with `register_taxonomy`'s options and WordPress's defaults.
///
/// Modelled: `object_type`, `labels`, `description`, `public`, `publicly_queryable`,
/// `hierarchical`, `show_ui`, `show_in_menu`, `show_in_nav_menus`, `show_tagcloud`,
/// `show_in_quick_edit`, `show_admin_column`, `show_in_rest`, `rest_base`, `rewrite` and
/// `capabilities`. Every other option (`query_var`, `sort`, `default_term`, `rest_namespace`,
/// `meta_box_cb`, ...) is kept in [`settings`](Self::settings) as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyDef {
    /// The taxonomy's name, stored in `term_taxonomy.taxonomy`: at most 32 characters of
    /// lowercase letters, digits, `_` and `-`. ACF exports it as `taxonomy`.
    #[serde(alias = "taxonomy")]
    pub slug: String,
    /// The post types the taxonomy is for, by slug.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub object_type: Vec<String>,
    /// The labels.
    #[serde(default)]
    pub labels: TaxonomyLabels,
    /// A short description.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "lenient::string"
    )]
    pub description: String,
    /// Whether the taxonomy is public. WordPress's default: true.
    #[serde(default = "true_", deserialize_with = "lenient::bool_true")]
    pub public: bool,
    /// Whether the front end serves the taxonomy's archives; defaults to `public`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub publicly_queryable: Option<bool>,
    /// Whether terms have parents, as categories do.
    #[serde(default, deserialize_with = "lenient::bool")]
    pub hierarchical: bool,
    /// Whether the admin has screens for the taxonomy; defaults to `public`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub show_ui: Option<bool>,
    /// Whether the taxonomy shows in the admin menu; defaults to `show_ui`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub show_in_menu: Option<bool>,
    /// Whether terms can be added to navigation menus; defaults to `public`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub show_in_nav_menus: Option<bool>,
    /// Whether the tag cloud widget lists the taxonomy; defaults to `show_ui`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub show_tagcloud: Option<bool>,
    /// Whether quick edit shows the taxonomy; defaults to `show_ui`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool"
    )]
    pub show_in_quick_edit: Option<bool>,
    /// Whether the posts list has a column for the taxonomy.
    #[serde(default, deserialize_with = "lenient::bool")]
    pub show_admin_column: bool,
    /// Whether the REST API serves the taxonomy. WordPress's default: false.
    #[serde(default, deserialize_with = "lenient::bool")]
    pub show_in_rest: bool,
    /// The taxonomy's route under `mc/v2`; defaults to the slug.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_string"
    )]
    pub rest_base: Option<String>,
    /// The taxonomy's permalinks.
    #[serde(default)]
    pub rewrite: Rewrite,
    /// The capability names; unset ones take the defaults.
    #[serde(default, skip_serializing_if = "TaxonomyCapabilities::is_empty")]
    pub capabilities: TaxonomyCapabilities,
    /// Who made the definition. Never read from or written to the JSON: `from_json` sets it.
    #[serde(
        skip_serializing,
        default = "loaded_origin",
        deserialize_with = "ignore_origin"
    )]
    pub origin: Origin,
    /// `register_taxonomy`'s other options, kept as written.
    #[serde(flatten)]
    pub settings: Map<String, Value>,
}

impl TaxonomyDef {
    /// A taxonomy named `slug` for `object_type`, with `register_taxonomy`'s defaults, its
    /// labels' `name` and `singular_name`, and the admin as origin.
    pub fn new(slug: &str, object_type: &[&str], name: &str, singular_name: &str) -> Self {
        Self {
            slug: slug.to_owned(),
            object_type: object_type.iter().map(|&t| t.to_owned()).collect(),
            labels: TaxonomyLabels {
                name: name.to_owned(),
                singular_name: singular_name.to_owned(),
                ..TaxonomyLabels::default()
            },
            description: String::new(),
            public: true,
            publicly_queryable: None,
            hierarchical: false,
            show_ui: None,
            show_in_menu: None,
            show_in_nav_menus: None,
            show_tagcloud: None,
            show_in_quick_edit: None,
            show_admin_column: false,
            show_in_rest: false,
            rest_base: None,
            rewrite: Rewrite::default(),
            capabilities: TaxonomyCapabilities::default(),
            origin: Origin::Admin,
            settings: Map::new(),
        }
    }

    /// Parses `json` (at most [`MAX_JSON_BYTES`]), gives it `origin`, and
    /// [validates](Self::validate) it. Any `origin` in the JSON is ignored.
    pub fn from_json(json: &str, origin: Origin) -> Result<Self, DefinitionError> {
        let mut def: Self = parse(json)?;
        def.origin = origin;
        def.validate()?;
        Ok(def)
    }

    /// Checks the rules WordPress applies, and the size limits: the slug is 1 to 32
    /// characters of lowercase letters, digits, `_` and `-`, and not one of WordPress's own
    /// taxonomies ([`RESERVED_TAXONOMIES`]); the object types are post type slugs; `settings`
    /// does not repeat a modelled option.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        check_slug("taxonomy", &self.slug, TAXONOMY_SLUG_MAX)?;
        if RESERVED_TAXONOMIES.contains(&self.slug.as_str()) {
            return Err(DefinitionError::ReservedSlug {
                what: "taxonomy",
                slug: self.slug.clone(),
            });
        }
        check_labels(self.labels.texts())?;
        check_len("description length", &self.description, MAX_TEXT_LEN)?;
        if let Some(base) = &self.rest_base {
            check_len("REST base length", base, MAX_TEXT_LEN)?;
        }
        self.rewrite.validate()?;
        check_count(
            "number of object types",
            self.object_type.len(),
            MAX_LIST_ITEMS,
        )?;
        for post_type in &self.object_type {
            check_slug("post type", post_type, POST_TYPE_SLUG_MAX)?;
        }
        self.capabilities.validate()?;
        check_settings(&self.slug, &self.settings, TAXONOMY_KEYS)?;
        self.origin.validate()
    }

    /// Whether the definition is read-only in the admin. See [`Origin::is_locked`].
    pub fn is_locked(&self) -> bool {
        self.origin.is_locked()
    }

    /// See [`Origin::ensure_editable`].
    pub fn ensure_editable(&self) -> Result<(), DefinitionError> {
        self.origin.ensure_editable()
    }

    /// Whether the admin has screens for the taxonomy: `show_ui`, or `public`.
    pub fn effective_show_ui(&self) -> bool {
        self.show_ui.unwrap_or(self.public)
    }

    /// The taxonomy's route under `mc/v2`: `rest_base`, or the slug.
    pub fn effective_rest_base(&self) -> &str {
        self.rest_base.as_deref().unwrap_or(&self.slug)
    }

    /// The taxonomy's permalink base, or `None` without permalinks.
    pub fn effective_rewrite_slug(&self) -> Option<&str> {
        self.rewrite.slug_or(&self.slug)
    }

    /// The taxonomy as the term functions and the REST controller read it: its name, whether it
    /// is hierarchical, and its post types.
    pub fn to_taxonomy(&self) -> Taxonomy {
        Taxonomy::new(
            self.slug.clone(),
            self.hierarchical,
            self.object_type.clone(),
        )
    }

    /// The taxonomy as the REST controller serves it: [`to_taxonomy`](Self::to_taxonomy), its
    /// `rest_base`, `show_in_rest` and [`caps`](Self::caps).
    pub fn to_taxonomy_spec(&self) -> TaxonomySpec {
        TaxonomySpec {
            taxonomy: self.to_taxonomy(),
            rest_base: self.effective_rest_base().to_owned(),
            show_in_rest: self.show_in_rest,
            caps: self.caps(),
        }
    }

    /// The taxonomy as a post type shows it in the REST API: [`to_taxonomy`](Self::to_taxonomy)
    /// and its `rest_base`.
    pub fn to_rest_taxonomy(&self) -> RestTaxonomy {
        RestTaxonomy {
            taxonomy: self.to_taxonomy(),
            rest_base: Cow::Owned(self.effective_rest_base().to_owned()),
        }
    }

    /// The taxonomy's capability names: its `capabilities`, and the defaults
    /// ([`TaxonomyCaps::for_taxonomy`]) for those unset.
    pub fn caps(&self) -> TaxonomyCaps {
        let defaults = TaxonomyCaps::for_taxonomy(&self.slug);
        let caps = &self.capabilities;
        TaxonomyCaps {
            manage_terms: caps.manage_terms.clone().unwrap_or(defaults.manage_terms),
            edit_terms: caps.edit_terms.clone().unwrap_or(defaults.edit_terms),
            delete_terms: caps.delete_terms.clone().unwrap_or(defaults.delete_terms),
            assign_terms: caps.assign_terms.clone().unwrap_or(defaults.assign_terms),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Field groups

const FIELD_PREFIX: &str = "field_";
const GROUP_PREFIX: &str = "group_";
const LAYOUT_PREFIX: &str = "layout_";

/// The keys of a field's JSON that have their own place in [`FieldDef`], so they may not be
/// in its `settings`. `step` is read into a number field's kind.
const FIELD_KEYS: &[&str] = &[
    "key",
    "label",
    "name",
    "type",
    "required",
    "instructions",
    "default_value",
    "choices",
    "step",
    "sub_fields",
    "layouts",
    "clone",
];

/// The keys of a layout's JSON that have their own place in [`LayoutDef`].
const LAYOUT_KEYS: &[&str] = &["key", "name", "label", "sub_fields"];

/// The keys of a group's JSON that have their own place in [`FieldGroupDef`].
const GROUP_KEYS: &[&str] = &[
    "key",
    "title",
    "fields",
    "location",
    "menu_order",
    "position",
    "hide_on_screen",
    "active",
    "description",
    "show_in_rest",
    "origin",
];

/// The ACF field types a definition takes: design 5.3's phase 1, then phase 2. ACF's
/// `checkbox` (several choices) is not one: Ignite's [`FieldKindDef::Checkbox`] is a yes/no
/// value, ACF's `true_false`.
pub const FIELD_TYPES: &[&str] = &[
    "text",
    "textarea",
    "number",
    "email",
    "url",
    "password",
    "select",
    "radio",
    "true_false",
    "date_picker",
    "date_time_picker",
    "color_picker",
    "image",
    "file",
    "link",
    "wysiwyg",
    "post_object",
    "relationship",
    "taxonomy",
    "user",
    "repeater",
    "group",
    "flexible_content",
    "gallery",
    "clone",
];

/// Checks an ACF key: `prefix` and at least one letter, digit, `_` or `-`, at most
/// [`MAX_KEY_LEN`] bytes in all.
fn check_key(key: &str, prefix: &'static str) -> Result<(), DefinitionError> {
    check_len("key length", key, MAX_KEY_LEN)?;
    let ok = key.strip_prefix(prefix).is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if ok {
        Ok(())
    } else {
        Err(DefinitionError::InvalidKey {
            key: key.to_owned(),
            prefix,
        })
    }
}

/// Whether `name` is a field or layout name: 1 to [`MAX_KEY_LEN`] letters, digits, `_` and
/// `-`.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_KEY_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One rule of a field group's location, as ACF writes it:
/// `{"param": "post_type", "operator": "==", "value": "book"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocationRule {
    /// What the rule tests.
    pub param: LocationParam,
    /// How it compares.
    pub operator: LocationOperator,
    /// The value it compares with, such as a post type's slug. A number (a post's id) is read
    /// as its digits.
    #[serde(default, deserialize_with = "lenient::scalar_string")]
    pub value: String,
}

/// One layout of a flexible content field, as ACF writes it: its key (`layout_…`), name,
/// label and sub-fields. Ignite's [`BlockDef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutDef {
    /// The layout's key, `layout_…`.
    pub key: String,
    /// The layout's name, stored as the `type` of each row that uses it: unique among the
    /// field's layouts.
    pub name: String,
    /// The label shown when choosing the layout.
    #[serde(default, deserialize_with = "lenient::string")]
    pub label: String,
    /// The layout's fields, in display order.
    #[serde(default, deserialize_with = "lenient::list")]
    pub sub_fields: Vec<FieldDef>,
    /// ACF's other layout settings (`display`, `min`, `max`), kept as written.
    #[serde(flatten)]
    pub settings: Map<String, Value>,
}

impl LayoutDef {
    /// The layout as Ignite's flexible content takes it.
    pub fn to_ignite(&self) -> BlockDef {
        BlockDef {
            name: self.name.clone(),
            label: self.label.clone(),
            fields: self.sub_fields.iter().map(FieldDef::to_ignite).collect(),
        }
    }
}

/// One field of a field group, as ACF writes it: a key (`field_…`), a name (the meta key its
/// value is stored under), a label, a kind, and the type-specific settings.
///
/// In JSON the kind is ACF's `type`, and the settings that make it are ACF's: `choices` for a
/// select or radio, `step` for a number (a step other than empty or 1 makes it
/// [`FieldKindDef::Decimal`]), `sub_fields` for a repeater or group, `layouts` for flexible
/// content, and `clone` (the keys of the fields or groups it reuses) for a clone. Every other
/// ACF setting (`placeholder`, `return_format`, `wrapper`, `conditional_logic`, ...) is kept in
/// [`settings`](Self::settings) as written.
///
/// A repeater's or group's kind holds Ignite's view of its `sub_fields`, and a flexible content
/// kind its `layouts`; build them with [`with_sub_fields`](Self::with_sub_fields) and
/// [`with_layouts`](Self::with_layouts) so they agree. A clone's kind is empty until the
/// registry resolves the fields it clones.
///
/// Deserializing refuses fields nested deeper than [`MAX_FIELD_DEPTH`], so a hostile
/// definition cannot exhaust the stack; the group's `validate` checks the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDef {
    /// The field's key, `field_…`, unique in the group.
    pub key: String,
    /// The field's name, the meta key of its value: unique among its siblings.
    pub name: String,
    /// The label shown next to the input.
    pub label: String,
    /// The input, and how the value is checked and stored.
    pub kind: FieldKindDef,
    /// Whether a value is required.
    pub required: bool,
    /// Instructions shown to the author.
    pub instructions: String,
    /// The value a new post starts with; `null` for none.
    pub default_value: Value,
    /// The fields of a repeater or group.
    pub sub_fields: Vec<FieldDef>,
    /// The layouts of a flexible content field.
    pub layouts: Vec<LayoutDef>,
    /// The keys of the fields or groups a clone field reuses.
    pub clone: Vec<String>,
    /// ACF's other settings for the type, as written.
    pub settings: Map<String, Value>,
}

impl FieldDef {
    /// A field with the given key, name, label and kind, and no other settings.
    pub fn new(key: &str, name: &str, label: &str, kind: FieldKindDef) -> Self {
        Self {
            key: key.to_owned(),
            name: name.to_owned(),
            label: label.to_owned(),
            kind,
            required: false,
            instructions: String::new(),
            default_value: Value::Null,
            sub_fields: Vec::new(),
            layouts: Vec::new(),
            clone: Vec::new(),
            settings: Map::new(),
        }
    }

    /// Sets the sub-fields of a repeater or group, and its kind to match.
    pub fn with_sub_fields(mut self, sub_fields: Vec<FieldDef>) -> Self {
        let ignite = sub_fields.iter().map(Self::to_ignite).collect();
        match self.kind {
            FieldKindDef::Repeater(_) => self.kind = FieldKindDef::Repeater(ignite),
            FieldKindDef::Group(_) => self.kind = FieldKindDef::Group(ignite),
            _ => {}
        }
        self.sub_fields = sub_fields;
        self
    }

    /// Sets the layouts of a flexible content field, and its kind to match.
    pub fn with_layouts(mut self, layouts: Vec<LayoutDef>) -> Self {
        if let FieldKindDef::FlexibleContent(_) = self.kind {
            self.kind =
                FieldKindDef::FlexibleContent(layouts.iter().map(LayoutDef::to_ignite).collect());
        }
        self.layouts = layouts;
        self
    }

    /// The default value as the field's kind stores it, or `None` without one: see
    /// [`get_field`](super::get_field), which gives it for a post with no value.
    pub fn typed_default(&self) -> Option<Value> {
        typed_default_value(&self.kind, &self.default_value, &self.settings)
    }

    /// The field as Ignite's forms take it.
    pub fn to_ignite(&self) -> IgniteFieldDef {
        IgniteFieldDef {
            name: self.name.clone(),
            label: self.label.clone(),
            kind: self.kind.clone(),
            required: self.required,
        }
    }

    /// The kind the field's own `sub_fields` and `layouts` make, for a repeater, group or
    /// flexible content field; `None` for another kind.
    fn kind_from_children(&self) -> Option<FieldKindDef> {
        let fields = || self.sub_fields.iter().map(Self::to_ignite).collect();
        match &self.kind {
            FieldKindDef::Repeater(_) => Some(FieldKindDef::Repeater(fields())),
            FieldKindDef::Group(_) => Some(FieldKindDef::Group(fields())),
            FieldKindDef::FlexibleContent(_) => Some(FieldKindDef::FlexibleContent(
                self.layouts.iter().map(LayoutDef::to_ignite).collect(),
            )),
            _ => None,
        }
    }

    /// Checks the field and its children, at `depth` (1 for a field of the group).
    fn validate(&self, check: &mut GroupCheck, depth: usize) -> Result<(), DefinitionError> {
        check_key(&self.key, FIELD_PREFIX)?;
        if !check.keys.insert(self.key.clone()) {
            return Err(DefinitionError::DuplicateKey(self.key.clone()));
        }
        check_len("label length", &self.label, MAX_TEXT_LEN)?;
        check_len("instructions length", &self.instructions, MAX_TEXT_LEN)?;
        if acf_type(&self.kind).is_none() {
            return Err(DefinitionError::UnsupportedFieldKind {
                key: self.key.clone(),
                kind: format!("{:?}", self.kind),
            });
        }
        match &self.kind {
            FieldKindDef::Select(choices) | FieldKindDef::Radio(choices) => {
                check_count("number of choices", choices.len(), MAX_CHOICES)?;
                for (value, label) in choices {
                    check_len("choice length", value, MAX_TEXT_LEN)?;
                    check_len("choice length", label, MAX_TEXT_LEN)?;
                }
            }
            FieldKindDef::Decimal(step) => check_len("number step length", step, MAX_KEY_LEN)?,
            _ => {}
        }
        check_settings(&self.key, &self.settings, FIELD_KEYS)?;
        let kind_matches = self
            .kind_from_children()
            .is_none_or(|kind| kind == self.kind);
        let children_allowed = match self.kind {
            FieldKindDef::Repeater(_) | FieldKindDef::Group(_) => self.layouts.is_empty(),
            FieldKindDef::FlexibleContent(_) => self.sub_fields.is_empty(),
            _ => self.sub_fields.is_empty() && self.layouts.is_empty(),
        };
        let clone_allowed = matches!(self.kind, FieldKindDef::Clone(_)) || self.clone.is_empty();
        if !(kind_matches && children_allowed && clone_allowed) {
            return Err(DefinitionError::SubFieldsMismatch(self.key.clone()));
        }
        check_count("number of cloned keys", self.clone.len(), MAX_LIST_ITEMS)?;
        for cloned in &self.clone {
            if check_key(cloned, FIELD_PREFIX).is_err() {
                check_key(cloned, GROUP_PREFIX)?;
            }
        }
        validate_fields(&self.sub_fields, check, depth + 1)?;
        check_count("number of layouts", self.layouts.len(), MAX_LAYOUTS)?;
        let mut names = HashSet::new();
        for layout in &self.layouts {
            check_key(&layout.key, LAYOUT_PREFIX)?;
            if !check.keys.insert(layout.key.clone()) {
                return Err(DefinitionError::DuplicateKey(layout.key.clone()));
            }
            if !is_name(&layout.name) {
                return Err(DefinitionError::InvalidLayoutName(
                    layout.name.chars().take(MAX_KEY_LEN).collect(),
                ));
            }
            if !names.insert(layout.name.as_str()) {
                return Err(DefinitionError::DuplicateLayoutName(layout.name.clone()));
            }
            check_len("label length", &layout.label, MAX_TEXT_LEN)?;
            check_settings(&layout.key, &layout.settings, LAYOUT_KEYS)?;
            validate_fields(&layout.sub_fields, check, depth + 1)?;
        }
        Ok(())
    }
}

/// What a group's validation tracks across its fields: the keys seen, and how many fields.
struct GroupCheck {
    keys: HashSet<String>,
    fields: usize,
}

/// Checks sibling fields at `depth`: each field, and their names unique among them.
fn validate_fields(
    fields: &[FieldDef],
    check: &mut GroupCheck,
    depth: usize,
) -> Result<(), DefinitionError> {
    if fields.is_empty() {
        return Ok(());
    }
    if depth > MAX_FIELD_DEPTH {
        return Err(DefinitionError::TooLarge {
            what: "field nesting depth",
            limit: MAX_FIELD_DEPTH,
        });
    }
    check.fields += fields.len();
    check_count("number of fields", check.fields, MAX_FIELDS)?;
    let mut names = HashSet::new();
    for field in fields {
        if !is_name(&field.name) {
            return Err(DefinitionError::InvalidFieldName(
                field.name.chars().take(MAX_KEY_LEN).collect(),
            ));
        }
        if !names.insert(field.name.as_str()) {
            return Err(DefinitionError::DuplicateFieldName(field.name.clone()));
        }
        field.validate(check, depth)?;
    }
    Ok(())
}

/// ACF's field type for `kind`; `None` for [`FieldKindDef::Lookup`], which ACF has not.
fn acf_type(kind: &FieldKindDef) -> Option<&'static str> {
    Some(match kind {
        FieldKindDef::Text => "text",
        FieldKindDef::Textarea => "textarea",
        FieldKindDef::Password => "password",
        FieldKindDef::Number | FieldKindDef::Decimal(_) => "number",
        FieldKindDef::Checkbox => "true_false",
        FieldKindDef::Select(_) => "select",
        FieldKindDef::Lookup => return None,
        FieldKindDef::Date => "date_picker",
        FieldKindDef::DateTime => "date_time_picker",
        FieldKindDef::Email => "email",
        FieldKindDef::Url => "url",
        FieldKindDef::Radio(_) => "radio",
        FieldKindDef::CheckboxList(_) => "checkbox",
        FieldKindDef::Color => "color_picker",
        FieldKindDef::Image => "image",
        FieldKindDef::File => "file",
        FieldKindDef::Link => "link",
        FieldKindDef::Wysiwyg => "wysiwyg",
        FieldKindDef::PostObject => "post_object",
        FieldKindDef::Relationship => "relationship",
        FieldKindDef::Taxonomy => "taxonomy",
        FieldKindDef::User => "user",
        FieldKindDef::Repeater(_) => "repeater",
        FieldKindDef::Group(_) => "group",
        FieldKindDef::FlexibleContent(_) => "flexible_content",
        FieldKindDef::Gallery => "gallery",
        FieldKindDef::Clone(_) => "clone",
    })
}

/// A field as ACF's JSON has it, before its `type` and settings become a [`FieldKindDef`].
#[derive(Serialize, Deserialize)]
struct RawField {
    key: String,
    #[serde(default, deserialize_with = "lenient::string")]
    label: String,
    #[serde(default, deserialize_with = "lenient::string")]
    name: String,
    #[serde(rename = "type")]
    field_type: String,
    #[serde(default, deserialize_with = "lenient::bool")]
    required: bool,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "lenient::string"
    )]
    instructions: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    default_value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "choices")]
    choices: Option<Vec<(String, String)>>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    step: Value,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient::list"
    )]
    sub_fields: Vec<FieldDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "layouts")]
    layouts: Vec<LayoutDef>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient::strings"
    )]
    clone: Vec<String>,
    #[serde(flatten)]
    settings: Map<String, Value>,
}

impl TryFrom<RawField> for FieldDef {
    type Error = DefinitionError;

    fn try_from(raw: RawField) -> Result<Self, Self::Error> {
        let RawField {
            key,
            label,
            name,
            field_type,
            required,
            instructions,
            default_value,
            choices,
            step,
            sub_fields,
            layouts,
            clone,
            settings,
        } = raw;
        let fields = || sub_fields.iter().map(FieldDef::to_ignite).collect();
        let kind = match field_type.as_str() {
            "text" => FieldKindDef::Text,
            "textarea" => FieldKindDef::Textarea,
            "password" => FieldKindDef::Password,
            "number" => match decimal_step(&step) {
                Some(step) => FieldKindDef::Decimal(step),
                None => FieldKindDef::Number,
            },
            "true_false" => FieldKindDef::Checkbox,
            "select" => FieldKindDef::Select(choices.unwrap_or_default()),
            "radio" => FieldKindDef::Radio(choices.unwrap_or_default()),
            "date_picker" => FieldKindDef::Date,
            "date_time_picker" => FieldKindDef::DateTime,
            "email" => FieldKindDef::Email,
            "url" => FieldKindDef::Url,
            "color_picker" => FieldKindDef::Color,
            "image" => FieldKindDef::Image,
            "file" => FieldKindDef::File,
            "link" => FieldKindDef::Link,
            "wysiwyg" => FieldKindDef::Wysiwyg,
            "post_object" => FieldKindDef::PostObject,
            "relationship" => FieldKindDef::Relationship,
            "taxonomy" => FieldKindDef::Taxonomy,
            "user" => FieldKindDef::User,
            "repeater" => FieldKindDef::Repeater(fields()),
            "group" => FieldKindDef::Group(fields()),
            "flexible_content" => {
                FieldKindDef::FlexibleContent(layouts.iter().map(LayoutDef::to_ignite).collect())
            }
            "gallery" => FieldKindDef::Gallery,
            "clone" => FieldKindDef::Clone(Vec::new()),
            other => {
                return Err(DefinitionError::Unknown {
                    what: "field type",
                    value: other.chars().take(MAX_KEY_LEN).collect(),
                });
            }
        };
        Ok(Self {
            key,
            name,
            label,
            kind,
            required,
            instructions,
            default_value,
            sub_fields,
            layouts,
            clone,
            settings,
        })
    }
}

/// A number field's `step` when it makes the field decimal: a step other than empty and 1
/// (ACF's default, whole numbers), as a string or a number.
fn decimal_step(step: &Value) -> Option<String> {
    let step = match step {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let whole_one = step.parse::<f64>().is_ok_and(|n| n == 1.0);
    (!step.is_empty() && !whole_one).then_some(step)
}

thread_local! {
    /// How deep the field being deserialized on this thread is nested.
    static FIELD_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Lowers [`FIELD_DEPTH`] when a field's deserialization ends, even on an error.
struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        FIELD_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

impl<'de> Deserialize<'de> for FieldDef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let depth = FIELD_DEPTH.with(|depth| {
            depth.set(depth.get() + 1);
            depth.get()
        });
        let _guard = DepthGuard;
        if depth > MAX_FIELD_DEPTH {
            return Err(de::Error::custom(DefinitionError::TooLarge {
                what: "field nesting depth",
                limit: MAX_FIELD_DEPTH,
            }));
        }
        let raw = RawField::deserialize(deserializer)?;
        Self::try_from(raw).map_err(de::Error::custom)
    }
}

impl Serialize for FieldDef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let field_type = acf_type(&self.kind).ok_or_else(|| {
            serde::ser::Error::custom(DefinitionError::UnsupportedFieldKind {
                key: self.key.clone(),
                kind: format!("{:?}", self.kind),
            })
        })?;
        let (choices, step) = match &self.kind {
            FieldKindDef::Select(choices) | FieldKindDef::Radio(choices) => {
                (Some(choices.clone()), Value::Null)
            }
            FieldKindDef::Decimal(step) => (None, Value::String(step.clone())),
            _ => (None, Value::Null),
        };
        RawField {
            key: self.key.clone(),
            label: self.label.clone(),
            name: self.name.clone(),
            field_type: field_type.to_owned(),
            required: self.required,
            instructions: self.instructions.clone(),
            default_value: self.default_value.clone(),
            choices,
            step,
            sub_fields: self.sub_fields.clone(),
            layouts: self.layouts.clone(),
            clone: self.clone.clone(),
            settings: self.settings.clone(),
        }
        .serialize(serializer)
    }
}

/// ACF's `choices`: an object of `value: label` pairs, in order. A list of strings (each its
/// own label), or an empty string or list, are read too.
mod choices {
    use super::*;

    pub(super) fn serialize<S: Serializer>(
        choices: &Option<Vec<(String, String)>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let choices = choices.as_deref().unwrap_or_default();
        let mut map = serializer.serialize_map(Some(choices.len()))?;
        for (value, label) in choices {
            map.serialize_entry(value, label)?;
        }
        map.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<(String, String)>>, D::Error> {
        struct ChoicesVisitor;

        impl<'de> Visitor<'de> for ChoicesVisitor {
            type Value = Vec<(String, String)>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of value: label choices")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut choices = Vec::new();
                while let Some((value, label)) = map.next_entry::<String, Value>()? {
                    let label = match label {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    choices.push((value, label));
                }
                Ok(choices)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut choices = Vec::new();
                while let Some(value) = seq.next_element::<String>()? {
                    choices.push((value.clone(), value));
                }
                Ok(choices)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.is_empty() {
                    Ok(Vec::new())
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Vec::new())
            }
        }

        deserializer.deserialize_any(ChoicesVisitor).map(Some)
    }
}

/// ACF's `layouts`: an object keyed by layout key, as ACF exports it, or a list. In the object
/// form each layout's own `key` must equal the key it is under.
mod layouts {
    use super::*;

    pub(super) fn serialize<S: Serializer>(
        layouts: &[LayoutDef],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(layouts.len()))?;
        for layout in layouts {
            map.serialize_entry(&layout.key, layout)?;
        }
        map.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<LayoutDef>, D::Error> {
        struct LayoutsVisitor;

        impl<'de> Visitor<'de> for LayoutsVisitor {
            type Value = Vec<LayoutDef>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list or an object of layouts")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut layouts = Vec::new();
                while let Some((key, layout)) = map.next_entry::<String, LayoutDef>()? {
                    if key != layout.key {
                        return Err(de::Error::custom(format!(
                            "the layout under `{}` has the key `{}`",
                            key.chars().take(MAX_KEY_LEN).collect::<String>(),
                            layout.key.chars().take(MAX_KEY_LEN).collect::<String>(),
                        )));
                    }
                    layouts.push(layout);
                }
                Ok(layouts)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut layouts = Vec::new();
                while let Some(layout) = seq.next_element()? {
                    layouts.push(layout);
                }
                Ok(layouts)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.is_empty() {
                    Ok(Vec::new())
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }
        }

        deserializer.deserialize_any(LayoutsVisitor)
    }
}

/// A field group, as ACF writes it: a key (`group_…`), a title, the fields, where it shows
/// (location rules), its position and order, and the edit screen items it hides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldGroupDef {
    /// The group's key, `group_…`.
    pub key: String,
    /// The group's title, the heading of its box.
    #[serde(default, deserialize_with = "lenient::string")]
    pub title: String,
    /// The fields, in display order.
    #[serde(default, deserialize_with = "lenient::list")]
    pub fields: Vec<FieldDef>,
    /// Where the group shows: any of the outer lists (or), when all of its rules match (and).
    #[serde(default, deserialize_with = "lenient::list")]
    pub location: Vec<Vec<LocationRule>>,
    /// The order of the group's box among the others, lowest first.
    #[serde(default, deserialize_with = "lenient::i32")]
    pub menu_order: i32,
    /// Where the box shows on the edit screen.
    #[serde(default)]
    pub position: Position,
    /// The edit screen items the group hides.
    #[serde(default, deserialize_with = "lenient::list")]
    pub hide_on_screen: Vec<HideOnScreen>,
    /// Whether the group is active. ACF's default: true.
    #[serde(default = "true_", deserialize_with = "lenient::bool_true")]
    pub active: bool,
    /// A short description, shown in the admin's list of groups.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "lenient::string"
    )]
    pub description: String,
    /// Whether the REST API shows the group's fields (the `acf` field).
    #[serde(default, deserialize_with = "lenient::bool")]
    pub show_in_rest: bool,
    /// Who made the definition. Never read from or written to the JSON: `from_json` sets it.
    #[serde(
        skip_serializing,
        default = "loaded_origin",
        deserialize_with = "ignore_origin"
    )]
    pub origin: Origin,
    /// ACF's other group settings (`style`, `label_placement`, `instruction_placement`, ...),
    /// kept as written.
    #[serde(flatten)]
    pub settings: Map<String, Value>,
}

impl FieldGroupDef {
    /// A group with the given key and title, no fields and no location, with ACF's defaults.
    pub fn new(key: &str, title: &str) -> Self {
        Self {
            key: key.to_owned(),
            title: title.to_owned(),
            fields: Vec::new(),
            location: Vec::new(),
            menu_order: 0,
            position: Position::Normal,
            hide_on_screen: Vec::new(),
            active: true,
            description: String::new(),
            show_in_rest: false,
            origin: Origin::Admin,
            settings: Map::new(),
        }
    }

    /// Parses `json` (at most [`MAX_JSON_BYTES`]), gives it `origin`, and
    /// [validates](Self::validate) it. Any `origin` in the JSON is ignored.
    pub fn from_json(json: &str, origin: Origin) -> Result<Self, DefinitionError> {
        let mut def: Self = parse(json)?;
        def.origin = origin;
        def.validate()?;
        Ok(def)
    }

    /// Checks the rules ACF applies, and the size limits: the group's key is `group_…`, it has
    /// a title, every field's key is `field_…` (and every layout's `layout_…`) and unique in
    /// the group, field names are unique among their siblings and layout names among their
    /// field's layouts, each field's kind matches its sub-fields and layouts, and `settings`
    /// do not repeat a modelled option.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        check_key(&self.key, GROUP_PREFIX)?;
        if self.title.trim().is_empty() {
            return Err(DefinitionError::MissingTitle(self.key.clone()));
        }
        check_len("title length", &self.title, MAX_TEXT_LEN)?;
        check_len("description length", &self.description, MAX_TEXT_LEN)?;
        let rules: usize = self.location.iter().map(Vec::len).sum();
        check_count("number of location rules", rules, MAX_LOCATION_RULES)?;
        for rule in self.location.iter().flatten() {
            check_len("location value length", &rule.value, MAX_TEXT_LEN)?;
        }
        check_count(
            "number of hide-on-screen items",
            self.hide_on_screen.len(),
            MAX_LIST_ITEMS,
        )?;
        check_settings(&self.key, &self.settings, GROUP_KEYS)?;
        let mut check = GroupCheck {
            keys: HashSet::from([self.key.clone()]),
            fields: 0,
        };
        validate_fields(&self.fields, &mut check, 1)?;
        self.origin.validate()
    }

    /// Whether the definition is read-only in the admin. See [`Origin::is_locked`].
    pub fn is_locked(&self) -> bool {
        self.origin.is_locked()
    }

    /// See [`Origin::ensure_editable`].
    pub fn ensure_editable(&self) -> Result<(), DefinitionError> {
        self.origin.ensure_editable()
    }
}
