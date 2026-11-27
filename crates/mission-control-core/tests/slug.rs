//! Tests for slugs: `make_slug`, `unique_slug`, the `__trashed` suffix, the old slugs a slug
//! change records (`_wp_old_slug`), and the minute of grace for scheduled posts (Review Focus item
//! 5). They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is
//! unset.

use chrono::{Duration, SubsecRound, Utc};
use mission_control_core::content::{
    MAX_SLUG_LENGTH, NewPost, Post, PostStatus, find_by_old_slug, get_post_meta,
    get_post_meta_values, make_slug, save_revision, unique_slug, unique_slug_for,
};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};
use serde_json::json;

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

/// A post of `post_type` with `slug` and `status`, under `parent_id`.
async fn post(
    db: &Database,
    author: i64,
    post_type: &str,
    slug: &str,
    status: PostStatus,
    parent_id: Option<i64>,
) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: slug.to_owned(),
            slug: slug.to_owned(),
            parent_id,
            published_at: Some(Utc::now().naive_utc().trunc_subsecs(0)),
            ..NewPost::new(author, post_type)
        },
    )
    .await
    .expect("creates the post")
}

/// The `_wp_old_slug` values of post `id`, in the order they were added.
async fn old_slugs(db: &Database, id: i64) -> Vec<String> {
    get_post_meta_values(db, id, "_wp_old_slug")
        .await
        .unwrap()
        .into_iter()
        .map(|v| v.as_str().expect("a string").to_owned())
        .collect()
}

#[test]
fn make_slug_follows_sanitize_title() {
    let cases = [
        ("Hello World", "hello-world"),
        ("  Hello,   World!  ", "hello-world"),
        ("<b>Bold</b> move", "bold-move"),
        ("<script>x</script>", "x"),
        ("version 1.2", "version-1-2"),
        ("AC/DC", "ac-dc"),
        ("snake_case stays", "snake_case-stays"),
        ("--- dashes ---", "dashes"),
        ("tab\there\nnewline", "tab-here-newline"),
        ("Tom &amp; Jerry", "tom-jerry"),
        ("Fish&nbsp;chips", "fish-chips"),
        ("Rock \u{2013} Roll \u{2014} Live", "rock-roll-live"),
        ("It\u{2019}s \u{201C}quoted\u{201D}", "its-quoted"),
        ("\u{A9} 2026 Acme\u{2122}", "2026-acme"),
        ("5 \u{D7} 3", "5-x-3"),
        ("zero\u{200B}width", "zerowidth"),
        ("100% sure", "100-sure"),
        ("", ""),
        ("!!!", ""),
    ];
    for (title, slug) in cases {
        assert_eq!(make_slug(title), slug, "{title:?}");
    }
}

#[test]
fn make_slug_transliterates_non_ascii() {
    assert_eq!(make_slug("Ünïcode"), "unicode");
    assert_eq!(make_slug("Straße café"), "strasse-cafe");
    assert_eq!(make_slug("Привет мир"), "privet-mir");
    for title in ["Ünïcode 日本", "日本語", "こんにちは", "مرحبا", "😀"] {
        let slug = make_slug(title);
        assert!(!slug.is_empty(), "{title:?} gives an empty slug");
        assert!(
            slug.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "{title:?} gives {slug:?}"
        );
        assert!(!slug.contains('%'), "{slug:?}");
    }
    assert_eq!(make_slug("日本"), "ri-ben");
    assert_eq!(make_slug("Ünïcode 日本"), "unicode-ri-ben");
}

/// The examples WordPress's `sanitize_title` gives (`remove_accents`, then
/// `sanitize_title_with_dashes`).
#[test]
fn make_slug_gives_wordpresss_examples() {
    for (title, slug) in [
        ("Hello, World!", "hello-world"),
        ("Ünïcödé Tëst", "unicode-test"),
        ("  multiple   spaces  ", "multiple-spaces"),
        ("Crème Brûlée", "creme-brulee"),
        ("a -- b", "a-b"),
    ] {
        assert_eq!(make_slug(title), slug, "{title:?}");
    }
}

