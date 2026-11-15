//! Registered meta: WordPress's `register_meta` (`register_post_meta`, `register_term_meta`) and
//! the `meta` field of the REST API (`WP_REST_Meta_Fields`).
//!
//! [`MetaKeys`] is the registry, owned by the app (`Arc<MetaKeys>`) and extended by code, as
//! [`PostTypes`](super::posts::PostTypes) is. A [`MetaKey`] names an object type (post, term or
//! user), optionally a subtype (a post type or a taxonomy), the key, its [`MetaType`], whether it
//! is `single`, its `default`, whether and with which schema it is shown in the REST API, who may
//! write it ([`MetaAuth`]) and an optional sanitizer.
//!
//! ```
//! use mission_control_core::rest::meta::{MetaKey, MetaKeys, MetaType};
//! use serde_json::json;
//!
//! let keys = MetaKeys::new();
//! keys.register(
//!     MetaKey::post("subtitle", MetaType::String)
//!         .subtype("post")
//!         .single()
//!         .default(json!("None yet"))
//!         .show_in_rest(),
//! )
//! .expect("a valid registration");
//! ```
//!
//! As WordPress:
//!
//! - Only keys registered with `show_in_rest` appear under `meta`; every other key, registered or
//!   not, never does. A subtype's registration of a key wins over the object type's.
//! - A single key shows its first stored value, or its default when it has none (the type's empty
//!   value, such as `""` or `0`, when no default is registered). A key that is not single shows
//!   the list of its values (`[default]` when it has none and a default is registered, `[]`
//!   otherwise). A stored value the schema refuses shows as `null`.
//! - Writes take `meta` in the body of a create or an update. A key that is not registered for the
//!   REST API is ignored. `null` (or `[]` for a key that is not single) deletes the key. A value is
//!   checked against the key's schema (`rest_invalid_type`, `rest_not_in_enum`, ... with
//!   WordPress's messages, status 400), then sanitized to the schema's type, then passed to the
//!   key's sanitizer.
//! - Writing a key needs what editing the object needs (`edit_post`, `edit_term`, `edit_user`)
//!   and the key's [`MetaAuth`]: by default a protected key (one starting with `_`) is refused
//!   unless the user holds `edit_post_meta` (or `delete_post_meta`, `edit_term_meta`, ...) by
//!   name, and any other key is allowed. A refusal is `rest_cannot_update` (or
//!   `rest_cannot_delete`) with the key in `data`. A value equal to the stored one is not
//!   written, and its permission is not checked, as WordPress skips it.
//! - A key that is not single is stored as one row per value, as WordPress stores it.
//! - On an update, `meta` is read after the edit permission check, so a user who may not edit the
//!   object gets `rest_cannot_edit` (403) before any 400 about a value.
//! - Discovery lists, under each route's `meta` argument, the schema of each key registered for
//!   the route's type (or taxonomy), read when discovery is asked for ([`MetaKeys::schemas`]).
//!
//! Where Mission Control differs:
//!
//! - **Validation comes first.** Every value is checked against its schema before the object is
//!   written, so a 400 leaves nothing written. WordPress writes the object first and may leave it
//!   saved. On an update the permission check also comes before the object is written; on a
//!   create it comes after, as in WordPress, since the object must exist to be checked: a refused
//!   key then leaves the new object saved, as WordPress does.
//! - **Not one transaction.** The meta writes run after the object write, each on its own (a key
//!   that is not single is replaced in one transaction). A database failure between them leaves
//!   the object saved, as in WordPress.
//! - **Errors.** A schema error has `param` in `data` beside `status`; WordPress moves `param` to
//!   `additional_data`.
//! - **Schemas.** The schema keywords checked are `type` (one or a list), `enum`, `items`,
//!   `properties`, `additionalProperties`, `minimum`, `maximum`, `exclusiveMinimum`,
//!   `exclusiveMaximum`, `minLength` and `maxLength`. Others (`format`, `pattern`, `minItems`,
//!   `uniqueItems`, `required`, `patternProperties`, ...) are not checked yet. A JSON list is not
//!   an object (PHP's arrays are both), except the empty list.
//! - **Bounds** (Ruling R66, Mission Control's own code): a key that is not single takes at most
//!   [`MAX_META_VALUES`] values, and one value at most [`MAX_META_VALUE_BYTES`] bytes of JSON; a
//!   request over either is refused with `rest_meta_too_large` (413, with `key` and `limit` in
//!   `data`) before anything is written. A response shows at most the first [`MAX_META_VALUES`]
//!   values of a key, so a key with more rows is never read whole.
//! - The `show_in_rest` `name` alias and `prepare_callback` are not supported yet.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use orbit_orm::Database;
use serde_json::{Map, Number, Value, json};

use super::RestError;
use super::params::{Params, as_integer, as_list, list_sentence};
use super::shared::{authorization_status, database_error};
use crate::content::{MetaTable, delete_meta, meta_rows, set_meta, set_meta_values};
use crate::users::meta_cap::is_protected_meta;
use crate::users::{UserCaps, has_cap};

/// The kind of object a meta key belongs to, as `register_meta`'s `$object_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaObject {
    /// A post of any type, attachments included (`post_meta`).
    Post,
    /// A term of any taxonomy (`term_meta`).
    Term,
    /// A user (`user_meta`).
    User,
}

impl MetaObject {
    /// WordPress's name for it: `post`, `term` or `user`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Term => "term",
            Self::User => "user",
        }
    }

    fn table(self) -> MetaTable {
        match self {
            Self::Post => MetaTable::Post,
            Self::Term => MetaTable::Term,
            Self::User => MetaTable::User,
        }
    }
}

/// The type of a meta value, as `register_meta`'s `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaType {
    /// `string`.
    String,
    /// `boolean`.
    Boolean,
    /// `integer`.
    Integer,
    /// `number`: an integer or a float.
    Number,
    /// `array`: shown in the REST API only with an `items` schema.
    Array,
    /// `object`: shown in the REST API with a `properties` schema.
    Object,
}

impl MetaType {
    /// The JSON Schema name: `string`, `boolean`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

/// What writing a meta key is, for [`MetaAuth`]: WordPress's `edit_{type}_meta` and
/// `delete_{type}_meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaAction {
    /// Setting a value.
    Edit,
    /// Deleting the key.
    Delete,
}

impl MetaAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }
}

