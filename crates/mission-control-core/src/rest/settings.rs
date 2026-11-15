//! The settings controller: `/wp/v2/settings`, as WordPress's `WP_REST_Settings_Controller`
//! serves it, over a registry of settings ([`Settings`]) as WordPress's `register_setting` keeps
//! them.
//!
//! [`Settings`] starts with the core settings WordPress shows in the REST API, each stored in the
//! option WordPress keeps it in: `title` (`blogname`), `description` (`blogdescription`), `url`
//! (`siteurl`), `email` (`admin_email`), `timezone` (`timezone_string`), `date_format`,
//! `time_format`, `start_of_week`, `language` (`WPLANG`), `use_smilies`, `default_category`,
//! `default_post_format`, `posts_per_page`, `show_on_front`, `page_on_front`, `page_for_posts`,
//! `default_ping_status`, `default_comment_status`, `site_logo` and `site_icon`. Code that
//! extends the site registers more with [`Settings::register`], then calls
//! [`Settings::register_routes`] so discovery lists them.
//!
//! As WordPress:
//!
//! - Only a user with `manage_options` may read or write settings; anyone else gets
//!   `rest_forbidden` (401 for a visitor, 403 for a user).
//! - `GET` answers every setting shown in the REST API: the option's value, or the setting's
//!   default when the option is not set, checked against the setting's schema; a value the
//!   schema refuses reads as `null`.
//! - `POST`, `PUT` and `PATCH` write the settings the body names (others are ignored), then
//!   answer as `GET`. Each value is checked against its schema first (its type, its `enum`, an
//!   email's format), and one bad value refuses the whole request with `rest_invalid_param`
//!   (400) before anything is written. `null` deletes the option, so the setting reads its
//!   default again; when the stored value is not valid either, that is
//!   `rest_invalid_stored_value` (500).
//! - Each value is sanitized as `sanitize_option` sanitizes its option: the title and tagline
//!   through `esc_html`; the date and time formats through `strip_tags` and `wp_kses_data`; the
//!   page, category, icon and week-day ids through `absint`; `posts_per_page` at least 1. A value
//!   `sanitize_option` refuses (a site address that is not `http://` or `https://`, an unknown
//!   timezone, a language that is not installed) keeps the stored value, as WordPress keeps it
//!   and reports the problem on the settings screen only.
//!
//! - A timezone is an IANA name (`chrono-tz`'s list, legacy names such as `US/Eastern` and `UTC`
//!   included), or empty. `UTC+N` and `UTC-N` with one of the offsets WordPress's timezone menu
//!   offers set `gmt_offset` to `N` and empty the timezone, as WordPress's settings screen saves
//!   them.
//! - A setting is shown in the REST API only when it asks for it (`show_in_rest`, or a REST
//!   name), as `register_setting` defaults `show_in_rest` to false; one that is not shown is
//!   neither listed nor written.
//! - `_fields` keeps only the settings named.
//!
//! Mission Control's differences: no language packs are installed, so `language` takes only the
//! empty value (English); an `array` or `object` setting is checked for its type only, not its
//! `items` or `properties`.

use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use orbit_orm::Database;
use serde_json::{Map, Number, Value, json};

use super::params::{Invalid, Params, as_boolean, as_integer, as_list, list_sentence, parse_form};
use super::shared::{Fields, authorization_status, database_error, option, read_envelope};
use super::{Endpoint, KsesLimiter, RestError, RestResponse, RestRoutes, RouteSpec};
use crate::content::{MetaError, delete_option, kses_data, set_option};
use crate::users::{UserCaps, can};

/// A setting's type, as `register_setting`'s `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingType {
    /// `string`.
    String,
    /// `integer`.
    Integer,
    /// `number`.
    Number,
    /// `boolean`.
    Boolean,
    /// `array`.
    Array,
    /// `object`.
    Object,
}

impl SettingType {
    /// The JSON Schema name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

/// A string setting's `format` in its REST schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingFormat {
    /// `email`: checked with `is_email`, sanitized with `sanitize_email`.
    Email,
    /// `uri`: sanitized with `esc_url_raw`.
    Uri,
}

