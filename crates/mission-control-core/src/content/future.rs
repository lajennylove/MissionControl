//! Scheduled posts: WordPress's `publish_future_post` cron event, which publishes a `future`
//! post at its date.

use chrono::{SubsecRound, Utc};
use orbit_orm::{Database, Model, Operator};
use tokio::runtime::Handle;

use super::taxonomy::recount_post_terms;
use super::{Post, PostStatus};
use crate::hooks::{Action, DEFAULT_PRIORITY, Owner};
use crate::scheduler::{self, Error, Scheduler};

/// The action the scheduler runs at a scheduled post's date, with the post's id, as WordPress's
/// `publish_future_post` cron event.
pub const PUBLISH_FUTURE_POST: Action<i64> = Action::new("publish_future_post");

/// Hooks [`check_and_publish_future_post`] onto [`PUBLISH_FUTURE_POST`] (owner
/// [`Owner::Core`]) and registers the hook with `scheduler`, as WordPress hooks
/// `check_and_publish_future_post` in its default filters. The app calls it once at start.
///
/// Hook callbacks are synchronous and the database is not, so the callback blocks on the
/// current Tokio runtime. The scheduler runs events on blocking threads, where that is allowed;
/// code that runs [`PUBLISH_FUTURE_POST`] itself must do the same (`spawn_blocking`). A
/// database error is logged at `error`.
pub fn register_publish_future_post(scheduler: &Scheduler) {
    let db = std::sync::Arc::clone(scheduler.database());
    scheduler.hooks().add_action(
        PUBLISH_FUTURE_POST,
        DEFAULT_PRIORITY,
        Owner::Core,
        move |id: &i64| {
            let outcome = Handle::current().block_on(check_and_publish_future_post(&db, *id));
            if let Err(e) = outcome {
                log::error!("could not publish the scheduled post {id}: {e}");
            }
        },
    );
    scheduler.register(PUBLISH_FUTURE_POST);
}

/// Publishes post `id` if it is still `future`, as WordPress's
/// `check_and_publish_future_post`. A post that is gone, or no longer `future`, is left alone.
/// A post whose date is still ahead, because the date moved after the event was scheduled, is
/// not published; its event is scheduled again for that date. Publishing sets only the status,
/// with a conditional update, so a post moved out of `future` meanwhile is not published, an
/// edit saved meanwhile is kept, and `modified_at` stays as it was, as with `wp_publish_post`.
/// Returns whether it published the post.
///
/// # Errors
///
/// Fails when the database refuses a query.
pub async fn check_and_publish_future_post(db: &Database, id: i64) -> Result<bool, Error> {
    let Some(post) = Post::query().find(db, id).await? else {
        return Ok(false);
    };
    if post.status != PostStatus::Future {
        return Ok(false);
    }
    if let Some(date) = post.published_at
        && date > Utc::now().naive_utc().trunc_subsecs(0)
    {
        scheduler::unschedule(db, PUBLISH_FUTURE_POST, &id).await?;
        scheduler::schedule_single(db, date, PUBLISH_FUTURE_POST, &id).await?;
        return Ok(false);
    }
    // WordPress's `wp_publish_post`: only the status changes, and only if the post is still
    // `future` and due, so an edit saved since the read is kept and `modified_at` is not touched.
    // Its terms are recounted in the same transaction, as WordPress does on the status change.
    let now = Utc::now().naive_utc().trunc_subsecs(0);
    let published = db
        .transaction(async |tx| {
            let published = Post::query()
                .where_eq(Post::PRIMARY_KEY, id)
                .where_eq("status", PostStatus::Future)
                .where_group(|q| {
                    q.where_null("published_at")
                        .or_where_op("published_at", Operator::Le, now)
                })
                .update(&mut *tx, &[("status", PostStatus::Publish.into())])
                .await?
                == 1;
            if published {
                recount_post_terms(tx, id).await?;
                if let Some(post) = Post::query().find(&mut *tx, id).await? {
                    crate::theme::auto_add_page_in(tx, &post, Some(PostStatus::Future)).await?;
                }
            }
            Ok::<_, orbit_orm::Error>(published)
        })
        .await?;
    Ok(published)
}

/// Schedules `post`'s publication, as WordPress's `_future_post_hook` does when a post becomes
/// `future`: any [`PUBLISH_FUTURE_POST`] event for the post is removed, and when the post is
/// `future` with a date, a new one is scheduled for that date. Call it after saving a post whose
/// status or date changed.
///
/// # Errors
///
/// Fails when the database refuses a query.
pub async fn schedule_future_post(scheduler: &Scheduler, post: &Post) -> Result<(), Error> {
    scheduler.unschedule(PUBLISH_FUTURE_POST, &post.id).await?;
    if let (PostStatus::Future, Some(date)) = (post.status, post.published_at) {
        scheduler
            .schedule(date, PUBLISH_FUTURE_POST, post.id)
            .await?;
    }
    Ok(())
}
