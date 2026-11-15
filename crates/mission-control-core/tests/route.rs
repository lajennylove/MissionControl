//! Tests for request routing, the inverse of permalinks: WordPress's rewrite rules and `WP_Query`
//! read a URL back to a post, a page, an archive, a search, the front page, a redirect or a 404.
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use chrono::{FixedOffset, NaiveDate, NaiveDateTime};
use mission_control_core::content::{
    NewPost, NewTerm, PermalinkContext, PermalinkStructure, Post, PostStatus, Taxonomy, Term,
    page_ancestors, permalink, set_option,
};
use mission_control_core::front::{Archive, Query, Resolved, resolve};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use serde_json::json;

const BASE: &str = "https://example.com";

fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(h, min, s)
        .unwrap()
}

fn structure(s: &str) -> PermalinkStructure {
    s.parse().unwrap_or_else(|e| panic!("{s:?}: {e}"))
}

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

/// A post of `post_type` with `slug` and `status`, published at `date`, under `parent_id`.
async fn post_at(
    db: &Database,
    author: i64,
    post_type: &str,
    slug: &str,
    status: PostStatus,
    parent_id: Option<i64>,
    date: NaiveDateTime,
) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: slug.to_owned(),
            slug: slug.to_owned(),
            parent_id,
            published_at: Some(date),
            ..NewPost::new(author, post_type)
        },
    )
    .await
    .expect("creates the post")
}

/// A published post of `post_type`, dated 2026-03-07 08:05:09 UTC.
async fn published(
    db: &Database,
    author: i64,
    post_type: &str,
    slug: &str,
    parent_id: Option<i64>,
) -> Post {
    post_at(
        db,
        author,
        post_type,
        slug,
        PostStatus::Publish,
        parent_id,
        at(2026, 3, 7, 8, 5, 9),
    )
    .await
}

async fn term(db: &Database, taxonomy: &Taxonomy, name: &str, parent: Option<&Term>) -> Term {
    Term::create(
        db,
        taxonomy,
        NewTerm {
            parent_id: parent.map(|p| p.term_id),
            ..NewTerm::new(name)
        },
    )
    .await
    .expect("creates the term")
}

async fn option(db: &Database, name: &str, value: serde_json::Value) {
    set_option(db, name, &value).await.expect("sets the option");
}

/// Resolves a path with an optional query string, as `/path/?a=b`.
async fn get(db: &Database, structure: &PermalinkStructure, url: &str) -> Resolved {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    resolve(db, path, &Query::parse(query), structure)
        .await
        .unwrap_or_else(|e| panic!("{url}: {e}"))
}

/// The link 4.4's renderer gives `post`, without the site address.
async fn link(
    db: &Database,
    structure: &PermalinkStructure,
    post: &Post,
    ctx: &PermalinkContext,
) -> String {
    let ctx = PermalinkContext {
        ancestors: page_ancestors(db, post).await.unwrap(),
        ..ctx.clone()
    };
    let full = permalink(structure, post, &ctx);
    full.strip_prefix(BASE)
        .expect("starts with the base")
        .to_owned()
}

fn redirect(location: &str) -> Resolved {
    Resolved::Redirect {
        location: location.to_owned(),
        status: 301,
    }
}

fn single(post: &Post) -> Resolved {
    Resolved::Single(post.clone())
}

fn archive(archive: Archive, paged: u32) -> Resolved {
    Resolved::Archive { archive, paged }
}

#[test]
fn query_strings_parse_as_php_reads_them() {
    let q = Query::parse("s=hello+world&p=4&p=5&cat%5B%5D=x&empty&name=caf%C3%A9");
    assert_eq!(q.get("s"), Some("hello world"));
    assert_eq!(q.get("p"), Some("5"), "the last value wins");
    assert_eq!(q.get("empty"), Some(""));
    assert_eq!(q.get("name"), Some("café"));
    assert_eq!(q.get("missing"), None);
    assert!(Query::parse("").is_empty());
}

