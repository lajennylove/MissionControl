//! The WordPress admin theme: wp-admin's look around the panel's screens. Ignite's own theme
//! stays the default; a user chooses this theme on the profile page.
//!
//! - Its own layout and partials, in `views/admin/wordpress/` and built into the binary: the
//!   admin bar above the dark admin menu (each navigation group after a separator, without a
//!   heading, as WordPress separates its menu), the grey content area, WordPress's admin notices
//!   for the flash message, and WordPress's colours as Ignite's colour tokens, so the screens'
//!   buttons and links are WordPress's blue. WordPress has no dark mode: the theme says so with
//!   Ignite's `AdminTheme::dark_mode()` (Filament's `darkMode(false)`), so its pages render light,
//!   and its admin bar has no light or dark switch.
//! - Its own profile page, WordPress's `profile.php` with all its fields (see `admin::profile`):
//!   the theme's `profile` view holds the page chrome and a marker where Mission Control puts
//!   WordPress's profile form.
//! - Its own sign-in, lost password and reset pages, as wp-login.php looks, on a layout of their
//!   own. They post the same fields to the same actions as the panel's pages. A signed-out page
//!   renders in the site's default admin theme (see `ignite_panel::theme`), so they show while
//!   this theme is the site's default.
//! - Its own dashboard view: Ignite's dashboard widgets, the ones the default theme shows, in
//!   WordPress's dashboard boxes (`.postbox`, its heading in `.postbox-header`): the list
//!   widgets At a Glance and Activity in the first column, each row as WordPress lists it, and
//!   the form widget Quick Draft in the second, posting to the Posts screen's create. It reads
//!   the dashboard's widget data (`widgets`: each widget's title, rows, empty state, and a form's
//!   action, fields and hidden values), not Ignite's widget partials, so the boxes have
//!   WordPress's markup. The stat cards are left out: WordPress has none, and At a Glance shows
//!   the same counts.
//! - WordPress's Dashicons for the menu, with the Dashicons stylesheet loaded only while this
//!   theme is the page's theme.

use ignite_panel::theme::{AdminTheme, ColorToken, ThemeError};

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

/// The theme's layout: wp-admin's chrome around every signed-in page.
const LAYOUT: &str = "themes.wordpress.layouts.app";

/// The layout of the theme's sign-in, lost password and reset pages, as wp-login.php looks.
const LOGIN_LAYOUT: &str = "themes.wordpress.layouts.login";

/// The admin menu.
const NAVIGATION: &str = "themes.wordpress.partials.nav";

/// The admin bar.
const TOP_BAR: &str = "themes.wordpress.partials.topbar";

/// The flash message, as an admin notice.
const FLASH: &str = "themes.wordpress.partials.flash";

/// The theme's views by name, built into the binary. `dashboard` (the dashboard's widgets in
/// WordPress's boxes), `profile` (the page that holds WordPress's profile form, see
/// `admin::profile`), `login`, `forgot` and `reset` replace the panel's pages of those names.
const VIEWS: &[(&str, &str)] = &[
    (
        LAYOUT,
        include_str!("../../views/admin/wordpress/layouts/app.propel"),
    ),
    (
        LOGIN_LAYOUT,
        include_str!("../../views/admin/wordpress/layouts/login.propel"),
    ),
    (
        NAVIGATION,
        include_str!("../../views/admin/wordpress/partials/nav.propel"),
    ),
    (
        TOP_BAR,
        include_str!("../../views/admin/wordpress/partials/topbar.propel"),
    ),
    (
        FLASH,
        include_str!("../../views/admin/wordpress/partials/flash.propel"),
    ),
    (
        "dashboard",
        include_str!("../../views/admin/wordpress/dashboard.propel"),
    ),
    (
        "profile",
        include_str!("../../views/admin/wordpress/profile.propel"),
    ),
    (
        "login",
        include_str!("../../views/admin/wordpress/login.propel"),
    ),
    (
        "forgot",
        include_str!("../../views/admin/wordpress/forgot.propel"),
    ),
    (
        "reset",
        include_str!("../../views/admin/wordpress/reset.propel"),
    ),
];

