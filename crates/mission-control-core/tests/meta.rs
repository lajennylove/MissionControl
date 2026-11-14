//! Tests for the meta tables and options, and for the trash meta `Post::trash` keeps. They run
//! on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use mission_control_core::content::{
    MetaError, NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, delete_option, delete_post_meta,
    delete_term_meta, delete_user_meta, get_option, get_post_meta, get_term_meta, get_user_meta,
    set_option, set_option_raw, set_post_meta, set_post_meta_raw, set_term_meta, set_term_meta_raw,
    set_user_meta, set_user_meta_raw,
};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Model, Value as Sql, raw};
use serde_json::{Value, json};

/// Runs the core migrations and returns the id of a new user.
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
    .expect("inserts the user");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the user id")
}

async fn new_post(db: &Database, author: i64, post_type: &str, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: "A post".into(),
            ..NewPost::new(author, post_type)
        },
    )
    .await
    .expect("creates the post")
}

async fn count(db: &Database, table: &str) -> i64 {
    raw::fetch_scalar(db, &format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .unwrap()
}

fn sample() -> Value {
    json!({"rows": [{"title": "One", "n": 1}, {"title": "Two", "n": 2.5}], "on": true, "none": null})
}

#[tokio::test]
async fn post_meta_set_get_replace_delete() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = new_post(db, author, "post", PostStatus::Draft).await;

    assert_eq!(get_post_meta(db, post.id, "color").await.unwrap(), None);
    set_post_meta(db, post.id, "color", &json!("red"))
        .await
        .unwrap();
    assert_eq!(
        get_post_meta(db, post.id, "color").await.unwrap(),
        Some(json!("red"))
    );
    // Replaces the value, never adds a second row.
    set_post_meta(db, post.id, "color", &sample())
        .await
        .unwrap();
    assert_eq!(
        get_post_meta(db, post.id, "color").await.unwrap(),
        Some(sample())
    );
    assert_eq!(count(db, "post_meta").await, 1);
    // Setting the same value again is still one row (MySQL reports no changed rows).
    set_post_meta(db, post.id, "color", &sample())
        .await
        .unwrap();
    assert_eq!(count(db, "post_meta").await, 1);

    // Another key and another post are independent.
    let other = new_post(db, author, "page", PostStatus::Draft).await;
    set_post_meta(db, post.id, "size", &json!(3)).await.unwrap();
    set_post_meta(db, other.id, "color", &json!("blue"))
        .await
        .unwrap();
    assert_eq!(
        get_post_meta(db, other.id, "color").await.unwrap(),
        Some(json!("blue"))
    );

    assert!(delete_post_meta(db, post.id, "color").await.unwrap());
    assert!(!delete_post_meta(db, post.id, "color").await.unwrap());
    assert_eq!(get_post_meta(db, post.id, "color").await.unwrap(), None);
    assert_eq!(
        get_post_meta(db, post.id, "size").await.unwrap(),
        Some(json!(3))
    );
}

#[tokio::test]
async fn values_of_every_json_kind_round_trip() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = new_post(db, author, "post", PostStatus::Draft).await;

    for (i, value) in [
        json!(null),
        json!(true),
        json!(42),
        json!(-1.5),
        json!("héllo \"quoted\" \n 日本"),
        json!([1, "two", [3]]),
        sample(),
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("k{i}");
        set_post_meta(db, post.id, &key, &value).await.unwrap();
        assert_eq!(
            get_post_meta(db, post.id, &key).await.unwrap(),
            Some(value),
            "{key}"
        );
    }
}

#[tokio::test]
async fn user_meta_set_get_replace_delete() {
    let test = database().await;
    let db = test.db();
    let user = setup(db).await;

    set_user_meta(db, user, "locale", &json!("fr"))
        .await
        .unwrap();
    set_user_meta(db, user, "locale", &json!(["fr", "en"]))
        .await
        .unwrap();
    assert_eq!(
        get_user_meta(db, user, "locale").await.unwrap(),
        Some(json!(["fr", "en"]))
    );
    assert_eq!(count(db, "user_meta").await, 1);
    assert!(delete_user_meta(db, user, "locale").await.unwrap());
    assert_eq!(get_user_meta(db, user, "locale").await.unwrap(), None);
}

