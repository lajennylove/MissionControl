//! The theme manifest: `theme.toml`, the equivalent of the header of a WordPress theme's
//! `style.css`.
//!
//! ```toml
//! name = "Orbit One"
//! author = "RocketBoost"
//! author_uri = "https://rocketboost.rs"
//! version = "1.0.0"
//! description = "The default theme."
//! license = "GPL-2.0-or-later"
//! requires = "0.1"
//! template = "orbit-base"   # the parent theme's slug, for a child theme
//! supports = ["menus", "post-thumbnails", "custom-logo"]
//!
//! [menus]                   # register_nav_menus: location => description
//! primary = "Primary menu"
//!
//! [sidebars]                # register_sidebar: id => name
//! footer = "Footer"
//! ```
//!
//! The theme's slug is the name of its folder, as in WordPress; the manifest cannot set it.
//! Parsing is strict: an unknown key or an unknown `supports` value is refused by name, so a
//! typo does not pass silently.
//!
//! Menu locations and widget area ids are template names (`menus.primary`,
//! `sidebars.footer`), so they are lowercase ASCII letters, digits and `_`, starting with a letter
//! or `_`, at most 64 characters. WordPress allows `-` too (`sidebar-1`), which Propel's paths
//! cannot read. Declaring a menu location or a widget area adds the `menus` or `widgets` support,
//! as `register_nav_menus` and `register_sidebar` do.
//!
//! Page templates are not declared here: as in WordPress, a template file with a
//! `Template Name:` header is one ([`Theme::page_templates`](super::Theme::page_templates)).

use std::path::Path;

use serde::Deserialize;

use super::ThemeError;

/// The manifest's file name, in the theme's folder.
pub const MANIFEST_FILE: &str = "theme.toml";

/// The largest accepted manifest, in bytes (64 KiB). A larger file is refused before it is read.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// A theme's details, read from its `theme.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The theme's name (WordPress's `Theme Name`), trimmed. Required.
    pub name: String,
    /// Who made the theme (`Author`).
    pub author: Option<String>,
    /// The author's web site (`Author URI`).
    pub author_uri: Option<String>,
    /// The theme's version (`Version`).
    pub version: Option<String>,
    /// A short description (`Description`).
    pub description: Option<String>,
    /// The theme's licence (`License`), as an SPDX expression such as `GPL-2.0-or-later`. The key
    /// is spelled `license`, as in Cargo and WordPress; `licence` is refused as an unknown key.
    pub license: Option<String>,
    /// The oldest Mission Control version the theme works with (`Requires at least`).
    pub requires: Option<String>,
    /// The parent theme's slug, for a child theme (WordPress's `Template` header). `None` for a
    /// standalone theme; an empty value is the same as none.
    pub template: Option<String>,
    /// The features the theme supports (`add_theme_support`), in the order written, then
    /// `menus` and `widgets` when the theme declares menu locations or widget areas and does not
    /// list them.
    pub supports: Vec<Support>,
    /// The theme's menu locations (`register_nav_menus`), in the order written.
    pub menus: Vec<MenuLocation>,
    /// The theme's widget areas (`register_sidebar`), in the order written.
    pub sidebars: Vec<Sidebar>,
}

/// A page template: WordPress's template file with a `Template Name:` header, which a page
/// chooses in the admin's Template box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageTemplate {
    /// The template's name, such as `templates/wide`: its file without `.propel`, and the value
    /// a page stores in `_wp_page_template`.
    pub name: String,
    /// The name the admin shows, such as `Full width` (`Template Name:`).
    pub title: String,
}

/// A menu location the theme declares, as `register_nav_menus( array( $location => $description ) )`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuLocation {
    /// The location's id, such as `primary`; the template reads its menu as `menus.{location}`.
    pub location: String,
    /// What the location is for, shown to the site owner.
    pub description: String,
}

/// A widget area (WordPress's sidebar) the theme declares, as `register_sidebar`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidebar {
    /// The area's id, such as `footer`; the template reads it as `sidebars.{id}`.
    pub id: String,
    /// The area's name, shown to the site owner.
    pub name: String,
}

