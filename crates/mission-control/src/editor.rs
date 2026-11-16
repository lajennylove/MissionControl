//! The classic editor of the admin (Task 6.3, design 5.4): TinyMCE with WordPress's toolbar, the
//! Visual and Text tabs and the Add Media button, for every rich text (`Wysiwyg`) field of
//! Ignite's forms, the post content included.
//!
//! # The files
//!
//! The editor is a fixed set of files in the repository's `assets/editor/` folder, compiled into
//! the binary, so a site's own binary and the container image need no extra folder:
//!
//! - `editor.js`, Mission Control's script. Ignite's form loads it (it is registered with
//!   [`register_editor`](ignite_panel::ignite_core::register_editor) when the panel mounts); it
//!   loads TinyMCE and turns each `textarea[data-ignite-editor="wysiwyg"]` into the editor.
//! - `media.js`, the Add Media picker (Task 6.4), which `editor.js` loads: it uploads a file or
//!   picks one from the media library, and inserts it as WordPress's editor does.
//! - `editor.css`, the tabs, the Add Media button, the picker and the media library's upload
//!   page.
//! - `tinymce/`, the parts of TinyMCE 7.9.3 (GPL-2.0-or-later, see `NOTICE`) the toolbar uses,
//!   in TinyMCE's own folder layout, since it loads its theme, model, icons, skins and plugins
//!   from its base URL. Ignite, which is MIT, bundles none of it.
//!
//! # Where they are served
//!
//! Under [`INCLUDES_PATH`] (`/mc-includes`), as WordPress serves them under `/wp-includes`:
//! `js/editor.js`, `js/media.js`, `css/editor.css` and `js/tinymce/...` (WordPress's
//! `/wp-includes/js/tinymce/`). The path is outside the panel's `/mc-admin/<slug>` routes, and
//! the public site's catch-all ranks below it.
//!
//! - Only the files of the bundle are served: a path is looked up in a fixed table, never on the
//!   disk, so `..` or an encoded separator cannot reach another file. Any other path under
//!   `/mc-includes` forwards, and the public site answers with the theme's 404 page.
//! - Each file has its `Content-Type` (`text/javascript` or `text/css`, UTF-8; the licence
//!   files as plain text) and `X-Content-Type-Options: nosniff`.
//! - Each URL the editor asks for carries `?ver=` [`editor_version`] (WordPress's `?ver=` and
//!   TinyMCE's `cache_suffix`), which changes whenever a file of the bundle changes. A request
//!   with the current version is cached for a year (`immutable`); any other is sent with
//!   `no-cache`, so a stale URL is never kept.

use std::sync::OnceLock;

use rocket::http::{ContentType, Header, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::{self, Responder};
use rocket::{Request, Response, Route};
use sha2::{Digest, Sha256};

/// Where the editor's files are served, as WordPress serves `/wp-includes`.
pub const INCLUDES_PATH: &str = "/mc-includes";

/// The version of TinyMCE in the bundle (Ruling R126).
pub const TINYMCE_VERSION: &str = "7.9.3";

/// The editor script's path under [`INCLUDES_PATH`].
const SCRIPT: &str = "js/editor.js";

/// How long a file requested with the current version is cached: a year.
const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// One file of the bundle: its path under [`INCLUDES_PATH`] and its bytes.
struct Asset {
    path: &'static str,
    bytes: &'static [u8],
}

/// The bundle, from `assets/editor/` at the repository's root: `path` under
/// [`INCLUDES_PATH`] => the file under `assets/editor/`.
macro_rules! bundle {
    ($($path:literal => $file:literal),* $(,)?) => {
        &[$(Asset {
            path: $path,
            bytes: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../assets/editor/",
                $file
            )),
        }),*]
    };
}

/// Every file served under [`INCLUDES_PATH`]: Mission Control's editor script, picker and
/// stylesheet, and the parts of TinyMCE that the settings in `editor.js` load: the silver
/// theme, the DOM model, the default icons, the oxide skin, the default content style and the
/// `charmap`, `fullscreen`, `help` (with its English keyboard help), `link` and `lists`
/// plugins.
static BUNDLE: &[Asset] = bundle! {
    "js/editor.js" => "editor.js",
    "js/media.js" => "media.js",
    "css/editor.css" => "editor.css",
    "js/tinymce/tinymce.min.js" => "tinymce/tinymce.min.js",
    "js/tinymce/license.md" => "tinymce/license.md",
    "js/tinymce/notices.txt" => "tinymce/notices.txt",
    "js/tinymce/models/dom/model.min.js" => "tinymce/models/dom/model.min.js",
    "js/tinymce/themes/silver/theme.min.js" => "tinymce/themes/silver/theme.min.js",
    "js/tinymce/icons/default/icons.min.js" => "tinymce/icons/default/icons.min.js",
    "js/tinymce/skins/ui/oxide/skin.min.css" => "tinymce/skins/ui/oxide/skin.min.css",
    "js/tinymce/skins/ui/oxide/content.min.css" => "tinymce/skins/ui/oxide/content.min.css",
    "js/tinymce/skins/ui/oxide/skin.shadowdom.min.css" =>
        "tinymce/skins/ui/oxide/skin.shadowdom.min.css",
    "js/tinymce/skins/content/default/content.min.css" =>
        "tinymce/skins/content/default/content.min.css",
    "js/tinymce/plugins/charmap/plugin.min.js" => "tinymce/plugins/charmap/plugin.min.js",
    "js/tinymce/plugins/fullscreen/plugin.min.js" => "tinymce/plugins/fullscreen/plugin.min.js",
    "js/tinymce/plugins/help/plugin.min.js" => "tinymce/plugins/help/plugin.min.js",
    "js/tinymce/plugins/help/js/i18n/keynav/en.js" =>
        "tinymce/plugins/help/js/i18n/keynav/en.js",
    "js/tinymce/plugins/link/plugin.min.js" => "tinymce/plugins/link/plugin.min.js",
    "js/tinymce/plugins/lists/plugin.min.js" => "tinymce/plugins/lists/plugin.min.js",
};

