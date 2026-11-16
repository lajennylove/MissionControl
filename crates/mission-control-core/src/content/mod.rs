//! The content model: posts of every type, after WordPress's `wp_posts`, their revisions and
//! autosaves, their slugs and permalinks, and the terms of their taxonomies, after `wp_terms`,
//! `wp_term_taxonomy` and `wp_term_relationships`.
//!
//! A post's title, content and excerpt are stored as given. A path that saves them for a user
//! passes them through [`kses_post_fields`] first, WordPress's HTML filter on save.
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

mod auto_drafts;
mod caption;
mod future;
mod kses;
mod meta;
mod options;
mod permalink;
mod post;
mod revision;
mod shortcode;
mod slug;
mod taxonomy;

pub use auto_drafts::{
    AUTO_DRAFT_MAX_AGE, DELETE_AUTO_DRAFTS, MAX_AUTO_DRAFTS_PER_RUN, delete_old_auto_drafts,
    register_delete_auto_drafts, schedule_auto_draft_deletion,
};
pub use future::{
    PUBLISH_FUTURE_POST, check_and_publish_future_post, register_publish_future_post,
    schedule_future_post,
};
pub use kses::{
    FieldTooLarge, MAX_ACTIVE_FORMATTING, MAX_FILTERED_BYTES, MAX_FORMATTING_ATTRIBUTES,
    MAX_NESTING, PostFields, kses_data, kses_post, kses_post_fields, kses_title,
    safecss_filter_attr, sanitize_text_field,
};
pub use meta::{
    MetaError, delete_option, delete_post_meta, delete_term_meta, delete_user_meta, get_option,
    get_post_meta, get_post_meta_values, get_term_meta, get_user_meta, set_option, set_option_raw,
    set_post_meta, set_post_meta_raw, set_term_meta, set_term_meta_raw, set_user_meta,
    set_user_meta_raw,
};
pub(crate) use meta::{MetaTable, delete_meta, meta_rows, set_meta, set_meta_values};
pub(crate) use meta::{get_option_in, gmt_offset, put_option_in, put_post_meta_in};
pub use options::{OptionError, sanitize_option};
pub(crate) use options::{esc_url_raw, is_email, is_timezone, utc_offset};
pub use permalink::{
    PermalinkContext, PermalinkError, PermalinkStructure, Segment, Tag, page_ancestors, permalink,
};
pub use post::{NewPost, Post, PostStatus, UnknownPostStatus};
pub(crate) use post::{next_modified_at, now};
pub use revision::{
    Revision, SaveError, create_post_autosave, get_post_autosave, get_post_revisions, save_revision,
};
pub use shortcode::{Attributes, InvalidShortcodeTag, Shortcodes};
pub use slug::{
    MAX_SLUG_LENGTH, find_by_old_slug, is_hierarchical, make_slug, unique_slug, unique_slug_for,
};
pub use taxonomy::{
    MAX_TERM_LENGTH, NewTerm, Taxonomy, Term, TermChanges, TermError, default_term_ids,
    update_term_counts,
};
pub(crate) use taxonomy::{recount_in, set_terms_in, terms_by_term_id};
