//! The `field_group_locations` table: the location rules of the stored field groups, one rule
//! per row, so the edit screens find the groups for a post type (or a template, a taxonomy, ...)
//! without reading every group's JSON. The group's JSON stays the source; the rows are rewritten
//! from it whenever the group is saved.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `field_group_locations`: `id`, `group_key` (the group's key; deleting the group
/// deletes its rows), `or_group` (which of the location's or-groups the rule is in, from 0),
/// `position` (the rule's place in its or-group, from 0: the rules of one or-group must all
/// match), `param` (32 characters, such as `post_type`), `operator` (`==` or `!=`) and `value`
/// (up to 4096 characters, the definition's limit). `param` and `value` are matched by lookups,
/// so `param` is indexed; `value` is too long for a portable index, and the rows of one
/// parameter are few.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261115120300_create_field_group_locations_table",
        vec![schema::create("field_group_locations", |t| {
            t.id();
            t.string("group_key").length(64);
            t.integer("or_group");
            t.integer("position");
            t.string("param").length(32);
            t.string("operator").length(2);
            t.string("value").length(4096);
            t.index(["group_key"]);
            t.index(["param"]);
            t.foreign("group_key")
                .references("field_groups", "group_key")
                .on_delete(Action::Cascade);
        })],
        vec![schema::drop_if_exists("field_group_locations")],
    )
}
