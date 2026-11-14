//! Tests for revisions and autosaves, and for the conflict check that keeps two editors from
//! silently overwriting each other (Review Focus item 4). They run on the database in
//! `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use chrono::{Duration, NaiveDateTime, SubsecRound, Utc};
use mission_control_core::content::{
    NewPost, Post, PostStatus, SaveError, create_post_autosave, get_post_autosave,
    get_post_revisions, save_revision,
};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};

/// Runs the core migrations and returns the id of a new user.
async fn setup(db: &Database) -> i64 {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    add_user(db, "ann").await
}

/// Inserts a user and returns its id.
async fn add_user(db: &Database, name: &str) -> i64 {
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &[name.into(), unique_email(name).into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the user id")
}

/// A published post by `author`.
async fn published_post(db: &Database, author: i64) -> Post {
    Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Hello world".into(),
            slug: "hello-world".into(),
            content: "<p>Welcome.</p>".into(),
            excerpt: "Welcome".into(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("creates the post")
}

/// Sets the stored `modified_at` of post `id`.
async fn set_modified_at(db: &Database, id: i64, at: NaiveDateTime) {
    raw::execute(
        db,
        "UPDATE posts SET modified_at = ? WHERE id = ?",
        &[at.into(), id.into()],
    )
    .await
    .unwrap();
}

/// How many rows of `posts` have `post_type` and `parent_id`.
async fn count_children(db: &Database, post_type: &str, parent: i64) -> i64 {
    raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM posts WHERE post_type = ? AND parent_id = ?",
        &[post_type.into(), parent.into()],
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_save_updates_the_post_and_records_a_revision() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published_post(db, author).await;
    let start = post.modified_at;

    let mut edited = post.clone();
    edited.title = "Hello, world".into();
    edited.content = "<p>Welcome back.</p>".into();
    edited.excerpt = "Welcome back".into();
    let revision = save_revision(db, &edited, start).await.expect("saves");

    // The post row holds the edit, with a newer modification time.
    let stored = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(stored.title, "Hello, world");
    assert_eq!(stored.content, "<p>Welcome back.</p>");
    assert_eq!(stored.status, PostStatus::Publish);
    assert!(stored.modified_at > start);
    assert_eq!(stored.modified_at.and_utc().timestamp_subsec_nanos(), 0);

    // The revision is a `posts` row, as WordPress's `_wp_put_post_revision` writes it.
    assert!(revision.id > post.id);
    assert_eq!(revision.post_type, "revision");
    assert_eq!(revision.status, PostStatus::Inherit);
    assert_eq!(revision.parent_id, Some(post.id));
    assert_eq!(revision.slug, format!("{}-revision-v1", post.id));
    assert_eq!(revision.title, "Hello, world");
    assert_eq!(revision.content, "<p>Welcome back.</p>");
    assert_eq!(revision.excerpt, "Welcome back");
    assert_eq!(revision.author_id, author);
    assert!(!revision.is_autosave());
    // Its modification time is the post's new one: the next save's expected value.
    assert_eq!(revision.modified_at, stored.modified_at);
    let read = Post::query().find_or_fail(db, revision.id).await.unwrap();
    assert_eq!(&read, &*revision);
}

#[tokio::test]
async fn two_saves_from_the_same_start_give_one_success_and_one_conflict() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published_post(db, author).await;
    // Both editors opened the post at this time. The two saves run within the same second, so
    // without a strictly newer stamp the second save would find the stamp it expects.
    let start = post.modified_at;

    let mut first = post.clone();
    first.title = "First editor".into();
    let mut second = post.clone();
    second.title = "Second editor".into();

    let saved = save_revision(db, &first, start).await.expect("first save");
    let err = save_revision(db, &second, start)
        .await
        .expect_err("the second save conflicts");
    match err {
        SaveError::Conflict {
            current_modified_at,
        } => assert_eq!(current_modified_at, saved.modified_at),
        other => panic!("expected a conflict, got {other:?}"),
    }

    // The first save is kept; the second wrote nothing.
    let stored = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(stored.title, "First editor");
    assert_eq!(stored.modified_at, saved.modified_at);
    assert_eq!(count_children(db, "revision", post.id).await, 1);

    // Saving again from the reported time succeeds.
    let again = save_revision(db, &second, saved.modified_at)
        .await
        .expect("saves from the current time");
    assert!(again.modified_at > saved.modified_at);
    assert_eq!(count_children(db, "revision", post.id).await, 2);
}

#[tokio::test]
async fn two_concurrent_saves_from_the_same_start_give_one_success_and_one_conflict() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;

    for round in 0..5 {
        let post = published_post(db, author).await;
        let start = post.modified_at;
        let mut first = post.clone();
        first.title = format!("First editor {round}");
        let mut second = post.clone();
        second.title = format!("Second editor {round}");

        let (a, b) = tokio::join!(
            save_revision(db, &first, start),
            save_revision(db, &second, start)
        );
        let (won, lost) = match (a, b) {
            (Ok(revision), Err(err)) => (revision, err),
            (Err(err), Ok(revision)) => (revision, err),
            (a, b) => panic!("expected one success and one error, got {a:?} and {b:?}"),
        };
        assert!(
            matches!(lost, SaveError::Conflict { current_modified_at } if current_modified_at == won.modified_at),
            "expected a conflict, got {lost:?}"
        );
        let stored = Post::query().find_or_fail(db, post.id).await.unwrap();
        assert_eq!(stored.title, won.title);
        assert_eq!(count_children(db, "revision", post.id).await, 1);
    }
}

