//! The WordPress admin theme: WordPress's Dashicons for the sidebar, with the Dashicons
//! stylesheet loaded only while this theme is active. Ignite's own theme stays the default and
//! keeps its Tabler icons; a user chooses this theme on the profile page.
//!
//! The layout is Ignite's for now: WordPress's sign-in layout and admin chrome are still to be
//! built as this theme's own views.

use ignite_panel::theme::{AdminTheme, IGNITE_LAYOUT, ThemeError};

use crate::editor::dashicons_stylesheet_url;

/// The theme's name, as the profile page and the users' settings store it.
pub(crate) const NAME: &str = "wordpress";

/// WordPress's Dashicons for each Ignite icon name the admin uses (see
/// `ignite_panel::theme::icons`). The names not listed here show the theme's resource icon.
const DASHICONS: &[(&str, &str)] = &[
    ("dashboard", "dashicons dashicons-dashboard"),
    ("post", "dashicons dashicons-admin-post"),
    ("page", "dashicons dashicons-admin-page"),
    ("media", "dashicons dashicons-admin-media"),
    ("comment", "dashicons dashicons-admin-comments"),
    ("appearance", "dashicons dashicons-admin-appearance"),
    ("plugins", "dashicons dashicons-admin-plugins"),
    ("users", "dashicons dashicons-admin-users"),
    ("tools", "dashicons dashicons-admin-tools"),
    ("settings", "dashicons dashicons-admin-settings"),
];

/// WordPress's generic icon, for a resource without one.
const RESOURCE_ICON: &str = "dashicons dashicons-admin-generic";

/// WordPress's admin theme, registered by [`register`].
pub(crate) struct WordPressTheme;

impl AdminTheme for WordPressTheme {
    fn name(&self) -> &str {
        NAME
    }

    fn label(&self) -> String {
        "WordPress".to_owned()
    }

    fn layout(&self) -> &str {
        IGNITE_LAYOUT
    }

    fn stylesheets(&self) -> Vec<String> {
        vec![dashicons_stylesheet_url()]
    }

    fn icon(&self, name: &str) -> Option<String> {
        if let Some((_, classes)) = DASHICONS.iter().find(|(known, _)| *known == name) {
            return Some((*classes).to_owned());
        }
        // A custom post type's own Dashicons name (`dashicons-calendar`, see
        // `mission_control_core::admin::navigation::post_type_icon`).
        let dashicon = name.strip_prefix("dashicons-")?;
        (!dashicon.is_empty()
            && dashicon
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| format!("dashicons {name}"))
    }

    fn resource_icon(&self) -> String {
        RESOURCE_ICON.to_owned()
    }
}

/// Registers the WordPress theme with Ignite. Registering it again, as a second app in the same
/// process does, is not an error.
pub(crate) fn register() {
    match ignite_panel::theme::register_admin_theme(WordPressTheme) {
        Ok(()) | Err(ThemeError::AlreadyRegistered(_)) => {}
        Err(e) => log::error!("The WordPress admin theme is not available: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidebar_names_map_to_dashicons() {
        assert_eq!(
            WordPressTheme.icon("post").as_deref(),
            Some("dashicons dashicons-admin-post")
        );
        assert_eq!(
            WordPressTheme.icon("settings").as_deref(),
            Some("dashicons dashicons-admin-settings")
        );
    }

    #[test]
    fn a_custom_types_dashicon_is_its_own_class() {
        assert_eq!(
            WordPressTheme.icon("dashicons-calendar").as_deref(),
            Some("dashicons dashicons-calendar")
        );
    }

    #[test]
    fn an_unknown_or_unsafe_name_shows_the_resource_icon() {
        assert_eq!(WordPressTheme.icon("unknown"), None);
        assert_eq!(WordPressTheme.icon("dashicons-"), None);
        assert_eq!(WordPressTheme.icon("dashicons-x\" onclick"), None);
        assert_eq!(WordPressTheme.resource_icon(), RESOURCE_ICON);
    }

    #[test]
    fn only_this_theme_loads_the_dashicons_stylesheet() {
        let stylesheets = WordPressTheme.stylesheets();
        assert_eq!(stylesheets, vec![dashicons_stylesheet_url()]);
    }
}
