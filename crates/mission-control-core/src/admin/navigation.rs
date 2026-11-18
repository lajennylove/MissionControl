//! The admin sidebar in WordPress's menu order (plan, "Admin menu order and groups"; source:
//! WordPress's `wp-admin/menu.php`).
//!
//! WordPress orders its admin menu by `menu_position`: Dashboard (2), a separator (4), Posts (5),
//! Media (10), Links (15, hidden), Pages (20), Comments (25), a separator (59), Appearance (60),
//! Plugins (65), Users (70), Tools (75), Settings (80) and a separator (99). Ignite's sidebar
//! lists the dashboard first, then its navigation groups, each sorted by `navigation_sort`, so
//! each screen here gets:
//!
//! - its group: [`CONTENT`] for an entry above WordPress's separator at 59, [`SITE`] for one at
//!   or below it ([`group_for`]);
//! - its WordPress position as the sort;
//! - WordPress's Dashicons class for it (`dashicons dashicons-admin-post` and so on; the base
//!   class `dashicons` is needed). Ignite ships no icon font: the app serves
//!   WordPress's Dashicons stylesheet and registers it with Ignite's `stylesheet`.
//!
//! A custom post type takes its `menu_position`, or the next free position after Comments, as
//! WordPress's `$_wp_last_object_menu` gives it, and a position already taken moves down to the
//! next free one, as WordPress's menu does ([`PostTypePositions`]). Ignite lists every resource
//! at the top level, with no submenus, so a type that WordPress keeps out of the top level
//! (`show_in_menu` false, or a parent menu's slug) is listed last in Content ([`NOT_IN_MENU`]).

use ignite_core::RuntimeResource;

use crate::types::{PostTypeDef, ShowInMenu};

/// The group of the content screens: Posts, Media, Pages, Comments and the custom post types.
pub const CONTENT: &str = "Content";

/// The group of the site screens: Appearance (Menus), Plugins, Users, Tools and Settings.
pub const SITE: &str = "Site";

/// The groups in WordPress's order, for Ignite's `navigation_groups`.
pub const GROUPS: [&str; 2] = [CONTENT, SITE];

/// WordPress's separator between the content and the site screens (`$menu[59]`).
const SITE_SEPARATOR: i32 = 59;

/// WordPress's positions (`wp-admin/menu.php`).
pub const POSTS: i32 = 5;
/// Media's position.
pub const MEDIA: i32 = 10;
/// Pages' position.
pub const PAGES: i32 = 20;
/// Comments' position, after which the custom post types go by default.
pub const COMMENTS: i32 = 25;
/// Appearance's position; the Menus screen is in Appearance.
pub const APPEARANCE: i32 = 60;
/// Plugins' position.
pub const PLUGINS: i32 = 65;
/// Users' position.
pub const USERS: i32 = 70;
/// Tools' position.
pub const TOOLS: i32 = 75;
/// Settings' position. The settings screens take 80 to 86, in WordPress's submenu order.
pub const SETTINGS: i32 = 80;

/// The positions a custom post type does not take: WordPress's `$core_menu_positions` (59, 60,
/// 65, 70, 75, 80, 85, 99) and the entries `$menu` already holds when the post types are added
/// (Dashboard 2, its separator 4, Posts 5, Media 10, Pages 20, Comments 25; Links, 15, is off by
/// default).
const CORE_POSITIONS: [i32; 14] = [2, 4, 5, 10, 20, 25, 59, 60, 65, 70, 75, 80, 85, 99];

/// Where a post type that is not in the menu (`show_in_menu` false, or under another menu) is
/// listed: Ignite lists every resource, so it goes last in the Content group, and takes no
/// position from the others.
pub const NOT_IN_MENU: i32 = 58;

/// WordPress's default post type icon (`dashicons-admin-post`), with the base class.
pub const POST_ICON: &str = "dashicons dashicons-admin-post";
/// Media's icon.
pub const MEDIA_ICON: &str = "dashicons dashicons-admin-media";
/// Pages' icon.
pub const PAGE_ICON: &str = "dashicons dashicons-admin-page";
/// Comments' icon.
pub const COMMENTS_ICON: &str = "dashicons dashicons-admin-comments";
/// Appearance's icon, which the Menus screen shows.
pub const APPEARANCE_ICON: &str = "dashicons dashicons-admin-appearance";
/// Plugins' icon.
pub const PLUGINS_ICON: &str = "dashicons dashicons-admin-plugins";
/// Users' icon.
pub const USERS_ICON: &str = "dashicons dashicons-admin-users";
/// Tools' icon.
pub const TOOLS_ICON: &str = "dashicons dashicons-admin-tools";
/// Settings' icon.
pub const SETTINGS_ICON: &str = "dashicons dashicons-admin-settings";

/// The group of an entry at `position`: [`CONTENT`] above WordPress's separator at 59, [`SITE`]
/// from it on.
pub fn group_for(position: i32) -> &'static str {
    if position < SITE_SEPARATOR {
        CONTENT
    } else {
        SITE
    }
}

/// Places `resource` in the sidebar at `position`, with `icon`.
pub fn place(resource: &mut RuntimeResource, position: i32, icon: &str) {
    resource.navigation_group = Some(group_for(position).to_owned());
    resource.navigation_sort = Some(position);
    resource.navigation_icon = Some(icon.to_owned());
}