/// What a [`MetaAuth::Callback`] decides on: WordPress's `auth_callback` arguments.
#[derive(Debug, Clone, Copy)]
pub struct MetaAuthRequest<'a> {
    /// The user who writes.
    pub user: &'a UserCaps,
    /// The object type.
    pub object: MetaObject,
    /// The object's id (a post id, a `term_id`, a user id).
    pub object_id: i64,
    /// The meta key.
    pub key: &'a str,
    /// Whether the key is protected (starts with `_`): WordPress's `$allowed` before the callback,
    /// negated.
    pub protected: bool,
    /// Whether the write sets or deletes the key.
    pub action: MetaAction,
}

/// A function that decides a [`MetaAuth::Callback`].
pub type MetaAuthFn = Arc<dyn Fn(&MetaAuthRequest<'_>) -> bool + Send + Sync>;

/// A function that sanitizes a value before it is stored, as `register_meta`'s
/// `sanitize_callback`. It runs after the value is sanitized to the schema's type, once per value
/// of a key that is not single.
pub type SanitizeMetaFn = Arc<dyn Fn(Value) -> Value + Send + Sync>;

/// Who may write a key, beside whoever may edit the object: `register_meta`'s `auth_callback`.
/// Whatever it decides, the user must also be able to edit the object, and a user who holds the
/// meta capability (`edit_post_meta`, ...) by name may write any key.
#[derive(Clone, Default)]
pub enum MetaAuth {
    /// WordPress's default: a protected key (starting with `_`) is refused, any other is allowed.
    #[default]
    Default,
    /// Allowed (`__return_true`), protected or not.
    Allow,
    /// Allowed for users who hold this primitive capability.
    Capability(String),
    /// Decided by a function.
    Callback(MetaAuthFn),
}

impl fmt::Debug for MetaAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("Default"),
            Self::Allow => f.write_str("Allow"),
            Self::Capability(cap) => f.debug_tuple("Capability").field(cap).finish(),
            Self::Callback(_) => f.write_str("Callback(..)"),
        }
    }
}

/// Whether and how a key is shown in the REST API: `register_meta`'s `show_in_rest`.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ShowInRest {
    /// Not shown (`false`), the default.
    #[default]
    No,
    /// Shown with the schema made from the type, description and default (`true`).
    Yes,
    /// Shown with this schema over the one made from the type (`['schema' => ...]`), such as
    /// `{"items": {"type": "string"}}` for an array or `{"enum": [...]}`.
    Schema(Map<String, Value>),
}

/// One registered meta key, as `register_meta`'s arguments. Built with [`MetaKey::post`],
/// [`MetaKey::term`] or [`MetaKey::user`] and the methods that follow.
#[derive(Clone)]
pub struct MetaKey {
    /// The object type.
    pub object: MetaObject,
    /// The subtype: a post type (`post`, `page`, `attachment`) or a taxonomy (`category`). `None`
    /// registers the key for every object of the type.
    pub subtype: Option<String>,
    /// The meta key.
    pub key: String,
    /// The value's type.
    pub meta_type: MetaType,
    /// What the key holds, shown in the schema.
    pub description: String,
    /// Whether the key holds one value (`true`) or a list of values, one row each.
    pub single: bool,
    /// The value shown when the key is not stored. `None` shows the type's empty value.
    pub default: Option<Value>,
    /// Whether and how the key is shown in the REST API.
    pub show_in_rest: ShowInRest,
    /// Who may write the key.
    pub auth: MetaAuth,
    /// The sanitizer run before a value is stored, after the schema's.
    pub sanitize: Option<SanitizeMetaFn>,
}

impl fmt::Debug for MetaKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetaKey")
            .field("object", &self.object)
            .field("subtype", &self.subtype)
            .field("key", &self.key)
            .field("meta_type", &self.meta_type)
            .field("single", &self.single)
            .field("default", &self.default)
            .field("show_in_rest", &self.show_in_rest)
            .field("auth", &self.auth)
            .field("sanitize", &self.sanitize.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

impl MetaKey {
    /// Key `key` of `object` holding `meta_type`, with WordPress's defaults: no subtype, not
    /// single, no default, not shown in the REST API, the default [`MetaAuth`], no sanitizer.
    pub fn new(object: MetaObject, key: impl Into<String>, meta_type: MetaType) -> Self {
        Self {
            object,
            subtype: None,
            key: key.into(),
            meta_type,
            description: String::new(),
            single: false,
            default: None,
            show_in_rest: ShowInRest::No,
            auth: MetaAuth::Default,
            sanitize: None,
        }
    }

    /// Post meta `key`, as `register_post_meta('', $key, ...)`.
    pub fn post(key: impl Into<String>, meta_type: MetaType) -> Self {
        Self::new(MetaObject::Post, key, meta_type)
    }

    /// Term meta `key`, as `register_term_meta('', $key, ...)`.
    pub fn term(key: impl Into<String>, meta_type: MetaType) -> Self {
        Self::new(MetaObject::Term, key, meta_type)
    }

    /// User meta `key`, as `register_meta('user', $key, ...)`.
    pub fn user(key: impl Into<String>, meta_type: MetaType) -> Self {
        Self::new(MetaObject::User, key, meta_type)
    }

    /// For the post type or taxonomy `subtype` only.
    pub fn subtype(mut self, subtype: impl Into<String>) -> Self {
        self.subtype = Some(subtype.into());
        self
    }

    /// Holding one value.
    pub fn single(mut self) -> Self {
        self.single = true;
        self
    }

    /// With `default` shown when the key is not stored.
    pub fn default(mut self, default: Value) -> Self {
        self.default = Some(default);
        self
    }

    /// With `description` in its schema.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Shown in the REST API with the schema made from its type.
    pub fn show_in_rest(mut self) -> Self {
        self.show_in_rest = ShowInRest::Yes;
        self
    }

    /// Shown in the REST API with `schema` (a JSON object) over the one made from its type.
    /// Anything but an object shows it with the schema made from its type.
    pub fn rest_schema(mut self, schema: Value) -> Self {
        self.show_in_rest = match schema {
            Value::Object(schema) => ShowInRest::Schema(schema),
            _ => ShowInRest::Yes,
        };
        self
    }

    /// Written by whoever `auth` allows.
    pub fn auth(mut self, auth: MetaAuth) -> Self {
        self.auth = auth;
        self
    }

    /// Sanitized by `sanitize` before it is stored.
    pub fn sanitize(mut self, sanitize: impl Fn(Value) -> Value + Send + Sync + 'static) -> Self {
        self.sanitize = Some(Arc::new(sanitize));
        self
    }

    /// The schema of one value: the type, description and default, with the registration's
    /// schema over them and `additionalProperties` false where an object leaves it out, as
    /// `WP_REST_Meta_Fields::get_registered_fields` builds it. `None` when the key is not shown or
    /// its schema's type is not one WordPress shows.
    fn value_schema(&self) -> Option<Map<String, Value>> {
        let given = match &self.show_in_rest {
            ShowInRest::No => return None,
            ShowInRest::Yes => Map::new(),
            ShowInRest::Schema(schema) => schema.clone(),
        };
        let mut schema = Map::new();
        schema.insert("type".into(), json!(self.meta_type.as_str()));
        schema.insert("description".into(), json!(self.description));
        schema.insert(
            "default".into(),
            self.default.clone().unwrap_or(Value::Null),
        );
        schema.extend(given);
        let kind = schema.get("type").and_then(Value::as_str)?.to_owned();
        if !matches!(
            kind.as_str(),
            "string" | "boolean" | "integer" | "number" | "array" | "object"
        ) {
            return None;
        }
        if schema.get("default").is_none_or(Value::is_null) {
            schema.insert("default".into(), empty_value(&kind));
        }
        Some(additional_properties_to_false(schema))
    }
}

/// Why a [`MetaKey`] was not registered, as `register_meta` refuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaKeyError {
    /// The key is empty.
    EmptyKey,
    /// An array shown in the REST API has no `items` schema.
    ArrayWithoutItems,
    /// The default does not match the type.
    InvalidDefault(String),
}

