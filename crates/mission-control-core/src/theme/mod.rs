//! Themes: which template renders a request, and the theme that has it.
//!
//! ```ignore
//! use mission_control_core::theme::{load_theme, templates_for};
//!
//! let theme = load_theme(Path::new("themes/orbit-one"))?;
//! // `single-post-hello-world`, `single-post`, `single`, `singular`, `index`
//! let candidates = templates_for(&resolved, None);
//! let found = theme.locate_template(&candidates).expect("every theme has an index");
//! let html = propel_runtime::render(&theme.loader(), &found.view, &context)?;
//! ```

mod hierarchy;
mod loader;
mod manifest;

pub use hierarchy::templates_for;
pub use loader::{LocatedTemplate, Theme, ThemeError, ThemeLoader, load_theme};
pub use manifest::{MANIFEST_FILE, MAX_MANIFEST_BYTES, Manifest, Support};
