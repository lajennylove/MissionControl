//! Tests for permalink structures and the links they render, after WordPress's Settings >
//! Permalinks and `get_permalink`. The rendering is pure; one test reads a page's ancestors from
//! the database in `MC_TEST_DATABASE_URL`, or a fresh SQLite file when it is unset.

use chrono::{FixedOffset, NaiveDate, NaiveDateTime};
use mission_control_core::content::{
    NewPost, PermalinkContext, PermalinkError, PermalinkStructure, Post, PostStatus, Segment, Tag,
    page_ancestors, permalink,
};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, raw};

const BASE: &str = "https://example.com";

fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(h, min, s)
        .unwrap()
}

/// A published post 42, `hello-world`, published 2026-03-07 08:05:09 UTC.
fn sample(post_type: &str) -> Post {
    Post {
        id: 42,
        author_id: 1,
        post_type: post_type.to_owned(),
        status: PostStatus::Publish,
        title: "Hello world".into(),
        slug: "hello-world".into(),
        content: String::new(),
        excerpt: String::new(),
        parent_id: None,
        menu_order: 0,
        published_at: Some(at(2026, 3, 7, 8, 5, 9)),
        modified_at: at(2026, 3, 8, 0, 0, 0),
        password: None,
    }
}

fn context() -> PermalinkContext {
    PermalinkContext {
        category: Some("news/local".into()),
        author: Some("ann".into()),
        ..PermalinkContext::new(BASE)
    }
}

fn structure(s: &str) -> PermalinkStructure {
    s.parse().unwrap_or_else(|e| panic!("{s:?}: {e}"))
}

#[test]
fn each_structure_renders_a_post() {
    let post = sample("post");
    let cases = [
        (PermalinkStructure::plain(), "https://example.com/?p=42"),
        (
            PermalinkStructure::day_and_name(),
            "https://example.com/2026/03/07/hello-world/",
        ),
        (
            PermalinkStructure::month_and_name(),
            "https://example.com/2026/03/hello-world/",
        ),
        (
            PermalinkStructure::numeric(),
            "https://example.com/archives/42",
        ),
        (
            PermalinkStructure::post_name(),
            "https://example.com/hello-world/",
        ),
        (
            structure("/%category%/%postname%/"),
            "https://example.com/news/local/hello-world/",
        ),
        (
            structure("/%author%/%post_id%-%postname%"),
            "https://example.com/ann/42-hello-world",
        ),
        (
            structure("/%year%%monthnum%%day%/%hour%-%minute%-%second%/%postname%.html"),
            "https://example.com/20260307/08-05-09/hello-world.html",
        ),
    ];
    for (structure, expected) in cases {
        assert_eq!(
            permalink(&structure, &post, &context()),
            expected,
            "{structure}"
        );
    }
}

#[test]
fn each_structure_renders_a_page_by_its_path() {
    let page = Post {
        slug: "team".into(),
        parent_id: Some(7),
        ..sample("page")
    };
    let ctx = PermalinkContext {
        ancestors: vec!["about".into(), "people".into()],
        ..context()
    };
    let cases = [
        (
            PermalinkStructure::plain(),
            "https://example.com/?page_id=42",
        ),
        (
            PermalinkStructure::day_and_name(),
            "https://example.com/about/people/team/",
        ),
        (
            PermalinkStructure::month_and_name(),
            "https://example.com/about/people/team/",
        ),
        // No trailing slash in the structure: none on the page either.
        (
            PermalinkStructure::numeric(),
            "https://example.com/about/people/team",
        ),
        (
            PermalinkStructure::post_name(),
            "https://example.com/about/people/team/",
        ),
        (
            structure("/blog/%postname%/"),
            "https://example.com/about/people/team/",
        ),
    ];
    for (structure, expected) in cases {
        assert_eq!(permalink(&structure, &page, &ctx), expected, "{structure}");
    }
}

#[test]
fn custom_types_and_attachments() {
    let book = sample("book");
    let cases = [
        (
            PermalinkStructure::plain(),
            "https://example.com/?book=hello-world",
        ),
        (
            PermalinkStructure::post_name(),
            "https://example.com/book/hello-world/",
        ),
        // The structure's front (`/archives/`) comes first, as `with_front` does.
        (
            PermalinkStructure::numeric(),
            "https://example.com/archives/book/hello-world",
        ),
        (
            structure("/blog/%year%/%postname%/"),
            "https://example.com/blog/book/hello-world/",
        ),
    ];
    for (structure, expected) in cases {
        assert_eq!(
            permalink(&structure, &book, &context()),
            expected,
            "{structure}"
        );
    }
    let draft = Post {
        status: PostStatus::Draft,
        ..sample("book")
    };
    assert_eq!(
        permalink(&PermalinkStructure::post_name(), &draft, &context()),
        "https://example.com/?post_type=book&p=42"
    );
    let file = Post {
        status: PostStatus::Inherit,
        ..sample("attachment")
    };
    assert_eq!(
        permalink(&PermalinkStructure::post_name(), &file, &context()),
        "https://example.com/?attachment_id=42"
    );
}