impl fmt::Display for MetaKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyKey => f.write_str("A meta key is required."),
            Self::ArrayWithoutItems => f.write_str(
                "When registering an \"array\" meta type to show in the REST API, you must \
                 specify the schema for each array item in \"show_in_rest.schema.items\".",
            ),
            Self::InvalidDefault(reason) => write!(
                f,
                "When registering a default meta value the data must match the type provided: \
                 {reason}"
            ),
        }
    }
}

impl std::error::Error for MetaKeyError {}

/// The registered meta keys, shared by the app and the code that registers keys (themes and
/// plugins), as WordPress's `$wp_meta_keys`.
#[derive(Debug, Default)]
pub struct MetaKeys {
    keys: RwLock<Vec<MetaKey>>,
}

impl MetaKeys {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `key`, replacing a registration of the same object type, subtype and key.
    ///
    /// # Errors
    ///
    /// As `register_meta` refuses it: an empty key, an array shown in the REST API without an
    /// `items` schema, or a default that does not match the type (or the REST schema).
    pub fn register(&self, key: MetaKey) -> Result<(), MetaKeyError> {
        if key.key.is_empty() {
            return Err(MetaKeyError::EmptyKey);
        }
        if key.meta_type == MetaType::Array && key.show_in_rest != ShowInRest::No {
            let has_items =
                matches!(&key.show_in_rest, ShowInRest::Schema(s) if s.contains_key("items"));
            if !has_items {
                return Err(MetaKeyError::ArrayWithoutItems);
            }
        }
        if let Some(default) = &key.default {
            let mut schema = Map::new();
            schema.insert("type".into(), json!(key.meta_type.as_str()));
            if let ShowInRest::Schema(given) = &key.show_in_rest {
                schema.extend(given.clone());
            }
            if let Err(failure) = validate(default, &schema, "default") {
                return Err(MetaKeyError::InvalidDefault(failure.message));
            }
        }
        let mut keys = self.keys.write().unwrap_or_else(|e| e.into_inner());
        keys.retain(|k| !(k.object == key.object && k.subtype == key.subtype && k.key == key.key));
        keys.push(key);
        Ok(())
    }

    /// Removes the registration of `key` for `object` and `subtype`, as `unregister_meta_key`.
    /// True when there was one.
    pub fn unregister(&self, object: MetaObject, subtype: Option<&str>, key: &str) -> bool {
        let mut keys = self.keys.write().unwrap_or_else(|e| e.into_inner());
        let before = keys.len();
        keys.retain(|k| !(k.object == object && k.subtype.as_deref() == subtype && k.key == key));
        keys.len() != before
    }

    /// The registrations for `object`: those of every subtype and those of `subtype` only, as
    /// `get_registered_meta_keys` lists them.
    pub fn registered(&self, object: MetaObject, subtype: &str) -> Vec<MetaKey> {
        self.read()
            .iter()
            .filter(|k| k.object == object && k.subtype.as_deref().is_none_or(|s| s == subtype))
            .cloned()
            .collect()
    }

    /// The keys shown under `meta` for an object of `object` and `subtype`, in the order
    /// registered; a subtype's registration of a key replaces the object type's.
    pub(crate) fn rest_fields(&self, object: MetaObject, subtype: &str) -> Vec<RestField> {
        let keys = self.read();
        let mut fields: Vec<RestField> = Vec::new();
        for key in keys
            .iter()
            .filter(|k| k.object == object && k.subtype.is_none())
        {
            fields.extend(RestField::new(key));
        }
        for key in keys
            .iter()
            .filter(|k| k.object == object && k.subtype.as_deref() == Some(subtype))
        {
            if let Some(field) = RestField::new(key) {
                match fields.iter_mut().find(|f| f.key.key == key.key) {
                    Some(existing) => *existing = field,
                    None => fields.push(field),
                }
            }
        }
        fields
    }

    /// The `properties` of the `meta` argument for `object` and `subtype`, as WordPress's
    /// discovery lists them: each shown key's schema (a list of values for a key that is not
    /// single) by name, or `[]` (WordPress's empty PHP array) when none is registered.
    pub fn schemas(&self, object: MetaObject, subtype: &str) -> Value {
        let fields = self.rest_fields(object, subtype);
        if fields.is_empty() {
            return json!([]);
        }
        Value::Object(
            fields
                .iter()
                .map(|f| (f.name().to_owned(), Value::Object(f.request_schema())))
                .collect(),
        )
    }

    // A poisoned lock only means a registering thread panicked; the list is still whole.
    fn read(&self) -> RwLockReadGuard<'_, Vec<MetaKey>> {
        self.keys.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// A key shown in the REST API: its registration and the schema of one value.
#[derive(Debug, Clone)]
pub(crate) struct RestField {
    key: MetaKey,
    schema: Map<String, Value>,
}

impl RestField {
    fn new(key: &MetaKey) -> Option<Self> {
        Some(Self {
            schema: key.value_schema()?,
            key: key.clone(),
        })
    }

