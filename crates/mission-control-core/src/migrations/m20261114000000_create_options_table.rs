//! The `options` table: site-wide settings by name, after WordPress's `wp_options`
//! (`option_id`, `option_name`, `option_value`, `autoload`), with Laravel-style column names.

use orbit_orm::Migration;
use orbit_orm::schema;

/// Creates `options`. `name` is 191 characters, as WordPress sizes `option_name`, so its unique
/// index fits MySQL's key limit with `utf8mb4`. `autoload` takes WordPress 6.6's values (`on`,
/// `off`, `auto`, `auto-on`, `auto-off`) in a 20-character string defaulting to `auto`, and is
/// indexed as WordPress indexes it.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000000_create_options_table",
        vec![schema::create("options", |t| {
            t.id();
            t.string("name").length(191);
            t.text("value");
            t.string("autoload").length(20).default("auto");
            t.unique(["name"]);
            t.index(["autoload"]);
        })],
        vec![schema::drop_if_exists("options")],
    )
}
