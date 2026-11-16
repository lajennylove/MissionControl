//! Users' roles read from Ignite's tables (Ruling R121): the [`UserRoles`] provider for a site
//! whose roles are Ignite roles (design 5.6), and the users that hold a capability.
//!
//! Ignite keeps roles in `roles` (by `name`) and gives them to users in `role_user`, the Spatie
//! laravel-permission shape. An Ignite role counts as the WordPress default role with the same
//! name ([`ADMINISTRATOR`], [`EDITOR`], [`AUTHOR`], [`CONTRIBUTOR`], [`SUBSCRIBER`]), and Ignite's
//! own `super-admin` as an administrator; any other Ignite role grants nothing in Mission
//! Control, as WordPress ignores a role that is not registered. Users have no capabilities of
//! their own here (WordPress's per-user grants are not stored in Ignite).
//!
//! ```ignore
//! let caps = IgniteUserRoles.roles(&db, user_id).await?.user_caps(user_id);
//! let authors = users_with_cap(&db, "edit_posts", MAX_OPTIONS).await?;
//! ```

use std::collections::BTreeMap;

use orbit_orm::{Database, Error, Value as Sql, raw};

use super::capabilities::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, SUBSCRIBER, UserCaps, has_cap,
};
use super::user_roles::{RoleAssignment, RolesFuture, UserRoles};

/// Ignite's role for the user who may do everything, which Mission Control reads as
/// [`ADMINISTRATOR`].
pub const IGNITE_SUPER_ADMIN: &str = "super-admin";

/// The most Ignite roles read for one user.
const MAX_ROLES: i64 = 100;

/// The WordPress default role an Ignite role named `name` counts as, or `None` for a role
/// Mission Control does not know.
pub fn mission_control_role(name: &str) -> Option<&'static str> {
    match name {
        IGNITE_SUPER_ADMIN | ADMINISTRATOR => Some(ADMINISTRATOR),
        EDITOR => Some(EDITOR),
        AUTHOR => Some(AUTHOR),
        CONTRIBUTOR => Some(CONTRIBUTOR),
        SUBSCRIBER => Some(SUBSCRIBER),
        _ => None,
    }
}

/// The Ignite role names whose Mission Control role grants `cap`, as the role's default
/// capabilities give it.
pub fn ignite_roles_with_cap(cap: &str) -> Vec<&'static str> {
    [
        IGNITE_SUPER_ADMIN,
        ADMINISTRATOR,
        EDITOR,
        AUTHOR,
        CONTRIBUTOR,
        SUBSCRIBER,
    ]
    .into_iter()
    .filter(|name| {
        mission_control_role(name).is_some_and(|role| {
            has_cap(
                &UserCaps::with_default_roles(0, &[role], &BTreeMap::new()),
                cap,
            )
        })
    })
    .collect()
}

/// Reads a user's roles from Ignite's `roles` and `role_user` tables. Not wired into the app
/// yet: the admin's authorization (Task 6.1) does that.
#[derive(Debug, Clone, Copy, Default)]
pub struct IgniteUserRoles;

impl UserRoles for IgniteUserRoles {
    fn roles<'a>(&'a self, db: &'a Database, user_id: i64) -> RolesFuture<'a> {
        Box::pin(async move {
            let names: Vec<(String,)> = raw::fetch_all(
                db,
                "SELECT roles.name FROM roles \
                 INNER JOIN role_user ON role_user.role_id = roles.id \
                 WHERE role_user.user_id = ? ORDER BY roles.name LIMIT ?",
                &[user_id.into(), MAX_ROLES.into()],
            )
            .await?;
            let mut roles: Vec<&'static str> = Vec::new();
            for (name,) in &names {
                if let Some(role) = mission_control_role(name)
                    && !roles.contains(&role)
                {
                    roles.push(role);
                }
            }
            Ok(RoleAssignment::new(roles, BTreeMap::new()))
        })
    }
}

/// The users whose Ignite roles grant `cap`, by name, at most `limit`: `(id, name)`. One query,
/// as `wp_dropdown_users` with `capability` lists them.
///
/// # Errors
///
/// The database's error, such as before Ignite's migrations made the role tables.
pub async fn users_with_cap(
    db: &Database,
    cap: &str,
    limit: usize,
) -> Result<Vec<(i64, String)>, Error> {
    let roles = ignite_roles_with_cap(cap);
    if roles.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut binds: Vec<Sql> = roles.iter().map(|r| Sql::from(*r)).collect();
    binds.push(i64::try_from(limit).unwrap_or(i64::MAX).into());
    let sql = format!(
        "SELECT u.id, u.name FROM users u WHERE EXISTS (SELECT 1 FROM role_user ru \
         INNER JOIN roles r ON r.id = ru.role_id WHERE ru.user_id = u.id AND r.name IN ({})) \
         ORDER BY u.name, u.id LIMIT ?",
        vec!["?"; roles.len()].join(", ")
    );
    raw::fetch_all(db, &sql, &binds).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignite_roles_map_to_wordpress_roles() {
        assert_eq!(mission_control_role("super-admin"), Some(ADMINISTRATOR));
        assert_eq!(mission_control_role("editor"), Some(EDITOR));
        assert_eq!(mission_control_role("manager"), None);
    }

    #[test]
    fn the_roles_that_grant_a_capability() {
        assert_eq!(
            ignite_roles_with_cap("edit_posts"),
            [
                "super-admin",
                "administrator",
                "editor",
                "author",
                "contributor"
            ]
        );
        assert_eq!(
            ignite_roles_with_cap("edit_pages"),
            ["super-admin", "administrator", "editor"]
        );
        assert!(ignite_roles_with_cap("edit_books").is_empty());
    }
}
