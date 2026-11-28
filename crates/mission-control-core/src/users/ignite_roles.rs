//! Users' roles read from Ignite's tables (Ruling R121): the [`UserRoles`] provider for a site
//! whose roles are Ignite roles (design 5.6), and the users that hold a capability.
//!
//! Ignite keeps roles in `roles` (by `name`), gives them to users in `role_user`, and gives each
//! role its permissions in `role_permission`, the Spatie laravel-permission shape. A role's
//! capabilities are its permissions whose names are WordPress capabilities (no `.`; Ignite's own
//! resource abilities, such as `post.viewAny`, are left out), as WordPress reads each role's
//! capabilities from its `wp_user_roles` option: the Roles screen changes them, and the next
//! request reads them. Any role counts, the five WordPress roles and the roles made on the Roles
//! screen, as `add_role` adds one. Ignite's own `super-admin`, which passes every Ignite check,
//! counts as WordPress's default administrator, and one of the five WordPress roles stored
//! without any capability (made before they were stored) has WordPress's defaults. Users have
//! no capabilities of their own here
//! (WordPress's per-user grants are not stored in Ignite).
//!
//! ```ignore
//! let caps = IgniteUserRoles.roles(&db, user_id).await?.user_caps(user_id);
//! let authors = users_with_cap(&db, "edit_posts", MAX_OPTIONS).await?;
//! ```

use std::collections::BTreeMap;

use orbit_orm::{Database, Error, Value as Sql, raw};

use super::capabilities::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, Role, SUBSCRIBER, UserCaps, default_role, has_cap,
    is_capability_name,
};
use super::user_roles::{RoleAssignment, RolesFuture, UserRoles};

/// Ignite's role for the user who may do everything, which Mission Control reads as
/// [`ADMINISTRATOR`].
pub const IGNITE_SUPER_ADMIN: &str = "super-admin";

/// The most role and permission rows read for one user.
const MAX_ROWS: i64 = 20_000;

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

/// The Ignite role names whose WordPress default role grants `cap`, as the role's default
/// capabilities give it (not as the Roles screen may have changed them).
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

/// The roles a role dropdown offers (`wp_dropdown_roles`), as `(name, label)`: WordPress's five
/// in their order, then the others by name, never Ignite's [`IGNITE_SUPER_ADMIN`]; each stored in
/// Ignite's `roles`. A default role is labelled with its WordPress name, another with its name.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn role_choices(db: &Database) -> Result<Vec<(String, String)>, Error> {
    // A site whose database has no Ignite tables (the admin runs on MySQL and MariaDB only,
    // Ruling R119) offers WordPress's five roles.
    let stored: Vec<String> =
        match raw::fetch_all::<(String,)>(db, "SELECT name FROM roles ORDER BY name", &[]).await {
            Ok(rows) => rows.into_iter().map(|(name,)| name).collect(),
            Err(e) => {
                log::warn!("The roles could not be read, WordPress's are offered: {e}");
                super::capabilities::WORDPRESS_ROLES
                    .iter()
                    .map(|role| (*role).to_owned())
                    .collect()
            }
        };
    let mut names: Vec<String> = super::capabilities::WORDPRESS_ROLES
        .iter()
        .filter(|role| stored.iter().any(|s| s == *role))
        .map(|role| (*role).to_owned())
        .collect();
    for role in stored {
        if role != IGNITE_SUPER_ADMIN && !names.contains(&role) {
            names.push(role);
        }
    }
    Ok(names
        .into_iter()
        .map(|name| {
            let label = default_role(&name).map_or_else(|| name.clone(), |r| r.display_name);
            (name, label)
        })
        .collect())
}

