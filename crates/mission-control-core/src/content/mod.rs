//! The content model: posts of every type, after WordPress's `wp_posts`.
//!
//! ```ignore
//! use mission_control_core::content::{NewPost, Post, PostStatus};
//!
//! let post = Post::create(&db, NewPost {
//!     status: PostStatus::Publish,
//!     title: "Hello world".into(),
//!     slug: "hello-world".into(),
//!     ..NewPost::new(author_id, "post")
//! })
//! .await?;
//! let posts = Post::of_type("post").get(&db).await?;
//! ```

mod future;
mod post;

pub use future::{
    PUBLISH_FUTURE_POST, check_and_publish_future_post, register_publish_future_post,
    schedule_future_post,
};
pub use post::{NewPost, Post, PostStatus, UnknownPostStatus};