impl SettingFormat {
    /// The JSON Schema name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Uri => "uri",
        }
    }
}

/// How an option's value is sanitized before it is stored, as WordPress's `sanitize_option` and
/// a setting's `sanitize_callback`: the value to store, or `None` to keep the stored value (as
/// WordPress keeps it when `sanitize_option` reports an error). It runs on a blocking thread.
pub type SanitizeOption = Arc<dyn Fn(Value) -> Option<Value> + Send + Sync>;

/// A registered setting: the option it is stored in and how the REST API shows it, as
/// `register_setting` with `show_in_rest`.
///
/// ```
/// use mission_control_core::rest::settings::{Setting, SettingType, Settings};
/// use serde_json::json;
///
/// let settings = Settings::new();
/// settings.register(
///     Setting::new("acme_greeting", SettingType::String)
///         .rest_name("greeting")
///         .description("The greeting on the home page.")
///         .default(json!("Hello")),
/// );
/// assert!(settings.shown_in_rest().iter().any(|s| s.rest_name == "greeting"));
///
/// // Without `show_in_rest` (or a REST name), a setting stays out of the REST API.
/// settings.register(Setting::new("acme_secret", SettingType::String));
/// assert!(!settings.shown_in_rest().iter().any(|s| s.option_name == "acme_secret"));
/// ```
#[derive(Clone)]
pub struct Setting {
    /// The option the value is stored in, such as `blogname`.
    pub option_name: String,
    /// The setting's name in the REST API, such as `title`.
    pub rest_name: String,
    /// The value's type.
    pub kind: SettingType,
    /// The label (`title` in the schema).
    pub title: String,
    /// The description.
    pub description: String,
    /// The value read when the option is not set; `None` reads as `null`.
    pub default: Option<Value>,
    /// A string's format.
    pub format: Option<SettingFormat>,
    /// The values a string may take (`enum`); empty for any.
    pub choices: Vec<String>,
    /// Whether the REST API shows the setting.
    pub show_in_rest: bool,
    /// How the value is sanitized before it is stored.
    pub sanitize: Option<SanitizeOption>,
}

impl fmt::Debug for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Setting")
            .field("option_name", &self.option_name)
            .field("rest_name", &self.rest_name)
            .field("kind", &self.kind)
            .field("default", &self.default)
            .field("format", &self.format)
            .field("choices", &self.choices)
            .field("show_in_rest", &self.show_in_rest)
            .field("sanitize", &self.sanitize.is_some())
            .finish_non_exhaustive()
    }
}

impl Setting {
    /// A setting stored in option `option_name`, of `kind`, with no label, description, default
    /// or sanitizing. As `register_setting`, it is not shown in the REST API until
    /// [`show_in_rest`](Self::show_in_rest) or [`rest_name`](Self::rest_name) asks for it; it is
    /// then shown under the option's name unless `rest_name` gives another.
    pub fn new(option_name: impl Into<String>, kind: SettingType) -> Self {
        let option_name = option_name.into();
        Self {
            rest_name: option_name.clone(),
            option_name,
            kind,
            title: String::new(),
            description: String::new(),
            default: None,
            format: None,
            choices: Vec::new(),
            show_in_rest: false,
            sanitize: None,
        }
    }

    /// The setting shown in the REST API (`show_in_rest => true`).
    pub fn show_in_rest(mut self) -> Self {
        self.show_in_rest = true;
        self
    }

    /// The setting shown in the REST API under `name` (`show_in_rest`'s `name`).
    pub fn rest_name(mut self, name: impl Into<String>) -> Self {
        self.rest_name = name.into();
        self.show_in_rest = true;
        self
    }

    /// The setting with label `title` (`register_setting`'s `label`).
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// The setting with `description`.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// The setting with `default`, read when the option is not set.
    pub fn default(mut self, default: Value) -> Self {
        self.default = Some(default);
        self
    }

    /// The string setting with `format`.
    pub fn format(mut self, format: SettingFormat) -> Self {
        self.format = Some(format);
        self
    }

