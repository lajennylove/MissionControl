//! The content model: posts of every type, after WordPress's `wp_posts`, and the terms of their
//! taxonomies, after `wp_terms`, `wp_term_taxonomy` and `wp_term_relationships`.
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
mod post;
mod taxonomy;

pub use future::{
    PUBLISH_FUTURE_POST, check_and_publish_future_post, register_publish_future_post,
    schedule_future_post,
};
pub use post::{NewPost, Post, PostStatus, UnknownPostStatus};
pub use taxonomy::{NewTerm, Taxonomy, Term, TermError, update_term_counts};
