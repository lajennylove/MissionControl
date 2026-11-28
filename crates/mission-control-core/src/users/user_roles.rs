//! Where a signed-in user's roles come from: the [`UserRoles`] provider (Controller ruling R38).
//!
//! Roles are stored as Ignite roles (design section 5.6), and [`IgniteUserRoles`] reads them
//! with the capabilities stored for each one. The app takes a provider when it is built, and
//! [`NoUserRoles`], the default, gives every user no role at all: an authenticated user can do
//! nothing a visitor cannot, so a missing provider fails closed. Tests use
//! [`InMemoryUserRoles`].
//!
//! [`IgniteUserRoles`]: super::IgniteUserRoles
//!
//! ```
//! use std::collections::BTreeMap;
//!
//! use mission_control_core::users::{EDITOR, InMemoryUserRoles, RoleAssignment};
//!
//! let roles = InMemoryUserRoles::new();
//! roles.assign(7, RoleAssignment::new([EDITOR], BTreeMap::new()));
//! ```

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;

use orbit_orm::Database;

use super::capabilities::{Role, UserCaps};

/// A user's roles and own capabilities, as WordPress keeps them in the `wp_capabilities` user
/// meta: role names, and capabilities granted (`true`) or denied (`false`) to the user alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleAssignment {
    /// The names of the user's roles, in order, such as `editor`.
    pub roles: Vec<String>,
    /// The user's own capabilities, which win over their roles'.
    pub caps: BTreeMap<String, bool>,
    /// The roles as they are stored, with the capabilities each one grants now (WordPress's
    /// `wp_user_roles` option, which the Roles screen changes), in the order of [`roles`]. `None`
    /// reads the roles as WordPress's default roles of the same names.
    ///
    /// [`roles`]: Self::roles
    pub stored: Option<Vec<Role>>,
}

impl RoleAssignment {
    /// The assignment of `roles` and the user's own `caps`.
    pub fn new<R, S>(roles: R, caps: BTreeMap<String, bool>) -> Self
    where
        R: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            roles: roles.into_iter().map(Into::into).collect(),
            caps,
            stored: None,
        }
    }

    /// The assignment of the stored `roles`, each with the capabilities it grants now, and the
    /// user's own `caps`.
    pub fn stored(roles: Vec<Role>, caps: BTreeMap<String, bool>) -> Self {
        Self {
            roles: roles.iter().map(|role| role.name.clone()).collect(),
            caps,
            stored: Some(roles),
        }
    }

    /// The capabilities of user `id` with this assignment: the stored roles' capabilities, or,
    /// without them, WordPress's default roles', where a name that is not one is left out, as
    /// WordPress leaves out a role that is not registered.
    pub fn user_caps(&self, id: i64) -> UserCaps {
        if let Some(roles) = &self.stored {
            let roles: Vec<&Role> = roles.iter().collect();
            return UserCaps::from_roles(id, &roles, &self.caps);
        }
        let names: Vec<&str> = self.roles.iter().map(String::as_str).collect();
        UserCaps::with_default_roles(id, &names, &self.caps)
    }
}

/// The future a [`UserRoles`] provider returns.
pub type RolesFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RoleAssignment, orbit_orm::Error>> + Send + 'a>>;

/// Gives the roles and own capabilities of a user, by id. The app holds one, shared by every
/// request, so it is `Send + Sync`.
pub trait UserRoles: Send + Sync {
    /// The roles of user `user_id`; an empty assignment for a user with none, or no such user.
    fn roles<'a>(&'a self, db: &'a Database, user_id: i64) -> RolesFuture<'a>;
}

/// The provider for an app built without one: every user has no role and no capability of their
/// own.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUserRoles;

impl UserRoles for NoUserRoles {
    fn roles<'a>(&'a self, _db: &'a Database, _user_id: i64) -> RolesFuture<'a> {
        Box::pin(async { Ok(RoleAssignment::default()) })
    }
}

/// A provider that keeps the assignments in memory, for tests and for sites that set roles in
/// code. A user it has no assignment for has no role.
#[derive(Debug, Default)]
pub struct InMemoryUserRoles {
    assignments: RwLock<HashMap<i64, RoleAssignment>>,
}

impl InMemoryUserRoles {
    /// A provider with no assignment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives user `user_id` the `assignment`, replacing the one they had.
    pub fn assign(&self, user_id: i64, assignment: RoleAssignment) {
        self.assignments
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(user_id, assignment);
    }

    /// Takes every role and own capability away from user `user_id`.
    pub fn remove(&self, user_id: i64) {
        self.assignments
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&user_id);
    }
}

impl UserRoles for InMemoryUserRoles {
    fn roles<'a>(&'a self, _db: &'a Database, user_id: i64) -> RolesFuture<'a> {
        let assignment = self
            .assignments
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&user_id)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(assignment) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::{ADMINISTRATOR, has_cap};

    #[test]
    fn an_assignment_builds_the_users_capabilities() {
        let mut caps = BTreeMap::new();
        caps.insert("edit_users".to_owned(), false);
        let user = RoleAssignment::new([ADMINISTRATOR, "no_such_role"], caps).user_caps(3);
        assert_eq!(user.id(), 3);
        assert_eq!(user.roles(), [ADMINISTRATOR]);
        assert!(has_cap(&user, "manage_options"));
        assert!(!has_cap(&user, "edit_users"), "the user's own denial wins");
    }

    #[test]
    fn no_assignment_is_no_role() {
        let user = RoleAssignment::default().user_caps(3);
        assert!(user.roles().is_empty());
        assert!(!has_cap(&user, "read"));
    }
}