#[tokio::test]
async fn each_permalink_structure_resolves_back_to_its_post() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let news = term(db, &Taxonomy::CATEGORY, "News", None).await;
    let local = term(db, &Taxonomy::CATEGORY, "Local", Some(&news)).await;
    let post = published(db, author, "post", "hello-world", None).await;
    post.sync_terms(db, &Taxonomy::CATEGORY, &[local.term_taxonomy_id])
        .await
        .unwrap();
    let about = published(db, author, "page", "about", None).await;
    let team = published(db, author, "page", "team", Some(about.id)).await;
    let ctx = PermalinkContext {
        category: Some("news/local".into()),
        ..PermalinkContext::new(BASE)
    };
    let structures = [
        PermalinkStructure::plain(),
        PermalinkStructure::day_and_name(),
        PermalinkStructure::month_and_name(),
        PermalinkStructure::numeric(),
        PermalinkStructure::post_name(),
        structure("/%category%/%postname%/"),
        structure("/%year%%monthnum%%day%/%hour%-%minute%-%second%/%postname%.html"),
        structure("/%post_id%-%postname%/"),
        structure("/archives/%year%/%post_id%"),
    ];
    for s in &structures {
        let url = link(db, s, &post, &ctx).await;
        assert_eq!(get(db, s, &url).await, single(&post), "{s}: {url}");
        // Without the trailing slash too, as the rules end in `/?$`.
        if url.len() > 1 && url.ends_with('/') {
            let bare = url.trim_end_matches('/');
            assert_eq!(get(db, s, bare).await, single(&post), "{s}: {bare}");
        }
        let url = link(db, s, &team, &ctx).await;
        assert_eq!(
            get(db, s, &url).await,
            Resolved::Page(team.clone()),
            "{s}: {url}"
        );
    }
}

#[tokio::test]
async fn an_author_segment_is_matched_but_not_checked_until_users_have_nicenames() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut post = published(db, author, "post", "hello-world", None).await;
    let s = structure("/%author%/%postname%/");
    let ctx = PermalinkContext {
        author: Some("ann".into()),
        ..PermalinkContext::new(BASE)
    };
    let url = link(db, &s, &post, &ctx).await;
    assert_eq!(url, "/ann/hello-world/");
    assert_eq!(get(db, &s, &url).await, single(&post));
    // Any author segment matches until Task 8.1 checks it (R29).
    assert_eq!(get(db, &s, "/someone/hello-world/").await, single(&post));
    // No redirect to a link that cannot be rendered yet: `?p=` is served as it is.
    assert_eq!(
        get(db, &s, &format!("/?p={}", post.id)).await,
        single(&post)
    );
    post.slug = "renamed".into();
    post.save(db).await.unwrap();
    assert_eq!(get(db, &s, "/ann/hello-world/").await, Resolved::NotFound);
    // The author base still needs nicenames.
    assert_eq!(
        get(db, &PermalinkStructure::post_name(), "/author/ann/").await,
        Resolved::NotFound
    );
}

#[tokio::test]
async fn a_child_page_resolves_by_its_full_path() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let about = published(db, author, "page", "about", None).await;
    let people = published(db, author, "page", "people", Some(about.id)).await;
    let team = published(db, author, "page", "team", Some(people.id)).await;
    // A top-level page with the same slug as the grandchild.
    let other = published(db, author, "page", "team-2", None).await;
    for s in [
        PermalinkStructure::post_name(),
        PermalinkStructure::day_and_name(),
        PermalinkStructure::numeric(),
    ] {
        assert_eq!(
            get(db, &s, "/about/people/team/").await,
            Resolved::Page(team.clone()),
            "{s}"
        );
        assert_eq!(
            get(db, &s, "/about/people/").await,
            Resolved::Page(people.clone())
        );
        assert_eq!(get(db, &s, "/team-2").await, Resolved::Page(other.clone()));
        assert_eq!(get(db, &s, "/team/").await, Resolved::NotFound, "{s}");
        assert_eq!(get(db, &s, "/about/team/").await, Resolved::NotFound);
        assert_eq!(get(db, &s, "/people/team/").await, Resolved::NotFound);
    }
}