    /// The string setting limited to `choices` (`enum`).
    pub fn choices<I, S>(mut self, choices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.choices = choices.into_iter().map(Into::into).collect();
        self
    }

    /// The setting sanitized by `sanitize` before it is stored.
    pub fn sanitize<F>(mut self, sanitize: F) -> Self
    where
        F: Fn(Value) -> Option<Value> + Send + Sync + 'static,
    {
        self.sanitize = Some(Arc::new(sanitize));
        self
    }

    /// The argument the route takes for this setting, as discovery lists it.
    fn arg(&self) -> Value {
        let mut arg = Map::new();
        arg.insert("title".into(), json!(self.title));
        arg.insert("description".into(), json!(self.description));
        arg.insert("type".into(), json!(self.kind.as_str()));
        if let Some(format) = self.format {
            arg.insert("format".into(), json!(format.as_str()));
        }
        if !self.choices.is_empty() {
            arg.insert("enum".into(), json!(self.choices));
        }
        // No `default`: WordPress lists one only for a creatable endpoint, and this one edits.
        arg.insert("required".into(), json!(false));
        Value::Object(arg)
    }

    /// `value` checked against the setting's schema and sanitized by it, as
    /// `rest_validate_value_from_schema` then `rest_sanitize_value_from_schema`: the error's code
    /// and message when it is not valid.
    fn check(&self, value: &Value) -> Result<Value, (&'static str, String)> {
        let name = &self.rest_name;
        let wrong = || {
            (
                "rest_invalid_type",
                format!("{name} is not of type {}.", self.kind.as_str()),
            )
        };
        let value = match self.kind {
            SettingType::String => match value {
                Value::String(s) => Value::String(s.clone()),
                _ => return Err(wrong()),
            },
            SettingType::Integer => Value::from(as_integer(value).ok_or_else(wrong)?),
            SettingType::Number => {
                let n = match value {
                    Value::Number(n) => n.as_f64(),
                    Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
                    _ => None,
                }
                .ok_or_else(wrong)?;
                Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            SettingType::Boolean => Value::Bool(as_boolean(value).ok_or_else(wrong)?),
            SettingType::Array => match value {
                Value::Array(_) | Value::String(_) => Value::Array(as_list(value)),
                _ => return Err(wrong()),
            },
            SettingType::Object => match value {
                Value::Object(_) => value.clone(),
                _ => return Err(wrong()),
            },
        };
        if let Value::String(text) = &value {
            if !self.choices.is_empty() && !self.choices.contains(text) {
                let choices: Vec<&str> = self.choices.iter().map(String::as_str).collect();
                return Err((
                    "rest_not_in_enum",
                    format!("{name} is not one of {}.", list_sentence(&choices)),
                ));
            }
            match self.format {
                Some(SettingFormat::Email) if !is_email(text) => {
                    return Err(("rest_invalid_email", "Invalid email address.".to_owned()));
                }
                Some(SettingFormat::Email) => return Ok(Value::String(text.trim().to_owned())),
                Some(SettingFormat::Uri) => return Ok(Value::String(esc_url_raw(text))),
                None => {}
            }
        }
        Ok(value)
    }

    /// The stored `value` as WordPress's `get_option` gives it: options are text in WordPress,
    /// so a number or boolean stored for a string setting reads as its text.
    fn as_stored(&self, value: Value) -> Value {
        match (self.kind, value) {
            (SettingType::String, Value::Number(n)) => Value::String(n.to_string()),
            (SettingType::String, Value::Bool(b)) => {
                Value::String(if b { "1" } else { "" }.to_owned())
            }
            (_, value) => value,
        }
    }
}

/// The settings registry, shared by the app and the code that registers settings. Starts with
/// WordPress's core settings shown in the REST API.
#[derive(Debug)]
pub struct Settings {
    settings: RwLock<Vec<Setting>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    /// The registry with WordPress's core settings, as `register_initial_settings` registers
    /// them.
    pub fn new() -> Self {
        Self {
            settings: RwLock::new(core_settings()),
        }
    }

