//! The `field_groups` table: the field groups made in the admin's manager screens (design 5.3),
//! one definition per row, as ACF stores a group as an `acf-field-group` post. Groups defined in
//! code or by a plugin are never stored, as ACF keeps local JSON out of the database.

use orbit_orm::Migration;
use orbit_orm::schema;

/// Creates `field_groups`: `id`, `group_key` (64 characters, unique: the group's `group_…` key;
/// not `key`, which MySQL reserves),
/// `definition` (the [`FieldGroupDef`](crate::types::FieldGroupDef) as JSON, with its fields, a
/// `long_text`), `created_at` and `updated_at` (UTC).
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261115120200_create_field_groups_table",
        vec![schema::create("field_groups", |t| {
            t.id();
            t.string("group_key").length(64);
            t.long_text("definition");
            t.date_time("created_at");
            t.date_time("updated_at");
            t.unique(["group_key"]);
        })],
        vec![schema::drop_if_exists("field_groups")],
    )
}