/// A feature a theme declares support for, as WordPress's `add_theme_support`. The manifest
/// spells each in kebab case, as WordPress does (`post-thumbnails`).
///
/// Only the classic-theme features Mission Control has a use for are listed; block-editor
/// features (`align-wide`, `editor-color-palette`, ...) are not, because Mission Control has no
/// block editor. Features that take arguments in WordPress (`html5`, `post-formats`,
/// `custom-logo`) are plain flags for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Support {
    /// `automatic-feed-links`: feed links in the document head.
    AutomaticFeedLinks,
    /// `custom-background`: a background chosen by the site owner.
    CustomBackground,
    /// `custom-header`: a header image chosen by the site owner.
    CustomHeader,
    /// `custom-logo`: a site logo chosen by the site owner.
    CustomLogo,
    /// `editor-styles`: the theme's styles in the editor.
    EditorStyles,
    /// `html5`: HTML5 markup for the search form, comments, galleries and captions.
    Html5,
    /// `menus`: navigation menus.
    Menus,
    /// `post-formats`: post formats (aside, gallery, link, ...).
    PostFormats,
    /// `post-thumbnails`: featured images.
    PostThumbnails,
    /// `responsive-embeds`: embeds that keep their aspect ratio.
    ResponsiveEmbeds,
    /// `title-tag`: the document title is managed by Mission Control.
    TitleTag,
    /// `widgets`: widget areas (sidebars).
    Widgets,
}

/// The manifest as written. Every field is optional here so a missing `name` gets its own error.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: Option<String>,
    author: Option<String>,
    author_uri: Option<String>,
    version: Option<String>,
    description: Option<String>,
    license: Option<String>,
    requires: Option<String>,
    template: Option<String>,
    #[serde(default)]
    supports: Vec<Support>,
    menus: Option<toml::Table>,
    sidebars: Option<toml::Table>,
}

impl Manifest {
    /// Parses a manifest's text. `path` is the file it came from, for error messages.
    pub fn parse(text: &str, path: &Path) -> Result<Manifest, ThemeError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|error| ThemeError::InvalidManifest {
                path: path.to_path_buf(),
                message: describe(&error, text),
            })?;
        let name = raw.name.as_deref().map(str::trim).unwrap_or_default();
        if name.is_empty() {
            return Err(ThemeError::MissingName {
                path: path.to_path_buf(),
            });
        }
        let invalid = |message: String| ThemeError::InvalidManifest {
            path: path.to_path_buf(),
            message,
        };
        let menus = declared(raw.menus, "menus", &[])
            .map_err(invalid)?
            .into_iter()
            .map(|(location, description)| MenuLocation {
                location,
                description,
            })
            .collect::<Vec<_>>();
        let sidebars = declared(raw.sidebars, "sidebars", &super::widgets::RESERVED_AREAS)
            .map_err(invalid)?
            .into_iter()
            .map(|(id, name)| Sidebar { id, name })
            .collect::<Vec<_>>();
        let mut supports = raw.supports;
        if !menus.is_empty() && !supports.contains(&Support::Menus) {
            supports.push(Support::Menus);
        }
        if !sidebars.is_empty() && !supports.contains(&Support::Widgets) {
            supports.push(Support::Widgets);
        }
        Ok(Manifest {
            name: name.to_owned(),
            author: raw.author,
            author_uri: raw.author_uri,
            version: raw.version,
            description: raw.description,
            license: raw.license,
            requires: raw.requires,
            template: raw.template.filter(|template| !template.is_empty()),
            supports,
            menus,
            sidebars,
        })
    }

    /// Whether the theme declares `feature`, as `current_theme_supports`.
    pub fn supports(&self, feature: Support) -> bool {
        self.supports.contains(&feature)
    }
}

/// The longest menu location or widget area id.
pub const MAX_AREA_ID: usize = 64;

