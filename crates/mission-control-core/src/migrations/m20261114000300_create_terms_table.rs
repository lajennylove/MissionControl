//! The `terms` table: the names and slugs of taxonomy terms, after WordPress's `wp_terms`. Which
//! taxonomy a term belongs to is in `term_taxonomy`; the split is WordPress's, so the REST API
//! and the WXR importer map onto it one to one.

use orbit_orm::Migration;
use orbit_orm::schema;

/// Creates `terms`. Sizes follow `wp_terms`: `name` and `slug` are 200 characters, and
/// `term_group` is a `BIGINT` that defaults to 0. `slug` and `name` are plain indexes, as in
/// WordPress: slugs are unique per taxonomy, which lives in another table, so the content API
/// enforces it.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000300_create_terms_table",
        vec![schema::create("terms", |t| {
            t.id();
            t.string("name").length(200).default("");
            t.string("slug").length(200).default("");
            t.big_integer("term_group").default(0);
            t.index(["slug"]);
            t.index(["name"]);
        })],
        vec![schema::drop_if_exists("terms")],
    )
}
