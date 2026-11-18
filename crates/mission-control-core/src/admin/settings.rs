//! The admin's settings screens (Task 6.5, design 5.10, Ruling R132): WordPress's Settings menu
//! (General, Writing, Reading, Discussion, Media, Permalinks and Privacy), each an Ignite runtime
//! resource holding one record (id [`SETTINGS_RECORD`]) whose fields are WordPress's options.
//! Nothing here needs Rocket: the `mission-control` crate registers the resources, answers each
//! list address with a redirect to the record's edit form, refuses New and Delete, and checks
//! [`authorize_settings_screen`] before Ignite's routes run.
//!
//! # Saving
//!
//! A save is WordPress's `options.php`: each option the form sent goes through
//! [`sanitize_option`], the rules the REST settings route also uses, and is stored in `options`.
//! Every option is sanitized before any is written. Ruling R135 (interim for owner item O10):
//! when `sanitize_option` refuses one, nothing is written: [`save_settings`] returns the errors
//! by field, and the data layer returns an error, so Ignite shows its error page rather than
//! "Changes saved." (WordPress keeps the stored value and shows the message as a notice; Ignite
//! cannot show it yet). Ignite's own checks come first and show on the form: a required value,
//! a whole number, an email address, a URL, one of a select's or a lookup's choices. The
//! title and tagline are stored escaped (`esc_html`) and shown decoded on the form and the
//! site, as WordPress's `esc_attr` and `esc_html` do not encode an entity twice. The Reading
//! screen keeps the post counts between 1 and 1000 (Ruling R136).
//!
//! The screens' own steps, as WordPress's screens take them:
//!
//! - General (`options-general.php`): a `UTC+N` or `UTC-N` time zone sets `gmt_offset` and
//!   empties `timezone_string`, as `options.php` does. A named time zone also sets `gmt_offset`
//!   to its current offset, because Mission Control's dates read `gmt_offset` only (WordPress
//!   computes it from the zone on every read; here a daylight saving change is followed when the
//!   screen is saved again). The date and time formats are WordPress's choices or a custom
//!   format (`date_format_custom`).
//! - Reading: "Discourage search engines" stores `blog_public` 0, as WordPress inverts it.
//! - Discussion: "Allow link notifications" and "Allow people to submit comments" store `open`
//!   or `closed`.
//! - Media: the sizes are stored and the image size registry ([`ImageSizes`]) takes them at
//!   once, so the next upload is resized to them ([`apply_media_sizes`] does the same at start).
//! - Permalinks (`options-permalink.php`): one of WordPress's common structures, or a custom one,
//!   which gets a leading `/` and loses `#` and repeated slashes; the category and tag bases
//!   likewise. The front end reads the structure on every request, so the next request uses it.
//! - Privacy (`options-privacy.php`): the privacy policy page.
//!
//! # Left out (phase 1)
//!
//! Options of features Mission Control does not have yet are not on the screens: General's
//! Membership (`users_can_register`, `default_role`) and Site Language (`WPLANG`; no language
//! packs); Writing's Default Post Format (no post formats), Post via email (`mailserver_*`,
//! `default_email_category`) and Update Services (`ping_sites`); Reading's page for a static
//! front page shows the pages, but no "Add New Page" link; Discussion's avatar previews (the
//! option values are offered as text); Media's "Organize my uploads into month- and year-based
//! folders" (uploads are always in `YYYY/MM`); Permalinks' optional bases for custom post types;
//! Privacy's "Create a new Privacy Policy page". The new admin email is stored at once: WordPress
//! mails a confirmation link first (`new_admin_email`), and Mission Control sends no mail yet.

use std::collections::BTreeMap;
use std::sync::Arc;

use ignite_core::{
    ColumnDef, FieldDef, FieldKindDef, ListQuery, Page, Row, RuntimeData, RuntimeError,
    RuntimeFuture, RuntimeResource,
};
use orbit_orm::{Database, Error, Value as Sql, raw};
use serde_json::{Value, json};

use super::posts::{PostScreen, Refusal};
use crate::content::{
    MetaError, OptionError, PostStatus, get_option, is_timezone, sanitize_option, set_option,
    utc_offset,
};
use crate::front::MAX_POSTS_PER_PAGE;
use crate::hooks::Owner;
use crate::media::ImageSizes;
use crate::rest::KsesLimiter;
use crate::types::MAX_OPTIONS;
use crate::users::{UserCaps, can};

/// The id of the one record a settings screen edits: `<mount>/<screen>/1/edit`.
pub const SETTINGS_RECORD: i64 = 1;

/// The errors of a save, by form field: WordPress's `add_settings_error` messages.
pub type SettingsErrors = BTreeMap<String, String>;

/// One of WordPress's settings screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsScreen {
    /// `options-general.php`.
    General,
    /// `options-writing.php`.
    Writing,
    /// `options-reading.php`.
    Reading,
    /// `options-discussion.php`.
    Discussion,
    /// `options-media.php`.
    Media,
    /// `options-permalink.php`.
    Permalink,
    /// `options-privacy.php`.
    Privacy,
}

impl SettingsScreen {
    /// Every screen, in WordPress's Settings menu order.
    pub const ALL: [Self; 7] = [
        Self::General,
        Self::Writing,
        Self::Reading,
        Self::Discussion,
        Self::Media,
        Self::Permalink,
        Self::Privacy,
    ];

