//! The `taxonomies` table: the custom taxonomies made in the admin's manager screens (design
//! 5.3), one definition per row. Taxonomies defined in code or by a plugin are never stored.

use orbit_orm::Migration;
use orbit_orm::schema;

/// Creates `taxonomies`: `id`, `slug` (32 characters, unique: `register_taxonomy`'s limit),
/// `definition` (the [`TaxonomyDef`](crate::types::TaxonomyDef) as JSON, a `long_text`),
/// `created_at` and `updated_at` (UTC).
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261115120100_create_taxonomies_table",
        vec![schema::create("taxonomies", |t| {
            t.id();
            t.string("slug").length(32);
            t.long_text("definition");
            t.date_time("created_at");
            t.date_time("updated_at");
            t.unique(["slug"]);
        })],
        vec![schema::drop_if_exists("taxonomies")],
    )
}
