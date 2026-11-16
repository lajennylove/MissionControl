//! The admin's rules that need neither Rocket nor Ignite's routes: who may use the screens of a
//! post type ([`posts`], Ruling R115). The `mission-control` crate mounts Ignite's panel at
//! `/mc-admin` and applies them before Ignite's routes run.

pub mod posts;

pub use posts::{
    PostScreen, Refusal, authorize_post_screen, may_use_post_screens, post_screen_caps,
};
