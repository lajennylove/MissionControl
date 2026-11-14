//! Tests for the `posts` table and the `Post` model. They run on the database in
//! `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use chrono::{NaiveDate, NaiveDateTime};
use mission_control_core::content::{NewPost, Post, PostStatus};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Direction, Error, Model, Value, raw};

/// Every status, with the string WordPress stores in `post_status`.
const STATUSES: [(PostStatus, &str); 8] = [
    (PostStatus::Draft, "draft"),
    (PostStatus::AutoDraft, "auto-draft"),
    (PostStatus::Pending, "pending"),
    (PostStatus::Private, "private"),
    (PostStatus::Publish, "publish"),
    (PostStatus::Future, "future"),
    (PostStatus::Trash, "trash"),
    (PostStatus::Inherit, "inherit"),
];

/// Runs the core migrations and returns the id of a new user to author posts.
async fn setup(db: &Database) -> i64 {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
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

fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(h, min, s)
        .unwrap()
}

#[test]
fn status_uses_wordpress_strings() {
    for (status, name) in STATUSES {
        assert_eq!(status.as_str(), name);
        assert_eq!(status.to_string(), name);
        assert_eq!(name.parse::<PostStatus>().unwrap(), status);
        assert_eq!(Value::from(status), Value::from(name));
    }
    assert!("published".parse::<PostStatus>().is_err());
    assert!("".parse::<PostStatus>().is_err());
}

#[test]
fn post_writes_every_column_but_its_id() {
    assert_eq!(Post::TABLE, "posts");
    assert_eq!(
        Post::FILLABLE,
        &[
            "author_id",
            "post_type",
            "status",
            "title",
            "slug",
            "content",
            "excerpt",
            "parent_id",
            "menu_order",
            "published_at",
            "modified_at",
            "password",
            "mime_type",
        ]
    );
}

#[tokio::test]
async fn creates_reads_updates_and_trashes_a_post() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;

    let published = at(2026, 11, 14, 9, 30, 0);
    let mut post = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Hello world".into(),
            slug: "hello-world".into(),
            content: "<p>Welcome.</p>".into(),
            excerpt: "Welcome".into(),
            published_at: Some(published),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("creates the post");
    assert!(post.id > 0);
    assert_eq!(post.modified_at.and_utc().timestamp_subsec_nanos(), 0);

    let read = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(read, post);
    assert_eq!(read.author_id, author);
    assert_eq!(read.post_type, "post");
    assert_eq!(read.status, PostStatus::Publish);
    assert_eq!(read.published_at, Some(published));
    assert_eq!(read.parent_id, None);
    assert_eq!(read.menu_order, 0);
    assert_eq!(read.password, None);
    // Only attachments have a MIME type (`post_mime_type`); other posts keep it empty.
    assert_eq!(read.mime_type, "");

    // An old modification time, so the update visibly moves it.
    let old = at(2001, 1, 1, 0, 0, 0);
    raw::execute(
        db,
        "UPDATE posts SET modified_at = ? WHERE id = ?",
        &[old.into(), post.id.into()],
    )
    .await
    .unwrap();
    post.title = "Hello, world".into();
    post.password = Some("secret".into());
    post.save(db).await.expect("updates the post");
    assert!(post.modified_at > old);
    let read = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(read, post);
    assert_eq!(read.title, "Hello, world");
    assert_eq!(read.password.as_deref(), Some("secret"));

    post.trash(db).await.expect("trashes the post");
    assert_eq!(post.status, PostStatus::Trash);
    let read = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(read.status, PostStatus::Trash);
    assert_eq!(read.title, "Hello, world");
}

#[tokio::test]
async fn every_status_round_trips_through_the_database() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;

    for (status, name) in STATUSES {
        let post = Post::create(
            db,
            NewPost {
                status,
                ..NewPost::new(author, "post")
            },
        )
        .await
        .unwrap();
        let stored: String = raw::fetch_scalar(
            db,
            "SELECT status FROM posts WHERE id = ?",
            &[post.id.into()],
        )
        .await
        .unwrap();
        assert_eq!(stored, name);
        let read = Post::query().find_or_fail(db, post.id).await.unwrap();
        assert_eq!(read.status, status);
    }
}

#[tokio::test]
async fn the_mime_type_round_trips_and_defaults_to_empty() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;

    let mut file = Post::create(
        db,
        NewPost {
            status: PostStatus::Inherit,
            title: "photo".into(),
            mime_type: "image/jpeg".into(),
            ..NewPost::new(author, "attachment")
        },
    )
    .await
    .unwrap();
    assert_eq!(file.mime_type, "image/jpeg");
    let stored: String = raw::fetch_scalar(
        db,
        "SELECT mime_type FROM posts WHERE id = ?",
        &[file.id.into()],
    )
    .await
    .unwrap();
    assert_eq!(stored, "image/jpeg");
    file.mime_type = "image/png".into();
    file.save(db).await.unwrap();
    let read = Post::query().find_or_fail(db, file.id).await.unwrap();
    assert_eq!(read.mime_type, "image/png");

    // A row inserted without the column gets the default, as `post_mime_type` does.
    raw::execute(
        db,
        "INSERT INTO posts (author_id, title, content, excerpt, modified_at) VALUES (?, ?, ?, ?, ?)",
        &[
            author.into(),
            "raw".into(),
            "".into(),
            "".into(),
            at(2026, 11, 14, 0, 0, 0).into(),
        ],
    )
    .await
    .unwrap();
    let id: i64 = raw::fetch_scalar(db, "SELECT MAX(id) FROM posts", &[])
        .await
        .unwrap();
    let read = Post::query().find_or_fail(db, id).await.unwrap();
    assert_eq!(read.mime_type, "");
}

