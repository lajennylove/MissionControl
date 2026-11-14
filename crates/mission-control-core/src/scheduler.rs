//! The scheduler: Mission Control's WP-Cron. It runs an action at a time, once or every
//! interval, and keeps its events in the `scheduled_events` table so they survive a restart.
//!
//! An event is a hook name and a JSON payload. When it is due, the scheduler runs the action
//! through [`Hooks::do_action`], so every callback on the hook runs, as WordPress runs a cron
//! event with `do_action_ref_array`. Hook callbacks are typed and rows are not, so after a
//! restart the app [registers](Scheduler::register) each hook it schedules; an event whose hook
//! is not registered stays in the table, and the scheduler logs a warning each time it is due.
//!
//! The app builds one scheduler at start, registers its hooks and spawns [`Scheduler::run`] as a
//! background task:
//!
//! ```ignore
//! let scheduler = Arc::new(Scheduler::new(Arc::clone(&db), Arc::clone(&hooks)));
//! mission_control_core::content::register_publish_future_post(&scheduler);
//! tokio::spawn({
//!     let scheduler = Arc::clone(&scheduler);
//!     async move { scheduler.run(Duration::from_secs(60)).await }
//! });
//! ```
//!
//! # Failures
//!
//! Each event runs on a blocking thread (`spawn_blocking`), under `catch_unwind`. A callback that
//! panics is logged at `error` and the tick goes on with the other due events. As in WordPress,
//! the scheduler takes an event before it runs it: a single event is deleted, and a recurring
//! one moved to its next run, so a failed single event is dropped and a failed recurring one
//! runs again at its next time. Taking an event is a conditional `DELETE` or `UPDATE`, so two
//! app processes on one database never run the same event twice.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{NaiveDateTime, SubsecRound, TimeDelta, Utc};
use orbit_orm::{Database, Direction, Executor, Model, Operator, OrbitModel, Value};

use crate::hooks::{Action, Hooks, Payload};

/// A row of `scheduled_events`: one action to run at `run_at`, once or every
/// `interval_seconds`.
#[derive(Debug, Clone, PartialEq, Eq, OrbitModel)]
#[orbit(table = "scheduled_events")]
pub struct ScheduledEvent {
    /// The id.
    pub id: i64,
    /// The action's name, such as `publish_future_post`.
    #[orbit(fillable)]
    pub hook: String,
    /// The action's payload, as JSON.
    #[orbit(fillable)]
    pub payload: String,
    /// When it runs next, in UTC, to the second.
    #[orbit(fillable)]
    pub run_at: NaiveDateTime,
    /// The period of a recurring event, in seconds; `None` for a single event.
    #[orbit(fillable)]
    pub interval_seconds: Option<i64>,
}

/// What went wrong while scheduling an event or reading the events.
#[derive(Debug)]
pub enum Error {
    /// The database refused a query.
    Database(orbit_orm::Error),
    /// The payload did not serialise to JSON.
    Payload(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(e) => write!(f, "scheduler database error: {e}"),
            Self::Payload(e) => write!(f, "the event payload is not JSON: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            Self::Payload(e) => Some(e),
        }
    }
}

impl From<orbit_orm::Error> for Error {
    fn from(e: orbit_orm::Error) -> Self {
        Self::Database(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Payload(e)
    }
}

/// What one [`Scheduler::run_due`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tick {
    /// Events whose callbacks all returned.
    pub completed: usize,
    /// Events that panicked or whose payload did not decode. Logged at `error`.
    pub failed: usize,
    /// Due events left in place because their hook is not registered. Logged at `warn`.
    pub unregistered: usize,
}

/// Decodes a stored payload and runs the action with it.
type Dispatcher = Arc<dyn Fn(&Hooks, &str) -> Result<(), serde_json::Error> + Send + Sync>;

/// The scheduler. Holds the database, the hooks it runs events through, and the hooks it may run.
/// Methods take `&self`; share it behind an `Arc`.
pub struct Scheduler {
    db: Arc<Database>,
    hooks: Arc<Hooks>,
    dispatchers: RwLock<HashMap<&'static str, Dispatcher>>,
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler").finish_non_exhaustive()
    }
}

impl Scheduler {
    /// A scheduler that keeps its events in `db` and runs them through `hooks`. No hook is
    /// registered yet.
    pub fn new(db: Arc<Database>, hooks: Arc<Hooks>) -> Self {
        Self {
            db,
            hooks,
            dispatchers: RwLock::default(),
        }
    }

    /// The database the events live in.
    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    /// The hooks the events run through.
    pub fn hooks(&self) -> &Arc<Hooks> {
        &self.hooks
    }

