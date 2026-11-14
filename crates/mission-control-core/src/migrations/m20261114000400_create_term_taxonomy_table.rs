//! The `term_taxonomy` table: a term in one taxonomy, with its description, parent and post
//! count, after WordPress's `wp_term_taxonomy`.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `term_taxonomy`. `taxonomy` is 32 characters, as in WordPress; `count` is a `BIGINT`
/// that defaults to 0. A term appears once per taxonomy (WordPress's `term_id_taxonomy` unique
/// key).
///
/// `parent_id` is the parent's `term_taxonomy` id, or null for a top-level term. WordPress stores
/// the parent's term id, with 0 for none; Mission Control keeps the row id so that a foreign key
/// guards it. Deleting a parent that still has children is refused, so the content API moves the
/// children to the deleted term's parent first, as WordPress does. Deleting a term deletes its
/// `term_taxonomy` rows.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000400_create_term_taxonomy_table",
        vec![schema::create("term_taxonomy", |t| {
            t.id();
            t.big_integer("term_id");
            t.string("taxonomy").length(32);
            t.text("description");
            t.big_integer("parent_id").nullable();
            t.big_integer("count").default(0);
            t.unique(["term_id", "taxonomy"]);
            t.index(["taxonomy"]);
            t.index(["parent_id"]);
            t.foreign("term_id")
                .references("terms", "id")
                .on_delete(Action::Cascade);
            t.foreign("parent_id")
                .references("term_taxonomy", "id")
                .on_delete(Action::Restrict);
        })],
        vec![schema::drop_if_exists("term_taxonomy")],
    )
}