    /// The resource's slug, WordPress's file name without `.php`: `options-general`.
    pub const fn slug(self) -> &'static str {
        match self {
            Self::General => "options-general",
            Self::Writing => "options-writing",
            Self::Reading => "options-reading",
            Self::Discussion => "options-discussion",
            Self::Media => "options-media",
            Self::Permalink => "options-permalink",
            Self::Privacy => "options-privacy",
        }
    }

    /// The screen whose slug is `slug`.
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.slug() == slug)
    }

    /// The screen's title, as WordPress's heading.
    pub const fn title(self) -> &'static str {
        match self {
            Self::General => "General Settings",
            Self::Writing => "Writing Settings",
            Self::Reading => "Reading Settings",
            Self::Discussion => "Discussion Settings",
            Self::Media => "Media Settings",
            Self::Permalink => "Permalink Settings",
            Self::Privacy => "Privacy",
        }
    }

    /// The capability the screen needs: `manage_options`, or `manage_privacy_options` for
    /// Privacy.
    pub const fn capability(self) -> &'static str {
        match self {
            Self::Privacy => "manage_privacy_options",
            _ => "manage_options",
        }
    }

    /// The options the screen stores.
    pub fn options(self) -> &'static [&'static str] {
        match self {
            Self::General => &[
                "blogname",
                "blogdescription",
                "siteurl",
                "home",
                "admin_email",
                "timezone_string",
                "gmt_offset",
                "date_format",
                "time_format",
                "start_of_week",
            ],
            Self::Writing => &["default_category"],
            Self::Reading => &[
                "show_on_front",
                "page_on_front",
                "page_for_posts",
                "posts_per_page",
                "posts_per_rss",
                "rss_use_excerpt",
                "blog_public",
            ],
            Self::Discussion => DISCUSSION_OPTIONS,
            Self::Media => &[
                "thumbnail_size_w",
                "thumbnail_size_h",
                "thumbnail_crop",
                "medium_size_w",
                "medium_size_h",
                "large_size_w",
                "large_size_h",
            ],
            Self::Permalink => &["permalink_structure", "category_base", "tag_base"],
            Self::Privacy => &["wp_page_for_privacy_policy"],
        }
    }
}