#[tokio::test]
async fn published_pages_get_a_number_and_drafts_keep_their_slug() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let page = |title: &str, status: PostStatus| NewPost {
        status,
        title: title.to_owned(),
        ..NewPost::new(author, "page")
    };
    let first = Post::create(db, page("About", PostStatus::Publish))
        .await
        .unwrap();
    let second = Post::create(db, page("About", PostStatus::Publish))
        .await
        .unwrap();
    assert_eq!(
        (first.slug.as_str(), second.slug.as_str()),
        ("about", "about-2")
    );
    // A draft given the taken slug keeps it until it is published.
    let mut draft = Post::create(
        db,
        NewPost {
            slug: "About".to_owned(),
            ..page("About", PostStatus::Draft)
        },
    )
    .await
    .unwrap();
    assert_eq!(draft.slug, "about");
    draft.status = PostStatus::Publish;
    draft.save(db).await.unwrap();
    assert_eq!(draft.slug, "about-3");
}

#[test]
fn make_slug_falls_back_to_code_points_when_nothing_transliterates() {
    // Private-use characters have no transliteration.
    assert_eq!(make_slug("\u{E000}"), "ue000");
    assert_eq!(make_slug("\u{E000} \u{E00A}"), "ue000-ue00a");
    // The fallback is only for titles that give nothing else.
    assert_eq!(make_slug("a \u{E000}"), "a");
}

#[test]
fn make_slug_fits_the_column() {
    let title = "word ".repeat(100);
    let slug = make_slug(&title);
    assert!(slug.len() <= MAX_SLUG_LENGTH, "{}", slug.len());
    assert!(!slug.ends_with('-'));
    assert!(slug.starts_with("word-word"));
}

#[tokio::test]
async fn a_taken_slug_gets_a_number() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let slug = make_slug("Hello World");
    assert_eq!(
        unique_slug(db, "post", None, &slug, None).await.unwrap(),
        "hello-world"
    );
    let first = post(db, author, "post", "hello-world", PostStatus::Publish, None).await;
    assert_eq!(
        unique_slug(db, "post", None, &slug, None).await.unwrap(),
        "hello-world-2"
    );
    post(db, author, "post", "hello-world-2", PostStatus::Draft, None).await;
    assert_eq!(
        unique_slug(db, "post", None, &slug, None).await.unwrap(),
        "hello-world-3"
    );
    // A post keeps its own slug.
    assert_eq!(
        unique_slug(db, "post", None, &slug, Some(first.id))
            .await
            .unwrap(),
        "hello-world"
    );
    // Another type, a trashed post, an auto-draft and a revision do not take the slug.
    for (post_type, status) in [
        ("page", PostStatus::Publish),
        ("post", PostStatus::Trash),
        ("post", PostStatus::AutoDraft),
        ("revision", PostStatus::Inherit),
    ] {
        post(db, author, post_type, "other", status, None).await;
    }
    assert_eq!(
        unique_slug(db, "post", None, "other", None).await.unwrap(),
        "other"
    );
    // An empty slug stays empty: there is nothing to make unique.
    assert_eq!(unique_slug(db, "post", None, "", None).await.unwrap(), "");
}

#[tokio::test]
async fn page_slugs_are_unique_among_siblings() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let about = post(db, author, "page", "about", PostStatus::Publish, None).await;
    let team = post(db, author, "page", "team", PostStatus::Publish, None).await;
    post(
        db,
        author,
        "page",
        "history",
        PostStatus::Publish,
        Some(about.id),
    )
    .await;

    // The same slug under another parent, or at the top, is free.
    assert_eq!(
        unique_slug(db, "page", Some(team.id), "history", None)
            .await
            .unwrap(),
        "history"
    );
    assert_eq!(
        unique_slug(db, "page", None, "history", None)
            .await
            .unwrap(),
        "history"
    );
    assert_eq!(
        unique_slug(db, "page", Some(about.id), "history", None)
            .await
            .unwrap(),
        "history-2"
    );
    assert_eq!(
        unique_slug(db, "page", None, "about", None).await.unwrap(),
        "about-2"
    );
    // An attachment under the same parent takes the slug, as in WordPress.
    post(
        db,
        author,
        "attachment",
        "logo",
        PostStatus::Inherit,
        Some(about.id),
    )
    .await;
    assert_eq!(
        unique_slug(db, "page", Some(about.id), "logo", None)
            .await
            .unwrap(),
        "logo-2"
    );
    // A custom type can be hierarchical too.
    post(db, author, "book", "intro", PostStatus::Publish, Some(1)).await;
    assert_eq!(
        unique_slug_for(db, "book", true, Some(2), "intro", None)
            .await
            .unwrap(),
        "intro"
    );
    assert_eq!(
        unique_slug_for(db, "book", false, Some(2), "intro", None)
            .await
            .unwrap(),
        "intro-2"
    );
}

