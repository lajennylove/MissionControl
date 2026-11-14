//! Users' roles and capabilities, after WordPress: the five default roles and their
//! capabilities ([`capabilities`]), and the meta capabilities that map a check on one object to
//! primitive capabilities ([`meta_cap`]).
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

pub mod capabilities;
pub mod meta_cap;

pub use capabilities::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, DO_NOT_ALLOW, EDITOR, EXIST, Role, SUBSCRIBER, UserCaps,
    default_role, default_roles, has_cap,
};
pub use meta_cap::{Object, PostContext, PostTypeCaps, SitePages, can, map_meta_cap};