/// Why `user` may not make the request `request` on the settings screen `screen`, or `None`
/// when they may: the screen needs its [capability](SettingsScreen::capability), and only the
/// record's list address (answered with a redirect), its edit form and its save exist; New,
/// Ignite's create and Delete are WordPress's unknown admin page.
pub fn authorize_settings_screen(
    user: &UserCaps,
    screen: SettingsScreen,
    request: PostScreen,
) -> Option<Refusal> {
    if !can(user, screen.capability(), None) {
        return Some(match screen {
            SettingsScreen::Privacy => Refusal::ManagePrivacyOptions,
            _ => Refusal::ManageOptions,
        });
    }
    match request {
        PostScreen::New | PostScreen::Create | PostScreen::Delete(_) => Some(Refusal::NoSuchPage),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The screens' fields

/// WordPress's date formats on the General screen.
const DATE_FORMATS: [&str; 4] = ["F j, Y", "Y-m-d", "m/d/Y", "d/m/Y"];

/// WordPress's time formats on the General screen.
const TIME_FORMATS: [&str; 3] = ["g:i a", "g:i A", "H:i"];

/// The radio value of a custom date or time format.
const CUSTOM: &str = "custom";

/// The Permalinks screen's common settings: (choice, structure, label).
const PERMALINK_CHOICES: [(&str, &str, &str); 5] = [
    ("plain", "", "Plain (/?p=123)"),
    (
        "day",
        "/%year%/%monthnum%/%day%/%postname%/",
        "Day and name (/%year%/%monthnum%/%day%/%postname%/)",
    ),
    (
        "month",
        "/%year%/%monthnum%/%postname%/",
        "Month and name (/%year%/%monthnum%/%postname%/)",
    ),
    (
        "numeric",
        "/archives/%post_id%",
        "Numeric (/archives/%post_id%)",
    ),
    ("postname", "/%postname%/", "Post name (/%postname%/)"),
];

/// The Discussion screen's options, in its order.
const DISCUSSION_OPTIONS: &[&str] = &[
    "default_pingback_flag",
    "default_ping_status",
    "default_comment_status",
    "require_name_email",
    "comment_registration",
    "close_comments_for_old_posts",
    "close_comments_days_old",
    "show_comments_cookies_opt_in",
    "thread_comments",
    "thread_comments_depth",
    "page_comments",
    "comments_per_page",
    "default_comments_page",
    "comment_order",
    "comments_notify",
    "moderation_notify",
    "comment_moderation",
    "comment_previously_approved",
    "comment_max_links",
    "moderation_keys",
    "disallowed_keys",
    "show_avatars",
    "avatar_rating",
    "avatar_default",
];

/// The Discussion options shown as a checkbox, with WordPress's default (`populate_options`).
const DISCUSSION_FLAGS: &[(&str, bool)] = &[
    ("default_pingback_flag", true),
    ("require_name_email", true),
    ("comment_registration", false),
    ("close_comments_for_old_posts", false),
    ("show_comments_cookies_opt_in", true),
    ("thread_comments", true),
    ("page_comments", false),
    ("comments_notify", true),
    ("moderation_notify", true),
    ("comment_moderation", false),
    ("comment_previously_approved", true),
    ("show_avatars", true),
];

/// The Discussion options shown as a whole number, with WordPress's default.
const DISCUSSION_NUMBERS: &[(&str, i64)] = &[
    ("close_comments_days_old", 14),
    ("comments_per_page", 50),
    ("comment_max_links", 2),
];

/// The Discussion options shown as a choice, with WordPress's default.
const DISCUSSION_CHOICES: &[(&str, &str)] = &[
    ("thread_comments_depth", "5"),
    ("default_comments_page", "newest"),
    ("comment_order", "asc"),
    ("avatar_rating", "G"),
    ("avatar_default", "mystery"),
];

/// The Media options shown as a whole number, with WordPress's default.
const MEDIA_SIZES: &[(&str, i64)] = &[
    ("thumbnail_size_w", 150),
    ("thumbnail_size_h", 150),
    ("medium_size_w", 300),
    ("medium_size_h", 300),
    ("large_size_w", 1024),
    ("large_size_h", 1024),
];

fn field(name: &str, label: &str, kind: FieldKindDef, required: bool) -> FieldDef {
    FieldDef {
        name: name.to_owned(),
        label: label.to_owned(),
        kind,
        required,
    }
}

fn pairs(choices: &[(&str, &str)]) -> Vec<(String, String)> {
    choices
        .iter()
        .map(|(value, label)| ((*value).to_owned(), (*label).to_owned()))
        .collect()
}

/// The form fields of `screen`, in WordPress's order.
pub fn settings_fields(screen: SettingsScreen) -> Vec<FieldDef> {
    use FieldKindDef::{Checkbox, Email, Lookup, Number, Radio, Select, Text, Textarea, Url};
    let formats = |formats: &[&str]| -> Vec<(String, String)> {
        formats
            .iter()
            .map(|f| ((*f).to_owned(), (*f).to_owned()))
            .chain([(CUSTOM.to_owned(), "Custom".to_owned())])
            .collect()
    };
    match screen {
        SettingsScreen::General => vec![
            field("blogname", "Site Title", Text, false),
            field("blogdescription", "Tagline", Text, false),
            field("siteurl", "Mission Control Address (URL)", Url, true),
            field("home", "Site Address (URL)", Url, true),
            field("admin_email", "Administration Email Address", Email, true),
            field("timezone_string", "Timezone", Lookup, true),
            field(
                "date_format",
                "Date Format",
                Radio(formats(&DATE_FORMATS)),
                true,
            ),
            field("date_format_custom", "Custom date format", Text, false),
            field(
                "time_format",
                "Time Format",
                Radio(formats(&TIME_FORMATS)),
                true,
            ),
            field("time_format_custom", "Custom time format", Text, false),
            field(
                "start_of_week",
                "Week Starts On",
                Select(pairs(&[
                    ("0", "Sunday"),
                    ("1", "Monday"),
                    ("2", "Tuesday"),
                    ("3", "Wednesday"),
                    ("4", "Thursday"),
                    ("5", "Friday"),
                    ("6", "Saturday"),
                ])),
                true,
            ),
        ],
        SettingsScreen::Writing => {
            vec![field(
                "default_category",
                "Default Post Category",
                Lookup,
                true,
            )]
        }
        SettingsScreen::Reading => vec![
            field(
                "show_on_front",
                "Your homepage displays",
                Radio(pairs(&[
                    ("posts", "Your latest posts"),
                    ("page", "A static page (select below)"),
                ])),
                true,
            ),
            field("page_on_front", "Homepage", Lookup, false),
            field("page_for_posts", "Posts page", Lookup, false),
            field("posts_per_page", "Blog pages show at most", Number, true),
            field(
                "posts_per_rss",
                "Syndication feeds show the most recent",
                Number,
                true,
            ),
            field(
                "rss_use_excerpt",
                "For each post in a feed, include",
                Radio(pairs(&[("0", "Full text"), ("1", "Excerpt")])),
                true,
            ),
            field(
                "blog_public",
                "Discourage search engines from indexing this site",
                Checkbox,
                false,
            ),
        ],
        SettingsScreen::Discussion => {
            let label = |name: &str| -> &'static str {
                match name {
                    "default_pingback_flag" => {
                        "Attempt to notify any blogs linked to from the post"
                    }
                    "default_ping_status" => {
                        "Allow link notifications from other blogs (pingbacks and trackbacks) on new posts"
                    }
                    "default_comment_status" => "Allow people to submit comments on new posts",
                    "require_name_email" => "Comment author must fill out name and email",
                    "comment_registration" => "Users must be registered and logged in to comment",
                    "close_comments_for_old_posts" => "Automatically close comments on old posts",
                    "close_comments_days_old" => "Close comments on posts older than (days)",
                    "show_comments_cookies_opt_in" => "Show comments cookies opt-in checkbox",
                    "thread_comments" => "Enable threaded (nested) comments",
                    "thread_comments_depth" => "Threaded comments levels deep",
                    "page_comments" => "Break comments into pages",
                    "comments_per_page" => "Top level comments per page",
                    "default_comments_page" => "Comments page displayed by default",
                    "comment_order" => "Comments at the top of each page",
                    "comments_notify" => "Email me whenever anyone posts a comment",
                    "moderation_notify" => "Email me whenever a comment is held for moderation",
                    "comment_moderation" => "Comment must be manually approved",
                    "comment_previously_approved" => {
                        "Comment author must have a previously approved comment"
                    }
                    "comment_max_links" => {
                        "Hold a comment in the queue if it contains this many links"
                    }
                    "moderation_keys" => "Comment Moderation (one word or IP address per line)",
                    "disallowed_keys" => "Disallowed Comment Keys (one per line)",
                    "show_avatars" => "Show Avatars",
                    "avatar_rating" => "Maximum Rating",
                    _ => "Default Avatar",
                }
            };
            DISCUSSION_OPTIONS
                .iter()
                .map(|&name| {
                    let kind = match name {
                        "default_ping_status" | "default_comment_status" => Checkbox,
                        _ if DISCUSSION_FLAGS.iter().any(|(n, _)| *n == name) => Checkbox,
                        _ if DISCUSSION_NUMBERS.iter().any(|(n, _)| *n == name) => Number,
                        "moderation_keys" | "disallowed_keys" => Textarea,
                        "thread_comments_depth" => {
                            Select((2..=10).map(|n| (n.to_string(), n.to_string())).collect())
                        }
                        "default_comments_page" => {
                            Select(pairs(&[("newest", "last"), ("oldest", "first")]))
                        }
                        "comment_order" => Select(pairs(&[("asc", "older"), ("desc", "newer")])),
                        "avatar_rating" => Radio(pairs(&[
                            ("G", "G: Suitable for all audiences"),
                            (
                                "PG",
                                "PG: Possibly offensive, usually for audiences 13 and above",
                            ),
                            ("R", "R: Intended for adult audiences above 17"),
                            ("X", "X: Even more mature than above"),
                        ])),
                        _ => Radio(pairs(&[
                            ("mystery", "Mystery Person"),
                            ("blank", "Blank"),
                            ("gravatar_default", "Gravatar Logo"),
                            ("identicon", "Identicon (Generated)"),
                            ("wavatar", "Wavatar (Generated)"),
                            ("monsterid", "MonsterID (Generated)"),
                            ("retro", "Retro (Generated)"),
                            ("robohash", "RoboHash (Generated)"),
                        ])),
                    };
                    let required = matches!(kind, Number | Select(_) | Radio(_));
                    field(name, label(name), kind, required)
                })
                .collect()
        }
        SettingsScreen::Media => vec![
            field("thumbnail_size_w", "Thumbnail size: Width", Number, true),
            field("thumbnail_size_h", "Thumbnail size: Height", Number, true),
            field(
                "thumbnail_crop",
                "Crop thumbnail to exact dimensions (normally thumbnails are proportional)",
                Checkbox,
                false,
            ),
            field("medium_size_w", "Medium size: Max Width", Number, true),
            field("medium_size_h", "Medium size: Max Height", Number, true),
            field("large_size_w", "Large size: Max Width", Number, true),
            field("large_size_h", "Large size: Max Height", Number, true),
        ],
        SettingsScreen::Permalink => vec![
            field(
                "permalink_selection",
                "Permalink structure",
                Radio(
                    PERMALINK_CHOICES
                        .iter()
                        .map(|(value, _, label)| ((*value).to_owned(), (*label).to_owned()))
                        .chain([(CUSTOM.to_owned(), "Custom Structure".to_owned())])
                        .collect(),
                ),
                true,
            ),
            field("permalink_structure", "Custom Structure", Text, false),
            field("category_base", "Category base", Text, false),
            field("tag_base", "Tag base", Text, false),
        ],
        SettingsScreen::Privacy => vec![field(
            "wp_page_for_privacy_policy",
            "Change your Privacy Policy page",
            Lookup,
            false,
        )],
    }
}

/// What the settings screens use from the app: the image size registry the Media screen
/// updates, and the limiter `sanitize_option`'s kses work runs under, as the REST API's does.
/// Without a limiter it runs on a blocking thread of its own.
#[derive(Debug, Clone, Default)]
pub struct SettingsServices {
    /// The image size registry.
    pub image_sizes: Option<Arc<ImageSizes>>,
    /// The app's kses limiter.
    pub kses: Option<KsesLimiter>,
}

/// The Ignite runtime resource of `screen`: its fields, one column, and [`SettingsData`] over
/// `options`.
pub fn settings_resource(screen: SettingsScreen, services: SettingsServices) -> RuntimeResource {
    let mut resource = RuntimeResource::new(
        screen.slug(),
        screen.title(),
        screen.title(),
        SettingsData { screen, services },
    );
    resource.fields = settings_fields(screen);
    resource.columns = vec![ColumnDef {
        key: COL_TITLE.to_owned(),
        label: "Settings".to_owned(),
        sortable: false,
        searchable: false,
    }];
    // Settings (80), with the screens in WordPress's submenu order.
    let index = SettingsScreen::ALL
        .iter()
        .position(|s| *s == screen)
        .unwrap_or_default();
    super::navigation::place(
        &mut resource,
        super::navigation::SETTINGS + i32::try_from(index).unwrap_or_default(),
        super::navigation::SETTINGS_ICON,
    );
    resource
}

/// Registers every settings screen with Ignite. Returns the slugs registered, and the
/// refusals (a slug another registration holds).
pub fn register_settings_resources(
    services: SettingsServices,
) -> (Vec<&'static str>, Vec<RuntimeError>) {
    let mut registered = Vec::new();
    let mut refused = Vec::new();
    for screen in SettingsScreen::ALL {
        match ignite_core::register_runtime(settings_resource(screen, services.clone())) {
            Ok(()) => registered.push(screen.slug()),
            Err(e) => refused.push(e),
        }
    }
    (registered, refused)
}

/// The list's one column.
const COL_TITLE: &str = "col_title";

// ---------------------------------------------------------------------------------------------
// Reading

/// Option `name`, with a value that is not valid JSON counted as unset.
async fn stored(db: &Database, name: &str) -> Result<Option<Value>, Error> {
    match get_option(db, name).await {
        Ok(value) => Ok(value),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e),
    }
}