/// The `(id, text)` pairs of the `[menus]` or `[sidebars]` table, in order: each id a template
/// name ([`is_area_id`]) and not `reserved`, and each value a string that is not empty once trimmed. The error names
/// the table, and the id when it is safe to repeat, never a value.
fn declared(
    table: Option<toml::Table>,
    name: &str,
    reserved: &[&str],
) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for (id, value) in table.unwrap_or_default() {
        if reserved.contains(&id.as_str()) {
            return Err(format!("`[{name}]` id `{id}` is reserved"));
        }
        if !is_area_id(&id) {
            return Err(match echoable(&id) {
                Some(id) => format!(
                    "`[{name}]` id `{id}` is not valid: use lowercase letters, digits and `_`, \
                     starting with a letter or `_`"
                ),
                None => format!(
                    "a `[{name}]` id is not valid: use lowercase letters, digits and `_`, \
                     starting with a letter or `_`"
                ),
            });
        }
        let toml::Value::String(text) = value else {
            return Err(format!(
                "`[{name}]` `{id}` has the wrong type (expected a string)"
            ));
        };
        let text = text.trim();
        if text.is_empty() {
            return Err(format!("`[{name}]` `{id}` is empty"));
        }
        pairs.push((id, text.to_owned()));
    }
    Ok(pairs)
}

/// The longest page template name, in bytes.
pub const MAX_TEMPLATE_NAME: usize = 128;

/// Whether `name` can name a page template: one or two segments joined by `/` (a template in
/// the theme's folder, or in one folder below it), each a run of ASCII letters, digits, `-` and
/// `_`, as [`Theme::locate_template`](super::Theme::locate_template) finds a file; at most
/// [`MAX_TEMPLATE_NAME`] bytes.
pub(crate) fn is_template_name(name: &str) -> bool {
    let segments: Vec<&str> = name.split('/').collect();
    name.len() <= MAX_TEMPLATE_NAME
        && segments.len() <= 2
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Whether `id` can name a menu location or widget area: a Propel path segment (an ASCII
/// letter or `_`, then letters, digits and `_`), lowercase, at most [`MAX_AREA_ID`] characters.
pub(crate) fn is_area_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    id.len() <= MAX_AREA_ID
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The longest key or `supports` value repeated in an error message.
const MAX_ECHOED_NAME: usize = 64;

/// What is wrong with a manifest and where, without echoing a value from the file: the text
/// could be anything, and messages end up in logs. A key or `supports` value is named only when
/// it is a short slug; a wrong type gives only the type expected. The plugin manifest uses it too.
pub(crate) fn describe(error: &toml::de::Error, text: &str) -> String {
    let message = error.message();
    let what = if let Some(key) = quoted_after(message, "unknown field `") {
        match echoable(key) {
            Some(key) => format!("unknown key `{key}`"),
            None => "an unknown key".to_owned(),
        }
    } else if let Some(value) = quoted_after(message, "unknown variant `") {
        match echoable(value) {
            Some(value) => format!("unknown `supports` value `{value}`"),
            None => "an unknown `supports` value".to_owned(),
        }
    } else if message.starts_with("invalid type:") {
        // serde writes "invalid type: <value>, expected <type>"; only the type is kept.
        match message.rsplit_once(", expected ") {
            Some((_, expected)) => format!("a value has the wrong type (expected {expected})"),
            None => "a value has the wrong type".to_owned(),
        }
    } else if message.starts_with("duplicate key") {
        "a key is written twice".to_owned()
    } else {
        "the TOML is not valid".to_owned()
    };
    match error
        .span()
        .and_then(|span| line_and_column(text, span.start))
    {
        Some((line, column)) => format!("{what} at line {line}, column {column}"),
        None => what,
    }
}

/// The text between `prefix` and the next backtick in `message`, if `message` starts with it.
fn quoted_after<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    message
        .strip_prefix(prefix)?
        .split_once('`')
        .map(|(name, _)| name)
}

/// `name` if it is safe to repeat: a short run of ASCII letters, digits, `-` and `_`.
fn echoable(name: &str) -> Option<&str> {
    (!name.is_empty()
        && name.len() <= MAX_ECHOED_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(name)
}

/// The 1-based line and column (in characters) of byte `offset` in `text`.
fn line_and_column(text: &str, offset: usize) -> Option<(usize, usize)> {
    let before = text.get(..offset)?;
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Some((line, before[line_start..].chars().count() + 1))
}
