//! Themes: which template renders a request.
//!
//! ```ignore
//! use mission_control_core::theme::templates_for;
//!
//! // `single-post-hello-world`, `single-post`, `single`, `singular`, `index`
//! let candidates = templates_for(&resolved, None);
//! ```

mod hierarchy;

pub use hierarchy::templates_for;