#[tokio::test]
async fn term_meta_set_get_replace_delete() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let term = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();

    set_term_meta(db, term.term_id, "icon", &json!("star"))
        .await
        .unwrap();
    set_term_meta(db, term.term_id, "icon", &json!({"name": "moon"}))
        .await
        .unwrap();
    assert_eq!(
        get_term_meta(db, term.term_id, "icon").await.unwrap(),
        Some(json!({"name": "moon"}))
    );
    assert_eq!(count(db, "term_meta").await, 1);
    assert!(delete_term_meta(db, term.term_id, "icon").await.unwrap());
    assert_eq!(get_term_meta(db, term.term_id, "icon").await.unwrap(), None);
}

#[tokio::test]
async fn options_set_get_replace_delete() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    assert_eq!(get_option(db, "blogname").await.unwrap(), None);
    set_option(db, "blogname", &json!("My Site")).await.unwrap();
    set_option(db, "blogname", &json!("Renamed")).await.unwrap();
    assert_eq!(
        get_option(db, "blogname").await.unwrap(),
        Some(json!("Renamed"))
    );
    assert_eq!(count(db, "options").await, 1);
    // The autoload column keeps its default, and a replace leaves it alone.
    raw::execute(db, "UPDATE options SET autoload = ?", &["off".into()])
        .await
        .unwrap();
    set_option(db, "blogname", &json!("Again")).await.unwrap();
    let autoload: String = raw::fetch_scalar(
        db,
        "SELECT autoload FROM options WHERE name = ?",
        &[Sql::from("blogname")],
    )
    .await
    .unwrap();
    assert_eq!(autoload, "off");

    assert!(delete_option(db, "blogname").await.unwrap());
    assert!(!delete_option(db, "blogname").await.unwrap());
    assert_eq!(get_option(db, "blogname").await.unwrap(), None);
}

#[tokio::test]
async fn raw_setters_refuse_text_that_is_not_json() {
    let test = database().await;
    let db = test.db();
    let user = setup(db).await;
    let author = user;
    let post = new_post(db, author, "post", PostStatus::Draft).await;
    let term = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();

    for bad in ["", "red", "{'a': 1}", "{\"a\": }", "[1, 2", "1 2", "nul"] {
        let e = set_post_meta_raw(db, post.id, "k", bad).await.unwrap_err();
        assert!(matches!(e, MetaError::InvalidJson(_)), "{bad:?}: {e}");
        assert!(e.to_string().contains("JSON"), "{e}");
        assert!(set_user_meta_raw(db, user, "k", bad).await.is_err());
        assert!(set_term_meta_raw(db, term.term_id, "k", bad).await.is_err());
        assert!(set_option_raw(db, "k", bad).await.is_err());
    }
    for table in ["post_meta", "user_meta", "term_meta", "options"] {
        assert_eq!(count(db, table).await, 0, "{table} stayed empty");
    }

    // A refused write leaves the value that was there.
    set_post_meta(db, post.id, "k", &json!(1)).await.unwrap();
    assert!(set_post_meta_raw(db, post.id, "k", "oops").await.is_err());
    assert_eq!(
        get_post_meta(db, post.id, "k").await.unwrap(),
        Some(json!(1))
    );

    // Valid text is stored and read back as JSON.
    set_post_meta_raw(db, post.id, "k", r#" {"a": [1, 2]} "#)
        .await
        .unwrap();
    assert_eq!(
        get_post_meta(db, post.id, "k").await.unwrap(),
        Some(json!({"a": [1, 2]}))
    );
    set_user_meta_raw(db, user, "k", "\"x\"").await.unwrap();
    set_term_meta_raw(db, term.term_id, "k", "2").await.unwrap();
    set_option_raw(db, "k", "[]").await.unwrap();
    assert_eq!(
        get_user_meta(db, user, "k").await.unwrap(),
        Some(json!("x"))
    );
    assert_eq!(
        get_term_meta(db, term.term_id, "k").await.unwrap(),
        Some(json!(2))
    );
    assert_eq!(get_option(db, "k").await.unwrap(), Some(json!([])));
}

#[tokio::test]
async fn a_stored_value_that_is_not_json_is_an_error_on_read() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = new_post(db, author, "post", PostStatus::Draft).await;
    raw::execute(
        db,
        "INSERT INTO post_meta (post_id, meta_key, meta_value) VALUES (?, ?, ?)",
        &[post.id.into(), "legacy".into(), "not json".into()],
    )
    .await
    .unwrap();
    let e = get_post_meta(db, post.id, "legacy").await.unwrap_err();
    assert!(matches!(e, MetaError::InvalidJson(_)));
}