/// WordPress's admin colours (its `common.css` and `forms.css`), as Ignite's colour tokens, so
/// the screens' own markup (buttons, links, cards, messages) takes them. The dark values are the
/// light ones: WordPress has no dark mode, and the theme's layout keeps the page light.
const COLORS: &[(&str, &str)] = &[
    ("primary", "#2271b1"),
    ("primary-emphasis", "#135e96"),
    ("lightprimary", "#f0f6fc"),
    ("secondary", "#50575e"),
    ("secondary-emphasis", "#3c434a"),
    ("lightsecondary", "#f6f7f7"),
    ("info", "#72aee6"),
    ("lightinfo", "#f0f6fc"),
    ("success", "#00a32a"),
    ("success-emphasis", "#008a20"),
    ("lightsuccess", "#edfaef"),
    ("warning", "#dba617"),
    ("warning-emphasis", "#996800"),
    ("lightwarning", "#fcf9e8"),
    ("error", "#d63638"),
    ("error-emphasis", "#b32d2e"),
    ("lighterror", "#fcf0f1"),
    ("surface", "#ffffff"),
    ("page", "#f0f0f1"),
    ("heading", "#1d2327"),
    ("text", "#3c434a"),
    ("link", "#2271b1"),
    ("border", "#c3c4c7"),
    ("bordergray", "#dcdcde"),
    ("code-bg", "#f6f7f7"),
];

/// WordPress's admin theme, registered by [`register`].
pub(crate) struct WordPressTheme;

impl AdminTheme for WordPressTheme {
    fn name(&self) -> &str {
        NAME
    }

    fn label(&self) -> String {
        "WordPress".to_owned()
    }

    /// WordPress has no dark mode: the pages render light, with no light or dark switch and no
    /// line about it on the profile page.
    fn dark_mode(&self) -> bool {
        false
    }

    fn layout(&self) -> &str {
        LAYOUT
    }

    fn navigation(&self) -> &str {
        NAVIGATION
    }

    fn top_bar(&self) -> &str {
        TOP_BAR
    }

    fn flash(&self) -> &str {
        FLASH
    }

    fn colors(&self) -> Vec<ColorToken> {
        COLORS
            .iter()
            .map(|(name, value)| ColorToken::new(*name, *value, *value))
            .collect()
    }