/// An option value as text: a string, a number written out, `1` or empty for a boolean.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "1".to_owned(),
        _ => String::new(),
    }
}

/// An option value as a whole number, as PHP's `(int)` reads one.
fn int_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// An option value as a checkbox: on for a true value (`1`, `"1"`, `true`), as WordPress's
/// `checked( '1', get_option( ... ) )`.
fn flag_of(value: &Value) -> bool {
    int_of(value).is_some_and(|n| n != 0)
}

/// `gmt_offset` as hours, 0 when unset or not a number.
fn offset_hours(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
    .clamp(-24.0, 24.0)
}

/// The General screen's time zone choice for an offset: `UTC+5.5`, `UTC-3`, `UTC+0`, as
/// `options-general.php` builds it.
fn offset_choice(hours: f64) -> String {
    if hours < 0.0 {
        format!("UTC{hours}")
    } else {
        format!("UTC+{hours}")
    }
}

/// The record of `screen`: each field's value, read from `options` (WordPress's default when an
/// option is not set), and `id` [`SETTINGS_RECORD`].
///
/// # Errors
///
/// Fails with the database's error.
pub async fn read_settings(db: &Database, screen: SettingsScreen) -> Result<Row, Error> {
    let mut row = Row::new();
    row.insert("id".into(), json!(SETTINGS_RECORD));
    row.insert(COL_TITLE.into(), json!(screen.title()));
    let text = async |name: &str, default: &str| -> Result<Value, Error> {
        Ok(Value::String(
            stored(db, name)
                .await?
                .map_or_else(|| default.to_owned(), |v| text_of(&v)),
        ))
    };
    let number = async |name: &str, default: i64| -> Result<Value, Error> {
        Ok(json!(
            stored(db, name)
                .await?
                .as_ref()
                .and_then(int_of)
                .unwrap_or(default)
        ))
    };
    let flag = async |name: &str, default: bool| -> Result<Value, Error> {
        Ok(json!(
            stored(db, name).await?.as_ref().map_or(default, flag_of)
        ))
    };
    match screen {
        SettingsScreen::General => {
            for name in [
                "blogname",
                "blogdescription",
                "siteurl",
                "home",
                "admin_email",
            ] {
                let value = text_of(&text(name, "").await?);
                // The title and tagline are stored through `esc_html`; the form escapes them
                // once more, as WordPress's `esc_attr` does not encode an entity twice.
                let value = if matches!(name, "blogname" | "blogdescription") {
                    crate::text::decode_entities(&value)
                } else {
                    value
                };
                row.insert(name.into(), json!(value));
            }
            row.insert("timezone_string".into(), json!(timezone_choice(db).await?));
            for (name, presets, default) in [
                ("date_format", &DATE_FORMATS[..], "F j, Y"),
                ("time_format", &TIME_FORMATS[..], "g:i a"),
            ] {
                let value = text_of(&text(name, default).await?);
                let choice = if presets.contains(&value.as_str()) {
                    value.clone()
                } else {
                    CUSTOM.to_owned()
                };
                row.insert(name.into(), json!(choice));
                row.insert(format!("{name}_custom"), json!(value));
            }
            row.insert(
                "start_of_week".into(),
                json!(text_of(&number("start_of_week", 1).await?)),
            );
        }
        SettingsScreen::Writing => {
            row.insert(
                "default_category".into(),
                json!(text_of(&number("default_category", 1).await?)),
            );
        }
        SettingsScreen::Reading => {
            row.insert(
                "show_on_front".into(),
                text("show_on_front", "posts").await?,
            );
            for name in ["page_on_front", "page_for_posts"] {
                let id = int_of(&number(name, 0).await?).unwrap_or(0);
                row.insert(name.into(), page_value(id));
            }
            row.insert("posts_per_page".into(), number("posts_per_page", 10).await?);
            row.insert("posts_per_rss".into(), number("posts_per_rss", 10).await?);
            let excerpt = flag("rss_use_excerpt", false).await?;
            row.insert(
                "rss_use_excerpt".into(),
                json!(if excerpt == json!(true) { "1" } else { "0" }),
            );
            let public = flag("blog_public", true).await?;
            row.insert("blog_public".into(), json!(public != json!(true)));
        }
        SettingsScreen::Discussion => {
            for (name, default) in DISCUSSION_FLAGS {
                row.insert((*name).into(), flag(name, *default).await?);
            }
            for name in ["default_ping_status", "default_comment_status"] {
                let status = text_of(&text(name, "open").await?);
                row.insert(name.into(), json!(status == "open"));
            }
            for (name, default) in DISCUSSION_NUMBERS {
                row.insert((*name).into(), number(name, *default).await?);
            }
            for (name, default) in DISCUSSION_CHOICES {
                row.insert((*name).into(), text(name, default).await?);
            }
            for name in ["moderation_keys", "disallowed_keys"] {
                row.insert(name.into(), text(name, "").await?);
            }
        }
        SettingsScreen::Media => {
            for (name, default) in MEDIA_SIZES {
                row.insert((*name).into(), number(name, *default).await?);
            }
            row.insert("thumbnail_crop".into(), flag("thumbnail_crop", true).await?);
        }
        SettingsScreen::Permalink => {
            let structure = text_of(&text("permalink_structure", "").await?);
            let choice = PERMALINK_CHOICES
                .iter()
                .find(|(_, preset, _)| *preset == structure)
                .map_or(CUSTOM, |(choice, _, _)| choice);
            row.insert("permalink_selection".into(), json!(choice));
            row.insert("permalink_structure".into(), json!(structure));
            row.insert("category_base".into(), text("category_base", "").await?);
            row.insert("tag_base".into(), text("tag_base", "").await?);
        }
        SettingsScreen::Privacy => {
            let id = int_of(&number("wp_page_for_privacy_policy", 0).await?).unwrap_or(0);
            row.insert("wp_page_for_privacy_policy".into(), page_value(id));
        }
    }
    Ok(row)
}

