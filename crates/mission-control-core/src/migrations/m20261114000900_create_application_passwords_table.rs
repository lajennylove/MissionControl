//! The `application_passwords` table: the passwords a user creates for REST API clients, after
//! the items WordPress keeps in the `_application_passwords` user meta.

use orbit_orm::Migration;
use orbit_orm::schema::{self, Action};

/// Creates `application_passwords`: `id`, `uuid` (WordPress's `wp_generate_uuid4`, unique),
/// `user_id`, `name`, `password` (the hash, never the password: `$generic$` and 40 characters),
/// `created_at`, `last_used_at` and `last_ip` (45 characters, the longest IPv6 text), as
/// WordPress's `uuid`, `name`, `password`, `created`, `last_used` and `last_ip`. Times are UTC.
/// `user_id` is indexed; deleting the user deletes its rows. Names are unique per user, but
/// compared without case as WordPress compares them, so the check is in code, not an index.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114000900_create_application_passwords_table",
        vec![schema::create("application_passwords", |t| {
            t.id();
            t.string("uuid").length(36);
            t.big_integer("user_id");
            t.string("name").length(255);
            t.string("password").length(255);
            t.date_time("created_at");
            t.date_time("last_used_at").nullable();
            t.string("last_ip").length(45).nullable();
            t.unique(["uuid"]);
            t.index(["user_id"]);
            t.foreign("user_id")
                .references("users", "id")
                .on_delete(Action::Cascade);
        })],
        vec![schema::drop_if_exists("application_passwords")],
    )
}
