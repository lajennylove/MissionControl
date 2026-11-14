//! Tests for the scheduler and the `scheduled_events` table. They run on the database in
//! `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use chrono::{NaiveDateTime, SubsecRound, TimeDelta, Utc};
use mission_control_core::content::{
    NewPost, PUBLISH_FUTURE_POST, Post, PostStatus, register_publish_future_post,
    schedule_future_post,
};
use mission_control_core::hooks::{Action, DEFAULT_PRIORITY, Hooks, Owner};
use mission_control_core::migrations::migrator;
use mission_control_core::scheduler::{ScheduledEvent, Scheduler, Tick};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};

const NOTE: Action<String> = Action::new("mc_test_note");
const BOOM: Action<()> = Action::new("mc_test_boom");
const HOURLY: Action<()> = Action::new("mc_test_hourly");
const NEVER_REGISTERED: Action<i64> = Action::new("mc_test_never_registered");

/// The log lines the tests wrote, captured by a logger installed once per test binary.
static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        LOGS.lock()
            .unwrap()
            .push(format!("{} {}", record.level(), record.args()));
    }
    fn flush(&self) {}
}

fn capture_logs() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&Capture).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    });
}

fn logged(needle: &str) -> Vec<String> {
    LOGS.lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains(needle))
        .cloned()
        .collect()
}

fn now() -> NaiveDateTime {
    Utc::now().naive_utc().trunc_subsecs(0)
}

async fn migrate(db: &Database) {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
}

async fn author(db: &Database) -> i64 {
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .expect("inserts the author");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the author id")
}

/// Hooks with a callback on `NOTE` that records every payload it receives.
fn recording_hooks() -> (Arc<Hooks>, Arc<Mutex<Vec<String>>>) {
    let hooks = Arc::new(Hooks::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    hooks.add_action(NOTE, DEFAULT_PRIORITY, Owner::Core, move |note: &String| {
        sink.lock().unwrap().push(note.clone());
    });
    (hooks, seen)
}

async fn events(db: &Database) -> Vec<ScheduledEvent> {
    ScheduledEvent::query()
        .order_by("id", orbit_orm::Direction::Asc)
        .get(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn runs_a_single_event_once_when_it_is_due() {
    let test = database().await;
    migrate(test.db()).await;
    let (hooks, seen) = recording_hooks();
    let scheduler = Scheduler::new(test.shared(), hooks);

    let at = now() + TimeDelta::minutes(5);
    scheduler
        .schedule(at, NOTE, "hello".to_owned())
        .await
        .unwrap();

    let stored = events(test.db()).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].hook, "mc_test_note");
    assert_eq!(stored[0].payload, "\"hello\"");
    assert_eq!(stored[0].run_at, at);
    assert_eq!(stored[0].interval_seconds, None);

    assert_eq!(
        scheduler.run_due(at - TimeDelta::seconds(1)).await.unwrap(),
        Tick::default()
    );
    assert!(seen.lock().unwrap().is_empty(), "ran before it was due");

    let tick = scheduler.run_due(at).await.unwrap();
    assert_eq!(tick.completed, 1);
    assert_eq!(*seen.lock().unwrap(), ["hello"]);
    assert!(
        events(test.db()).await.is_empty(),
        "a single event is removed"
    );

    scheduler.run_due(at + TimeDelta::hours(1)).await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1, "ran twice");
}

#[tokio::test]
async fn runs_due_events_in_time_order() {
    let test = database().await;
    migrate(test.db()).await;
    let (hooks, seen) = recording_hooks();
    let scheduler = Scheduler::new(test.shared(), hooks);

    let t = now();
    scheduler
        .schedule(t + TimeDelta::seconds(3), NOTE, "third".into())
        .await
        .unwrap();
    scheduler
        .schedule(t + TimeDelta::seconds(1), NOTE, "first".into())
        .await
        .unwrap();
    scheduler
        .schedule(t + TimeDelta::seconds(2), NOTE, "second".into())
        .await
        .unwrap();
    scheduler
        .schedule(t + TimeDelta::hours(1), NOTE, "later".into())
        .await
        .unwrap();

    let tick = scheduler.run_due(t + TimeDelta::seconds(10)).await.unwrap();
    assert_eq!(tick.completed, 3);
    assert_eq!(*seen.lock().unwrap(), ["first", "second", "third"]);
    assert_eq!(events(test.db()).await.len(), 1);
}

#[tokio::test]
async fn an_event_scheduled_before_a_restart_runs_after_it() {
    let test = database().await;
    migrate(test.db()).await;
    let at = now() + TimeDelta::seconds(1);
    {
        let (hooks, _) = recording_hooks();
        let before = Scheduler::new(test.shared(), hooks);
        before.schedule(at, NOTE, "survived".into()).await.unwrap();
    }

    // A new process: new hooks, a new scheduler, the same database.
    let (hooks, seen) = recording_hooks();
    let after = Scheduler::new(test.shared(), hooks);
    after.register(NOTE);
    let tick = after.run_due(at + TimeDelta::seconds(1)).await.unwrap();
    assert_eq!(tick.completed, 1);
    assert_eq!(*seen.lock().unwrap(), ["survived"]);
}