#[tokio::test]
async fn deleting_the_owner_deletes_its_meta() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = new_post(db, author, "post", PostStatus::Draft).await;
    let term = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    set_post_meta(db, post.id, "k", &json!(1)).await.unwrap();
    set_term_meta(db, term.term_id, "k", &json!(1))
        .await
        .unwrap();

    raw::execute(db, "DELETE FROM posts WHERE id = ?", &[post.id.into()])
        .await
        .unwrap();
    raw::execute(db, "DELETE FROM terms WHERE id = ?", &[term.term_id.into()])
        .await
        .unwrap();
    assert_eq!(count(db, "post_meta").await, 0);
    assert_eq!(count(db, "term_meta").await, 0);

    // Meta for a post that does not exist is refused.
    assert!(
        set_post_meta(db, post.id, "k", &json!(1)).await.is_err(),
        "no such post"
    );
}

#[tokio::test]
async fn deleting_a_user_with_meta_but_no_posts_deletes_the_meta() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Bo".into(), unique_email("bo").into(), "hash".into()],
    )
    .await
    .unwrap();
    let bo: i64 = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .unwrap();
    set_user_meta(db, bo, "k", &json!(1)).await.unwrap();
    raw::execute(db, "DELETE FROM users WHERE id = ?", &[bo.into()])
        .await
        .unwrap();
    assert_eq!(count(db, "user_meta").await, 0);
}

#[tokio::test]
async fn trash_records_the_previous_status_and_time_and_untrash_restores_a_draft() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut post = new_post(db, author, "post", PostStatus::Publish).await;

    let before = chrono::Utc::now().timestamp();
    post.trash(db).await.unwrap();
    let after = chrono::Utc::now().timestamp();
    assert_eq!(post.status, PostStatus::Trash);
    assert_eq!(
        get_post_meta(db, post.id, "_wp_trash_meta_status")
            .await
            .unwrap(),
        Some(json!("publish"))
    );
    let time = get_post_meta(db, post.id, "_wp_trash_meta_time")
        .await
        .unwrap()
        .and_then(|v| v.as_i64())
        .expect("a unix time");
    assert!((before..=after).contains(&time), "{time}");

    // Trashing a trashed post changes nothing: the first status is kept.
    post.trash(db).await.unwrap();
    assert_eq!(
        get_post_meta(db, post.id, "_wp_trash_meta_status")
            .await
            .unwrap(),
        Some(json!("publish"))
    );
    assert_eq!(count(db, "post_meta").await, 2);

    post.untrash(db).await.unwrap();
    // WordPress 5.6+ restores to draft, whatever the post was before.
    assert_eq!(post.status, PostStatus::Draft);
    let read = Post::query().find_or_fail(db, post.id).await.unwrap();
    assert_eq!(read.status, PostStatus::Draft);
    assert_eq!(count(db, "post_meta").await, 0);

    // Untrashing a post that is not in the trash changes nothing.
    post.untrash(db).await.unwrap();
    assert_eq!(post.status, PostStatus::Draft);
}

#[tokio::test]
async fn untrash_returns_an_attachment_to_inherit_and_keeps_other_meta() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut file = new_post(db, author, "attachment", PostStatus::Inherit).await;
    set_post_meta(db, file.id, "_wp_attached_file", &json!("a.png"))
        .await
        .unwrap();

    file.trash(db).await.unwrap();
    assert_eq!(
        get_post_meta(db, file.id, "_wp_trash_meta_status")
            .await
            .unwrap(),
        Some(json!("inherit"))
    );
    file.untrash(db).await.unwrap();
    assert_eq!(file.status, PostStatus::Inherit);
    assert_eq!(
        get_post_meta(db, file.id, "_wp_attached_file")
            .await
            .unwrap(),
        Some(json!("a.png"))
    );
    assert_eq!(count(db, "post_meta").await, 1);
}

#[tokio::test]
async fn trash_and_untrash_keep_term_counts_in_step() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut post = new_post(db, author, "post", PostStatus::Publish).await;
    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    post.sync_terms(db, &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    let term_count = || async {
        raw::fetch_scalar::<i64>(
            db,
            "SELECT count FROM term_taxonomy WHERE id = ?",
            &[news.term_taxonomy_id.into()],
        )
        .await
        .unwrap()
    };
    assert_eq!(term_count().await, 1);
    post.trash(db).await.unwrap();
    assert_eq!(term_count().await, 0);
    post.untrash(db).await.unwrap();
    assert_eq!(post.status, PostStatus::Draft);
    assert_eq!(term_count().await, 0);
    post.status = PostStatus::Publish;
    post.save(db).await.unwrap();
    assert_eq!(term_count().await, 1);
}