    /// Adds `setting`, replacing one stored in the same option. Call
    /// [`register_routes`](Self::register_routes) afterwards so discovery lists it.
    pub fn register(&self, setting: Setting) {
        let mut settings = self.settings.write().unwrap_or_else(|e| e.into_inner());
        settings.retain(|s| s.option_name != setting.option_name);
        settings.push(setting);
    }

    /// The settings shown in the REST API, in the order registered.
    pub fn shown_in_rest(&self) -> Vec<Setting> {
        self.read()
            .iter()
            .filter(|s| s.show_in_rest)
            .cloned()
            .collect()
    }

    /// Registers `/wp/v2/settings` on `routes`, so discovery lists it: `GET` with no arguments,
    /// and `POST`, `PUT` and `PATCH` with one argument per setting.
    pub fn register_routes(&self, routes: &RestRoutes) {
        let args: Map<String, Value> = self
            .shown_in_rest()
            .iter()
            .map(|s| (s.rest_name.clone(), s.arg()))
            .collect();
        routes.register(RouteSpec::new(
            "/wp/v2/settings",
            vec![
                Endpoint::new(["GET"], json!({})),
                Endpoint::new(["POST", "PUT", "PATCH"], Value::Object(args)),
            ],
        ));
    }

    // A poisoned lock only means a registering thread panicked; the list is still whole.
    fn read(&self) -> RwLockReadGuard<'_, Vec<Setting>> {
        self.settings.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// WordPress's core settings, in the order `register_initial_settings` registers them.
fn core_settings() -> Vec<Setting> {
    use SettingType::{Boolean, Integer, String as Text};
    let absint = |value: Value| as_integer(&value).map(|n| Value::from(n.unsigned_abs()));
    vec![
        Setting::new("blogname", Text)
            .rest_name("title")
            .title("Title")
            .description("Site title.")
            .sanitize(|value| text(&value).map(|t| Value::String(esc_html(&t)))),
        Setting::new("blogdescription", Text)
            .rest_name("description")
            .title("Tagline")
            .description("Site tagline.")
            .sanitize(|value| text(&value).map(|t| Value::String(esc_html(&t)))),
        Setting::new("siteurl", Text)
            .rest_name("url")
            .description("Site URL.")
            .format(SettingFormat::Uri)
            .sanitize(|value| {
                let url = text(&value)?;
                is_http_url(&url).then(|| Value::String(esc_url_raw(&url)))
            }),
        Setting::new("admin_email", Text)
            .rest_name("email")
            .description(
                "This address is used for admin purposes, like new user notification.",
            )
            .format(SettingFormat::Email)
            .sanitize(|value| {
                let email = text(&value)?;
                is_email(&email).then(|| Value::String(email.trim().to_owned()))
            }),
        Setting::new("timezone_string", Text)
            .rest_name("timezone")
            .description("A city in the same timezone as you.")
            .sanitize(|value| {
                let zone = text(&value)?;
                (zone.is_empty() || is_timezone(&zone)).then_some(Value::String(zone))
            }),
        Setting::new("date_format", Text)
            .show_in_rest()
            .description("A date format for all date strings.")
            .sanitize(|value| text(&value).map(|t| Value::String(kses_data(&strip_tags(&t))))),
        Setting::new("time_format", Text)
            .show_in_rest()
            .description("A time format for all time strings.")
            .sanitize(|value| text(&value).map(|t| Value::String(kses_data(&strip_tags(&t))))),
        Setting::new("start_of_week", Integer)
            .show_in_rest()
            .description("A day number of the week that the week should start on.")
            .sanitize(absint),
        Setting::new("WPLANG", Text)
            .rest_name("language")
            .description("WordPress locale code.")
            .default(json!("en_US"))
            .sanitize(|value| {
                // No language packs are installed: only English, the empty value.
                text(&value).filter(|t| t.is_empty()).map(Value::String)
            }),
        Setting::new("use_smilies", Boolean)
            .show_in_rest()
            .description("Convert emoticons like :-) and :-P to graphics on display.")
            .default(json!(true)),
        Setting::new("default_category", Integer)
            .show_in_rest()
            .description("Default post category.")
            .sanitize(absint),
        Setting::new("default_post_format", Text)
            .show_in_rest().description("Default post format."),
        Setting::new("posts_per_page", Integer)
            .show_in_rest()
            .title("Maximum posts per page")
            .description("Blog pages show at most.")
            .default(json!(10))
            .sanitize(|value| {
                let n = as_integer(&value)?;
                Some(Value::from(match n {
                    0 => 1,
                    n if n < -1 => n.saturating_abs(),
                    n => n,
                }))
            }),
        Setting::new("show_on_front", Text)
            .show_in_rest()
            .title("Show on front")
            .description("What to show on the front page"),
        Setting::new("page_on_front", Integer)
            .show_in_rest()
            .title("Page on front")
            .description("The ID of the page that should be displayed on the front page")
            .sanitize(absint),
        Setting::new("page_for_posts", Integer)
            .show_in_rest()
            .description("The ID of the page that should display the latest posts")
            .sanitize(absint),
        Setting::new("default_ping_status", Text)
            .show_in_rest()
            .description(
                "Allow link notifications from other blogs (pingbacks and trackbacks) on new articles.",
            )
            .choices(["open", "closed"]),
        Setting::new("default_comment_status", Text)
            .show_in_rest()
            .title("Allow comments on new posts")
            .description("Allow people to submit comments on new posts.")
            .choices(["open", "closed"]),
        Setting::new("site_logo", Integer)
            .show_in_rest()
            .title("Logo")
            .description("Site logo."),
        Setting::new("site_icon", Integer)
            .show_in_rest()
            .title("Icon")
            .description("Site icon.")
            .sanitize(absint),
    ]
}

/// One request to the settings controller.
///
/// ```ignore
/// let controller = SettingsController {
///     db: &db,
///     settings: &settings,
///     user: &caps,
///     kses: &kses,
/// };
/// let response = controller.get().await?;
/// ```
pub struct SettingsController<'a> {
    /// The database.
    pub db: &'a Database,
    /// The registered settings.
    pub settings: &'a Settings,
    /// The current user's capabilities: a visitor's (id 0, no role) when nobody signed in.
    pub user: &'a UserCaps,
    /// Where the settings' sanitizers run.
    pub kses: &'a KsesLimiter,
}

impl SettingsController<'_> {
    /// `GET /wp/v2/settings`: every setting shown in the REST API, or those `_fields` names.
    /// `query` is the request's query string.
    ///
    /// # Errors
    ///
    /// `rest_forbidden` without `manage_options`.
    pub async fn get(&self, query: &str) -> Result<RestResponse, RestError> {
        self.check_permission()?;
        let (fields, _) = read_envelope(&parse_form(query));
        self.read_all(&fields).await
    }