#[test]
fn unpublished_posts_get_the_plain_link() {
    for status in [
        PostStatus::Draft,
        PostStatus::Pending,
        PostStatus::AutoDraft,
        PostStatus::Future,
        PostStatus::Trash,
    ] {
        let post = Post {
            status,
            ..sample("post")
        };
        assert_eq!(
            permalink(&PermalinkStructure::post_name(), &post, &context()),
            "https://example.com/?p=42",
            "{status}"
        );
        let page = Post {
            status,
            ..sample("page")
        };
        assert_eq!(
            permalink(&PermalinkStructure::post_name(), &page, &context()),
            "https://example.com/?page_id=42",
            "{status}"
        );
    }
    let private = Post {
        status: PostStatus::Private,
        ..sample("post")
    };
    assert_eq!(
        permalink(&PermalinkStructure::post_name(), &private, &context()),
        "https://example.com/hello-world/"
    );
}

#[test]
fn dates_are_in_the_site_time_zone() {
    // 2026-03-07 08:05 UTC is 2026-03-06 22:05 at UTC-10.
    let ctx = PermalinkContext {
        utc_offset: FixedOffset::west_opt(10 * 3600).unwrap(),
        ..context()
    };
    assert_eq!(
        permalink(&PermalinkStructure::day_and_name(), &sample("post"), &ctx),
        "https://example.com/2026/03/06/hello-world/"
    );
}

#[test]
fn a_missing_category_renders_empty_and_the_base_slash_is_trimmed() {
    let ctx = PermalinkContext::new("https://example.com/blog/");
    assert_eq!(
        permalink(&structure("/%category%/%postname%/"), &sample("post"), &ctx),
        "https://example.com/blog/hello-world/"
    );
    assert_eq!(
        permalink(&PermalinkStructure::plain(), &sample("post"), &ctx),
        "https://example.com/blog/?p=42"
    );
}

#[test]
fn structures_parse_and_print_back() {
    for (text, printed) in [
        ("", ""),
        (
            "/%year%/%monthnum%/%day%/%postname%/",
            "/%year%/%monthnum%/%day%/%postname%/",
        ),
        (
            "/%year%/%monthnum%/%postname%/",
            "/%year%/%monthnum%/%postname%/",
        ),
        ("/archives/%post_id%", "/archives/%post_id%"),
        ("/%postname%/", "/%postname%/"),
        // A leading slash is added and repeated slashes are folded, as the settings screen does.
        ("%postname%", "/%postname%"),
        ("//blog///%postname%/", "/blog/%postname%/"),
    ] {
        let parsed = structure(text);
        assert_eq!(parsed.to_string(), printed, "{text:?}");
        assert_eq!(structure(printed), parsed);
    }
    assert!(structure("").is_plain());
    assert_eq!(
        PermalinkStructure::day_and_name(),
        structure("/%year%/%monthnum%/%day%/%postname%/")
    );
    assert_eq!(
        PermalinkStructure::month_and_name(),
        structure("/%year%/%monthnum%/%postname%/")
    );
    assert_eq!(
        PermalinkStructure::numeric(),
        structure("/archives/%post_id%")
    );
    assert_eq!(PermalinkStructure::post_name(), structure("/%postname%/"));
    assert_eq!(
        structure("/archives/%post_id%").segments(),
        &[
            Segment::Literal("/archives/".into()),
            Segment::Tag(Tag::PostId)
        ]
    );
    assert_eq!(
        structure("/%category%/%author%/%hour%%minute%%second%-%postname%").tags(),
        vec![
            Tag::Category,
            Tag::Author,
            Tag::Hour,
            Tag::Minute,
            Tag::Second,
            Tag::Postname
        ]
    );
    for tag in Tag::ALL {
        assert_eq!(tag.token().parse::<Tag>().unwrap(), tag);
    }
}

#[test]
fn bad_structures_are_refused() {
    assert_eq!(
        "/%nope%/".parse::<PermalinkStructure>(),
        Err(PermalinkError::UnknownTag("%nope%".into()))
    );
    assert_eq!(
        "/blog/".parse::<PermalinkStructure>(),
        Err(PermalinkError::NoTag)
    );
    assert_eq!(
        "/%postname".parse::<PermalinkStructure>(),
        Err(PermalinkError::Unclosed)
    );
}

#[tokio::test]
async fn page_ancestors_reads_the_parents_root_first() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let about = page(db, author, "about", None).await;
    let people = page(db, author, "people", Some(about.id)).await;
    let team = page(db, author, "team", Some(people.id)).await;
    assert_eq!(
        page_ancestors(db, &team).await.unwrap(),
        ["about", "people"]
    );
    assert!(page_ancestors(db, &about).await.unwrap().is_empty());
    let ctx = PermalinkContext {
        ancestors: page_ancestors(db, &team).await.unwrap(),
        ..PermalinkContext::new(BASE)
    };
    assert_eq!(
        permalink(&PermalinkStructure::post_name(), &team, &ctx),
        "https://example.com/about/people/team/"
    );
}

async fn setup(db: &Database) -> i64 {
    create_users_table(db).await;
    migrator().unwrap().run(db).await.unwrap();
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .unwrap();
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .unwrap()
}

async fn page(db: &Database, author: i64, slug: &str, parent_id: Option<i64>) -> Post {
    Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            slug: slug.into(),
            parent_id,
            ..NewPost::new(author, "page")
        },
    )
    .await
    .unwrap()
}
