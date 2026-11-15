//! The `posts` table: every post type in one table, after WordPress's `wp_posts`, with
//! Laravel-style column names. Posts, pages, custom types, attachments, revisions and menu items
//! share one ID space, which the REST API (`featured_media`, `parent`) and the WXR importer rely
//! on.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `posts`. Sizes follow `wp_posts`: `post_type` and `status` are 20 characters, `slug`
/// is 200 (`post_name`), `password` is 255, `mime_type` is 100 (`post_mime_type`, empty but for
/// attachments). `title`, `content` and `excerpt` are `long_text`: `LONGTEXT` on MySQL and
/// MariaDB, as `wp_posts` sizes `post_content` (it has `TEXT` for `post_title` and
/// `post_excerpt`; the larger type keeps an imported title or excerpt whole), and `TEXT`
/// elsewhere. `author_id` is a `BIGINT`, the type of Ignite's `users.id`, so MySQL and MariaDB
/// accept the foreign key; deleting a user who has posts is refused.
///
/// `(post_type, slug)` is a plain index, not a unique key: WordPress keeps slugs unique per type
/// (per parent for hierarchical types) among posts that are not trashed, and a draft may have no
/// slug yet, so the content API enforces it. The other indexes are those of `wp_posts`:
/// `type_status_date`, `post_parent` and `post_author`.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000100_create_posts_table",
        vec![schema::create("posts", |t| {
            t.id();
            t.big_integer("author_id");
            t.string("post_type").length(20).default("post");
            t.string("status").length(20).default("publish");
            t.long_text("title");
            t.string("slug").length(200).default("");
            t.long_text("content");
            t.long_text("excerpt");
            t.big_integer("parent_id").nullable();
            t.integer("menu_order").default(0);
            t.date_time("published_at").nullable();
            t.date_time("modified_at");
            t.string("password").nullable();
            t.string("mime_type").length(100).default("");
            t.index(["post_type", "slug"]);
            t.index(["post_type", "status", "published_at", "id"]);
            t.index(["parent_id"]);
            t.index(["author_id"]);
            t.foreign("author_id")
                .references("users", "id")
                .on_delete(Action::Restrict);
        })],
        vec![schema::drop_if_exists("posts")],
    )
}