#[tokio::test]
async fn an_unknown_status_in_the_table_is_a_decode_error() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = Post::create(db, NewPost::new(author, "post"))
        .await
        .unwrap();
    raw::execute(
        db,
        "UPDATE posts SET status = ? WHERE id = ?",
        &["published".into(), post.id.into()],
    )
    .await
    .unwrap();
    assert!(Post::query().find(db, post.id).await.is_err());
}

#[tokio::test]
async fn attachments_and_revisions_share_the_table_but_are_not_listed_with_posts() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;

    let post = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "With a photo".into(),
            slug: "with-a-photo".into(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    let attachment = Post::create(
        db,
        NewPost {
            status: PostStatus::Inherit,
            title: "photo.jpg".into(),
            slug: "photo".into(),
            parent_id: Some(post.id),
            ..NewPost::new(author, "attachment")
        },
    )
    .await
    .unwrap();
    let revision = Post::create(
        db,
        NewPost {
            status: PostStatus::Inherit,
            title: "With a photo".into(),
            slug: format!("{}-revision-v1", post.id),
            parent_id: Some(post.id),
            ..NewPost::new(author, "revision")
        },
    )
    .await
    .unwrap();
    let page = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "About".into(),
            slug: "about".into(),
            menu_order: 2,
            ..NewPost::new(author, "page")
        },
    )
    .await
    .unwrap();

    // One ID space for every type, as `wp_posts` has.
    let mut ids = vec![post.id, attachment.id, revision.id, page.id];
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 4);
    assert_eq!(Post::query().count(db).await.unwrap(), 4);

    let read = Post::query().find_or_fail(db, attachment.id).await.unwrap();
    assert_eq!(read.post_type, "attachment");
    assert_eq!(read.status, PostStatus::Inherit);
    assert_eq!(read.parent_id, Some(post.id));
    let read = Post::query().find_or_fail(db, revision.id).await.unwrap();
    assert_eq!(read.post_type, "revision");
    assert_eq!(read.parent_id, Some(post.id));

    let posts = Post::of_type("post").get(db).await.unwrap();
    assert_eq!(posts, vec![post]);
    let pages = Post::of_type("page")
        .order_by("menu_order", Direction::Asc)
        .get(db)
        .await
        .unwrap();
    assert_eq!(pages, vec![page]);
}

#[tokio::test]
async fn slugs_may_repeat_and_may_be_empty() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;

    for _ in 0..2 {
        Post::create(
            db,
            NewPost {
                slug: "same".into(),
                ..NewPost::new(author, "post")
            },
        )
        .await
        .expect("a repeated slug is stored; Task 4.4 makes slugs unique");
        Post::create(db, NewPost::new(author, "post"))
            .await
            .expect("a draft has no slug yet");
    }
    assert_eq!(Post::of_type("post").count(db).await.unwrap(), 4);
}

/// Whether `error` is a refused `ON DELETE RESTRICT`. SQLite reports it with the extended code
/// `SQLITE_CONSTRAINT_TRIGGER` (1811), which Orbit at c2ee11f leaves as `Error::Database`, not
/// `ForeignKeyViolation`; the message is SQLite's foreign key message.
fn restricted(error: &Error) -> bool {
    matches!(error, Error::ForeignKeyViolation { .. })
        || error.to_string().contains("FOREIGN KEY constraint failed")
}

#[tokio::test]
async fn an_author_with_posts_cannot_be_deleted() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    Post::create(db, NewPost::new(author, "post"))
        .await
        .unwrap();

    let deleted = raw::execute(db, "DELETE FROM users WHERE id = ?", &[author.into()]).await;
    assert!(
        deleted.as_ref().is_err_and(restricted),
        "deleting the author gave {deleted:?}"
    );

    let missing = Post::create(db, NewPost::new(author + 1_000_000, "post")).await;
    assert!(
        matches!(missing, Err(Error::ForeignKeyViolation { .. })),
        "a post by a missing author gave {missing:?}"
    );
}

#[tokio::test]
async fn content_holds_sixty_thousand_bytes() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let long = "x".repeat(60_000);
    let post = Post::create(
        db,
        NewPost {
            content: long.clone(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    let read = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(read.content.len(), long.len());
}
