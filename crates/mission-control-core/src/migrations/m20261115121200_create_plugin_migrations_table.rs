//! The `plugin_migrations` table: the migrations each plugin ran (Task 12.6), as Laravel's
//! `migrations` table records an app's. Plugins are not recorded in `__diesel_schema_migrations`,
//! which holds the core's and the panel's 14-digit versions only: a plugin's version could equal
//! one of theirs, and the installer's rollback refuses a version it does not know.

use orbit_orm::Migration;
use orbit_orm::schema;

/// Creates `plugin_migrations`: `id`, `plugin` (the plugin's slug, at most 32 characters),
/// `migration` (its name, such as `20261115000000_create_books`, at most 128), `batch` (the
/// activation that ran it; one activation is one batch, as one `php artisan migrate`) and
/// `applied_at` (when, in UTC). A plugin runs each migration once: `plugin` and `migration` are
/// unique together.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261115121200_create_plugin_migrations_table",
        vec![schema::create("plugin_migrations", |t| {
            t.id();
            t.string("plugin").length(32);
            t.string("migration").length(128);
            t.integer("batch");
            t.date_time("applied_at");
            t.unique(["plugin", "migration"]);
        })],
        vec![schema::drop_if_exists("plugin_migrations")],
    )
}
