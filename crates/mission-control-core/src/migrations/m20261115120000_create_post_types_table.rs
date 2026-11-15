//! The `post_types` table: the custom post types made in the admin's manager screens (design
//! 5.3), one definition per row. Types defined in code or by a plugin are held in memory and
//! never stored, as ACF keeps local JSON out of the database.

use orbit_orm::Migration;
use orbit_orm::schema;

/// Creates `post_types`: `id`, `slug` (20 characters, unique: `register_post_type`'s limit),
/// `definition` (the [`PostTypeDef`](crate::types::PostTypeDef) as JSON, a `long_text`: room for
/// a large SVG menu icon), `created_at` and `updated_at` (UTC).
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261115120000_create_post_types_table",
        vec![schema::create("post_types", |t| {
            t.id();
            t.string("slug").length(20);
            t.long_text("definition");
            t.date_time("created_at");
            t.date_time("updated_at");
            t.unique(["slug"]);
        })],
        vec![schema::drop_if_exists("post_types")],
    )
}