/// A page select's value: the id, or `null` (the "Select" placeholder) for none.
fn page_value(id: i64) -> Value {
    if id > 0 {
        json!(id.to_string())
    } else {
        Value::Null
    }
}

/// The General screen's time zone value: `timezone_string`, or the `UTC+N` choice of
/// `gmt_offset` when it is empty (or one of the old `Etc/GMT` names), as
/// `options-general.php` chooses it.
async fn timezone_choice(db: &Database) -> Result<String, Error> {
    let zone = stored(db, "timezone_string")
        .await?
        .map(|v| text_of(&v))
        .unwrap_or_default();
    if !zone.is_empty() && !zone.contains("Etc/GMT") {
        return Ok(zone);
    }
    Ok(offset_choice(offset_hours(
        stored(db, "gmt_offset").await?.as_ref(),
    )))
}

// ---------------------------------------------------------------------------------------------
// Choices

/// The continents WordPress's time zone menu lists (`wp_timezone_choice`).
const ZONE_CONTINENTS: [&str; 10] = [
    "Africa",
    "America",
    "Antarctica",
    "Arctic",
    "Asia",
    "Atlantic",
    "Australia",
    "Europe",
    "Indian",
    "Pacific",
];

/// The `UTC+N` offsets WordPress's time zone menu offers, in hours.
fn menu_offsets() -> Vec<f64> {
    let mut offsets: Vec<f64> = (-24..=28).map(|half| f64::from(half) / 2.0).collect();
    offsets.extend([5.75, 8.75, 12.75, 13.75]);
    offsets.sort_by(f64::total_cmp);
    offsets
}

/// The General screen's time zones, as WordPress's menu lists them: the cities of each
/// continent, `UTC`, then the `UTC±N` offsets.
pub fn timezone_choices() -> Vec<(String, String)> {
    let mut zones: Vec<(String, String)> = chrono_tz::TZ_VARIANTS
        .iter()
        .map(|tz| tz.name())
        .filter(|name| {
            name.split_once('/')
                .is_some_and(|(continent, _)| ZONE_CONTINENTS.contains(&continent))
        })
        .map(|name| (name.to_owned(), name.replace('_', " ")))
        .collect();
    zones.sort();
    zones.push(("UTC".to_owned(), "UTC".to_owned()));
    zones.extend(menu_offsets().into_iter().map(|hours| {
        let choice = offset_choice(hours);
        (choice.clone(), choice)
    }));
    zones
}

