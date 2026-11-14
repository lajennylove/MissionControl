//! The `term_relationships` table: which terms each post has, after WordPress's
//! `wp_term_relationships`.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `term_relationships`. `(object_id, term_taxonomy_id)` is unique, WordPress's primary
/// key; the schema builder has no composite primary key, so it is a unique key and the table has
/// no `id`. `term_order` defaults to 0. Deleting a post or a term deletes its rows.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000500_create_term_relationships_table",
        vec![schema::create("term_relationships", |t| {
            t.big_integer("object_id");
            t.big_integer("term_taxonomy_id");
            t.integer("term_order").default(0);
            t.unique(["object_id", "term_taxonomy_id"]);
            t.index(["term_taxonomy_id"]);
            t.foreign("object_id")
                .references("posts", "id")
                .on_delete(Action::Cascade);
            t.foreign("term_taxonomy_id")
                .references("term_taxonomy", "id")
                .on_delete(Action::Cascade);
        })],
        vec![schema::drop_if_exists("term_relationships")],
    )
}