    fn name(&self) -> &str {
        &self.key.key
    }

    /// The schema a request's value is checked against: the value's, or a list of them for a key
    /// that is not single.
    fn request_schema(&self) -> Map<String, Value> {
        if self.key.single {
            self.schema.clone()
        } else {
            let mut schema = Map::new();
            schema.insert("type".into(), json!("array"));
            schema.insert("items".into(), Value::Object(self.schema.clone()));
            schema
        }
    }

    /// The value shown for `stored` (the key's values, in order), as `get_value` and
    /// `prepare_value` give it.
    fn response_value(&self, stored: &[Value]) -> Value {
        if self.key.single {
            let value = stored
                .first()
                .cloned()
                .unwrap_or_else(|| self.schema["default"].clone());
            return prepare_value(value, &self.schema);
        }
        if stored.is_empty() {
            return match &self.key.default {
                Some(default) => json!([prepare_value(default.clone(), &self.schema)]),
                None => json!([]),
            };
        }
        Value::Array(
            stored
                .iter()
                .map(|value| prepare_value(value.clone(), &self.schema))
                .collect(),
        )
    }

    /// One value as it is stored: sanitized to the schema, then by the key's sanitizer.
    fn sanitize_for_storage(&self, value: &Value) -> Value {
        let value = sanitize(value, &self.schema);
        match &self.key.sanitize {
            Some(sanitize) => sanitize(value),
            None => value,
        }
    }

    /// Whether `user` may do `action` on the key of object `object_id`, given whether they may
    /// edit the object: `map_meta_cap` for `edit_{type}_meta` and `delete_{type}_meta`.
    fn allows(
        &self,
        user: &UserCaps,
        object_id: i64,
        action: MetaAction,
        can_edit_object: bool,
    ) -> bool {
        if !can_edit_object {
            return false;
        }
        let protected = is_protected_meta(&self.key.key);
        let allowed = match &self.key.auth {
            MetaAuth::Default => !protected,
            MetaAuth::Allow => true,
            MetaAuth::Capability(cap) => has_cap(user, cap),
            MetaAuth::Callback(decide) => decide(&MetaAuthRequest {
                user,
                object: self.key.object,
                object_id,
                key: &self.key.key,
                protected,
                action,
            }),
        };
        allowed
            || has_cap(
                user,
                &format!("{}_{}_meta", action.as_str(), self.key.object.as_str()),
            )
    }
}

/// The type's empty value, as `WP_REST_Meta_Fields::get_empty_value_for_type`. An object's is
/// `{}` (WordPress's empty PHP array is `[]` in JSON).
fn empty_value(kind: &str) -> Value {
    match kind {
        "array" => json!([]),
        "object" => json!({}),
        "boolean" => json!(false),
        "integer" => json!(0),
        "number" => json!(0.0),
        _ => json!(""),
    }
}

/// `additionalProperties: false` on every object of `schema` that leaves it out, as
/// `rest_default_additional_properties_to_false`.
fn additional_properties_to_false(mut schema: Map<String, Value>) -> Map<String, Value> {
    let kinds = types_of(&schema);
    if kinds.iter().any(|k| k == "object") {
        if let Some(Value::Object(properties)) = schema.get_mut("properties") {
            for child in properties.values_mut() {
                if let Value::Object(child_schema) = child {
                    *child_schema = additional_properties_to_false(std::mem::take(child_schema));
                }
            }
        }
        schema
            .entry("additionalProperties")
            .or_insert(Value::Bool(false));
    }
    if kinds.iter().any(|k| k == "array")
        && let Some(Value::Object(items)) = schema.get_mut("items")
    {
        *items = additional_properties_to_false(std::mem::take(items));
    }
    schema
}

/// The schema's types: its `type`, or each of a list.
fn types_of(schema: &Map<String, Value>) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.clone()],
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// A stored value as the response shows it, as `WP_REST_Meta_Fields::prepare_value`: `""` reads
/// as the empty value of a boolean or number, a value the schema refuses is `null`, and anything
/// else is sanitized to the schema.
fn prepare_value(value: Value, schema: &Map<String, Value>) -> Value {
    let kinds = types_of(schema);
    let value = match (&value, kinds.first().map(String::as_str)) {
        (Value::String(s), Some(kind @ ("boolean" | "integer" | "number")))
            if s.is_empty() && kinds.len() == 1 =>
        {
            empty_value(kind)
        }
        _ => value,
    };
    if validate(&value, schema, "").is_err() {
        return Value::Null;
    }
    sanitize(&value, schema)
}

/// A schema check that failed: WordPress's code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    code: &'static str,
    message: String,
    param: String,
}

impl Failure {
    fn new(code: &'static str, param: &str, message: String) -> Self {
        Self {
            code,
            message,
            param: param.to_owned(),
        }
    }

    fn invalid_type(param: &str, kinds: &[String]) -> Self {
        Self::new(
            "rest_invalid_type",
            param,
            format!("{param} is not of type {}.", kinds.join(",")),
        )
    }
}

/// `rest_is_boolean`: a boolean, `"true"`, `"false"`, `"1"`, `"0"` (any case), 1 or 0.
fn is_boolean(value: &Value) -> bool {
    match value {
        Value::Bool(_) => true,
        Value::String(s) => matches!(
            s.to_ascii_lowercase().as_str(),
            "true" | "false" | "1" | "0"
        ),
        Value::Number(n) => matches!(n.as_i64(), Some(0 | 1)),
        _ => false,
    }
}

/// PHP's `is_numeric`, for a JSON value: a number, or a string that parses as one (leading and
/// trailing whitespace allowed).
fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s
            .trim_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c'])
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite()),
        _ => None,
    }
}

/// `rest_is_array`: a list, or a scalar (which `wp_parse_list` splits).
fn is_array(value: &Value) -> bool {
    !matches!(value, Value::Object(object) if !object.is_empty()) && !value.is_null()
}

/// `rest_is_object`: an object, `""`, or the empty list (PHP's empty array).
fn is_object(value: &Value) -> bool {
    match value {
        Value::Object(_) => true,
        Value::String(s) => s.is_empty(),
        Value::Array(list) => list.is_empty(),
        _ => false,
    }
}