#[tokio::test]
async fn category_and_tag_archives_resolve_by_their_bases() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let news = term(db, &Taxonomy::CATEGORY, "News", None).await;
    let local = term(db, &Taxonomy::CATEGORY, "Local", Some(&news)).await;
    let rust = term(db, &Taxonomy::POST_TAG, "Rust", None).await;
    let quiet = term(db, &Taxonomy::POST_TAG, "Quiet", None).await;
    let a = published(db, author, "post", "a", None).await;
    let b = published(db, author, "post", "b", None).await;
    a.sync_terms(db, &Taxonomy::CATEGORY, &[local.term_taxonomy_id])
        .await
        .unwrap();
    b.sync_terms(db, &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    a.sync_terms(db, &Taxonomy::POST_TAG, &[rust.term_taxonomy_id])
        .await
        .unwrap();
    // Read again, with their counts.
    let reread = async |t: &Term| Term::find(db, t.term_taxonomy_id).await.unwrap().unwrap();
    let (news, local, rust) = (
        reread(&news).await,
        reread(&local).await,
        reread(&rust).await,
    );
    let s = PermalinkStructure::post_name();
    let category = |t: &Term, paged| archive(Archive::Category(t.clone()), paged);

    assert_eq!(get(db, &s, "/category/news/").await, category(&news, 1));
    assert_eq!(
        get(db, &s, "/category/news/local/").await,
        category(&local, 1)
    );
    assert_eq!(
        get(db, &s, "/category/news/local").await,
        category(&local, 1)
    );
    // The leaf alone is WordPress's canonical redirect to the full path.
    assert_eq!(
        get(db, &s, "/category/local/").await,
        redirect("/category/news/local/")
    );
    assert_eq!(get(db, &s, "/category/nope/").await, Resolved::NotFound);
    assert_eq!(
        get(db, &s, "/category/local/news/").await,
        redirect("/category/news/")
    );
    assert_eq!(
        get(db, &s, "/category/local/page/2/").await,
        redirect("/category/news/local/page/2/")
    );
    let cat = format!("/?cat={}", news.term_id);
    assert_eq!(get(db, &s, &cat).await, category(&news, 1));

    // Ten posts a page: page 2 of a category with two posts is empty, so not found.
    assert_eq!(
        get(db, &s, "/category/news/page/2/").await,
        Resolved::NotFound
    );
    option(db, "posts_per_page", json!(1)).await;
    // News counts its child's posts too, as `include_children`.
    assert_eq!(
        get(db, &s, "/category/news/page/2/").await,
        category(&news, 2)
    );
    assert_eq!(
        get(db, &s, "/category/news/page/3/").await,
        Resolved::NotFound
    );

    assert_eq!(
        get(db, &s, "/tag/rust/").await,
        archive(Archive::Tag(rust.clone()), 1)
    );
    // A tag with no posts is still its archive, as `handle_404` keeps it.
    assert_eq!(
        get(db, &s, "/tag/quiet/").await,
        archive(Archive::Tag(quiet.clone()), 1)
    );
    assert_eq!(get(db, &s, "/tag/nope/").await, Resolved::NotFound);
    assert_eq!(
        get(db, &s, "/?tag=rust").await,
        archive(Archive::Tag(rust.clone()), 1)
    );

    // The numeric structure puts the bases under its front, `/archives/`.
    let numeric = PermalinkStructure::numeric();
    assert_eq!(
        get(db, &numeric, "/archives/category/news/").await,
        category(&news, 1)
    );
    assert_eq!(
        get(db, &numeric, "/archives/tag/rust/").await,
        archive(Archive::Tag(rust.clone()), 1)
    );
    // Custom bases from the options, without the front.
    option(db, "category_base", json!("topics")).await;
    option(db, "tag_base", json!("labels")).await;
    assert_eq!(get(db, &numeric, "/topics/news/").await, category(&news, 1));
    assert_eq!(
        get(db, &numeric, "/labels/rust/").await,
        archive(Archive::Tag(rust), 1)
    );
    assert_eq!(
        get(db, &numeric, "/archives/category/news/").await,
        Resolved::NotFound
    );
}

