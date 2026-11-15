//! The `scheduled_events` table: the scheduler's events, after WordPress's `cron` option, so that
//! they survive a restart.

use orbit_orm::Migration;
use orbit_orm::schema;

/// Creates `scheduled_events`. `hook` is the action's name, 191 characters like option names so
/// its index fits MySQL's key limit with `utf8mb4`; `payload` is the action's argument as JSON,
/// in a `long_text` as the `cron` option WordPress keeps events in; `run_at` is the next run,
/// in UTC; `interval_seconds` is null for a single event and the period of a recurring one.
/// `run_at` is indexed for the scheduler's "what is due" query, and `hook` for looking events
/// up by hook.
pub(super) fn migration() -> Migration {
    Migration::schema(
        "20261114120200_create_scheduled_events_table",
        vec![schema::create("scheduled_events", |t| {
            t.id();
            t.string("hook").length(191);
            t.long_text("payload");
            t.date_time("run_at");
            t.big_integer("interval_seconds").nullable();
            t.index(["run_at"]);
            t.index(["hook"]);
        })],
        vec![schema::drop_if_exists("scheduled_events")],
    )
}
