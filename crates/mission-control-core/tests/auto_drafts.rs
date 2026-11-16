//! Old auto-drafts (Ruling R125): WordPress's daily `wp_scheduled_auto_draft_delete` deletes
//! auto-drafts older than 7 days. They run on the database in `MC_TEST_DATABASE_URL`, or on a
//! fresh SQLite file when it is unset.

use std::sync::Arc;

use chrono::{SubsecRound, TimeDelta, Utc};
use mission_control_core::content::{
    DELETE_AUTO_DRAFTS, NewPost, Post, PostStatus, delete_old_auto_drafts,
    register_delete_auto_drafts, schedule_auto_draft_deletion,
};
use mission_control_core::hooks::Hooks;
use mission_control_core::migrations::migrator;
use mission_control_core::scheduler::Scheduler;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};

async fn post(db: &Database, author: i64, status: PostStatus, age_days: i64) -> i64 {
    let post = Post::create(
        db,
        NewPost {
            status,
            title: "Auto Draft".to_owned(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    let at = Utc::now().naive_utc().trunc_subsecs(0) - TimeDelta::days(age_days);
    raw::execute(
        db,
        "UPDATE posts SET modified_at = ? WHERE id = ?",
        &[at.into(), post.id.into()],
    )
    .await
    .unwrap();
    post.id
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_drafts_older_than_a_week_are_deleted_daily() {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator().unwrap().run(db).await.unwrap();
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .unwrap();
    let author: i64 = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .unwrap();

    let old = post(db, author, PostStatus::AutoDraft, 8).await;
    let recent = post(db, author, PostStatus::AutoDraft, 6).await;
    let old_draft = post(db, author, PostStatus::Draft, 30).await;

    assert_eq!(delete_old_auto_drafts(db).await.unwrap(), 1);
    let exists = async |id| Post::query().find(db, id).await.unwrap().is_some();
    assert!(!exists(old).await);
    assert!(exists(recent).await);
    assert!(exists(old_draft).await, "only auto-drafts go");

    // The event: daily, first a day from now, one event however many starts.
    let scheduler = Arc::new(Scheduler::new(test.shared(), Arc::new(Hooks::new())));
    register_delete_auto_drafts(&scheduler);
    schedule_auto_draft_deletion(&scheduler).await.unwrap();
    schedule_auto_draft_deletion(&scheduler).await.unwrap();
    let events: Vec<(i64,)> = raw::fetch_all(
        db,
        "SELECT interval_seconds FROM scheduled_events WHERE hook = ?",
        &[DELETE_AUTO_DRAFTS.name().into()],
    )
    .await
    .unwrap();
    assert_eq!(events, [(86_400,)]);
    let next = scheduler
        .next_scheduled(DELETE_AUTO_DRAFTS, &())
        .await
        .unwrap()
        .unwrap();
    assert!(
        next > Utc::now().naive_utc() + TimeDelta::hours(23),
        "{next}"
    );
    // Run as the scheduler runs it, on a blocking thread, a day later.
    let tick = scheduler
        .run_due(next + TimeDelta::seconds(1))
        .await
        .unwrap();
    assert_eq!(tick.completed, 1, "{tick:?}");
}