#[tokio::test]
async fn date_archives_resolve_in_the_sites_time_zone() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let late = post_at(
        db,
        author,
        "post",
        "late",
        PostStatus::Publish,
        None,
        at(2026, 3, 31, 23, 30, 0),
    )
    .await;
    let s = PermalinkStructure::post_name();
    let date = |year, month, day| archive(Archive::Date { year, month, day }, 1);
    assert_eq!(get(db, &s, "/2026/").await, date(2026, None, None));
    assert_eq!(get(db, &s, "/2026/03/").await, date(2026, Some(3), None));
    assert_eq!(
        get(db, &s, "/2026/3/31/").await,
        date(2026, Some(3), Some(31))
    );
    assert_eq!(get(db, &s, "/?m=202603").await, date(2026, Some(3), None));
    assert_eq!(
        get(db, &s, "/?year=2026&monthnum=3&day=31").await,
        date(2026, Some(3), Some(31))
    );
    // No posts that year, or not a date: not found.
    assert_eq!(get(db, &s, "/2025/").await, Resolved::NotFound);
    assert_eq!(get(db, &s, "/2026/13/").await, Resolved::NotFound);
    assert_eq!(get(db, &s, "/2026/02/30/").await, Resolved::NotFound);
    assert_eq!(get(db, &s, "/2026/page/2/").await, Resolved::NotFound);
    // The numeric structure moves dates under `date/`, as `%post_id%` comes first.
    let numeric = PermalinkStructure::numeric();
    assert_eq!(
        get(db, &numeric, "/archives/date/2026/03/").await,
        date(2026, Some(3), None)
    );

    // Two hours east of UTC, the post was published on April 1st.
    option(db, "gmt_offset", json!(2)).await;
    assert_eq!(get(db, &s, "/2026/03/31/").await, Resolved::NotFound);
    assert_eq!(
        get(db, &s, "/2026/04/01/").await,
        date(2026, Some(4), Some(1))
    );
    let day = PermalinkStructure::day_and_name();
    let ctx = PermalinkContext {
        utc_offset: FixedOffset::east_opt(7200).unwrap(),
        ..PermalinkContext::new(BASE)
    };
    let url = link(db, &day, &late, &ctx).await;
    assert_eq!(url, "/2026/04/01/late/");
    assert_eq!(get(db, &day, &url).await, single(&late));
    assert_eq!(get(db, &day, "/2026/03/31/late/").await, Resolved::NotFound);
}

#[tokio::test]
async fn a_search_resolves_from_the_query_and_the_search_base() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    published(db, author, "post", "hello-world", None).await;
    let s = PermalinkStructure::post_name();
    let search = |terms: &str, paged| Resolved::Search {
        terms: terms.to_owned(),
        paged,
    };
    assert_eq!(get(db, &s, "/?s=hello").await, search("hello", 1));
    assert_eq!(
        get(db, &s, "/search/hello+world/").await,
        search("hello world", 1)
    );
    assert_eq!(
        get(db, &s, "/search/hello%20world").await,
        search("hello world", 1)
    );
    // An empty search is a search, as in WordPress.
    assert_eq!(get(db, &s, "/?s=").await, search("", 1));
    assert_eq!(
        get(db, &PermalinkStructure::plain(), "/?s=hello").await,
        search("hello", 1)
    );
    // The search wins over an archive.
    assert_eq!(get(db, &s, "/2026/?s=hello").await, search("hello", 1));
    // One match: page 2 is empty.
    assert_eq!(
        get(db, &s, "/search/hello/page/2/").await,
        Resolved::NotFound
    );
    option(db, "posts_per_page", json!(1)).await;
    published(db, author, "page", "hello-again", None).await;
    assert_eq!(
        get(db, &s, "/search/HELLO/page/2/").await,
        search("HELLO", 2)
    );
}