    /// `POST`, `PUT` or `PATCH /wp/v2/settings`: writes the settings `params` names, then
    /// answers as [`get`](Self::get) (with the `_fields` in `params`).
    ///
    /// # Errors
    ///
    /// `rest_invalid_param` (400) for a value its schema refuses, `rest_forbidden` without
    /// `manage_options`, and `rest_invalid_stored_value` (500) for `null` over a stored value
    /// that is not valid.
    pub async fn update(&self, params: &Params) -> Result<RestResponse, RestError> {
        let settings = self.settings.shown_in_rest();
        // As `WP_REST_Server`: the arguments are checked before the permission.
        let mut invalid = Invalid::default();
        let mut writes: Vec<(Setting, Option<Value>)> = Vec::new();
        // Options written beside the settings: `gmt_offset` for a `UTC+N` timezone.
        let mut extra: Vec<(&str, Value)> = Vec::new();
        for setting in settings {
            let Some(value) = params.get(&setting.rest_name) else {
                continue;
            };
            if value.is_null() {
                writes.push((setting, None));
                continue;
            }
            match setting.check(value) {
                // As WordPress's settings screen: `UTC+N` is no timezone but an offset, so it
                // sets `gmt_offset` and empties `timezone_string`.
                Ok(Value::String(zone))
                    if setting.option_name == "timezone_string" && utc_offset(&zone).is_some() =>
                {
                    let offset = utc_offset(&zone).unwrap_or_default();
                    extra.push(("gmt_offset", Value::String(offset)));
                    writes.push((setting, Some(Value::String(String::new()))));
                }
                Ok(value) => writes.push((setting, Some(value))),
                Err((code, message)) => {
                    let param = setting.rest_name.clone();
                    invalid.add(&param, code, message, json!({"param": param}));
                }
            }
        }
        invalid.finish()?;
        self.check_permission()?;
        // `sanitize_option` and the settings' own sanitizers, all on one blocking thread.
        let writes = self
            .kses
            .run(move || {
                writes
                    .into_iter()
                    .map(|(setting, value)| {
                        let value = value.map(|v| match &setting.sanitize {
                            Some(sanitize) => sanitize(v),
                            None => Some(v),
                        });
                        (setting, value)
                    })
                    .collect::<Vec<_>>()
            })
            .await?;
        for (setting, value) in writes {
            match value {
                // `null`: back to the default, unless the stored value cannot be read either.
                None => {
                    let stored = option(self.db, &setting.option_name)
                        .await?
                        .unwrap_or(Value::Bool(false));
                    if setting.check(&setting.as_stored(stored)).is_err() {
                        return Err(RestError::new(
                            "rest_invalid_stored_value",
                            format!(
                                "The {} property has an invalid stored value, and cannot be updated to null.",
                                setting.rest_name
                            ),
                            500,
                        ));
                    }
                    delete_option(self.db, &setting.option_name)
                        .await
                        .map_err(|e| meta_error(&e))?;
                }
                // `sanitize_option` refused the value: the stored one stays.
                Some(None) => {}
                Some(Some(value)) => set_option(self.db, &setting.option_name, &value)
                    .await
                    .map_err(|e| meta_error(&e))?,
            }
        }
        for (name, value) in extra {
            set_option(self.db, name, &value)
                .await
                .map_err(|e| meta_error(&e))?;
        }
        self.read_all(&read_envelope(params).0).await
    }

