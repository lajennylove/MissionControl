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
//! ```
//!
//! The theme's slug is the name of its folder, as in WordPress; the manifest cannot set it.
//! Parsing is strict: an unknown key or an unknown `supports` value is refused by name, so a
//! typo does not pass silently.

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
    /// The features the theme supports (`add_theme_support`), in the order written.
    pub supports: Vec<Support>,
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
}

impl Manifest {
    /// Parses a manifest's text. `path` is the file it came from, for error messages.
    pub fn parse(text: &str, path: &Path) -> Result<Manifest, ThemeError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|error| ThemeError::InvalidManifest {
                path: path.to_path_buf(),
                message: error.message().to_owned(),
            })?;
        let name = raw.name.as_deref().map(str::trim).unwrap_or_default();
        if name.is_empty() {
            return Err(ThemeError::MissingName {
                path: path.to_path_buf(),
            });
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
            supports: raw.supports,
        })
    }

    /// Whether the theme declares `feature`, as `current_theme_supports`.
    pub fn supports(&self, feature: Support) -> bool {
        self.supports.contains(&feature)
    }
}