#[tokio::test]
async fn an_event_whose_hook_is_not_registered_stays_pending() {
    capture_logs();
    let test = database().await;
    migrate(test.db()).await;
    let at = now();
    {
        let before = Scheduler::new(test.shared(), Arc::new(Hooks::new()));
        before.schedule(at, NEVER_REGISTERED, 7).await.unwrap();
    }

    let after = Scheduler::new(test.shared(), Arc::new(Hooks::new()));
    let tick = after.run_due(at + TimeDelta::seconds(1)).await.unwrap();
    assert_eq!(
        tick,
        Tick {
            unregistered: 1,
            ..Tick::default()
        }
    );
    assert_eq!(events(test.db()).await.len(), 1, "the event was dropped");
    let lines = logged("mc_test_never_registered");
    assert!(lines.iter().any(|l| l.starts_with("WARN")), "{lines:?}");
}

#[tokio::test]
async fn a_panicking_event_is_logged_and_the_others_run() {
    capture_logs();
    let test = database().await;
    migrate(test.db()).await;
    let (hooks, seen) = recording_hooks();
    hooks.add_action(BOOM, DEFAULT_PRIORITY, Owner::Core, |_: &()| {
        panic!("mc_test_boom went off");
    });
    let scheduler = Scheduler::new(test.shared(), hooks);

    let t = now();
    scheduler.schedule(t, NOTE, "before".into()).await.unwrap();
    scheduler
        .schedule(t + TimeDelta::seconds(1), BOOM, ())
        .await
        .unwrap();
    scheduler
        .schedule(t + TimeDelta::seconds(2), NOTE, "after".into())
        .await
        .unwrap();

    let tick = scheduler.run_due(t + TimeDelta::seconds(5)).await.unwrap();
    assert_eq!(
        tick,
        Tick {
            completed: 2,
            failed: 1,
            unregistered: 0
        }
    );
    assert_eq!(*seen.lock().unwrap(), ["before", "after"]);
    // WordPress drops a single event once it has been attempted.
    assert!(events(test.db()).await.is_empty());

    let lines = logged("mc_test_boom went off");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("ERROR") && l.contains("mc_test_boom")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn a_recurring_event_runs_every_interval_and_is_not_duplicated() {
    let test = database().await;
    migrate(test.db()).await;
    let hooks = Arc::new(Hooks::new());
    let runs = Arc::new(Mutex::new(0));
    let count = Arc::clone(&runs);
    hooks.add_action(HOURLY, DEFAULT_PRIORITY, Owner::Core, move |_: &()| {
        *count.lock().unwrap() += 1;
    });
    let scheduler = Scheduler::new(test.shared(), Arc::clone(&hooks));

    let t = now();
    let hour = Duration::from_secs(3600);
    scheduler.schedule_recurring(hour, HOURLY).await.unwrap();
    // The app calls it at every start; a second call keeps the one event.
    scheduler.schedule_recurring(hour, HOURLY).await.unwrap();
    let stored = events(test.db()).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].interval_seconds, Some(3600));
    let first = stored[0].run_at;
    assert!(
        first >= t && first <= now(),
        "due at once, as WordPress schedules it"
    );

    scheduler.run_due(first).await.unwrap();
    assert_eq!(*runs.lock().unwrap(), 1);
    assert_eq!(
        events(test.db()).await[0].run_at,
        first + TimeDelta::hours(1)
    );

    // Three hours missed: it runs once and keeps its phase, as wp_reschedule_event does.
    let late = first + TimeDelta::hours(3) + TimeDelta::minutes(10);
    scheduler.run_due(late).await.unwrap();
    assert_eq!(*runs.lock().unwrap(), 2);
    assert_eq!(
        events(test.db()).await[0].run_at,
        first + TimeDelta::hours(4)
    );
}

#[tokio::test]
async fn a_panicking_recurring_event_is_rescheduled() {
    capture_logs();
    let test = database().await;
    migrate(test.db()).await;
    let hooks = Arc::new(Hooks::new());
    hooks.add_action(HOURLY, DEFAULT_PRIORITY, Owner::Core, |_: &()| {
        panic!("mc_test_hourly failed");
    });
    let scheduler = Scheduler::new(test.shared(), hooks);
    scheduler
        .schedule_recurring(Duration::from_secs(3600), HOURLY)
        .await
        .unwrap();
    let first = events(test.db()).await[0].run_at;

    let tick = scheduler.run_due(first).await.unwrap();
    assert_eq!(tick.failed, 1);
    let stored = events(test.db()).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].run_at, first + TimeDelta::hours(1));
}