/// Whether `value` is of the single type `kind`.
fn is_of(value: &Value, kind: &str) -> bool {
    match kind {
        "array" => is_array(value),
        "object" => is_object(value),
        "integer" => as_integer(value).is_some() && !matches!(value, Value::Bool(_)),
        "number" => as_number(value).is_some(),
        "boolean" => is_boolean(value),
        "string" => value.is_string(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// The type of `kinds` that `value` is, checked in WordPress's order, as
/// `rest_get_best_type_for_value`.
fn best_type(value: &Value, kinds: &[String]) -> Option<String> {
    const ORDER: [&str; 7] = [
        "array", "object", "integer", "number", "boolean", "string", "null",
    ];
    ORDER
        .iter()
        .find(|kind| kinds.iter().any(|k| k == *kind) && is_of(value, kind))
        .map(|kind| (*kind).to_owned())
}

/// A value's list form, as `rest_sanitize_array` reads it: a list as it is, an empty object as
/// an empty list, a scalar split as `wp_parse_list` splits it.
fn to_list(value: &Value) -> Vec<Value> {
    match value {
        Value::Object(_) => Vec::new(),
        other => as_list(other),
    }
}

/// A value's object form: an object as it is, anything else (`""`, `[]`) empty.
fn to_object(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object.clone(),
        _ => Map::new(),
    }
}

/// Checks `value` against `schema`, as `rest_validate_value_from_schema` does for the keywords
/// this module supports (see the module's notes). `param` names the value in messages, such as
/// `meta.subtitle`.
fn validate(value: &Value, schema: &Map<String, Value>, param: &str) -> Result<(), Failure> {
    let kinds = types_of(schema);
    let kind = if kinds.len() > 1 {
        best_type(value, &kinds).ok_or_else(|| Failure::invalid_type(param, &kinds))?
    } else {
        let Some(kind) = kinds.first() else {
            return Ok(());
        };
        if !is_of(value, kind) {
            return Err(Failure::invalid_type(param, &kinds));
        }
        kind.clone()
    };
    let mut single = schema.clone();
    single.insert("type".into(), json!(kind));
    if let Some(Value::Array(allowed)) = schema.get("enum") {
        check_enum(value, &single, allowed, param)?;
    }
    match kind.as_str() {
        "array" => {
            if let Some(Value::Object(items)) = schema.get("items") {
                for (i, item) in to_list(value).iter().enumerate() {
                    validate(item, items, &format!("{param}[{i}]"))?;
                }
            }
        }
        "object" => {
            let properties = schema.get("properties").and_then(Value::as_object);
            for (name, child) in &to_object(value) {
                if let Some(Value::Object(child_schema)) = properties.and_then(|p| p.get(name)) {
                    validate(child, child_schema, &format!("{param}[{name}]"))?;
                    continue;
                }
                match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        return Err(Failure::new(
                            "rest_additional_properties_forbidden",
                            param,
                            format!("{name} is not a valid property of Object."),
                        ));
                    }
                    Some(Value::Object(extra)) => {
                        validate(child, extra, &format!("{param}[{name}]"))?;
                    }
                    _ => {}
                }
            }
        }
        "integer" | "number" => {
            let n = as_number(value).unwrap_or(0.0);
            check_bounds(n, schema, param)?;
        }
        "string" => {
            let length = value.as_str().map_or(0, |s| s.chars().count());
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && (length as u64) < min
            {
                let unit = if min == 1 { "character" } else { "characters" };
                return Err(Failure::new(
                    "rest_too_short",
                    param,
                    format!("{param} must be at least {min} {unit} long."),
                ));
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && (length as u64) > max
            {
                let unit = if max == 1 { "character" } else { "characters" };
                return Err(Failure::new(
                    "rest_too_long",
                    param,
                    format!("{param} must be at most {max} {unit} long."),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// `rest_validate_enum`: the value, sanitized to the schema, must equal one of `allowed`.
fn check_enum(
    value: &Value,
    schema: &Map<String, Value>,
    allowed: &[Value],
    param: &str,
) -> Result<(), Failure> {
    let sanitized = sanitize(value, schema);
    let equal = |a: &Value, b: &Value| match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    };
    if allowed.iter().any(|candidate| equal(candidate, &sanitized)) {
        return Ok(());
    }
    let shown: Vec<String> = allowed
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Bool(true) => "1".to_owned(),
            Value::Bool(false) => String::new(),
            other => other.to_string(),
        })
        .collect();
    let message = match shown.as_slice() {
        [one] => format!("{param} is not {one}."),
        _ => {
            let shown: Vec<&str> = shown.iter().map(String::as_str).collect();
            format!("{param} is not one of {}.", list_sentence(&shown))
        }
    };
    Err(Failure::new("rest_not_in_enum", param, message))
}

/// `minimum`, `maximum` and their exclusive forms, with WordPress's messages (`%d`: the bounds
/// are shown as integers).
fn check_bounds(n: f64, schema: &Map<String, Value>, param: &str) -> Result<(), Failure> {
    let min = schema.get("minimum").and_then(Value::as_f64);
    let max = schema.get("maximum").and_then(Value::as_f64);
    let flag = |name: &str| schema.get(name).and_then(Value::as_bool).unwrap_or(false);
    let (ex_min, ex_max) = (flag("exclusiveMinimum"), flag("exclusiveMaximum"));
    let below = |min: f64| if ex_min { n <= min } else { n < min };
    let above = |max: f64| if ex_max { n >= max } else { n > max };
    let shown = |bound: f64| (bound.trunc() as i64).to_string();
    let message = match (min, max) {
        (Some(min), None) if below(min) => Some(if ex_min {
            format!("{param} must be greater than {}", shown(min))
        } else {
            format!("{param} must be greater than or equal to {}", shown(min))
        }),
        (None, Some(max)) if above(max) => Some(if ex_max {
            format!("{param} must be less than {}", shown(max))
        } else {
            format!("{param} must be less than or equal to {}", shown(max))
        }),
        (Some(min), Some(max)) if below(min) || above(max) => {
            let side = |exclusive: bool| if exclusive { "exclusive" } else { "inclusive" };
            Some(format!(
                "{param} must be between {} ({}) and {} ({})",
                shown(min),
                side(ex_min),
                shown(max),
                side(ex_max)
            ))
        }
        _ => None,
    };
    match message {
        Some(message) => Err(Failure::new("rest_out_of_bounds", param, message)),
        None => Ok(()),
    }
}

/// `value` sanitized to `schema`, as `rest_sanitize_value_from_schema`: to the schema's type
/// (the best of a list), lists and objects item by item, properties an object does not allow
/// dropped. Run on a value [`validate`] accepted.
fn sanitize(value: &Value, schema: &Map<String, Value>) -> Value {
    let kinds = types_of(schema);
    let kind = if kinds.len() > 1 {
        match best_type(value, &kinds) {
            Some(kind) => kind,
            None => return value.clone(),
        }
    } else {
        match kinds.first() {
            Some(kind) => kind.clone(),
            None => return value.clone(),
        }
    };
    match kind.as_str() {
        "array" => {
            let items = schema.get("items").and_then(Value::as_object);
            Value::Array(
                to_list(value)
                    .iter()
                    .map(|item| items.map_or_else(|| item.clone(), |s| sanitize(item, s)))
                    .collect(),
            )
        }
        "object" => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let mut out = Map::new();
            for (name, child) in to_object(value) {
                if let Some(Value::Object(child_schema)) = properties.and_then(|p| p.get(&name)) {
                    out.insert(name, sanitize(&child, child_schema));
                    continue;
                }
                match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {}
                    Some(Value::Object(extra)) => {
                        out.insert(name, sanitize(&child, extra));
                    }
                    _ => {
                        out.insert(name, child);
                    }
                }
            }
            Value::Object(out)
        }
        "integer" => match as_integer(value) {
            Some(n) => json!(n),
            None => json!(as_number(value).map_or(0, |f| f.trunc() as i64)),
        },
        "number" => as_number(value)
            .and_then(Number::from_f64)
            .map_or(json!(0.0), Value::Number),
        "boolean" => Value::Bool(match value {
            Value::Bool(b) => *b,
            Value::String(s) => !(s.is_empty() || s == "0" || s.eq_ignore_ascii_case("false")),
            Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
            _ => false,
        }),
        "string" => match value {
            Value::String(s) => Value::String(s.clone()),
            Value::Number(n) => Value::String(n.to_string()),
            Value::Bool(true) => json!("1"),
            Value::Bool(false) | Value::Null => json!(""),
            other => Value::String(other.to_string()),
        },
        "null" => Value::Null,
        _ => value.clone(),
    }
}

/// A stored row's text as a value: JSON, or the text itself when it is not JSON (a value written
/// by something other than Mission Control).
fn stored_value(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// The registered meta of a page of objects, read in one query, for the `meta` field.
#[derive(Debug, Default)]
pub(crate) struct MetaValues {
    fields: Vec<RestField>,
    rows: HashMap<(i64, String), Vec<Value>>,
}

impl MetaValues {
    /// The values of the keys registered for `object` and `subtype` on the objects `ids`.
    pub(crate) async fn load(
        db: &Database,
        keys: &MetaKeys,
        object: MetaObject,
        subtype: &str,
        ids: &[i64],
    ) -> Result<Self, RestError> {
        let fields = keys.rest_fields(object, subtype);
        let names: Vec<&str> = fields.iter().map(RestField::name).collect();
        let mut rows: HashMap<(i64, String), Vec<Value>> = HashMap::new();
        for (owner, key, text) in meta_rows(db, object.table(), ids, &names, MAX_META_VALUES)
            .await
            .map_err(|e| database_error(&e))?
        {
            rows.entry((owner, key))
                .or_default()
                .push(stored_value(text));
        }
        Ok(Self { fields, rows })
    }

    /// The `meta` field of object `id`: each key's value by name, in the order registered.
    pub(crate) fn json(&self, id: i64) -> Value {
        let mut meta = Map::new();
        for field in &self.fields {
            let stored = self
                .rows
                .get(&(id, field.name().to_owned()))
                .map_or(&[][..], Vec::as_slice);
            meta.insert(field.name().to_owned(), field.response_value(stored));
        }
        Value::Object(meta)
    }
}

/// What a request writes to one key.
#[derive(Debug, Clone, PartialEq)]
enum Change {
    /// Delete the key.
    Delete,
    /// Store one value (a single key).
    Set(Value),
    /// Store these values, one row each (a key that is not single).
    SetAll(Vec<Value>),
}

/// A write to one key.
#[derive(Debug, Clone)]
struct Write {
    field: RestField,
    change: Change,
}

/// The `meta` of a create or update, read and checked against the registered schemas, not yet
/// authorized: `update_value` of `WP_REST_Meta_Fields` up to its permission checks.
#[derive(Debug, Default)]
pub(crate) struct MetaUpdate {
    object: Option<MetaObject>,
    writes: Vec<Write>,
}

impl MetaUpdate {
    /// Reads `meta` from `params` for an object of `object` and `subtype`. Keys that are not
    /// registered for the REST API are ignored; a `meta` that is not an object writes nothing
    /// (the controllers refuse it before).
    ///
    /// # Errors
    ///
    /// WordPress's schema error for the first value the schema refuses (400), or
    /// `rest_invalid_stored_value` (500) for a list holding `null`.
    pub(crate) fn read(
        keys: &MetaKeys,
        object: MetaObject,
        subtype: &str,
        params: &Params,
    ) -> Result<Self, RestError> {
        let mut update = Self {
            object: Some(object),
            writes: Vec::new(),
        };
        let Some(Value::Object(meta)) = params.get("meta") else {
            return Ok(update);
        };
        for field in keys.rest_fields(object, subtype) {
            let Some(value) = meta.get(field.name()) else {
                continue;
            };
            let empty_list = value.as_array().is_some_and(Vec::is_empty);
            if value.is_null() || (empty_list && !field.key.single) {
                update.writes.push(Write {
                    field,
                    change: Change::Delete,
                });
                continue;
            }
            if !field.key.single
                && value
                    .as_array()
                    .is_some_and(|l| l.iter().any(Value::is_null))
            {
                return Err(invalid_stored_value(field.name()));
            }
            if !field.key.single {
                let count = match value {
                    Value::Array(list) => list.len(),
                    other => to_list(other).len(),
                };
                if count > MAX_META_VALUES {
                    return Err(too_large(
                        field.name(),
                        format!(
                            "The {} custom field is too large to save: it has {count} values, \
                             and at most {MAX_META_VALUES} are allowed.",
                            field.name()
                        ),
                        MAX_META_VALUES,
                    ));
                }
            }
            // A value too large to store is refused before it is checked, as are its sanitized
            // values below. The bound is per value: each item of a key that is not single.
            if field.key.single {
                check_value_size(field.name(), value)?;
            } else {
                match value {
                    Value::Array(items) => {
                        for item in items {
                            check_value_size(field.name(), item)?;
                        }
                    }
                    other => {
                        for item in &to_list(other) {
                            check_value_size(field.name(), item)?;
                        }
                    }
                }
            }
            let param = format!("meta.{}", field.name());
            if let Err(failure) = validate(value, &field.request_schema(), &param) {
                return Err(RestError::new(failure.code, failure.message, 400)
                    .with_data("param", json!(failure.param)));
            }
            let change = if field.key.single {
                Change::Set(field.sanitize_for_storage(value))
            } else {
                let list = sanitize(value, &field.request_schema());
                Change::SetAll(
                    to_list(&list)
                        .iter()
                        .map(|item| match &field.key.sanitize {
                            Some(sanitize) => sanitize(item.clone()),
                            None => item.clone(),
                        })
                        .collect(),
                )
            };
            match &change {
                Change::Set(value) => check_value_size(field.name(), value)?,
                Change::SetAll(values) => {
                    for value in values {
                        check_value_size(field.name(), value)?;
                    }
                }
                Change::Delete => {}
            }
            update.writes.push(Write { field, change });
        }
        Ok(update)
    }

    /// Checks the writes on object `object_id` for `user`, who may edit the object when
    /// `can_edit_object`: a value equal to the stored one is dropped unchecked, as WordPress
    /// skips it; every other write needs the key's permission.
    ///
    /// # Errors
    ///
    /// `rest_cannot_update` or `rest_cannot_delete` (401 or 403) naming the key,
    /// `rest_invalid_stored_value` (500) when a single key to delete holds a value its schema
    /// refuses, or a 500 when the stored values cannot be read.
    pub(crate) async fn authorize(
        self,
        db: &Database,
        user: &UserCaps,
        object_id: i64,
        can_edit_object: bool,
    ) -> Result<AuthorizedMeta, RestError> {
        let Some(object) = self.object else {
            return Ok(AuthorizedMeta::default());
        };
        let mut writes = Vec::new();
        for write in self.writes {
            let name = write.field.name().to_owned();
            // One more than a request may write, so a key holding more rows never reads as equal
            // to a list of the most a request may write.
            let current: Vec<Value> = meta_rows(
                db,
                object.table(),
                &[object_id],
                &[&name],
                MAX_META_VALUES + 1,
            )
            .await
            .map_err(|e| database_error(&e))?
            .into_iter()
            .map(|(_, _, text)| stored_value(text))
            .collect();
            let action = match &write.change {
                Change::Delete => {
                    if write.field.key.single
                        && let Some(stored) = current.first()
                        && validate(stored, &write.field.schema, "").is_err()
                    {
                        return Err(invalid_stored_value(&name));
                    }
                    MetaAction::Delete
                }
                Change::Set(value) => {
                    if current.len() == 1 && current[0] == *value {
                        continue;
                    }
                    MetaAction::Edit
                }
                Change::SetAll(_) => MetaAction::Edit,
            };
            if !write.field.allows(user, object_id, action, can_edit_object) {
                let code = match action {
                    MetaAction::Edit => "rest_cannot_update",
                    MetaAction::Delete => "rest_cannot_delete",
                };
                return Err(RestError::new(
                    code,
                    format!("Sorry, you are not allowed to edit the {name} custom field."),
                    authorization_status(user),
                )
                .with_data("key", json!(name)));
            }
            let unchanged = match &write.change {
                Change::Delete => current.is_empty(),
                Change::SetAll(values) => *values == current,
                Change::Set(_) => false,
            };
            if !unchanged {
                writes.push(write);
            }
        }
        Ok(AuthorizedMeta {
            object: Some(object),
            object_id,
            writes,
        })
    }
}

/// The most values a key that is not single may hold through the REST API (Ruling R66): a
/// request with more is refused with `rest_meta_too_large`, and a response shows the first ones
/// only. WordPress has no such bound.
pub const MAX_META_VALUES: usize = 1000;

/// The most bytes one value may take as JSON text (1 MiB), for a single key or each value of a
/// key that is not single (Ruling R66): a larger one is refused with `rest_meta_too_large`.
/// WordPress has no such bound.
pub const MAX_META_VALUE_BYTES: usize = 1024 * 1024;

/// `rest_meta_too_large` (413) for key `name`, with `limit` in `data`.
fn too_large(name: &str, message: String, limit: usize) -> RestError {
    RestError::new("rest_meta_too_large", message, 413)
        .with_data("key", json!(name))
        .with_data("limit", json!(limit))
}

/// Refuses `value` when its JSON text is longer than [`MAX_META_VALUE_BYTES`]. The size is
/// counted without building the text.
fn check_value_size(name: &str, value: &Value) -> Result<(), RestError> {
    /// Counts bytes, failing as soon as the count passes the limit.
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            if self.0 > MAX_META_VALUE_BYTES {
                return Err(std::io::Error::other("too large"));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if serde_json::to_writer(Counter(0), value).is_err() {
        return Err(too_large(
            name,
            format!(
                "A value of the {name} custom field is too large to save: at most \
                 {MAX_META_VALUE_BYTES} bytes are allowed."
            ),
            MAX_META_VALUE_BYTES,
        ));
    }
    Ok(())
}

/// `rest_invalid_stored_value`, 500, as WordPress refuses to delete a single key whose stored
/// value its schema refuses, or to store a list holding `null`.
fn invalid_stored_value(name: &str) -> RestError {
    RestError::new(
        "rest_invalid_stored_value",
        format!("The {name} property has an invalid stored value, and cannot be updated to null."),
        500,
    )
}

/// Meta writes that passed their permission checks, ready to run after the object is written.
#[derive(Debug, Default)]
pub(crate) struct AuthorizedMeta {
    object: Option<MetaObject>,
    object_id: i64,
    writes: Vec<Write>,
}

impl AuthorizedMeta {
    /// Runs the writes, in the order the keys were registered.
    ///
    /// # Errors
    ///
    /// `rest_meta_database_error` (500) naming the key when the database refuses a write; the
    /// cause is logged.
    pub(crate) async fn apply(self, db: &Database) -> Result<(), RestError> {
        let Some(object) = self.object else {
            return Ok(());
        };
        let table = object.table();
        for write in self.writes {
            let name = write.field.name();
            let result = match &write.change {
                Change::Delete => delete_meta(db, table, self.object_id, name)
                    .await
                    .map(|_| ()),
                Change::Set(value) => set_meta(db, table, self.object_id, name, value).await,
                Change::SetAll(values) => {
                    set_meta_values(db, table, self.object_id, name, values).await
                }
            };
            if let Err(e) = result {
                log::error!(
                    "Could not write meta {name} of {} {}: {e}",
                    object.as_str(),
                    self.object_id
                );
                let message = match write.change {
                    Change::Delete => "Could not delete meta value from database.".to_owned(),
                    _ => format!("Could not update the meta value of {name} in database."),
                };
                return Err(RestError::new("rest_meta_database_error", message, 500)
                    .with_data("key", json!(name)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    #[test]
    fn values_are_checked_with_wordpresss_messages() {
        let int = schema(json!({"type": "integer"}));
        assert!(validate(&json!(5), &int, "meta.n").is_ok());
        assert!(validate(&json!("5"), &int, "meta.n").is_ok());
        let failure = validate(&json!("five"), &int, "meta.n").unwrap_err();
        assert_eq!(failure.code, "rest_invalid_type");
        assert_eq!(failure.message, "meta.n is not of type integer.");
        assert!(validate(&json!(true), &int, "meta.n").is_err());

        let boolean = schema(json!({"type": "boolean"}));
        assert!(validate(&json!("false"), &boolean, "b").is_ok());
        assert!(validate(&json!(""), &boolean, "b").is_err());

        let list = schema(json!({"type": "array", "items": {"type": "integer"}}));
        let failure = validate(&json!([1, "x"]), &list, "meta.l").unwrap_err();
        assert_eq!(failure.message, "meta.l[1] is not of type integer.");
        assert_eq!(sanitize(&json!("1, 2"), &list), json!([1, 2]));

        let object = additional_properties_to_false(schema(json!({
            "type": "object", "properties": {"a": {"type": "string"}},
        })));
        let failure = validate(&json!({"a": "x", "b": 1}), &object, "meta.o").unwrap_err();
        assert_eq!(
            (failure.code, failure.message.as_str()),
            (
                "rest_additional_properties_forbidden",
                "b is not a valid property of Object."
            )
        );

        let colours = schema(json!({"type": "string", "enum": ["red", "green", "blue"]}));
        assert_eq!(
            validate(&json!("pink"), &colours, "meta.c")
                .unwrap_err()
                .message,
            "meta.c is not one of red, green, and blue."
        );
        let bounded = schema(json!({"type": "integer", "minimum": 1, "maximum": 5}));
        assert_eq!(
            validate(&json!(9), &bounded, "meta.n").unwrap_err().message,
            "meta.n must be between 1 (inclusive) and 5 (inclusive)"
        );
        let short = schema(json!({"type": "string", "maxLength": 1}));
        assert_eq!(
            validate(&json!("ab"), &short, "meta.s")
                .unwrap_err()
                .message,
            "meta.s must be at most 1 character long."
        );
    }

    #[test]
    fn values_are_sanitized_to_their_type() {
        assert_eq!(
            sanitize(&json!("7"), &schema(json!({"type": "integer"}))),
            json!(7)
        );
        assert_eq!(
            sanitize(&json!("1.5"), &schema(json!({"type": "number"}))),
            json!(1.5)
        );
        assert_eq!(
            sanitize(&json!("false"), &schema(json!({"type": "boolean"}))),
            json!(false)
        );
        assert_eq!(
            sanitize(&json!("0"), &schema(json!({"type": "boolean"}))),
            json!(false)
        );
        let union = schema(json!({"type": ["integer", "string"]}));
        assert_eq!(sanitize(&json!("12"), &union), json!(12));
        assert_eq!(sanitize(&json!("twelve"), &union), json!("twelve"));
    }

    #[test]
    fn stored_values_are_prepared_as_wordpress_prepares_them() {
        let int = schema(json!({"type": "integer"}));
        assert_eq!(prepare_value(json!(""), &int), json!(0));
        assert_eq!(prepare_value(json!("12"), &int), json!(12));
        assert_eq!(prepare_value(json!("twelve"), &int), Value::Null);
    }

    #[test]
    fn registration_follows_register_meta() {
        let keys = MetaKeys::new();
        assert_eq!(
            keys.register(MetaKey::post("list", MetaType::Array).show_in_rest()),
            Err(MetaKeyError::ArrayWithoutItems)
        );
        assert!(matches!(
            keys.register(MetaKey::post("n", MetaType::Integer).default(json!("x"))),
            Err(MetaKeyError::InvalidDefault(_))
        ));
        keys.register(MetaKey::post("hidden", MetaType::String))
            .unwrap();
        keys.register(MetaKey::post("a", MetaType::String).show_in_rest())
            .unwrap();
        keys.register(
            MetaKey::post("a", MetaType::Integer)
                .subtype("page")
                .single()
                .show_in_rest(),
        )
        .unwrap();
        let names = |subtype| -> Vec<(String, bool)> {
            keys.rest_fields(MetaObject::Post, subtype)
                .iter()
                .map(|f| (f.name().to_owned(), f.key.single))
                .collect()
        };
        assert_eq!(names("post"), [("a".to_owned(), false)]);
        assert_eq!(names("page"), [("a".to_owned(), true)]);
        assert!(keys.rest_fields(MetaObject::Term, "category").is_empty());
        assert!(keys.unregister(MetaObject::Post, Some("page"), "a"));
        assert_eq!(names("page"), [("a".to_owned(), false)]);
    }

    #[test]
    fn unset_keys_show_their_default() {
        let single = RestField::new(
            &MetaKey::post("n", MetaType::Integer)
                .single()
                .show_in_rest(),
        )
        .unwrap();
        assert_eq!(single.response_value(&[]), json!(0));
        assert_eq!(single.response_value(&[json!(3), json!(4)]), json!(3));
        let many = RestField::new(
            &MetaKey::post("s", MetaType::String)
                .default(json!("x"))
                .show_in_rest(),
        )
        .unwrap();
        assert_eq!(many.response_value(&[]), json!(["x"]));
        assert_eq!(
            many.response_value(&[json!("a"), json!(1)]),
            json!(["a", null])
        );
    }

    #[test]
    fn protected_keys_need_the_meta_capability() {
        let field =
            RestField::new(&MetaKey::post("_secret", MetaType::String).show_in_rest()).unwrap();
        let user = UserCaps::from_roles(1, &[], &Default::default());
        assert!(!field.allows(&user, 1, MetaAction::Edit, true));
        let open = RestField::new(
            &MetaKey::post("_secret", MetaType::String)
                .auth(MetaAuth::Allow)
                .show_in_rest(),
        )
        .unwrap();
        assert!(open.allows(&user, 1, MetaAction::Edit, true));
        assert!(!open.allows(&user, 1, MetaAction::Edit, false));
    }
}