#[tokio::test]
async fn the_front_page_follows_the_reading_settings() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    published(db, author, "post", "hello-world", None).await;
    let home = published(db, author, "page", "home", None).await;
    let blog = published(db, author, "page", "blog", None).await;
    let s = PermalinkStructure::post_name();
    let plain = PermalinkStructure::plain();

    // Your latest posts.
    for structure in [&s, &plain] {
        assert_eq!(
            get(db, structure, "/").await,
            Resolved::FrontPage {
                page: None,
                paged: 1
            }
        );
        assert_eq!(get(db, structure, "").await, get(db, structure, "/").await);
    }
    assert_eq!(get(db, &s, "/page/2/").await, Resolved::NotFound);
    assert_eq!(get(db, &s, "/blog/").await, Resolved::Page(blog.clone()));

    // A static page.
    option(db, "show_on_front", json!("page")).await;
    option(db, "page_on_front", json!(home.id)).await;
    option(db, "page_for_posts", json!(blog.id.to_string())).await;
    let front = |paged| Resolved::FrontPage {
        page: Some(home.clone()),
        paged,
    };
    assert_eq!(get(db, &s, "/").await, front(1));
    assert_eq!(get(db, &plain, "/").await, front(1));
    assert_eq!(get(db, &s, "/page/2/").await, front(2));
    assert_eq!(
        get(db, &s, "/blog/").await,
        Resolved::Home {
            page: blog.clone(),
            paged: 1
        }
    );
    let posts_page = format!("/?page_id={}", blog.id);
    assert_eq!(
        get(db, &plain, &posts_page).await,
        Resolved::Home {
            page: blog.clone(),
            paged: 1
        }
    );
    assert_eq!(get(db, &s, "/blog/page/2/").await, Resolved::NotFound);
    // The front page at its own address goes to `/`.
    assert_eq!(get(db, &s, "/home/").await, redirect("/"));
    let front_id = format!("/?page_id={}", home.id);
    assert_eq!(get(db, &plain, &front_id).await, redirect("/"));
}

#[tokio::test]
async fn a_renamed_slug_redirects_to_the_new_link() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let mut post = published(db, author, "post", "old-name", None).await;
    post.slug = "new-name".into();
    post.save(db).await.unwrap();
    assert_eq!(post.slug, "new-name");

    let s = PermalinkStructure::post_name();
    assert_eq!(get(db, &s, "/old-name/").await, redirect("/new-name/"));
    assert_eq!(get(db, &s, "/new-name/").await, single(&post));
    let day = PermalinkStructure::day_and_name();
    assert_eq!(
        get(db, &day, "/2026/03/07/old-name/").await,
        redirect("/2026/03/07/new-name/")
    );
    // The date narrows the old slug, as `_find_post_by_old_slug` does.
    assert_eq!(
        get(db, &day, "/2025/01/01/old-name/").await,
        Resolved::NotFound
    );
    let plain = PermalinkStructure::plain();
    assert_eq!(
        get(db, &plain, "/?name=old-name").await,
        redirect(&format!("/?p={}", post.id))
    );
}

#[tokio::test]
async fn an_unknown_path_is_not_found() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    published(db, author, "post", "hello-world", None).await;
    for s in [
        PermalinkStructure::post_name(),
        PermalinkStructure::day_and_name(),
        PermalinkStructure::numeric(),
    ] {
        for url in [
            "/nothing-here/",
            "/a/b/c/d/",
            "/archives/999999",
            "/2026/03/07/nothing/",
            "/?p=999999",
            "/?page_id=999999",
            "/?attachment_id=999999",
        ] {
            assert_eq!(get(db, &s, url).await, Resolved::NotFound, "{s}: {url}");
        }
    }
    // Plain links have no rewrite rules: any path but the root is not found.
    assert_eq!(
        get(db, &PermalinkStructure::plain(), "/hello-world/").await,
        Resolved::NotFound
    );
}

#[tokio::test]
async fn unpublished_content_is_not_found() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let s = PermalinkStructure::post_name();
    let plain = PermalinkStructure::plain();
    for status in [PostStatus::Draft, PostStatus::Pending, PostStatus::Private] {
        let slug = format!("hidden-{status:?}").to_lowercase();
        let post = post_at(
            db,
            author,
            "post",
            &slug,
            status,
            None,
            at(2026, 3, 7, 8, 5, 9),
        )
        .await;
        let page = post_at(
            db,
            author,
            "page",
            &format!("{slug}-page"),
            status,
            None,
            at(2026, 3, 7, 8, 5, 9),
        )
        .await;
        assert_eq!(get(db, &s, &format!("/{slug}/")).await, Resolved::NotFound);
        assert_eq!(
            get(db, &plain, &format!("/?p={}", post.id)).await,
            Resolved::NotFound
        );
        assert_eq!(
            get(db, &s, &format!("/{}/", page.slug)).await,
            Resolved::NotFound
        );
        assert_eq!(
            get(db, &plain, &format!("/?page_id={}", page.id)).await,
            Resolved::NotFound
        );
    }
    let future = post_at(
        db,
        author,
        "post",
        "later",
        PostStatus::Future,
        None,
        at(2099, 1, 1, 0, 0, 0),
    )
    .await;
    assert_eq!(future.status, PostStatus::Future);
    assert_eq!(get(db, &s, "/later/").await, Resolved::NotFound);
    let mut trashed = published(db, author, "post", "gone", None).await;
    let id = trashed.id;
    trashed.trash(db).await.unwrap();
    assert_eq!(get(db, &s, "/gone/").await, Resolved::NotFound);
    assert_eq!(get(db, &s, "/gone__trashed/").await, Resolved::NotFound);
    assert_eq!(
        get(db, &plain, &format!("/?p={id}")).await,
        Resolved::NotFound
    );
}