#[tokio::test]
async fn a_save_stamps_a_time_strictly_after_the_expected_one() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published_post(db, author).await;
    // A stored time ahead of the clock, as several saves in one second leave it.
    let ahead = Utc::now().naive_utc().trunc_subsecs(0) + Duration::hours(1);
    set_modified_at(db, post.id, ahead).await;

    let revision = save_revision(db, &post, ahead).await.expect("saves");
    assert_eq!(revision.modified_at, ahead + Duration::seconds(1));
    let stored = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(stored.modified_at, ahead + Duration::seconds(1));
}

#[tokio::test]
async fn a_save_from_a_stale_time_conflicts_and_reports_the_current_one() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published_post(db, author).await;
    let stale = post.modified_at - Duration::minutes(5);

    let mut edited = post.clone();
    edited.title = "Stale".into();
    let err = save_revision(db, &edited, stale).await.unwrap_err();
    assert!(
        matches!(err, SaveError::Conflict { current_modified_at } if current_modified_at == post.modified_at),
        "expected a conflict, got {err:?}"
    );
    let stored = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(stored, post);
    assert_eq!(count_children(db, "revision", post.id).await, 0);
}

#[tokio::test]
async fn saving_a_missing_post_is_not_found() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published_post(db, author).await;
    let mut gone = post.clone();
    gone.id = post.id + 1000;

    let err = save_revision(db, &gone, post.modified_at)
        .await
        .unwrap_err();
    assert!(
        matches!(err, SaveError::NotFound { id } if id == gone.id),
        "expected not found, got {err:?}"
    );
    assert_eq!(count_children(db, "revision", gone.id).await, 0);
}

#[tokio::test]
async fn revisions_are_listed_newest_first_and_not_with_posts() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published_post(db, author).await;

    let mut edited = post.clone();
    let mut expected = post.modified_at;
    let mut ids = Vec::new();
    for n in 1..=3 {
        edited.title = format!("Edit {n}");
        let revision = save_revision(db, &edited, expected).await.unwrap();
        expected = revision.modified_at;
        ids.push(revision.id);
    }
    ids.reverse();

    let revisions = get_post_revisions(db, post.id).await.unwrap();
    let listed: Vec<i64> = revisions.iter().map(|r| r.id).collect();
    assert_eq!(listed, ids);
    assert_eq!(revisions[0].title, "Edit 3");

    let posts = Post::of_type("post").get(db).await.unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].id, post.id);
    assert_eq!(posts[0].title, "Edit 3");

    // Another post's revisions are not listed.
    let other = published_post(db, author).await;
    assert!(get_post_revisions(db, other.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_autosave_is_a_revision_that_leaves_the_post_alone() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = Post::create(
        db,
        NewPost {
            title: "Draft".into(),
            content: "Old".into(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    // An old time, so a change to the post row would show.
    let old = post.modified_at - Duration::days(1);
    set_modified_at(db, post.id, old).await;
    let before = Post::query().find_or_fail(db, post.id).await.unwrap();

    let mut edited = before.clone();
    edited.title = "Draft, edited".into();
    edited.content = "New".into();
    edited.status = PostStatus::Publish;
    let autosave = create_post_autosave(db, &edited, author)
        .await
        .expect("autosaves");

    assert_eq!(autosave.post_type, "revision");
    assert_eq!(autosave.status, PostStatus::Inherit);
    assert_eq!(autosave.parent_id, Some(post.id));
    assert_eq!(autosave.slug, format!("{}-autosave-v1", post.id));
    assert_eq!(autosave.author_id, author);
    assert_eq!(autosave.title, "Draft, edited");
    assert_eq!(autosave.content, "New");
    assert!(autosave.is_autosave());

    // The post row is untouched: same status, same content, same modification time.
    let after = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(after, before);
    assert_eq!(after.status, PostStatus::Draft);

    // A save that started before the autosave still succeeds: an autosave is not a save.
    save_revision(db, &before, before.modified_at)
        .await
        .expect("an autosave does not cause a conflict");
}

#[tokio::test]
async fn repeated_autosaves_keep_one_row_per_post_and_user() {
    let test = database().await;
    let db = test.db();
    let ann = setup(db).await;
    let bob = add_user(db, "bob").await;
    let post = published_post(db, ann).await;

    let mut edited = post.clone();
    let mut first_id = None;
    for n in 1..=3 {
        edited.content = format!("Ann's draft {n}");
        let autosave = create_post_autosave(db, &edited, ann).await.unwrap();
        // Replaced in place, as `wp_create_post_autosave` updates the user's old autosave.
        assert_eq!(*first_id.get_or_insert(autosave.id), autosave.id);
    }
    edited.content = "Bob's draft".into();
    let bobs = create_post_autosave(db, &edited, bob).await.unwrap();
    assert_ne!(Some(bobs.id), first_id);

    // One autosave row per (post, user).
    assert_eq!(count_children(db, "revision", post.id).await, 2);
    let anns = get_post_autosave(db, post.id, ann).await.unwrap().unwrap();
    assert_eq!(anns.content, "Ann's draft 3");
    assert_eq!(Some(anns.id), first_id);
    let read = get_post_autosave(db, post.id, bob).await.unwrap().unwrap();
    assert_eq!(read.content, "Bob's draft");
    assert!(
        get_post_autosave(db, post.id, ann + bob + 1000)
            .await
            .unwrap()
            .is_none()
    );

    // The post is unchanged and still the only `post` listed.
    let stored = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(stored, post);
    assert_eq!(Post::of_type("post").get(db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn autosaving_a_missing_post_is_not_found() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published_post(db, author).await;
    let mut gone = post.clone();
    gone.id = post.id + 1000;

    let err = create_post_autosave(db, &gone, author).await.unwrap_err();
    assert!(
        matches!(err, SaveError::NotFound { id } if id == gone.id),
        "expected not found, got {err:?}"
    );
}