#[tokio::test]
async fn attachment_slugs_are_unique_across_types() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    post(db, author, "page", "photo", PostStatus::Publish, None).await;
    assert_eq!(
        unique_slug(db, "attachment", None, "photo", None)
            .await
            .unwrap(),
        "photo-2"
    );
}

#[tokio::test]
async fn slugs_that_clash_with_feeds_and_pages_get_a_number() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    for feed in ["feed", "rdf", "rss", "rss2", "atom", "embed"] {
        assert_eq!(
            unique_slug(db, "post", None, feed, None).await.unwrap(),
            format!("{feed}-2")
        );
        assert_eq!(
            unique_slug(db, "page", None, feed, None).await.unwrap(),
            format!("{feed}-2")
        );
    }
    // A page slug that reads as a page number would clash with paging.
    assert_eq!(
        unique_slug(db, "page", None, "2", None).await.unwrap(),
        "2-2"
    );
    assert_eq!(
        unique_slug(db, "page", None, "page3", None).await.unwrap(),
        "page3-2"
    );
    assert_eq!(unique_slug(db, "post", None, "2", None).await.unwrap(), "2");
}

#[tokio::test]
async fn a_numbered_slug_fits_the_column() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let long = "a".repeat(MAX_SLUG_LENGTH);
    post(db, author, "post", &long, PostStatus::Publish, None).await;
    let slug = unique_slug(db, "post", None, &long, None).await.unwrap();
    assert_eq!(slug, format!("{}-2", "a".repeat(MAX_SLUG_LENGTH - 2)));
    // A base longer than the column is cut to it first.
    let longer = "b".repeat(MAX_SLUG_LENGTH + 20);
    let slug = unique_slug(db, "post", None, &longer, None).await.unwrap();
    assert_eq!(slug.len(), MAX_SLUG_LENGTH);
}

#[tokio::test]
async fn trash_frees_the_slug_and_untrash_takes_it_back() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut first = post(db, author, "post", "hello", PostStatus::Publish, None).await;

    first.trash(db).await.unwrap();
    assert_eq!(first.slug, "hello__trashed");
    let stored = Post::query().find_or_fail(db, first.id).await.unwrap();
    assert_eq!(stored.slug, "hello__trashed");
    assert_eq!(
        get_post_meta(db, first.id, "_wp_desired_post_slug")
            .await
            .unwrap(),
        Some(json!("hello"))
    );
    // The slug is free again.
    assert_eq!(
        unique_slug(db, "post", None, "hello", None).await.unwrap(),
        "hello"
    );
    // Trashing again does not add a second suffix.
    first.trash(db).await.unwrap();
    assert_eq!(first.slug, "hello__trashed");

    first.untrash(db).await.unwrap();
    assert_eq!(first.slug, "hello");
    assert_eq!(first.status, PostStatus::Draft);
    assert_eq!(
        Post::query().find_or_fail(db, first.id).await.unwrap().slug,
        "hello"
    );
    assert_eq!(
        get_post_meta(db, first.id, "_wp_desired_post_slug")
            .await
            .unwrap(),
        None
    );

    // When another post took the slug meanwhile, untrash makes it unique (`-2`), so two posts
    // never share a slug after a restore; publishing keeps it.
    first.trash(db).await.unwrap();
    post(db, author, "post", "hello", PostStatus::Publish, None).await;
    first.untrash(db).await.unwrap();
    assert_eq!(first.slug, "hello-2");
    first.status = PostStatus::Publish;
    first.save(db).await.unwrap();
    assert_eq!(first.slug, "hello-2");
}

