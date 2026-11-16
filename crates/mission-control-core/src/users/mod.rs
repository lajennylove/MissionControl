//! Users' roles and capabilities, after WordPress: the five default roles and their
//! capabilities ([`capabilities`]), and the meta capabilities that map a check on one object to
//! primitive capabilities ([`meta_cap`]). Users sign REST API clients in with application
//! passwords ([`app_password`]); a signed-in user's roles come from a [`UserRoles`] provider
//! ([`user_roles`]), such as [`IgniteUserRoles`], which reads Ignite's role tables
//! ([`ignite_roles`]).
//!
//! ```
//! use std::collections::BTreeMap;
//!
//! use mission_control_core::users::{AUTHOR, UserCaps, can, has_cap};
//!
//! let author = UserCaps::with_default_roles(7, &[AUTHOR], &BTreeMap::new());
//! assert!(has_cap(&author, "publish_posts"));
//! assert!(!can(&author, "edit_others_posts", None));
//! ```

pub mod app_password;
pub mod capabilities;
pub mod ignite_roles;
pub mod meta_cap;
pub mod user_roles;

pub use app_password::{
    AppPasswordError, ApplicationPassword, BasicAuthError, ByEmail, PlainPassword, RandomError,
    UserLookup,
};
pub use capabilities::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, DO_NOT_ALLOW, EDITOR, EXIST, Role, SUBSCRIBER, UserCaps,
    default_role, default_roles, has_cap,
};
pub use ignite_roles::{
    IGNITE_SUPER_ADMIN, IgniteUserRoles, ignite_roles_with_cap, mission_control_role,
    users_with_cap,
};
pub use meta_cap::{
    Object, PostContext, PostTypeCaps, SitePages, TaxonomyCaps, TermContext, can, map_meta_cap,
};
pub use user_roles::{InMemoryUserRoles, NoUserRoles, RoleAssignment, RolesFuture, UserRoles};
