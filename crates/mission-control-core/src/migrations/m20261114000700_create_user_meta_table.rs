//! The `user_meta` table: the custom fields of users,
//! after WordPress's `wp_usermeta`, with Laravel-style names.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `user_meta`: `id`, `user_id`, `meta_key` (255 characters, as WordPress sizes it) and
/// `meta_value` (JSON, in a `long_text`: `LONGTEXT` on MySQL and MariaDB, as WordPress sizes
/// `meta_value`, and `TEXT` elsewhere). The columns are `meta_key` and `meta_value` rather than
/// `key` and `value` because `KEY` is a reserved word in MySQL and MariaDB. A key may repeat
/// for one user, as in WordPress. `(user_id, meta_key)` is indexed, and `meta_key` alone, as WordPress
/// indexes both. Deleting the user deletes its rows.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000700_create_user_meta_table",
        vec![schema::create("user_meta", |t| {
            t.id();
            t.big_integer("user_id");
            t.string("meta_key").length(255);
            t.long_text("meta_value");
            t.index(["user_id", "meta_key"]);
            t.index(["meta_key"]);
            t.foreign("user_id")
                .references("users", "id")
                .on_delete(Action::Cascade);
        })],
        vec![schema::drop_if_exists("user_meta")],
    )
}
