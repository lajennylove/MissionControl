//! The `term_meta` table: the custom fields of terms,
//! after WordPress's `wp_termmeta`, with Laravel-style names.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `term_meta`: `id`, `term_id`, `meta_key` (255 characters, as WordPress sizes it) and
/// `meta_value` (`TEXT`, holding JSON). The columns are `meta_key` and `meta_value` rather than
/// `key` and `value` because `KEY` is a reserved word in MySQL and MariaDB. A key may repeat
/// for one term, as in WordPress. `(term_id, meta_key)` is indexed, and `meta_key` alone, as WordPress
/// indexes both. Deleting the term deletes its rows.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000800_create_term_meta_table",
        vec![schema::create("term_meta", |t| {
            t.id();
            t.big_integer("term_id");
            t.string("meta_key").length(255);
            t.text("meta_value");
            t.index(["term_id", "meta_key"]);
            t.index(["meta_key"]);
            t.foreign("term_id")
                .references("terms", "id")
                .on_delete(Action::Cascade);
        })],
        vec![schema::drop_if_exists("term_meta")],
    )
}