#[tokio::test]
async fn unschedule_removes_the_matching_events() {
    let test = database().await;
    migrate(test.db()).await;
    let (hooks, _) = recording_hooks();
    let scheduler = Scheduler::new(test.shared(), hooks);
    let t = now() + TimeDelta::hours(1);
    scheduler.schedule(t, NOTE, "a".into()).await.unwrap();
    scheduler.schedule(t, NOTE, "b".into()).await.unwrap();

    assert_eq!(
        scheduler.unschedule(NOTE, &"a".to_owned()).await.unwrap(),
        1
    );
    let left = events(test.db()).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].payload, "\"b\"");
    assert_eq!(
        scheduler
            .next_scheduled(NOTE, &"b".to_owned())
            .await
            .unwrap(),
        Some(t)
    );
    assert_eq!(
        scheduler
            .next_scheduled(NOTE, &"a".to_owned())
            .await
            .unwrap(),
        None
    );
}

/// The done criterion: a post scheduled one second ahead is published by the scheduler's own
/// background loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publishes_a_future_post_at_its_date() {
    let test = database().await;
    migrate(test.db()).await;
    let author = author(test.db()).await;
    let scheduler = Arc::new(Scheduler::new(test.shared(), Arc::new(Hooks::new())));
    register_publish_future_post(&scheduler);

    let date = now() + TimeDelta::seconds(1);
    let post = Post::create(
        test.db(),
        NewPost {
            status: PostStatus::Future,
            title: "Tomorrow".into(),
            published_at: Some(date),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    schedule_future_post(&scheduler, &post).await.unwrap();
    assert_eq!(
        scheduler
            .next_scheduled(PUBLISH_FUTURE_POST, &post.id)
            .await
            .unwrap(),
        Some(date)
    );

    scheduler
        .run_due(date - TimeDelta::seconds(1))
        .await
        .unwrap();
    let read = Post::query()
        .find_or_fail(test.db(), post.id)
        .await
        .unwrap();
    assert_eq!(read.status, PostStatus::Future, "published early");

    let background = tokio::spawn({
        let scheduler = Arc::clone(&scheduler);
        async move { scheduler.run(Duration::from_millis(100)).await }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let read = Post::query()
            .find_or_fail(test.db(), post.id)
            .await
            .unwrap();
        if read.status == PostStatus::Publish {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the post was not published"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    background.abort();
    assert!(events(test.db()).await.is_empty());
}

#[tokio::test]
async fn publishes_only_a_post_that_is_still_future() {
    let test = database().await;
    migrate(test.db()).await;
    let author = author(test.db()).await;
    let scheduler = Arc::new(Scheduler::new(test.shared(), Arc::new(Hooks::new())));
    register_publish_future_post(&scheduler);

    let date = now() - TimeDelta::minutes(1);
    let mut post = Post::create(
        test.db(),
        NewPost {
            status: PostStatus::Future,
            published_at: Some(date),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    schedule_future_post(&scheduler, &post).await.unwrap();
    // Moved back to draft after it was scheduled.
    post.status = PostStatus::Draft;
    post.save(test.db()).await.unwrap();

    let tick = scheduler.run_due(now()).await.unwrap();
    assert_eq!(tick.completed, 1);
    let read = Post::query()
        .find_or_fail(test.db(), post.id)
        .await
        .unwrap();
    assert_eq!(read.status, PostStatus::Draft);
}

#[tokio::test]
async fn a_post_whose_date_moved_later_is_rescheduled_not_published() {
    let test = database().await;
    migrate(test.db()).await;
    let author = author(test.db()).await;
    let scheduler = Arc::new(Scheduler::new(test.shared(), Arc::new(Hooks::new())));
    register_publish_future_post(&scheduler);

    let date = now() + TimeDelta::hours(2);
    let post = Post::create(
        test.db(),
        NewPost {
            status: PostStatus::Future,
            published_at: Some(date),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    // An event that fires early, as when the date changed without rescheduling.
    scheduler
        .schedule(now() - TimeDelta::seconds(1), PUBLISH_FUTURE_POST, post.id)
        .await
        .unwrap();

    scheduler.run_due(now()).await.unwrap();
    let read = Post::query()
        .find_or_fail(test.db(), post.id)
        .await
        .unwrap();
    assert_eq!(read.status, PostStatus::Future);
    assert_eq!(
        scheduler
            .next_scheduled(PUBLISH_FUTURE_POST, &post.id)
            .await
            .unwrap(),
        Some(date),
        "WordPress reschedules it for the post's date"
    );
}

#[tokio::test]
async fn scheduling_a_post_again_replaces_its_event() {
    let test = database().await;
    migrate(test.db()).await;
    let author = author(test.db()).await;
    let scheduler = Arc::new(Scheduler::new(test.shared(), Arc::new(Hooks::new())));
    register_publish_future_post(&scheduler);

    let mut post = Post::create(
        test.db(),
        NewPost {
            status: PostStatus::Future,
            published_at: Some(now() + TimeDelta::hours(1)),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    schedule_future_post(&scheduler, &post).await.unwrap();
    let later = now() + TimeDelta::hours(5);
    post.published_at = Some(later);
    schedule_future_post(&scheduler, &post).await.unwrap();
    let stored = events(test.db()).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].run_at, later);

    // No longer future: the event goes.
    post.status = PostStatus::Draft;
    schedule_future_post(&scheduler, &post).await.unwrap();
    assert!(events(test.db()).await.is_empty());
}