    /// Every setting's value, as `get_item` reads them.
    async fn read_all(&self, fields: &Fields) -> Result<RestResponse, RestError> {
        let mut data = Map::new();
        for setting in self.settings.shown_in_rest() {
            let stored = option(self.db, &setting.option_name).await?;
            let value = stored
                .map(|v| setting.as_stored(v))
                .or_else(|| setting.default.clone())
                .unwrap_or(Value::Null);
            // `prepare_value`: a value the schema refuses reads as `null`.
            let value = setting.check(&value).unwrap_or(Value::Null);
            data.insert(setting.rest_name.clone(), value);
        }
        Ok(RestResponse::ok(Value::Object(fields.filter(data))))
    }

    /// `manage_options`, or `rest_forbidden`.
    fn check_permission(&self) -> Result<(), RestError> {
        if can(self.user, "manage_options", None) {
            Ok(())
        } else {
            Err(RestError::new(
                "rest_forbidden",
                "Sorry, you are not allowed to do that.",
                authorization_status(self.user),
            ))
        }
    }
}

/// A database error from the options, as WordPress's 500.
fn meta_error(error: &MetaError) -> RestError {
    database_error(error)
}

/// A string value's text.
fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// `esc_html`: `&`, `<`, `>`, `"` and `'` escaped, without encoding an entity again (`&amp;`
/// stays `&amp;`), so a value read and saved back does not change.
fn esc_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        match c {
            '&' if starts_entity(&text[i..]) => out.push('&'),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#039;"),
            _ => out.push(c),
        }
    }
    out
}

/// Whether `text` starts with an entity: `&name;`, `&#123;` or `&#x1F;`.
fn starts_entity(text: &str) -> bool {
    let Some(end) = text.get(1..).and_then(|rest| rest.find(';')) else {
        return false;
    };
    let body = &text[1..=end];
    if let Some(number) = body.strip_prefix('#') {
        match number.strip_prefix(['x', 'X']) {
            Some(hex) => {
                !hex.is_empty() && hex.len() <= 6 && hex.chars().all(|c| c.is_ascii_hexdigit())
            }
            None => {
                !number.is_empty()
                    && number.len() <= 7
                    && number.chars().all(|c| c.is_ascii_digit())
            }
        }
    } else {
        body.len() <= 32
            && body.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && body.chars().all(|c| c.is_ascii_alphanumeric())
    }
}

