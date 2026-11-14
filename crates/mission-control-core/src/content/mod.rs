//! The content model: posts of every type, after WordPress's `wp_posts`, their revisions and
//! autosaves, and the terms of their taxonomies, after `wp_terms`, `wp_term_taxonomy` and
//! `wp_term_relationships`.
//!
//! ```ignore
//! use mission_control_core::content::{NewPost, NewTerm, Post, PostStatus, Taxonomy, Term};
//!
//! let post = Post::create(&db, NewPost {
//!     status: PostStatus::Publish,
//!     title: "Hello world".into(),
//!     slug: "hello-world".into(),
//!     ..NewPost::new(author_id, "post")
//! })
//! .await?;
//! let posts = Post::of_type("post").get(&db).await?;
//!
//! let news = Term::create(&db, &Taxonomy::CATEGORY, NewTerm::new("News")).await?;
//! post.sync_terms(&db, &Taxonomy::CATEGORY, &[news.term_taxonomy_id]).await?;
//! ```

mod future;
mod meta;
mod post;
mod revision;
mod taxonomy;

pub use future::{
    PUBLISH_FUTURE_POST, check_and_publish_future_post, register_publish_future_post,
    schedule_future_post,
};
pub use meta::{
    MetaError, delete_option, delete_post_meta, delete_term_meta, delete_user_meta, get_option,
    get_post_meta, get_term_meta, get_user_meta, set_option, set_option_raw, set_post_meta,
    set_post_meta_raw, set_term_meta, set_term_meta_raw, set_user_meta, set_user_meta_raw,
};
pub use post::{NewPost, Post, PostStatus, UnknownPostStatus};
pub use revision::{
    Revision, SaveError, create_post_autosave, get_post_autosave, get_post_revisions, save_revision,
};
pub use taxonomy::{MAX_TERM_LENGTH, NewTerm, Taxonomy, Term, TermError, update_term_counts};
