//! Old auto-drafts: WordPress's `wp_scheduled_auto_draft_delete` cron event, which deletes the
//! `auto-draft` posts a New screen opened and nobody saved (`wp_delete_auto_drafts`), once a
//! day (Ruling R125).

use std::time::Duration;

use chrono::{SubsecRound, TimeDelta, Utc};
use orbit_orm::{Database, Error, Model, Operator};
use tokio::runtime::Handle;

use super::{Post, PostStatus};
use crate::hooks::{Action, DEFAULT_PRIORITY, Owner};
use crate::scheduler::Scheduler;

/// The daily action that deletes old auto-drafts, as WordPress's `wp_scheduled_auto_draft_delete`.
pub const DELETE_AUTO_DRAFTS: Action<()> = Action::new("wp_scheduled_auto_draft_delete");

/// How old an auto-draft must be to be deleted: 7 days, as `wp_delete_auto_drafts`.
pub const AUTO_DRAFT_MAX_AGE: TimeDelta = TimeDelta::days(7);

/// The most auto-drafts one run deletes; the next day's run takes the rest.
pub const MAX_AUTO_DRAFTS_PER_RUN: u64 = 1000;

/// How often the event runs: daily.
const DAILY: Duration = Duration::from_secs(24 * 60 * 60);

/// Deletes the auto-drafts last changed more than [`AUTO_DRAFT_MAX_AGE`] ago, at most
/// [`MAX_AUTO_DRAFTS_PER_RUN`], each for good with [`Post::delete`] (its meta, terms and
/// revisions with it), as `wp_delete_auto_drafts` calls `wp_delete_post( $id, true )`. Returns how
/// many it deleted.
///
/// # Errors
///
/// The database's error; the posts deleted before it stay deleted.
pub async fn delete_old_auto_drafts(db: &Database) -> Result<u64, Error> {
    let cutoff = Utc::now().naive_utc().trunc_subsecs(0) - AUTO_DRAFT_MAX_AGE;
    let old = Post::query()
        .where_eq("status", PostStatus::AutoDraft)
        .where_op("modified_at", Operator::Lt, cutoff)
        .order_by(Post::PRIMARY_KEY, orbit_orm::Direction::Asc)
        .limit(MAX_AUTO_DRAFTS_PER_RUN)
        .get(db)
        .await?;
    let mut deleted = 0;
    for post in old {
        if post.delete(db).await? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Hooks [`delete_old_auto_drafts`] onto [`DELETE_AUTO_DRAFTS`] (owner [`Owner::Core`]) and
/// registers the hook with `scheduler`, as [`register_publish_future_post`] does. A database
/// error is logged at `error`. Then [`schedule_auto_draft_deletion`] makes it run daily.
///
/// [`register_publish_future_post`]: super::register_publish_future_post
pub fn register_delete_auto_drafts(scheduler: &Scheduler) {
    let db = std::sync::Arc::clone(scheduler.database());
    scheduler.hooks().add_action(
        DELETE_AUTO_DRAFTS,
        DEFAULT_PRIORITY,
        Owner::Core,
        move |(): &()| match Handle::current().block_on(delete_old_auto_drafts(&db)) {
            Ok(0) => {}
            Ok(n) => log::info!("Deleted {n} old auto-drafts"),
            Err(e) => log::error!("could not delete the old auto-drafts: {e}"),
        },
    );
    scheduler.register(DELETE_AUTO_DRAFTS);
}

/// Schedules [`DELETE_AUTO_DRAFTS`] daily, as `wp_schedule_event( time(), 'daily',
/// 'wp_scheduled_auto_draft_delete' )`; the app calls it at each start, which keeps one event.
/// The first run is a day after the first start (WordPress's is at the first cron run): no
/// auto-draft can be 7 days old before then, and a start does not run it.
///
/// # Errors
///
/// The scheduler's error, from the database.
pub async fn schedule_auto_draft_deletion(
    scheduler: &Scheduler,
) -> Result<(), crate::scheduler::Error> {
    let first = Utc::now().naive_utc() + TimeDelta::days(1);
    scheduler
        .schedule_recurring_from(first, DAILY, DELETE_AUTO_DRAFTS)
        .await
}