/// The pages whose status is one of `statuses`, by title, at most [`MAX_OPTIONS`]: `(id,
/// title)`, "(no title)" for none, as `wp_dropdown_pages` lists them.
async fn pages_with(
    db: &Database,
    statuses: &[PostStatus],
) -> Result<Vec<(String, String)>, Error> {
    let marks = vec!["?"; statuses.len()].join(", ");
    let mut binds: Vec<Sql> = vec!["page".into()];
    binds.extend(statuses.iter().map(|s| Sql::from(*s)));
    binds.push((MAX_OPTIONS as i64).into());
    let rows: Vec<(i64, String, String)> = raw::fetch_all(
        db,
        &format!(
            "SELECT id, title, post_type FROM posts WHERE post_type = ? AND status IN ({marks}) \
             ORDER BY menu_order, title, id LIMIT ?"
        ),
        &binds,
    )
    .await?;
    Ok(rows
        .into_iter()
        // Exact comparison: MySQL compares text without case.
        .filter(|(_, _, post_type)| post_type == "page")
        .map(|(id, title, _)| {
            let title = if title.trim().is_empty() {
                "(no title)".to_owned()
            } else {
                title
            };
            (id.to_string(), title)
        })
        .collect())
}

/// The categories, by name, at most [`MAX_OPTIONS`]: `(term_id, name)`.
async fn categories(db: &Database) -> Result<Vec<(String, String)>, Error> {
    let rows: Vec<(i64, String, String)> = raw::fetch_all(
        db,
        "SELECT t.id, t.name, tt.taxonomy FROM term_taxonomy tt \
         INNER JOIN terms t ON t.id = tt.term_id WHERE tt.taxonomy = ? \
         ORDER BY t.name, t.id LIMIT ?",
        &["category".into(), (MAX_OPTIONS as i64).into()],
    )
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, _, taxonomy)| taxonomy == "category")
        .map(|(id, name, _)| (id.to_string(), name))
        .collect())
}

/// `choices` with the stored value `current` added when it is not among them, as WordPress's
/// dropdowns keep the selected value: the form can then save without changing it.
fn with_current(
    mut choices: Vec<(String, String)>,
    current: Option<String>,
) -> Vec<(String, String)> {
    if let Some(current) = current.filter(|c| !c.is_empty() && c != "0")
        && !choices.iter().any(|(value, _)| *value == current)
    {
        choices.push((current.clone(), current));
    }
    choices
}

/// The choices of the lookup field `field` of `screen`.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn settings_choices(
    db: &Database,
    screen: SettingsScreen,
    field: &str,
) -> Result<Vec<(String, String)>, Error> {
    let current = async |name: &str| -> Result<Option<String>, Error> {
        Ok(stored(db, name).await?.map(|v| text_of(&v)))
    };
    Ok(match (screen, field) {
        (SettingsScreen::General, "timezone_string") => {
            let zone = timezone_choice(db).await?;
            let zone = (is_timezone(&zone) || utc_offset(&zone).is_some()).then_some(zone);
            with_current(timezone_choices(), zone)
        }
        (SettingsScreen::Writing, "default_category") => {
            with_current(categories(db).await?, current("default_category").await?)
        }
        (SettingsScreen::Reading, "page_on_front" | "page_for_posts") => with_current(
            pages_with(db, &[PostStatus::Publish]).await?,
            current(field).await?,
        ),
        (SettingsScreen::Privacy, "wp_page_for_privacy_policy") => with_current(
            pages_with(db, &[PostStatus::Publish, PostStatus::Draft]).await?,
            current(field).await?,
        ),
        _ => Vec::new(),
    })
}

// ---------------------------------------------------------------------------------------------
// Saving

/// A value of the validated input, as text: what Ignite gives a text, select or lookup field.
/// `None` when the form did not send the field; empty when it was sent blank.
fn input_text(input: &Row, name: &str) -> Option<String> {
    input.get(name).map(text_of)
}

/// A checkbox of the validated input: `None` when the form did not send the field.
fn input_flag(input: &Row, name: &str) -> Option<bool> {
    input.get(name).map(|v| match v {
        Value::Bool(b) => *b,
        other => flag_of(other),
    })
}

/// A whole number of the validated input; a blank number is 0, as PHP's `(int) ''`.
fn input_number(input: &Row, name: &str) -> Option<Value> {
    input.get(name).map(|v| json!(int_of(v).unwrap_or(0)))
}

/// `value` with `#` taken out, a leading `/` added and repeated slashes folded, as
/// `options-permalink.php` prepares a structure or a base; empty stays empty.
fn permalink_path(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(value.len() + 1);
    for c in format!("/{}", value.replace('#', "")).chars() {
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
    }
    out
}