#[tokio::test]
async fn ids_in_the_query_redirect_to_the_canonical_link() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = published(db, author, "post", "hello-world", None).await;
    let about = published(db, author, "page", "about", None).await;
    let team = published(db, author, "page", "team", Some(about.id)).await;
    let s = PermalinkStructure::post_name();
    let plain = PermalinkStructure::plain();
    assert_eq!(
        get(db, &s, &format!("/?p={}", post.id)).await,
        redirect("/hello-world/")
    );
    assert_eq!(
        get(db, &s, &format!("/?page_id={}", team.id)).await,
        redirect("/about/team/")
    );
    assert_eq!(
        get(db, &plain, &format!("/?p={}", post.id)).await,
        single(&post)
    );
    assert_eq!(
        get(db, &plain, &format!("/?page_id={}", team.id)).await,
        Resolved::Page(team.clone())
    );
    // `?p=` names a page: not a post, so WordPress's 404 redirect finds the page.
    assert_eq!(
        get(db, &plain, &format!("/?p={}", team.id)).await,
        redirect(&format!("/?page_id={}", team.id))
    );
}

#[tokio::test]
async fn author_archives_resolve_by_id() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let s = PermalinkStructure::post_name();
    // An author with no posts is still an archive.
    assert_eq!(
        get(db, &s, &format!("/?author={author}")).await,
        archive(Archive::Author(author), 1)
    );
    assert_eq!(get(db, &s, "/?author=999999").await, Resolved::NotFound);
}

#[tokio::test]
async fn custom_types_and_attachments_resolve() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let book = published(db, author, "book", "dune", None).await;
    let file = post_at(
        db,
        author,
        "attachment",
        "photo",
        PostStatus::Inherit,
        None,
        at(2026, 3, 7, 8, 5, 9),
    )
    .await;
    let s = PermalinkStructure::post_name();
    let plain = PermalinkStructure::plain();
    let numeric = PermalinkStructure::numeric();
    let ctx = PermalinkContext::new(BASE);
    for structure in [&s, &plain, &numeric] {
        let url = link(db, structure, &book, &ctx).await;
        assert_eq!(get(db, structure, &url).await, single(&book), "{url}");
        let url = link(db, structure, &file, &ctx).await;
        assert_eq!(
            get(db, structure, &url).await,
            Resolved::Attachment(file.clone()),
            "{url}"
        );
    }
    assert_eq!(get(db, &s, "/book/nope/").await, Resolved::NotFound);
    assert_eq!(
        get(db, &s, "/?post_type=book").await,
        archive(Archive::PostType("book".into()), 1)
    );
    assert_eq!(get(db, &s, "/?post_type=nothing").await, Resolved::NotFound);
}

#[tokio::test]
async fn a_post_outside_the_category_in_its_link_redirects() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let news = term(db, &Taxonomy::CATEGORY, "News", None).await;
    let local = term(db, &Taxonomy::CATEGORY, "Local", Some(&news)).await;
    let post = published(db, author, "post", "hello-world", None).await;
    post.sync_terms(db, &Taxonomy::CATEGORY, &[local.term_taxonomy_id])
        .await
        .unwrap();
    let s = structure("/%category%/%postname%/");
    assert_eq!(get(db, &s, "/news/local/hello-world/").await, single(&post));
    assert_eq!(
        get(db, &s, "/news/hello-world/").await,
        redirect("/news/local/hello-world/")
    );
    // A page's path does not start with a category.
    let page = published(db, author, "page", "about", None).await;
    assert_eq!(get(db, &s, "/about/").await, Resolved::Page(page));
}