/// The version every editor URL carries: TinyMCE's version and the start of a SHA-256 of the
/// whole bundle (paths and bytes), so it changes whenever a file does.
pub fn editor_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut hasher = Sha256::new();
        for asset in BUNDLE {
            hasher.update(asset.path.as_bytes());
            hasher.update([0]);
            hasher.update((asset.bytes.len() as u64).to_le_bytes());
            hasher.update(asset.bytes);
        }
        let digest: String = hasher
            .finalize()
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("{TINYMCE_VERSION}-{digest}")
    })
}

/// The URL of the editor script that Ignite's forms load: `/mc-includes/js/editor.js?ver=...`.
pub fn editor_script_url() -> String {
    format!("{INCLUDES_PATH}/{SCRIPT}?ver={}", editor_version())
}

/// The URL of the editor's stylesheet, `/mc-includes/css/editor.css?ver=...`, which also styles
/// the Add Media picker and the media library's upload page.
pub(crate) fn editor_stylesheet_url() -> String {
    format!("{INCLUDES_PATH}/css/editor.css?ver={}", editor_version())
}

/// The `Content-Type` of a file of the bundle, from its extension.
fn content_type(path: &str) -> ContentType {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("js") => ContentType::new("text", "javascript").with_params(("charset", "utf-8")),
        Some("css") => ContentType::CSS,
        _ => ContentType::Plain,
    }
}

/// A file of the bundle, found for the request's path.
pub(crate) struct EditorFile {
    asset: &'static Asset,
    current: bool,
}

/// The file of the bundle at `segments` (percent-decoded, after `/mc-includes`), if any: an
/// exact match of the joined segments, so no path outside the table is ever read.
fn find(segments: &[&str]) -> Option<&'static Asset> {
    if segments.is_empty()
        || segments
            .iter()
            .any(|s| s.is_empty() || s.contains(['/', '\\']))
    {
        return None;
    }
    let path = segments.join("/");
    BUNDLE.iter().find(|asset| asset.path == path)
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for EditorFile {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        // The route's own `/mc-includes`, then the file's path.
        let segments: Vec<&str> = request.uri().path().segments().skip(1).collect();
        let Some(asset) = find(&segments) else {
            return Outcome::Forward(Status::NotFound);
        };
        let current = request
            .query_value::<&str>("ver")
            .and_then(Result::ok)
            .is_some_and(|ver| ver == editor_version());
        Outcome::Success(Self { asset, current })
    }
}

impl<'r> Responder<'r, 'static> for EditorFile {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let cache = if self.current {
            CACHE_IMMUTABLE
        } else {
            "no-cache"
        };
        Response::build()
            .status(Status::Ok)
            .header(content_type(self.asset.path))
            .header(Header::new("X-Content-Type-Options", "nosniff"))
            .header(Header::new("Cache-Control", cache))
            .sized_body(
                self.asset.bytes.len(),
                std::io::Cursor::new(self.asset.bytes),
            )
            .ok()
    }
}

/// The route that serves the bundle.
pub(crate) fn routes() -> Vec<Route> {
    rocket::routes![editor_file]
}

/// A file of the editor's bundle.
#[rocket::get("/mc-includes/<_..>")]
fn editor_file(file: EditorFile) -> EditorFile {
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_files_of_the_bundle_are_found() {
        assert!(find(&["js", "editor.js"]).is_some());
        assert!(find(&["js", "tinymce", "tinymce.min.js"]).is_some());
        for refused in [
            &[][..],
            &["js"],
            &["js", "..", "js", "editor.js"],
            &["js", "tinymce", "..", "..", "..", "Cargo.toml"],
            &["js/editor.js"],
            &["js", "", "editor.js"],
            &["js", "tinymce", "plugins", "table", "plugin.min.js"],
        ] {
            assert!(find(refused).is_none(), "{refused:?}");
        }
    }

    #[test]
    fn the_version_names_tinymce_and_the_bundle() {
        let version = editor_version();
        assert!(version.starts_with("7.9.3-"), "{version}");
        assert_eq!(version.len(), "7.9.3-".len() + 12);
        assert_eq!(
            editor_script_url(),
            format!("/mc-includes/js/editor.js?ver={version}")
        );
    }

    #[test]
    fn each_file_has_its_type() {
        assert_eq!(
            content_type("js/editor.js").to_string(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type("css/editor.css").to_string(),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type("js/tinymce/license.md").to_string(),
            "text/plain; charset=utf-8"
        );
    }
}