/// What a save of `screen` writes: (field, option, value before `sanitize_option`).
fn writes_of(screen: SettingsScreen, input: &Row) -> Vec<(&'static str, &'static str, Value)> {
    let mut writes: Vec<(&'static str, &'static str, Value)> = Vec::new();
    let text = |writes: &mut Vec<_>, name: &'static str| {
        if let Some(value) = input_text(input, name) {
            writes.push((name, name, Value::String(value)));
        }
    };
    match screen {
        SettingsScreen::General => {
            for name in [
                "blogname",
                "blogdescription",
                "siteurl",
                "home",
                "admin_email",
            ] {
                text(&mut writes, name);
            }
            if let Some(zone) = input_text(input, "timezone_string") {
                // `options.php`: a `UTC+N` choice is an offset, and no time zone.
                match utc_offset(&zone) {
                    Some(offset) => {
                        writes.push(("timezone_string", "gmt_offset", Value::String(offset)));
                        writes.push(("timezone_string", "timezone_string", json!("")));
                    }
                    // A named zone is stored alone: its offset is read when it is needed
                    // (Task A10, `wp_timezone_override_offset`).
                    None => {
                        writes.push(("timezone_string", "timezone_string", Value::String(zone)));
                    }
                }
            }
            for name in ["date_format", "time_format"] {
                let Some(choice) = input_text(input, name) else {
                    continue;
                };
                let custom = if name == "date_format" {
                    "date_format_custom"
                } else {
                    "time_format_custom"
                };
                let value = if choice == CUSTOM {
                    input_text(input, custom).unwrap_or_default()
                } else {
                    choice
                };
                writes.push((name, name, Value::String(value)));
            }
            if let Some(day) = input_number(input, "start_of_week") {
                writes.push(("start_of_week", "start_of_week", day));
            }
        }
        SettingsScreen::Writing => {
            if let Some(Value::String(id)) = input.get("default_category") {
                writes.push(("default_category", "default_category", json!(id)));
            }
        }
        SettingsScreen::Reading => {
            text(&mut writes, "show_on_front");
            for name in ["page_on_front", "page_for_posts"] {
                if let Some(value) = input_number(input, name) {
                    writes.push((name, name, value));
                }
            }
            for name in ["posts_per_page", "posts_per_rss"] {
                if let Some(value) = input_number(input, name) {
                    writes.push((name, name, value));
                }
            }
            if let Some(value) = input_number(input, "rss_use_excerpt") {
                writes.push(("rss_use_excerpt", "rss_use_excerpt", value));
            }
            if let Some(discourage) = input_flag(input, "blog_public") {
                writes.push(("blog_public", "blog_public", json!(i64::from(!discourage))));
            }
        }
        SettingsScreen::Discussion => {
            for (name, _) in DISCUSSION_FLAGS {
                if let Some(on) = input_flag(input, name) {
                    writes.push((name, name, json!(i64::from(on))));
                }
            }
            for name in ["default_ping_status", "default_comment_status"] {
                if let Some(on) = input_flag(input, name) {
                    writes.push((name, name, json!(if on { "open" } else { "closed" })));
                }
            }
            for (name, _) in DISCUSSION_NUMBERS {
                if let Some(value) = input_number(input, name) {
                    writes.push((name, name, value));
                }
            }
            for (name, _) in DISCUSSION_CHOICES {
                if *name == "thread_comments_depth" {
                    if let Some(value) = input_number(input, name) {
                        writes.push((name, name, value));
                    }
                } else {
                    text(&mut writes, name);
                }
            }
            text(&mut writes, "moderation_keys");
            text(&mut writes, "disallowed_keys");
        }
        SettingsScreen::Media => {
            for (name, _) in MEDIA_SIZES {
                if let Some(value) = input_number(input, name) {
                    writes.push((name, name, value));
                }
            }
            if let Some(crop) = input_flag(input, "thumbnail_crop") {
                writes.push(("thumbnail_crop", "thumbnail_crop", json!(i64::from(crop))));
            }
        }
        SettingsScreen::Permalink => {
            if let Some(choice) = input_text(input, "permalink_selection") {
                let structure = match PERMALINK_CHOICES.iter().find(|(c, _, _)| *c == choice) {
                    Some((_, preset, _)) => (*preset).to_owned(),
                    None => permalink_path(
                        &input_text(input, "permalink_structure").unwrap_or_default(),
                    ),
                };
                writes.push((
                    "permalink_structure",
                    "permalink_structure",
                    Value::String(structure),
                ));
            }
            for name in ["category_base", "tag_base"] {
                if let Some(base) = input_text(input, name) {
                    writes.push((name, name, Value::String(permalink_path(&base))));
                }
            }
        }
        SettingsScreen::Privacy => {
            if let Some(value) = input_number(input, "wp_page_for_privacy_policy") {
                // `options-privacy.php` stores `(int)` of the chosen page.
                let id = int_of(&value).unwrap_or(0).max(0);
                writes.push((
                    "wp_page_for_privacy_policy",
                    "wp_page_for_privacy_policy",
                    json!(id),
                ));
            }
        }
    }
    writes
}

/// Saves `input`, the validated form of `screen`, to `options`, as WordPress's `options.php`
/// does: each option the form sent goes through [`sanitize_option`] first. Ruling R135: when it
/// refuses any of them, nothing is written and the refusals are returned under their fields
/// (WordPress keeps the stored value and shows the error; Ignite cannot show it yet, owner item
/// O10). The Reading screen's post counts are kept between 1 and [`MAX_POSTS_PER_PAGE`]
/// (Ruling R136). The Media sizes are also given to the registry in `services`.
///
/// # Errors
///
/// Fails with the database's error, or when the limiter is closed.
pub async fn save_settings(
    db: &Database,
    screen: SettingsScreen,
    input: &Row,
    services: &SettingsServices,
) -> Result<SettingsErrors, Error> {
    let writes = writes_of(screen, input);
    // `sanitize_option` runs `wp_kses_data` on the formats: on a blocking thread.
    let sanitize = move || {
        writes
            .into_iter()
            .map(|(field, option, value)| (field, option, sanitize_option(option, value)))
            .collect::<Vec<_>>()
    };
    let sanitized = match &services.kses {
        Some(kses) => kses.run(sanitize).await.map_err(|_| {
            Error::Database(sqlx::Error::Protocol("the kses limiter failed".to_owned()))
        })?,
        None => tokio::task::spawn_blocking(sanitize).await.map_err(|e| {
            Error::Database(sqlx::Error::Protocol(format!("sanitizing failed: {e}")))
        })?,
    };
    let mut errors = SettingsErrors::new();
    for (field, _, result) in &sanitized {
        if let Err(OptionError { message, .. }) = result {
            errors
                .entry((*field).to_owned())
                .or_insert_with(|| message.clone());
        }
    }
    if !errors.is_empty() {
        return Ok(errors);
    }
    for (_, option, result) in sanitized {
        let Ok(mut value) = result else { continue };
        if matches!(option, "posts_per_page" | "posts_per_rss") {
            value = json!(value.as_i64().unwrap_or(1).clamp(1, MAX_POSTS_PER_PAGE));
        }
        set_option(db, option, &value).await.map_err(meta_error)?;
    }
    if screen == SettingsScreen::Media
        && let Some(sizes) = &services.image_sizes
    {
        apply_media_sizes(db, sizes).await?;
    }
    Ok(errors)
}

/// The error a refused save returns (Ruling R135): Ignite answers it with its error page.
pub(crate) fn refused_save(reason: &str) -> Error {
    Error::Database(sqlx::Error::Protocol(format!(
        "the save was refused: {reason}"
    )))
}