/// The Dashicons class of a post type's `menu_icon`: a Dashicons name (`dashicons-calendar`)
/// with the base class. WordPress also takes an image URL, a base64 SVG or `none`; Ignite's
/// navigation takes CSS classes only, so those show the default post icon.
pub fn post_type_icon(def: &PostTypeDef) -> String {
    match def.menu_icon.as_deref().map(str::trim) {
        Some(icon)
            if icon.starts_with("dashicons-")
                && icon.len() > "dashicons-".len()
                && icon
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            format!("dashicons {icon}")
        }
        _ => POST_ICON.to_owned(),
    }
}

/// The menu positions of the post types, given out as WordPress's `wp-admin/menu.php` does:
/// `post` at 5 and `page` at 20; a custom type at its `menu_position`, or else after Comments
/// (26, 27, ... in the order the types come); a position that is WordPress's own or already
/// given moves down to the next free one.
#[derive(Debug, Clone)]
pub struct PostTypePositions {
    /// The last default position given (`$_wp_last_object_menu`).
    last_object: i32,
    /// The positions given to custom types so far.
    taken: Vec<i32>,
}

impl Default for PostTypePositions {
    fn default() -> Self {
        Self {
            last_object: COMMENTS,
            // The settings screens after the first (81 to 86), which WordPress's Settings menu
            // holds as one entry at 80.
            taken: (SETTINGS + 1..=SETTINGS + 6).collect(),
        }
    }
}

impl PostTypePositions {
    /// The position of `def`, the next type in order.
    pub fn next(&mut self, def: &PostTypeDef) -> i32 {
        match def.slug.as_str() {
            "post" => return POSTS,
            "page" => return PAGES,
            _ => {}
        }
        // WordPress adds only the types with `show_in_menu` true to the top level.
        if def.effective_show_in_menu() != ShowInMenu::Show(true) {
            return NOT_IN_MENU;
        }
        let mut position = match def.menu_position {
            Some(position) => position,
            None => {
                self.last_object += 1;
                self.last_object
            }
        };
        while (CORE_POSITIONS.contains(&position) || self.taken.contains(&position))
            && position < i32::MAX
        {
            position += 1;
        }
        self.taken.push(position);
        position
    }
}

/// Places the resource of the post type `def` at `position`: `post` and `page` with their own
/// icons, a custom type with its `menu_icon` ([`post_type_icon`]).
pub fn place_post_type(resource: &mut RuntimeResource, def: &PostTypeDef, position: i32) {
    let icon = match def.slug.as_str() {
        "page" => PAGE_ICON.to_owned(),
        "post" => POST_ICON.to_owned(),
        _ => post_type_icon(def),
    };
    place(resource, position, &icon);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(slug: &str, position: Option<i32>, icon: Option<&str>) -> PostTypeDef {
        PostTypeDef {
            menu_position: position,
            menu_icon: icon.map(str::to_owned),
            public: true,
            ..PostTypeDef::new(slug, slug, slug)
        }
    }

    #[test]
    fn groups_split_at_wordpress_separator() {
        assert_eq!(group_for(POSTS), CONTENT);
        assert_eq!(group_for(58), CONTENT);
        assert_eq!(group_for(59), SITE);
        assert_eq!(group_for(SETTINGS), SITE);
        assert_eq!(group_for(120), SITE);
    }

    #[test]
    fn custom_types_follow_comments_and_skip_taken_positions() {
        let mut positions = PostTypePositions::default();
        assert_eq!(positions.next(&def("post", None, None)), POSTS);
        assert_eq!(positions.next(&def("page", None, None)), PAGES);
        assert_eq!(positions.next(&def("book", None, None)), 26);
        assert_eq!(positions.next(&def("movie", None, None)), 27);
        // 5 is Posts': the next free one is 6.
        assert_eq!(positions.next(&def("event", Some(5), None)), 6);
        // 6 is now taken too.
        assert_eq!(positions.next(&def("venue", Some(5), None)), 7);
        // 59 and 60 are WordPress's: 61.
        assert_eq!(positions.next(&def("shop", Some(59), None)), 61);
        // Links (15) is off, so 15 is free; 81 to 85 are the settings screens' and WordPress's.
        assert_eq!(positions.next(&def("links", Some(15), None)), 15);
        assert_eq!(positions.next(&def("late", Some(81), None)), 87);
        // A type that is not in the menu takes no position.
        let hidden = PostTypeDef {
            show_in_menu: Some(ShowInMenu::Show(false)),
            ..def("hidden", None, None)
        };
        assert_eq!(positions.next(&hidden), NOT_IN_MENU);
        let under = PostTypeDef {
            show_in_menu: Some(ShowInMenu::Under("tools.php".to_owned())),
            ..def("under", None, None)
        };
        assert_eq!(positions.next(&under), NOT_IN_MENU);
        assert_eq!(positions.next(&def("next", None, None)), 28);
        // No overflow at the end of the range.
        assert_eq!(positions.next(&def("max", Some(i32::MAX), None)), i32::MAX);
        assert_eq!(positions.next(&def("max2", Some(i32::MAX), None)), i32::MAX);
    }

    #[test]
    fn icons_are_dashicons_classes() {
        assert_eq!(
            post_type_icon(&def("book", None, Some("dashicons-book"))),
            "dashicons dashicons-book"
        );
        assert_eq!(post_type_icon(&def("book", None, None)), POST_ICON);
        assert_eq!(
            post_type_icon(&def("book", None, Some("https://example.com/i.png"))),
            POST_ICON
        );
        assert_eq!(post_type_icon(&def("book", None, Some("none"))), POST_ICON);
        assert_eq!(
            post_type_icon(&def("book", None, Some("dashicons-x\" onclick"))),
            POST_ICON
        );
    }
}