#[tokio::test]
async fn trash_cuts_a_long_slug_to_fit_the_suffix() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let long = "a".repeat(MAX_SLUG_LENGTH);
    let mut p = post(db, author, "post", &long, PostStatus::Publish, None).await;
    p.trash(db).await.unwrap();
    assert_eq!(p.slug, format!("{}__trashed", "a".repeat(191)));
    p.untrash(db).await.unwrap();
    assert_eq!(p.slug, long);
}

#[tokio::test]
async fn a_slug_change_records_the_old_slug() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut p = post(db, author, "post", "a", PostStatus::Publish, None).await;

    p.slug = "b".into();
    p.save(db).await.unwrap();
    assert_eq!(old_slugs(db, p.id).await, ["a"]);
    assert_eq!(
        find_by_old_slug(db, "post", "a")
            .await
            .unwrap()
            .map(|f| f.id),
        Some(p.id)
    );

    p.slug = "c".into();
    p.save(db).await.unwrap();
    assert_eq!(old_slugs(db, p.id).await, ["a", "b"]);

    // Going back to an old slug removes it from the list; it is the slug again.
    p.slug = "a".into();
    p.save(db).await.unwrap();
    assert_eq!(old_slugs(db, p.id).await, ["b", "c"]);
    assert!(find_by_old_slug(db, "post", "a").await.unwrap().is_none());

    // A save that keeps the slug adds nothing.
    p.title = "New title".into();
    p.save(db).await.unwrap();
    assert_eq!(old_slugs(db, p.id).await, ["b", "c"]);

    // The lookup is per type.
    assert!(find_by_old_slug(db, "page", "b").await.unwrap().is_none());
    assert_eq!(
        find_by_old_slug(db, "post", "b")
            .await
            .unwrap()
            .map(|f| f.id),
        Some(p.id)
    );
}

#[tokio::test]
async fn save_revision_records_the_old_slug_too() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut p = post(db, author, "post", "first", PostStatus::Publish, None).await;
    let opened = p.modified_at;
    p.slug = "second".into();
    save_revision(db, &p, opened).await.unwrap();
    assert_eq!(old_slugs(db, p.id).await, ["first"]);
    assert_eq!(
        Post::query().find_or_fail(db, p.id).await.unwrap().slug,
        "second"
    );
}

#[tokio::test]
async fn only_published_flat_posts_record_old_slugs() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    // A draft's slug is not public yet.
    let mut draft = post(db, author, "post", "d1", PostStatus::Draft, None).await;
    draft.slug = "d2".into();
    draft.save(db).await.unwrap();
    assert!(old_slugs(db, draft.id).await.is_empty());
    // Pages are hierarchical: WordPress does not redirect their old slugs.
    let mut page = post(db, author, "page", "p1", PostStatus::Publish, None).await;
    page.slug = "p2".into();
    page.save(db).await.unwrap();
    assert!(old_slugs(db, page.id).await.is_empty());
    // An attachment counts when it is `inherit`, as WordPress has it.
    let mut file = post(db, author, "attachment", "f1", PostStatus::Inherit, None).await;
    file.slug = "f2".into();
    file.save(db).await.unwrap();
    assert_eq!(old_slugs(db, file.id).await, ["f1"]);
    // An empty slug is not recorded: a draft without one, published with a slug.
    let mut bare = post(db, author, "post", "", PostStatus::Draft, None).await;
    bare.status = PostStatus::Publish;
    bare.slug = "named".into();
    bare.save(db).await.unwrap();
    assert!(old_slugs(db, bare.id).await.is_empty());
}

