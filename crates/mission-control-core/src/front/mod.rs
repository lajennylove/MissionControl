//! The public site: what a request asks for, read from its URL as WordPress's rewrite rules and
//! `WP_Query` read it, for the theme runtime to render.
//!
//! ```ignore
//! use mission_control_core::front::{Query, Resolved, resolve};
//!
//! let resolved = resolve(&db, "/2026/03/hello-world/", &Query::parse(""), &structure).await?;
//! if let Resolved::Single(post) = resolved {
//!     // render single-{post_type}, then single, then index
//! }
//! ```

mod route;

pub use route::{Archive, Query, Resolved, resolve};