    /// Lets the scheduler run `hook`'s events: it decodes their payload as `A` and calls
    /// [`Hooks::do_action`]. Call it at start for every hook the app schedules, so events stored
    /// before a restart run. Scheduling an event registers its hook too. Registering twice is
    /// harmless.
    pub fn register<A: Payload>(&self, hook: Action<A>) {
        let dispatch: Dispatcher = Arc::new(move |hooks: &Hooks, json: &str| {
            let payload: A = serde_json::from_str(json)?;
            hooks.do_action(hook, &payload);
            Ok(())
        });
        self.dispatchers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hook.name(), dispatch);
    }

    /// Runs `hook` with `payload` once, at `at` (UTC), as WordPress's
    /// `wp_schedule_single_event`. Times are kept to the second. Returns the event's id.
    ///
    /// # Errors
    ///
    /// Fails when the payload does not serialise or the database refuses the insert.
    pub async fn schedule<A: Payload>(
        &self,
        at: NaiveDateTime,
        hook: Action<A>,
        payload: A,
    ) -> Result<i64, Error> {
        self.register(hook);
        insert(&*self.db, at, hook, &payload, None).await
    }

    /// Runs `hook` every `every`, as WordPress's `wp_schedule_event`: first at once (on the next
    /// tick), then every interval. The app calls it at each start, so when the hook already has
    /// a recurring event, that event is kept and only its interval updated.
    ///
    /// # Errors
    ///
    /// Fails when the database refuses the query.
    ///
    /// # Panics
    ///
    /// Panics when `every` is shorter than one second, the scheduler's resolution.
    pub async fn schedule_recurring(&self, every: Duration, hook: Action<()>) -> Result<(), Error> {
        let seconds = i64::try_from(every.as_secs()).unwrap_or(i64::MAX);
        assert!(
            seconds > 0,
            "a recurring event's interval is at least one second"
        );
        self.register(hook);
        let payload = serde_json::to_string(&())?;
        let updated = ScheduledEvent::query()
            .where_eq("hook", hook.name())
            .where_eq("payload", payload)
            .where_not_null("interval_seconds")
            .update(&*self.db, &[("interval_seconds", seconds.into())])
            .await?;
        if updated == 0 {
            insert(&*self.db, now(), hook, &(), Some(seconds)).await?;
        }
        Ok(())
    }

    /// Removes every event, single or recurring, of `hook` with `payload`, as WordPress's
    /// `wp_clear_scheduled_hook`. Returns how many were removed.
    ///
    /// # Errors
    ///
    /// Fails when the payload does not serialise or the database refuses the delete.
    pub async fn unschedule<A: Payload>(&self, hook: Action<A>, payload: &A) -> Result<u64, Error> {
        unschedule(&*self.db, hook, payload).await
    }

    /// The next time `hook` runs with `payload`, if it is scheduled, as WordPress's
    /// `wp_next_scheduled`.
    ///
    /// # Errors
    ///
    /// Fails when the payload does not serialise or the database refuses the query.
    pub async fn next_scheduled<A: Payload>(
        &self,
        hook: Action<A>,
        payload: &A,
    ) -> Result<Option<NaiveDateTime>, Error> {
        let event = ScheduledEvent::query()
            .where_eq("hook", hook.name())
            .where_eq("payload", serde_json::to_string(payload)?)
            .order_by("run_at", Direction::Asc)
            .first(&*self.db)
            .await?;
        Ok(event.map(|e| e.run_at))
    }

    /// Runs every event due at `now` (UTC), oldest first. A tick: [`run`](Self::run) calls it on
    /// its interval, and tests call it with the time they need.
    ///
    /// # Errors
    ///
    /// Fails when the database refuses a query; events already run stay run.
    pub async fn run_due(&self, now: NaiveDateTime) -> Result<Tick, Error> {
        let due = ScheduledEvent::query()
            .where_op("run_at", Operator::Le, now)
            .order_by("run_at", Direction::Asc)
            .order_by("id", Direction::Asc)
            .get(&*self.db)
            .await?;
        let mut tick = Tick::default();
        for event in due {
            let Some(dispatch) = self.dispatcher(&event.hook) else {
                log::warn!(
                    "scheduled event {} for `{}` is due, but the hook is not registered with \
                     the scheduler; it stays pending",
                    event.id,
                    event.hook
                );
                tick.unregistered += 1;
                continue;
            };
            if !self.take(&event, now).await? {
                // Another process took it.
                continue;
            }
            if self.dispatch(dispatch, &event).await {
                tick.completed += 1;
            } else {
                tick.failed += 1;
            }
        }
        Ok(tick)
    }

    /// Runs the due events every `interval`, forever: the app's background task. A tick that
    /// fails is logged at `error` and the loop goes on. A minute matches Laravel's scheduler.
    pub async fn run(&self, interval: Duration) {
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            if let Err(e) = self.run_due(Utc::now().naive_utc()).await {
                log::error!("the scheduler could not run the due events: {e}");
            }
        }
    }

    fn dispatcher(&self, hook: &str) -> Option<Dispatcher> {
        self.dispatchers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hook)
            .cloned()
    }

    /// Claims `event` before running it, as WordPress unschedules an event before it runs it: a
    /// single event is deleted, a recurring one moved to its next run after `now`. The query
    /// matches the row as it was read, so `false` means another process claimed it first.
    async fn take(&self, event: &ScheduledEvent, now: NaiveDateTime) -> Result<bool, Error> {
        let row = ScheduledEvent::query()
            .where_eq(ScheduledEvent::PRIMARY_KEY, event.id)
            .where_eq("run_at", event.run_at);
        let claimed = match event.interval_seconds {
            None => row.delete(&*self.db).await?,
            Some(seconds) => {
                let next = next_run(event.run_at, seconds, now);
                row.update(&*self.db, &[("run_at", next.into())]).await?
            }
        };
        Ok(claimed == 1)
    }

    /// Runs one event's callbacks on a blocking thread, catching a panic. Returns whether it
    /// completed; a failure is logged.
    async fn dispatch(&self, dispatch: Dispatcher, event: &ScheduledEvent) -> bool {
        let hooks = Arc::clone(&self.hooks);
        let payload = event.payload.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            catch_unwind(AssertUnwindSafe(|| dispatch(&hooks, &payload)))
        })
        .await;
        let failure = match outcome {
            Ok(Ok(Ok(()))) => return true,
            Ok(Ok(Err(e))) => format!("its payload {:?} did not decode: {e}", event.payload),
            Ok(Err(panic)) => format!("it panicked: {}", panic_message(&*panic)),
            Err(e) => format!("its task failed: {e}"),
        };
        log::error!(
            "scheduled event {} for `{}` failed; {failure}",
            event.id,
            event.hook
        );
        false
    }
}

