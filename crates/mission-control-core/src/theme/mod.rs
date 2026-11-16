//! Themes: which template renders a request, and the theme that has it; the theme's navigation
//! menus and widget areas, stored as WordPress stores them.
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
mod menus;
mod request;
mod widgets;

pub(crate) use hierarchy::page_template_name;
pub use hierarchy::templates_for;
pub use loader::{LocatedTemplate, Theme, ThemeError, ThemeLoader, load_theme};
pub(crate) use manifest::describe as describe_toml_error;
pub use manifest::{
    MANIFEST_FILE, MAX_AREA_ID, MAX_MANIFEST_BYTES, MAX_TEMPLATE_NAME, Manifest, MenuLocation,
    PageTemplate, Sidebar, Support,
};
pub use menus::{
    MAX_MENU_DEPTH, MenuError, MenuItemObject, NAV_MENU_ITEM, NAV_MENU_LOCATIONS, NavMenu,
    NavMenuItem, NewMenuItem, add_menu_item, create_nav_menu, menu_locations, nav_menu,
    set_menu_location, theme_menus, theme_mods_option,
};
pub use request::PageRequest;
pub use widgets::{
    RESERVED_AREAS, SIDEBARS_WIDGETS, SidebarView, WIDGET_TYPES, WidgetError, WidgetView,
    save_widget, set_sidebar_widgets, sidebars_widgets, theme_sidebars,
};
