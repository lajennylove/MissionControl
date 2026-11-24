//! WordPress's default roles and their capabilities, and the capabilities of one user.
//!
//! The roles are data here: the five roles of a fresh single-site WordPress, with exactly the
//! capabilities `populate_roles` gives them (WordPress 1.6 to 3.0, which later versions keep),
//! including the legacy `level_0` to `level_10` capabilities, which old plugins still check.
//!
//! **Where roles will be stored.** Nothing is stored yet. Roles become Ignite roles and
//! capabilities Ignite permissions (Spatie's shape, design section 5.6) once Ignite's tables run
//! on every database (Task 8.2b, after Ignite 3a.3). Then the stored roles are read into
//! [`Role`] values and a user's capabilities are built with [`UserCaps::from_roles`], as here.
//!
//! A user with only the `subscriber` role holds `read` and `level_0`: none of the capabilities
//! the content screens need (`edit_posts`, `edit_pages`, `upload_files`, ...), so they cannot
//! use the content admin. As in WordPress, `read` still lets them reach their own profile.

use std::collections::BTreeMap;

/// The `administrator` role: everything on a single site.
pub const ADMINISTRATOR: &str = "administrator";
/// The `editor` role: publishes and manages everyone's posts and pages.
pub const EDITOR: &str = "editor";
/// The `author` role: publishes and manages their own posts.
pub const AUTHOR: &str = "author";
/// The `contributor` role: writes their own posts, but cannot publish them.
pub const CONTRIBUTOR: &str = "contributor";
/// The `subscriber` role: reads, and manages their profile.
pub const SUBSCRIBER: &str = "subscriber";

/// The five WordPress roles, as the admin makes sure they exist as Ignite roles (Ruling R121)
/// and the installer creates them before it gives the first user `administrator`.
pub const WORDPRESS_ROLES: [&str; 5] = [ADMINISTRATOR, EDITOR, AUTHOR, CONTRIBUTOR, SUBSCRIBER];

/// The capability nobody has, whatever their roles or own capabilities say. A meta capability
/// maps to it when the check cannot be allowed, such as `edit_post` with no post.
pub const DO_NOT_ALLOW: &str = "do_not_allow";
/// The capability everyone has, even a user with no role.
pub const EXIST: &str = "exist";

/// The capabilities of `populate_roles_160`, granted to the administrator.
const ADMINISTRATOR_160: &[&str] = &[
    "switch_themes",
    "edit_themes",
    "activate_plugins",
    "edit_plugins",
    "edit_users",
    "edit_files",
    "manage_options",
    "moderate_comments",
    "manage_categories",
    "manage_links",
    "upload_files",
    "import",
    "unfiltered_html",
    "edit_posts",
    "edit_others_posts",
    "edit_published_posts",
    "publish_posts",
    "edit_pages",
    "read",
    "level_10",
    "level_9",
    "level_8",
    "level_7",
    "level_6",
    "level_5",
    "level_4",
    "level_3",
    "level_2",
    "level_1",
    "level_0",
];

/// The capabilities of `populate_roles_160`, granted to the editor.
const EDITOR_160: &[&str] = &[
    "moderate_comments",
    "manage_categories",
    "manage_links",
    "upload_files",
    "unfiltered_html",
    "edit_posts",
    "edit_others_posts",
    "edit_published_posts",
    "publish_posts",
    "edit_pages",
    "read",
    "level_7",
    "level_6",
    "level_5",
    "level_4",
    "level_3",
    "level_2",
    "level_1",
    "level_0",
];

/// The capabilities `populate_roles_210` adds to both the administrator and the editor.
const ADMINISTRATOR_AND_EDITOR_210: &[&str] = &[
    "edit_others_pages",
    "edit_published_pages",
    "publish_pages",
    "delete_pages",
    "delete_others_pages",
    "delete_published_pages",
    "delete_posts",
    "delete_others_posts",
    "delete_published_posts",
    "delete_private_posts",
    "edit_private_posts",
    "read_private_posts",
    "delete_private_pages",
    "edit_private_pages",
    "read_private_pages",
];

/// The capabilities `populate_roles_210` to `populate_roles_300` add to the administrator alone.
const ADMINISTRATOR_210_TO_300: &[&str] = &[
    // 2.1
    "delete_users",
    "create_users",
    // 2.3
    "unfiltered_upload",
    // 2.5
    "edit_dashboard",
    // 2.6
    "update_plugins",
    "delete_plugins",
    // 2.7
    "install_plugins",
    "update_themes",
    // 2.8
    "install_themes",
    // 3.0
    "update_core",
    "list_users",
    "remove_users",
    "promote_users",
    "edit_theme_options",
    "delete_themes",
    "export",
];

/// The capabilities of the author: `populate_roles_160`, then `populate_roles_210`.
const AUTHOR_CAPS: &[&str] = &[
    "upload_files",
    "edit_posts",
    "edit_published_posts",
    "publish_posts",
    "read",
    "level_2",
    "level_1",
    "level_0",
    "delete_posts",
    "delete_published_posts",
];