fn meta_error(e: MetaError) -> Error {
    match e {
        MetaError::Database(e) => e,
        MetaError::InvalidJson(e) => Error::Database(sqlx::Error::Decode(Box::new(e))),
    }
}

/// Registers the Media screen's sizes (`thumbnail`, `medium`, `large`, from their options, or
/// WordPress's defaults) with `sizes`, as WordPress reads them from the options when it resizes.
/// The app calls it at start; a save of the Media screen calls it again.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn apply_media_sizes(db: &Database, sizes: &ImageSizes) -> Result<(), Error> {
    let number = async |name: &str, default: i64| -> Result<u32, Error> {
        let n = stored(db, name)
            .await?
            .as_ref()
            .and_then(int_of)
            .unwrap_or(default);
        Ok(u32::try_from(n.max(0)).unwrap_or(u32::MAX))
    };
    let crop = stored(db, "thumbnail_crop")
        .await?
        .as_ref()
        .is_none_or(flag_of);
    for (name, crop) in [("thumbnail", crop), ("medium", false), ("large", false)] {
        let (w, h) = match name {
            "thumbnail" => (150, 150),
            "medium" => (300, 300),
            _ => (1024, 1024),
        };
        let width = number(&format!("{name}_size_w"), w).await?;
        let height = number(&format!("{name}_size_h"), h).await?;
        sizes.add_image_size(name, width, height, crop, Owner::Core);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The data layer

/// Where a settings screen's one record lives: the `options` table. See the [module](self).
#[derive(Clone)]
pub struct SettingsData {
    screen: SettingsScreen,
    services: SettingsServices,
}

impl SettingsData {
    /// The data of `screen`, with the app's `services`.
    pub fn new(screen: SettingsScreen, services: SettingsServices) -> Self {
        Self { screen, services }
    }
}

impl RuntimeData for SettingsData {
    /// The one record, which the app's redirect answers before Ignite lists it.
    fn list<'a>(
        &'a self,
        _db: &'a Database,
        _query: &'a ListQuery,
    ) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move {
            let mut row = Row::new();
            row.insert("id".into(), json!(SETTINGS_RECORD));
            row.insert(COL_TITLE.into(), json!(self.screen.title()));
            Ok(Page {
                rows: vec![row],
                total: 1,
            })
        })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move {
            if id != SETTINGS_RECORD {
                return Ok(None);
            }
            read_settings(db, self.screen).await.map(Some)
        })
    }

    /// Refuses: a settings screen has one record (Ruling R132).
    fn create<'a>(&'a self, _db: &'a Database, _input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "creating a record of a settings screen",
                driver: "Mission Control's settings",
            })
        })
    }

    /// Saves the screen ([`save_settings`]). Ruling R135: when `sanitize_option` refuses a value,
    /// nothing is saved, the reason is logged, and an error is returned, so Ignite shows its
    /// error page instead of "Changes saved." (owner item O10: Ignite cannot show the message
    /// on the form yet).
    fn update<'a>(&'a self, db: &'a Database, id: i64, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            if id != SETTINGS_RECORD {
                return Err(Error::NotFound { table: "options" });
            }
            let errors = save_settings(db, self.screen, &input, &self.services).await?;
            if !errors.is_empty() {
                let reasons: Vec<String> = errors
                    .iter()
                    .map(|(field, message)| format!("{field}: {message}"))
                    .collect();
                log::warn!(
                    "The {} screen was not saved: {}",
                    self.screen.slug(),
                    reasons.join("; ")
                );
                return Err(refused_save(&reasons.join("; ")));
            }
            read_settings(db, self.screen).await
        })
    }

    /// Refuses: a settings screen's record is never deleted.
    fn delete<'a>(&'a self, _db: &'a Database, _id: i64) -> RuntimeFuture<'a, u64> {
        Box::pin(async { Ok(0) })
    }

    fn options<'a>(
        &'a self,
        db: &'a Database,
        field: &'a str,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move { settings_choices(db, self.screen, field).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_have_wordpresss_slugs_and_valid_field_names() {
        let slugs: Vec<&str> = SettingsScreen::ALL.iter().map(|s| s.slug()).collect();
        assert_eq!(
            slugs,
            [
                "options-general",
                "options-writing",
                "options-reading",
                "options-discussion",
                "options-media",
                "options-permalink",
                "options-privacy",
            ]
        );
        for screen in SettingsScreen::ALL {
            assert_eq!(SettingsScreen::from_slug(screen.slug()), Some(screen));
            for f in settings_fields(screen) {
                assert!(
                    f.name.len() <= 64
                        && f.name.starts_with(|c: char| c.is_ascii_lowercase())
                        && f.name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{}",
                    f.name
                );
            }
        }
    }

    #[test]
    fn the_time_zone_menu_is_wordpresss() {
        let zones = timezone_choices();
        let has = |value: &str| zones.iter().any(|(v, _)| v == value);
        assert!(has("America/Toronto"));
        assert!(has("America/Argentina/Buenos_Aires"));
        assert!(has("UTC"));
        assert!(has("UTC+5.5"));
        assert!(has("UTC-12"));
        assert!(has("UTC+14"));
        assert!(has("UTC+0"));
        assert!(has("UTC+13.75"));
        assert!(!has("US/Eastern"));
        assert!(!has("Etc/GMT+5"));
        assert!(zones.len() > 300 && zones.len() < 600, "{}", zones.len());
        for (value, _) in zones.iter().filter(|(v, _)| v.starts_with("UTC")) {
            assert!(value == "UTC" || utc_offset(value).is_some(), "{value}");
        }
    }

    #[test]
    fn permalink_paths_are_prepared_as_options_permalink_prepares_them() {
        assert_eq!(permalink_path("%postname%"), "/%postname%");
        assert_eq!(permalink_path("//a//#b/%post_id%/"), "/a/b/%post_id%/");
        assert_eq!(permalink_path("  "), "");
        assert_eq!(permalink_path("topics"), "/topics");
    }
}
