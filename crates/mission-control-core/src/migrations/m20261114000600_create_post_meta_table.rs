//! The `post_meta` table: the custom fields of posts of every type,
//! after WordPress's `wp_postmeta`, with Laravel-style names.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `post_meta`: `id`, `post_id`, `meta_key` (255 characters, as WordPress sizes it) and
/// `meta_value` (`TEXT`, holding JSON). The columns are `meta_key` and `meta_value` rather than
/// `key` and `value` because `KEY` is a reserved word in MySQL and MariaDB. A key may repeat
/// for one post, as in WordPress. `(post_id, meta_key)` is indexed, and `meta_key` alone, as WordPress
/// indexes both. Deleting the post deletes its rows.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000600_create_post_meta_table",
        vec![schema::create("post_meta", |t| {
            t.id();
            t.big_integer("post_id");
            t.string("meta_key").length(255);
            t.text("meta_value");
            t.index(["post_id", "meta_key"]);
            t.index(["meta_key"]);
            t.foreign("post_id")
                .references("posts", "id")
                .on_delete(Action::Cascade);
        })],
        vec![schema::drop_if_exists("post_meta")],
    )
}