#[tokio::test]
async fn non_public_types_never_resolve() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let template = published(db, author, "wp_template", "tpl", None).await;
    let item = published(db, author, "nav_menu_item", "menu-item", None).await;
    let pretty = PermalinkStructure::post_name();
    let numeric = PermalinkStructure::numeric();
    let plain = PermalinkStructure::plain();
    for (post, post_type) in [(&template, "wp_template"), (&item, "nav_menu_item")] {
        let (id, slug) = (post.id, post.slug.as_str());
        let urls = [
            format!("/{post_type}/{slug}/"),
            format!("/archives/{post_type}/{slug}"),
            format!("/?post_type={post_type}&name={slug}"),
            format!("/?p={id}"),
            format!("/?p={id}&post_type={post_type}"),
            format!("/?page_id={id}"),
            format!("/?attachment_id={id}"),
        ];
        for s in [&pretty, &numeric, &plain] {
            for url in &urls {
                assert_eq!(get(db, s, url).await, Resolved::NotFound, "{s}: {url}");
            }
            // WordPress drops a `post_type` that is not public, and such a type has no query
            // variable: the request is the home page.
            for url in [
                format!("/?post_type={post_type}"),
                format!("/?{post_type}={slug}"),
            ] {
                assert_eq!(get(db, s, &url).await, get(db, s, "/").await, "{s}: {url}");
            }
        }
    }
}

#[tokio::test]
async fn attachments_and_old_slugs_of_unpublished_posts_are_not_found() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let s = PermalinkStructure::post_name();
    for status in [PostStatus::Draft, PostStatus::Private] {
        let parent = post_at(
            db,
            author,
            "post",
            "parent",
            status,
            None,
            at(2026, 3, 7, 0, 0, 0),
        )
        .await;
        let file = post_at(
            db,
            author,
            "attachment",
            "photo",
            PostStatus::Inherit,
            Some(parent.id),
            at(2026, 3, 7, 0, 0, 0),
        )
        .await;
        assert_eq!(
            get(db, &s, &format!("/?attachment_id={}", file.id)).await,
            Resolved::NotFound,
            "{status:?}"
        );
    }
    let mut post = published(db, author, "post", "old-name", None).await;
    post.slug = "new-name".into();
    post.save(db).await.unwrap();
    assert_eq!(get(db, &s, "/old-name/").await, redirect("/new-name/"));
    post.status = PostStatus::Draft;
    post.save(db).await.unwrap();
    assert_eq!(get(db, &s, "/old-name/").await, Resolved::NotFound);
}

#[tokio::test]
async fn long_paths_and_many_query_keys_are_bounded() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let book = published(db, author, "book", "dune", None).await;
    let s = PermalinkStructure::post_name();
    let plain = PermalinkStructure::plain();
    // Past 1,024 bytes or 32 segments: not found, by path and by `pagename`.
    let long = format!("/{}/", "x".repeat(2000));
    let deep = format!("/{}", "a/".repeat(40));
    for url in [&long, &deep] {
        assert_eq!(get(db, &s, url).await, Resolved::NotFound);
        assert_eq!(get(db, &plain, url).await, Resolved::NotFound);
    }
    let pagename = format!("/?pagename={}", "a/".repeat(40));
    assert_eq!(get(db, &plain, &pagename).await, Resolved::NotFound);
    let category = format!("/category/{}", "c/".repeat(40));
    assert_eq!(get(db, &s, &category).await, Resolved::NotFound);
    // A long single segment under the limit is read without trouble.
    let near = format!("/{}/", "y".repeat(1000));
    assert_eq!(get(db, &s, &near).await, Resolved::NotFound);

    // Many keys: only the first eight distinct type-shaped keys are tried, in one query.
    let junk: Vec<String> = (0..200).map(|i| format!("k{i}=x")).collect();
    let first = format!("/?book=dune&{}", junk.join("&"));
    assert_eq!(get(db, &plain, &first).await, single(&book));
    let last = format!("/?{}&book=dune", junk.join("&"));
    assert_eq!(get(db, &plain, &last).await, get(db, &plain, "/").await);
    // A repeated key counts once, its last value winning.
    let repeated = format!("/?{}&book=dune", vec!["book=nope"; 50].join("&"));
    assert_eq!(get(db, &plain, &repeated).await, single(&book));
}