/// The capabilities of the contributor: `populate_roles_160`, then `populate_roles_210`.
const CONTRIBUTOR_CAPS: &[&str] = &["edit_posts", "read", "level_1", "level_0", "delete_posts"];

/// The capabilities of the subscriber, from `populate_roles_160`.
const SUBSCRIBER_CAPS: &[&str] = &["read", "level_0"];

/// A role: a name and the capabilities it grants or denies, as WordPress's `WP_Role`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    /// The role's name, such as `editor`.
    pub name: String,
    /// The name shown to people, such as `Editor`.
    pub display_name: String,
    /// Each capability and whether the role grants (`true`) or denies (`false`) it.
    pub capabilities: BTreeMap<String, bool>,
}

impl Role {
    /// A role named `name`, shown as `display_name`, granting every capability in `granted`.
    fn granting(name: &str, display_name: &str, granted: &[&[&str]]) -> Self {
        Self {
            name: name.to_owned(),
            display_name: display_name.to_owned(),
            capabilities: granted
                .iter()
                .flat_map(|caps| caps.iter())
                .map(|cap| ((*cap).to_owned(), true))
                .collect(),
        }
    }
}

/// WordPress's five default roles, in the order `populate_roles` adds them: administrator,
/// editor, author, contributor, subscriber.
pub fn default_roles() -> Vec<Role> {
    vec![
        Role::granting(
            ADMINISTRATOR,
            "Administrator",
            &[
                ADMINISTRATOR_160,
                ADMINISTRATOR_AND_EDITOR_210,
                ADMINISTRATOR_210_TO_300,
            ],
        ),
        Role::granting(
            EDITOR,
            "Editor",
            &[EDITOR_160, ADMINISTRATOR_AND_EDITOR_210],
        ),
        Role::granting(AUTHOR, "Author", &[AUTHOR_CAPS]),
        Role::granting(CONTRIBUTOR, "Contributor", &[CONTRIBUTOR_CAPS]),
        Role::granting(SUBSCRIBER, "Subscriber", &[SUBSCRIBER_CAPS]),
    ]
}

/// The default role named `name`, or `None` when it is not one of the five.
pub fn default_role(name: &str) -> Option<Role> {
    default_roles().into_iter().find(|role| role.name == name)
}

/// What one user may do: their id, their roles, and every capability their roles and their own
/// capabilities grant or deny, merged as WordPress's `WP_User::get_role_caps` merges them into
/// `allcaps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCaps {
    /// The user's id, a row of Ignite's `users`; 0 for a visitor who is not logged in.
    id: i64,
    /// The names of the user's roles, in order.
    roles: Vec<String>,
    /// Every capability and whether it is granted.
    allcaps: BTreeMap<String, bool>,
}

impl UserCaps {
    /// The capabilities of user `id` with `roles`, overlaid with the user's own capabilities
    /// `extra` (each granted with `true` or denied with `false`).
    ///
    /// As `WP_User::get_role_caps` does, the roles' capabilities are merged in order, so a later
    /// role's entry replaces an earlier one's; each role's name is then a granted capability, as
    /// WordPress keeps role names among a user's own capabilities; and `extra` comes last, so a
    /// user's own grant or denial wins over their roles.
    pub fn from_roles(id: i64, roles: &[&Role], extra: &BTreeMap<String, bool>) -> Self {
        let mut allcaps = BTreeMap::new();
        for role in roles {
            allcaps.extend(role.capabilities.clone());
        }
        for role in roles {
            allcaps.insert(role.name.clone(), true);
        }
        allcaps.extend(extra.clone());
        Self {
            id,
            roles: roles.iter().map(|role| role.name.clone()).collect(),
            allcaps,
        }
    }

    /// [`UserCaps::from_roles`] with the default roles named in `role_names`. A name that is not
    /// a default role is left out, as WordPress leaves out a role that is not registered.
    pub fn with_default_roles(
        id: i64,
        role_names: &[&str],
        extra: &BTreeMap<String, bool>,
    ) -> Self {
        let roles: Vec<Role> = role_names
            .iter()
            .filter_map(|name| default_role(name))
            .collect();
        let roles: Vec<&Role> = roles.iter().collect();
        Self::from_roles(id, &roles, extra)
    }

    /// The user's id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The names of the user's roles, in order.
    pub fn roles(&self) -> &[String] {
        &self.roles
    }

    /// Every capability the user's roles and own capabilities name, and whether it is granted.
    pub fn allcaps(&self) -> &BTreeMap<String, bool> {
        &self.allcaps
    }
}

/// Whether `user` holds the primitive capability `cap`, as the end of WordPress's
/// `WP_User::has_cap` decides: [`EXIST`] is always held, [`DO_NOT_ALLOW`] never, and any other
/// capability only when it is granted. Meta capabilities such as `edit_post` go through
/// [`can`](super::can), which maps them first.
pub fn has_cap(user: &UserCaps, cap: &str) -> bool {
    match cap {
        EXIST => true,
        DO_NOT_ALLOW => false,
        _ => user.allcaps.get(cap).copied().unwrap_or(false),
    }
}