/// PHP's `strip_tags`, as `sanitize_option` runs it on the date and time formats.
fn strip_tags(text: &str) -> String {
    crate::text::strip_all_tags(text)
}

/// The protocols `esc_url` keeps, as `wp_allowed_protocols` lists them.
const ALLOWED_PROTOCOLS: &[&str] = &[
    "http", "https", "ftp", "ftps", "mailto", "news", "irc", "irc6", "ircs", "gopher", "nntp",
    "feed", "telnet", "mms", "rtsp", "sms", "svn", "tel", "fax", "xmpp", "webcal", "urn",
];

/// `esc_url_raw`: spaces encoded, characters a URL cannot hold removed, `http://` added to an
/// address with no scheme, and empty for a protocol that is not allowed.
fn esc_url_raw(url: &str) -> String {
    let url = url.trim().replace(' ', "%20");
    let mut url: String = url
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || !c.is_ascii() || "-~+_.?#=!&;,/:%@$|*'()[]".contains(*c)
        })
        .collect();
    for encoded in ["%0d", "%0a", "%0D", "%0A"] {
        while url.contains(encoded) {
            url = url.replace(encoded, "");
        }
    }
    if url.is_empty() {
        return url;
    }
    let looks_like_php = {
        let name: String = url
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        !name.is_empty() && url[name.len()..].to_ascii_lowercase().starts_with(".php")
    };
    if !url.contains(':') && !url.starts_with(['/', '#', '?']) && !looks_like_php {
        url = format!("http://{url}");
    }
    if let Some((scheme, _)) = url.split_once(':') {
        let scheme_like = !scheme.is_empty()
            && !scheme.contains(['/', '?', '#'])
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        if scheme_like && !ALLOWED_PROTOCOLS.contains(&scheme.to_ascii_lowercase().as_str()) {
            return String::new();
        }
    }
    url
}

/// Whether `url` holds `http://` or `https://` and an address after it, as `sanitize_option`
/// checks `siteurl` and `home`.
fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ["http://", "https://"].iter().any(|scheme| {
        lower
            .find(scheme)
            .is_some_and(|at| at + scheme.len() < lower.len())
    })
}

/// WordPress's `is_email`: a local part of the allowed characters, an `@`, and a domain of at
/// least two dot-separated labels of letters, digits and inner hyphens.
fn is_email(email: &str) -> bool {
    if email.len() < 6 {
        return false;
    }
    let Some(at) = email.find('@').filter(|at| *at > 0) else {
        return false;
    };
    let (local, domain) = (&email[..at], &email[at + 1..]);
    let local_ok = local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c));
    if !local_ok || domain.contains("..") {
        return false;
    }
    let domain = domain.trim_matches(|c: char| c == '.' || c.is_whitespace() || c == '\0');
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            let label = label.trim_matches(|c: char| c == '-' || c.is_whitespace());
            !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Whether `zone` is a timezone identifier PHP's `timezone_identifiers_list( ALL_WITH_BC )`
/// lists: an IANA name, such as `America/Toronto`, `UTC` or a legacy `US/Eastern`.
fn is_timezone(zone: &str) -> bool {
    zone.parse::<chrono_tz::Tz>().is_ok()
}

/// The offsets WordPress's timezone menu offers as `UTC+N` and `UTC-N`, in hours.
fn is_menu_offset(hours: f64) -> bool {
    (-12.0..=14.0).contains(&hours)
        && ((hours * 2.0).fract() == 0.0 || [5.75, 8.75, 12.75, 13.75].contains(&hours))
}