#[tokio::test]
async fn out_of_range_dates_and_pages_are_not_found() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    published(db, author, "post", "hello-world", None).await;
    let s = PermalinkStructure::post_name();
    for offset in [json!(0), json!(14), json!(-12)] {
        option(db, "gmt_offset", offset).await;
        for url in [
            "/?year=0",
            "/?year=-5",
            "/?year=10000",
            "/?year=2147483647",
            "/?year=9999",
            "/?year=1",
            "/?year=9999&monthnum=12&day=31",
            "/?m=99999999",
            "/?m=00010101",
            "/9999/12/31/",
            "/0000/",
        ] {
            assert_eq!(get(db, &s, url).await, Resolved::NotFound, "{url}");
        }
    }
    option(db, "posts_per_page", json!(9_000_000_000_000_000_000u64)).await;
    assert_eq!(get(db, &s, "/page/4294967295/").await, Resolved::NotFound);
    assert_eq!(get(db, &s, "/?paged=4294967295").await, Resolved::NotFound);
}

#[tokio::test]
async fn case_and_trailing_space_variants_do_not_match() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let template = published(db, author, "wp_template", "tpl", None).await;
    let book = published(db, author, "book", "dune", None).await;
    published(db, author, "page", "about", None).await;
    term(db, &Taxonomy::POST_TAG, "Rust", None).await;
    let mut post = published(db, author, "post", "old-name", None).await;
    post.slug = "hello-world".into();
    post.save(db).await.unwrap();
    let home = |s| async move { get(db, s, "/").await };
    for s in [
        &PermalinkStructure::post_name(),
        &PermalinkStructure::day_and_name(),
        &PermalinkStructure::plain(),
    ] {
        // The post type variable: dropped unless it is a public type's exact name, so these look
        // for a `post` named `tpl` or `dune`, and find none.
        for url in [
            "/?post_type=WP_Template&name=tpl",
            "/?post_type=wp_template%20&name=tpl",
            "/?post_type=%20wp_template&name=tpl",
            "/?post_type=Book&name=dune",
            "/?post_type=book%20&name=dune",
            "/?name=Hello-World",
            "/?name=hello-world%20",
            "/?name=Old-Name",
            "/?name=old-name%20",
            "/?book=Dune",
            "/?book=dune%20",
        ] {
            assert_eq!(get(db, s, url).await, Resolved::NotFound, "{s}: {url}");
        }
        assert_eq!(
            get(db, s, &format!("/?p={}&post_type=WP_TEMPLATE", template.id)).await,
            Resolved::NotFound
        );
        // Keys that are not a type's exact name are ignored: the home page.
        for url in ["/?Book=dune", "/?post_type=BOOK", "/?post_type=book%20"] {
            assert_eq!(get(db, s, url).await, home(s).await, "{s}: {url}");
        }
        assert_eq!(get(db, s, "/?book=dune").await, single(&book));
        assert_eq!(
            get(db, s, "/?post_type=book&name=dune").await,
            single(&book)
        );
    }
    let s = PermalinkStructure::post_name();
    for url in [
        "/Hello-World/",
        "/hello-world%20/",
        "/HELLO-WORLD",
        "/Old-Name/",
        "/old-name%20/",
        "/About/",
        "/about%20/",
        "/book/Dune/",
        "/book/dune%20/",
        "/Book/dune/",
        "/tag/RUST/",
        "/tag/rust%20/",
        "/WP_Template/tpl/",
    ] {
        assert_eq!(get(db, &s, url).await, Resolved::NotFound, "{url}");
    }
    let day = PermalinkStructure::day_and_name();
    for url in ["/2026/03/07/Hello-World/", "/2026/03/07/hello-world%20/"] {
        assert_eq!(get(db, &day, url).await, Resolved::NotFound, "{url}");
    }
    // The exact forms still resolve.
    assert_eq!(get(db, &s, "/hello-world/").await, single(&post));
    assert_eq!(get(db, &s, "/old-name/").await, redirect("/hello-world/"));
    assert_eq!(get(db, &s, "/book/dune/").await, single(&book));
}