    fn view(&self, name: &str) -> Option<String> {
        VIEWS
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, source)| (*source).to_owned())
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
    fn the_theme_has_its_own_layout_and_partials() {
        assert_ne!(WordPressTheme.layout(), ignite_panel::theme::IGNITE_LAYOUT);
        assert_ne!(
            WordPressTheme.navigation(),
            ignite_panel::theme::IGNITE_NAVIGATION
        );
        assert_ne!(
            WordPressTheme.top_bar(),
            ignite_panel::theme::IGNITE_TOP_BAR
        );
        assert_ne!(WordPressTheme.flash(), ignite_panel::theme::IGNITE_FLASH);
        for name in [
            WordPressTheme.layout(),
            WordPressTheme.navigation(),
            WordPressTheme.top_bar(),
            WordPressTheme.flash(),
            LOGIN_LAYOUT,
        ] {
            assert!(WordPressTheme.view(name).is_some(), "{name}");
        }
        // The panel's other views are its own.
        assert_eq!(WordPressTheme.view("index"), None);
        assert_eq!(WordPressTheme.view("partials.field"), None);
    }

    #[test]
    fn the_sign_in_page_is_wp_logins() {
        let login = WordPressTheme.view("login").expect("the sign-in page");
        assert!(login.contains(LOGIN_LAYOUT), "{login}");
        for expected in [
            "<label for=\"user_login\">Email Address</label>",
            "name=\"email\"",
            "<label for=\"user_pass\">Password</label>",
            "name=\"password\"",
            "name=\"_token\" value=\"{{ $csrf_token }}\"",
            "action=\"{{ $action }}\"",
            "value=\"Log In\"",
            ">Lost your password?</a>",
        ] {
            assert!(login.contains(expected), "{expected}");
        }
        let layout = WordPressTheme
            .view(LOGIN_LAYOUT)
            .expect("the sign-in layout");
        assert!(layout.contains("background: #f0f0f1"), "{layout}");
        assert!(layout.contains("background: #2271b1"), "{layout}");
    }

    #[test]
    fn the_colours_are_wordpresss() {
        let colors = WordPressTheme.colors();
        assert!(colors.contains(&ColorToken::new("primary", "#2271b1", "#2271b1")));
        assert!(colors.contains(&ColorToken::new("page", "#f0f0f1", "#f0f0f1")));
    }

    #[test]
    fn a_button_link_keeps_its_white_text() {
        let layout = WordPressTheme.view(LAYOUT).expect("the layout");
        // WordPress's link colour skips the buttons, so "New Post" is white on the blue, as
        // WordPress's .page-title-action.
        assert!(
            layout.contains("body.wp-admin a:not(.ig-btn) { color: #2271b1; }"),
            "{layout}"
        );
        assert!(
            layout.contains("body.wp-admin a.ig-btn-primary, body.wp-admin a.ig-btn-primary:hover, body.wp-admin a.ig-btn-danger { color: #fff; }"),
            "{layout}"
        );
        assert!(!layout.contains("body.wp-admin a { color"), "{layout}");
    }

    #[test]
    fn the_admin_bar_shows_the_avatar_or_the_initials() {
        let bar = WordPressTheme.view(TOP_BAR).expect("the admin bar");
        assert!(
            bar.contains("@if (has_avatar)<img src=\"{{ $avatar_url }}\" alt=\"\" class=\"avatar avatar-26\">@else<span class=\"avatar avatar-26 avatar-initials\" aria-hidden=\"true\">{{ $user_initials }}</span>@endif"),
            "{bar}"
        );
        let layout = WordPressTheme.view(LAYOUT).expect("the layout");
        assert!(
            layout.contains(
                "#wpadminbar .avatar-initials { align-items: center; background: #c3c4c7;"
            ),
            "{layout}"
        );
    }

    #[test]
    fn the_howdy_menu_opens_without_hover() {
        let bar = WordPressTheme.view(TOP_BAR).expect("the admin bar");
        // A button opens the menu on a click; its script sets WordPress's .hover class.
        assert!(
            bar.contains("<button type=\"button\" class=\"ab-item\" aria-haspopup=\"true\" aria-expanded=\"false\" aria-controls=\"wp-admin-bar-user-actions\">Howdy, "),
            "{bar}"
        );
        assert!(
            bar.contains("item.classList.toggle('hover', open);"),
            "{bar}"
        );
        // The keyboard's focus opens it too, and it stays open while the focus is in it.
        assert!(
            bar.contains("if (event.target.matches(':focus-visible')) show(true);"),
            "{bar}"
        );
        assert!(
            bar.contains("if (!item.contains(event.relatedTarget)) show(false);"),
            "{bar}"
        );
        for expected in [">Edit Profile</a>", ">Log Out</button>"] {
            assert!(bar.contains(expected), "{expected}");
        }
        let layout = WordPressTheme.view(LAYOUT).expect("the layout");
        assert!(
            layout.contains("#wpadminbar .menupop:hover > .ab-sub-wrapper, #wpadminbar .menupop.hover > .ab-sub-wrapper { display: block; }"),
            "{layout}"
        );
    }

    #[test]
    fn the_theme_has_no_dark_mode_and_its_own_profile_page() {
        assert!(!WordPressTheme.dark_mode());
        assert!(!WordPressTheme.theme_switcher());
        assert_eq!(WordPressTheme.theme_switcher_note(), None);
        let bar = WordPressTheme.view(TOP_BAR).expect("the admin bar");
        assert!(!bar.contains("data-theme-toggle"), "{bar}");
        let profile = WordPressTheme.view("profile").expect("the profile page");
        assert!(profile.contains(super::super::profile::MARKER), "{profile}");
    }

    #[test]
    fn the_logo_sits_on_a_light_panel_in_the_dark_bar() {
        let bar = WordPressTheme.view(TOP_BAR).expect("the admin bar");
        assert!(
            bar.contains("<span class=\"mc-wp-brand-panel\"><img"),
            "{bar}"
        );
        let layout = WordPressTheme.view(LAYOUT).expect("the layout");
        assert!(
            layout.contains("#wpadminbar .mc-wp-brand-panel { background: #fff;"),
            "{layout}"
        );
    }

    #[test]
    fn only_this_theme_loads_the_dashicons_stylesheet() {
        let stylesheets = WordPressTheme.stylesheets();
        assert_eq!(stylesheets, vec![dashicons_stylesheet_url()]);
    }
}