/// Inserts an event and returns its id.
async fn insert<'a, A: Payload>(
    db: impl Executor<'a>,
    at: NaiveDateTime,
    hook: Action<A>,
    payload: &A,
    interval_seconds: Option<i64>,
) -> Result<i64, Error> {
    let columns: [(&str, Value); 4] = [
        ("hook", hook.name().into()),
        ("payload", serde_json::to_string(payload)?.into()),
        ("run_at", at.trunc_subsecs(0).into()),
        ("interval_seconds", interval_seconds.into()),
    ];
    let id = ScheduledEvent::query().insert(db, &columns).await?;
    Ok(i64::try_from(id).expect("an id from a BIGINT column fits in i64"))
}

/// Schedules a single event without registering its hook: for core handlers that hold the
/// database but not the scheduler.
pub(crate) async fn schedule_single<'a, A: Payload>(
    db: impl Executor<'a>,
    at: NaiveDateTime,
    hook: Action<A>,
    payload: &A,
) -> Result<i64, Error> {
    insert(db, at, hook, payload, None).await
}

/// Deletes every event of `hook` with `payload`.
pub(crate) async fn unschedule<'a, A: Payload>(
    db: impl Executor<'a>,
    hook: Action<A>,
    payload: &A,
) -> Result<u64, Error> {
    Ok(ScheduledEvent::query()
        .where_eq("hook", hook.name())
        .where_eq("payload", serde_json::to_string(payload)?)
        .delete(db)
        .await?)
}

/// The first run of a recurring event after `now`, keeping its phase: runs it missed are
/// skipped, not caught up, as WordPress's `wp_reschedule_event` does.
fn next_run(run_at: NaiveDateTime, seconds: i64, now: NaiveDateTime) -> NaiveDateTime {
    let behind = (now - run_at).num_seconds().max(0);
    let periods = behind / seconds + 1;
    run_at + TimeDelta::seconds(periods.saturating_mul(seconds))
}

/// The current time in UTC, to the second.
fn now() -> NaiveDateTime {
    Utc::now().naive_utc().trunc_subsecs(0)
}

/// The message a panic carried, when it is a string.
fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a non-string panic payload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 14)
            .unwrap()
            .and_hms_opt(h, m, s)
            .unwrap()
    }

    #[test]
    fn next_run_keeps_the_phase_and_skips_missed_runs() {
        assert_eq!(next_run(at(9, 0, 0), 3600, at(9, 0, 0)), at(10, 0, 0));
        assert_eq!(next_run(at(9, 0, 0), 3600, at(9, 59, 59)), at(10, 0, 0));
        assert_eq!(next_run(at(9, 0, 0), 3600, at(10, 0, 0)), at(11, 0, 0));
        assert_eq!(next_run(at(9, 0, 0), 3600, at(12, 30, 0)), at(13, 0, 0));
        // A clock that went back does not run it early.
        assert_eq!(next_run(at(9, 0, 0), 60, at(8, 0, 0)), at(9, 1, 0));
    }

    #[test]
    fn panic_message_reads_string_payloads() {
        let panic = std::panic::catch_unwind(|| panic!("static")).unwrap_err();
        assert_eq!(panic_message(&*panic), "static");
        let panic = std::panic::catch_unwind(|| panic!("formatted {}", 1)).unwrap_err();
        assert_eq!(panic_message(&*panic), "formatted 1");
    }
}