/// A post created with `title`, no slug and `status`.
async fn titled(db: &Database, author: i64, title: &str, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: title.into(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("creates the post")
}

#[tokio::test]
async fn publishing_gives_a_post_a_unique_slug_from_its_title() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let first = titled(db, author, "Hello World", PostStatus::Publish).await;
    assert_eq!(first.slug, "hello-world");
    assert_eq!(
        Post::query().find_or_fail(db, first.id).await.unwrap().slug,
        "hello-world"
    );

    // A draft keeps an empty slug, even with a taken title.
    let mut draft = titled(db, author, "Hello World", PostStatus::Draft).await;
    assert_eq!(draft.slug, "");
    draft.title = "Hello World".into();
    draft.save(db).await.unwrap();
    assert_eq!(draft.slug, "");

    // Publishing it makes the slug from the title, unique.
    draft.status = PostStatus::Publish;
    draft.save(db).await.unwrap();
    assert_eq!(draft.slug, "hello-world-2");
    assert_eq!(
        Post::query().find_or_fail(db, draft.id).await.unwrap().slug,
        "hello-world-2"
    );
    // A later save keeps it: the post's own slug does not count against it.
    draft.save(db).await.unwrap();
    assert_eq!(draft.slug, "hello-world-2");

    // A published post created with a taken slug gets a number at once.
    let third = post(db, author, "post", "hello-world", PostStatus::Publish, None).await;
    assert_eq!(third.slug, "hello-world-3");
    // A draft keeps a taken slug as given; pending and auto-drafts too.
    for status in [
        PostStatus::Draft,
        PostStatus::Pending,
        PostStatus::AutoDraft,
    ] {
        let kept = post(db, author, "post", "hello-world", status, None).await;
        assert_eq!(kept.slug, "hello-world", "{status}");
    }
}

#[tokio::test]
async fn a_given_slug_is_sanitized_and_an_untitled_post_takes_its_id() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let given = post(db, author, "post", "Hello World!", PostStatus::Draft, None).await;
    assert_eq!(given.slug, "hello-world");

    // A published post whose title gives no slug takes its id, as `wp_insert_post` does.
    let untitled = titled(db, author, "!!!", PostStatus::Publish).await;
    assert_eq!(untitled.slug, untitled.id.to_string());
    assert_eq!(
        Post::query()
            .find_or_fail(db, untitled.id)
            .await
            .unwrap()
            .slug,
        untitled.id.to_string()
    );
}

#[tokio::test]
async fn save_revision_settles_the_slug() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    titled(db, author, "Taken", PostStatus::Publish).await;
    let mut edited = titled(db, author, "Taken", PostStatus::Draft).await;
    let opened = edited.modified_at;
    edited.status = PostStatus::Publish;
    save_revision(db, &edited, opened).await.unwrap();
    assert_eq!(
        Post::query()
            .find_or_fail(db, edited.id)
            .await
            .unwrap()
            .slug,
        "taken-2"
    );
}

#[tokio::test]
async fn a_future_post_less_than_a_minute_ahead_is_published() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let now = Utc::now().naive_utc().trunc_subsecs(0);

    let mut soon = post(db, author, "post", "soon", PostStatus::Draft, None).await;
    soon.status = PostStatus::Future;
    soon.published_at = Some(now + Duration::seconds(30));
    soon.save(db).await.unwrap();
    assert_eq!(soon.status, PostStatus::Publish);

    let mut later = post(db, author, "post", "later", PostStatus::Draft, None).await;
    later.status = PostStatus::Future;
    later.published_at = Some(now + Duration::minutes(5));
    later.save(db).await.unwrap();
    assert_eq!(later.status, PostStatus::Future);

    // save_revision follows the same rule.
    let mut edited = post(db, author, "post", "edited", PostStatus::Draft, None).await;
    let opened = edited.modified_at;
    edited.status = PostStatus::Future;
    edited.published_at = Some(now + Duration::seconds(30));
    save_revision(db, &edited, opened).await.unwrap();
    assert_eq!(
        Post::query()
            .find_or_fail(db, edited.id)
            .await
            .unwrap()
            .status,
        PostStatus::Publish
    );
}

#[tokio::test]
async fn the_future_rule_skips_attachments() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut file = post(db, author, "attachment", "file", PostStatus::Inherit, None).await;
    file.status = PostStatus::Future;
    file.published_at = Some(Utc::now().naive_utc() - Duration::days(1));
    file.save(db).await.unwrap();
    assert_eq!(file.status, PostStatus::Future);
}