/// The `gmt_offset` a `UTC+N` or `UTC-N` timezone choice sets, as WordPress's settings screen
/// (`options-general.php`) turns it into one: `UTC+5.5` is `5.5`, `UTC-3` is `-3`. `None` for
/// any other value.
fn utc_offset(zone: &str) -> Option<String> {
    let rest = zone.strip_prefix("UTC")?;
    let (negative, number) = match rest.as_bytes().first()? {
        b'+' => (false, &rest[1..]),
        b'-' => (true, &rest[1..]),
        _ => return None,
    };
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let hours: f64 = number.parse().ok()?;
    let hours = if negative { -hours } else { hours };
    is_menu_offset(hours).then(|| format!("{}{number}", if negative { "-" } else { "" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_settings_are_wordpresss_in_order() {
        let names: Vec<String> = Settings::new()
            .shown_in_rest()
            .into_iter()
            .map(|s| s.rest_name)
            .collect();
        assert_eq!(
            names,
            [
                "title",
                "description",
                "url",
                "email",
                "timezone",
                "date_format",
                "time_format",
                "start_of_week",
                "language",
                "use_smilies",
                "default_category",
                "default_post_format",
                "posts_per_page",
                "show_on_front",
                "page_on_front",
                "page_for_posts",
                "default_ping_status",
                "default_comment_status",
                "site_logo",
                "site_icon",
            ]
        );
    }

    #[test]
    fn values_are_checked_against_the_schema() {
        let integer = Setting::new("posts_per_page", SettingType::Integer);
        assert_eq!(integer.check(&json!("12")), Ok(json!(12)));
        assert_eq!(
            integer.check(&json!("ten")).unwrap_err().0,
            "rest_invalid_type"
        );
        let text = Setting::new("blogname", SettingType::String).rest_name("title");
        assert_eq!(
            text.check(&json!(5)).unwrap_err().1,
            "title is not of type string."
        );
        let status =
            Setting::new("default_ping_status", SettingType::String).choices(["open", "closed"]);
        assert_eq!(
            status.check(&json!("maybe")).unwrap_err().1,
            "default_ping_status is not one of open and closed."
        );
        let email = Setting::new("admin_email", SettingType::String).format(SettingFormat::Email);
        assert_eq!(
            email.check(&json!("nope")).unwrap_err().0,
            "rest_invalid_email"
        );
        assert_eq!(
            email.check(&json!("me@example.com")),
            Ok(json!("me@example.com"))
        );
        let flag = Setting::new("use_smilies", SettingType::Boolean);
        assert_eq!(flag.check(&json!("0")), Ok(json!(false)));
    }

    #[test]
    fn titles_are_escaped_once() {
        assert_eq!(esc_html("Tom & Jerry <b>"), "Tom &amp; Jerry &lt;b&gt;");
        assert_eq!(
            esc_html("Tom &amp; Jerry &#8217;"),
            "Tom &amp; Jerry &#8217;"
        );
        assert_eq!(esc_html("a &b c"), "a &amp;b c");
    }

    #[test]
    fn urls_are_cleaned_as_esc_url_raw_cleans_them() {
        assert_eq!(esc_url_raw(" example.com/a b "), "http://example.com/a%20b");
        assert_eq!(esc_url_raw("https://x.test/<p>"), "https://x.test/p");
        assert_eq!(esc_url_raw("javascript:alert(1)"), "");
        assert!(is_http_url("https://x.test"));
        assert!(!is_http_url("ftp://x.test"));
    }

    #[test]
    fn emails_and_timezones_are_checked() {
        assert!(is_email("admin@example.com"));
        assert!(!is_email("admin@localhost"));
        assert!(!is_email("@example.com"));
        assert!(is_timezone("America/Toronto"));
        assert!(is_timezone("America/Argentina/Buenos_Aires"));
        assert!(is_timezone("UTC"));
        assert!(is_timezone("US/Eastern"));
        assert!(!is_timezone("Mars/Olympus"));
        assert!(!is_timezone("Europe/Narnia"));
        assert!(!is_timezone("UTC+5"));
        assert_eq!(utc_offset("UTC+5.5").as_deref(), Some("5.5"));
        assert_eq!(utc_offset("UTC-3").as_deref(), Some("-3"));
        assert_eq!(utc_offset("UTC+13.75").as_deref(), Some("13.75"));
        assert_eq!(utc_offset("UTC+5.3"), None);
        assert_eq!(utc_offset("UTC+15"), None);
        assert_eq!(utc_offset("UTC5"), None);
    }
}