/// The role a new user gets when none is chosen, as `wp_insert_user` gives the site's
/// `default_role` option (Settings > General, "New User Default Role"; `subscriber` when it is
/// unset): the option's role when it is stored in Ignite's `roles`, else Subscriber when it is
/// stored, else `None`.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn new_user_default_role(db: &Database) -> Result<Option<String>, Error> {
    let chosen = match crate::content::get_option(db, "default_role").await {
        Ok(Some(serde_json::Value::String(name))) if !name.trim().is_empty() => name,
        Ok(_) | Err(crate::content::MetaError::InvalidJson(_)) => SUBSCRIBER.to_owned(),
        Err(crate::content::MetaError::Database(e)) => return Err(e),
    };
    let choices = role_choices(db).await?;
    for name in [chosen.as_str(), SUBSCRIBER] {
        if choices.iter().any(|(role, _)| role == name) {
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

/// Reads a user's roles and each role's capabilities from Ignite's `roles`, `role_user`,
/// `role_permission` and `permissions` tables. The app's default provider, for the REST API and
/// the admin's authorization (Ruling R121).
#[derive(Debug, Clone, Copy, Default)]
pub struct IgniteUserRoles;

impl UserRoles for IgniteUserRoles {
    fn roles<'a>(&'a self, db: &'a Database, user_id: i64) -> RolesFuture<'a> {
        Box::pin(async move {
            let rows: Vec<(String, Option<String>)> = raw::fetch_all(
                db,
                "SELECT roles.name, permissions.name FROM role_user \
                 INNER JOIN roles ON roles.id = role_user.role_id \
                 LEFT JOIN role_permission ON role_permission.role_id = roles.id \
                 LEFT JOIN permissions ON permissions.id = role_permission.permission_id \
                 WHERE role_user.user_id = ? ORDER BY roles.name, permissions.name LIMIT ?",
                &[user_id.into(), MAX_ROWS.into()],
            )
            .await?;
            Ok(RoleAssignment::stored(stored_roles(rows), BTreeMap::new()))
        })
    }
}

/// The roles of `(role, permission)` rows, in order: each role with the capabilities among its
/// permissions; `super-admin` as the default administrator.
fn stored_roles(rows: Vec<(String, Option<String>)>) -> Vec<Role> {
    let mut roles: Vec<Role> = Vec::new();
    for (name, permission) in rows {
        if name == IGNITE_SUPER_ADMIN {
            if !roles.iter().any(|role| role.name == ADMINISTRATOR) {
                roles.extend(default_role(ADMINISTRATOR));
            }
            continue;
        }
        let index = match roles.iter().position(|role| role.name == name) {
            Some(index) => index,
            None => {
                roles.push(Role::new(&name, &name, Vec::<String>::new()));
                roles.len() - 1
            }
        };
        if let Some(cap) = permission.filter(|cap| is_capability_name(cap)) {
            roles[index].capabilities.insert(cap, true);
        }
    }
    // A WordPress role stored without any capability was made before the capabilities were
    // stored (the installer, an earlier version, or a site without the admin): it has
    // WordPress's defaults, as the admin gives it at start.
    for role in &mut roles {
        if role.capabilities.is_empty()
            && let Some(default) = default_role(&role.name)
        {
            *role = default;
        }
    }
    roles
}

/// The users whose stored roles grant `cap`, by name, at most `limit`: `(id, name)`. One query,
/// as `wp_dropdown_users` with `capability` lists them. A `super-admin` holds every capability
/// of the default administrator.
///
/// # Errors
///
/// The database's error, such as before Ignite's migrations made the role tables.
pub async fn users_with_cap(
    db: &Database,
    cap: &str,
    limit: usize,
) -> Result<Vec<(i64, String)>, Error> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let super_admin = default_role(ADMINISTRATOR)
        .is_some_and(|role| role.capabilities.get(cap).copied().unwrap_or(false));
    let mut binds: Vec<Sql> = vec![cap.into(), IGNITE_SUPER_ADMIN.into(), super_admin.into()];
    // A WordPress role stored without any capability has its defaults (see `stored_roles`).
    let defaults: Vec<&str> = ignite_roles_with_cap(cap)
        .into_iter()
        .filter(|role| *role != IGNITE_SUPER_ADMIN)
        .collect();
    let unstored = if defaults.is_empty() {
        String::new()
    } else {
        binds.extend(defaults.iter().map(|role| Sql::from(*role)));
        format!(
            " OR (r.name IN ({}) AND NOT EXISTS (SELECT 1 FROM role_permission rp2 \
             INNER JOIN permissions p2 ON p2.id = rp2.permission_id \
             WHERE rp2.role_id = r.id AND p2.name NOT LIKE '%.%'))",
            vec!["?"; defaults.len()].join(", ")
        )
    };
    binds.push(i64::try_from(limit).unwrap_or(i64::MAX).into());
    let sql = format!(
        "SELECT u.id, u.name FROM users u WHERE EXISTS (SELECT 1 FROM role_user ru \
         INNER JOIN roles r ON r.id = ru.role_id WHERE ru.user_id = u.id AND (\
         EXISTS (SELECT 1 FROM role_permission rp INNER JOIN permissions p \
         ON p.id = rp.permission_id WHERE rp.role_id = r.id AND p.name = ?) \
         OR (r.name = ? AND ?){unstored})) ORDER BY u.name, u.id LIMIT ?"
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
    fn stored_roles_grant_their_capabilities_and_super_admin_is_an_administrator() {
        let row = |role: &str, cap: Option<&str>| (role.to_owned(), cap.map(str::to_owned));
        let roles = stored_roles(vec![
            row("author", Some("edit_posts")),
            row("author", Some("post.viewAny")),
            row("author", Some("read")),
            row("shop_manager", None),
            row("subscriber", Some("post.viewAny")),
            row("super-admin", None),
        ]);
        let names: Vec<&str> = roles.iter().map(|role| role.name.as_str()).collect();
        assert_eq!(names, ["author", "shop_manager", SUBSCRIBER, ADMINISTRATOR]);
        assert_eq!(roles[2], default_role(SUBSCRIBER).unwrap());
        let author: Vec<&str> = roles[0].granted().collect();
        assert_eq!(author, ["edit_posts", "read"]);
        assert_eq!(roles[1].granted().count(), 0);
        assert!(roles[3].capabilities.contains_key("manage_options"));
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
