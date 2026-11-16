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

mod render;
mod route;

pub(crate) use render::trim_excerpt;
pub use render::{
    ActiveThemeError, DEFAULT_THEME, FrontError, Frontend, Response, THEME_ENTRIES,
    load_active_theme, site_address,
};
pub use route::{
    Archive, MAX_POSTS_PER_PAGE, MainQuery, Query, Resolved, post_links, query_posts, resolve,
    term_links,
};
pub(crate) use route::{
    NON_PUBLIC_TYPES, SearchTerm, decode, escape_like, search_terms, visitor_readable,
};
