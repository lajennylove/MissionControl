//! The default theme, rendered through the front-end request path: the home page, a post, a page,
//! an archive, a search and a 404, with the default theme's templates. Also the redirects and
//! visibility rules the router gives, and the theme confinement and escaping checks of Task 2.2,
//! run against a copy of the default theme.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mission_control::rocket_with_database;
use mission_control_core::config::Config;
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Shortcodes, Taxonomy, Term, set_option,
};
use mission_control_core::hooks::Owner;
use mission_control_core::migrations::migrator;
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use serde_json::json;

/// The themes shipped with Mission Control: `MissionControl/themes`.
fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

/// A test site: the database, with the core tables, options for a site named `Test Site` with
/// `/%postname%/` links, and a user to author posts.
struct Site {
    test: TestDatabase,
    author: i64,
}

impl Site {
    async fn new() -> Self {
        let test = database().await;
        let db = test.db();
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
        let author = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
            .await
            .expect("reads the author id");
        let site = Self { test, author };
        site.option("blogname", json!("Test Site")).await;
        site.option("blogdescription", json!("Just another test site"))
            .await;
        site.option("permalink_structure", json!("/%postname%/"))
            .await;
        site
    }

    fn db(&self) -> &Database {
        self.test.db()
    }

    async fn option(&self, name: &str, value: serde_json::Value) {
        set_option(self.db(), name, &value)
            .await
            .expect("sets the option");
    }

    /// A post of `post_type` with `title`, `slug` and `content`, in `status`.
    async fn post(
        &self,
        post_type: &str,
        status: PostStatus,
        title: &str,
        slug: &str,
        content: &str,
    ) -> Post {
        Post::create(
            self.db(),
            NewPost {
                status,
                title: title.to_owned(),
                slug: slug.to_owned(),
                content: content.to_owned(),
                published_at: Some(chrono_at(2026, 3, 7)),
                ..NewPost::new(self.author, post_type)
            },
        )
        .await
        .expect("creates the post")
    }

    async fn published(&self, title: &str, slug: &str) -> Post {
        self.post("post", PostStatus::Publish, title, slug, "Body text.")
            .await
    }

    /// A client for the app with the themes in `themes_dir`.
    async fn client_with_themes(&self, themes_dir: PathBuf) -> Client {
        let config = Config {
            database_url: "unused: the database is passed in".to_owned(),
            secret_key: "unused".to_owned(),
            debug: false,
            upload_max_bytes: 1024,
            themes_dir,
        };
        Client::tracked(rocket_with_database(config, self.test.shared()))
            .await
            .expect("the app ignites")
    }

    /// A client for the app with the shipped themes.
    async fn client(&self) -> Client {
        self.client_with_themes(shipped_themes()).await
    }
}

fn chrono_at(y: i32, m: u32, d: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(8, 5, 9)
        .unwrap()
}

/// GETs `uri` and returns the status and the body.
async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

/// The marker each default theme template puts on its `<main>`.
fn marker(template: &str) -> String {
    format!("data-template=\"{template}\"")
}

/// A copy of the default theme in a new themes folder, for tests that change it.
fn copy_of_default_theme() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("creates a temporary folder");
    copy_dir(
        &shipped_themes().join("default"),
        &dir.path().join("themes/default"),
    );
    dir
}

/// Copies the theme's files, leaving out `node_modules` and a local build.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "node_modules" || name == "public" {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[rocket::async_test]
async fn the_home_page_lists_the_latest_posts() {
    let site = Site::new().await;
    site.published("Hello world", "hello-world").await;
    site.published("Second post", "second-post").await;
    let client = site.client().await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("home")), "{body}");
    assert!(body.contains("Test Site"), "{body}");
    assert!(body.contains("Just another test site"), "{body}");
    assert!(body.contains("Hello world"), "{body}");
    assert!(body.contains("Second post"), "{body}");
    assert!(body.contains("href=\"/hello-world/\""), "{body}");
    assert!(body.starts_with("<!doctype html>"), "{body}");
}

#[rocket::async_test]
async fn a_post_renders_with_the_single_template_and_its_content_filtered() {
    let site = Site::new().await;
    site.post(
        "post",
        PostStatus::Publish,
        "Hello world",
        "hello-world",
        "<p>Before [greeting name=\"Ann\"] after.</p>",
    )
    .await;
    let client = site.client().await;
    let shortcodes = client
        .rocket()
        .state::<Arc<Shortcodes>>()
        .expect("the app manages its shortcodes");
    shortcodes
        .add_shortcode("greeting", Owner::Core, |attrs, _| {
            format!("<strong>Hi {}</strong>", attrs.get("name").unwrap_or("you"))
        })
        .unwrap();

    let (status, body) = get(&client, "/hello-world/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("single")), "{body}");
    assert!(body.contains("Hello world"), "{body}");
    // `the_content` runs the shortcodes, and the content is output raw, as WordPress does.
    assert!(
        body.contains("<p>Before <strong>Hi Ann</strong> after.</p>"),
        "{body}"
    );
    assert!(
        body.contains("<title>Hello world – Test Site</title>"),
        "{body}"
    );
}

#[rocket::async_test]
async fn a_page_renders_with_the_page_template() {
    let site = Site::new().await;
    site.post(
        "page",
        PostStatus::Publish,
        "About us",
        "about",
        "<p>Who we are.</p>",
    )
    .await;
    let client = site.client().await;

    let (status, body) = get(&client, "/about/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("page")), "{body}");
    assert!(body.contains("About us"), "{body}");
    assert!(body.contains("<p>Who we are.</p>"), "{body}");
}

#[rocket::async_test]
async fn a_category_archive_lists_its_posts_only() {
    let site = Site::new().await;
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .expect("creates the category");
    let in_news = site.published("In the news", "in-the-news").await;
    in_news
        .sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .expect("adds the category");
    site.published("Elsewhere", "elsewhere").await;
    let client = site.client().await;

    let (status, body) = get(&client, "/category/news/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("archive")), "{body}");
    assert!(body.contains("Category: News"), "{body}");
    assert!(body.contains("In the news"), "{body}");
    assert!(!body.contains("Elsewhere"), "{body}");
}

#[rocket::async_test]
async fn a_search_renders_with_the_search_template() {
    let site = Site::new().await;
    site.published("Rockets are fast", "rockets").await;
    site.published("Boats are slow", "boats").await;
    let client = site.client().await;

    let (status, body) = get(&client, "/?s=rockets").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("search")), "{body}");
    assert!(body.contains("Rockets are fast"), "{body}");
    assert!(!body.contains("Boats are slow"), "{body}");
}

#[rocket::async_test]
async fn a_missing_page_is_a_404_with_the_404_template() {
    let site = Site::new().await;
    let client = site.client().await;

    let (status, body) = get(&client, "/no-such-thing/").await;
    assert_eq!(status, Status::NotFound, "{body}");
    assert!(body.contains(&marker("404")), "{body}");
    assert!(body.contains("Test Site"), "{body}");
}

#[rocket::async_test]
async fn a_draft_is_not_visible() {
    let site = Site::new().await;
    site.post(
        "post",
        PostStatus::Draft,
        "Secret draft",
        "secret-draft",
        "Not yet.",
    )
    .await;
    site.published("Public post", "public-post").await;
    let client = site.client().await;

    let (status, body) = get(&client, "/secret-draft/").await;
    assert_eq!(status, Status::NotFound, "{body}");
    assert!(!body.contains("Secret draft"), "{body}");

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains("Public post"), "{body}");
    assert!(!body.contains("Secret draft"), "{body}");
}

#[rocket::async_test]
async fn an_old_slug_redirects_with_301() {
    let site = Site::new().await;
    let mut post = site.published("Renamed", "old-name").await;
    post.slug = "new-name".to_owned();
    post.save(site.db()).await.expect("saves the new slug");
    let client = site.client().await;

    let response = client.get("/old-name/").dispatch().await;
    assert_eq!(response.status(), Status::MovedPermanently);
    assert_eq!(response.headers().get_one("Location"), Some("/new-name/"));

    let (status, body) = get(&client, "/new-name/").await;
    assert_eq!(status, Status::Ok, "{body}");
}

#[rocket::async_test]
async fn lists_are_paginated_by_posts_per_page() {
    let site = Site::new().await;
    site.option("posts_per_page", json!(1)).await;
    let mut older = site.published("Earlier entry", "older").await;
    older.published_at = Some(chrono_at(2026, 1, 1));
    older.save(site.db()).await.unwrap();
    site.published("Later entry", "newer").await;
    let client = site.client().await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains("Later entry"), "{body}");
    assert!(!body.contains("Earlier entry"), "{body}");
    assert!(body.contains("href=\"/page/2/\""), "{body}");

    let (status, body) = get(&client, "/page/2/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains("Earlier entry"), "{body}");
    assert!(body.contains("href=\"/\""), "{body}");

    let (status, _) = get(&client, "/page/3/").await;
    assert_eq!(status, Status::NotFound);
}

#[rocket::async_test]
async fn echoed_values_are_escaped() {
    let site = Site::new().await;
    site.post(
        "post",
        PostStatus::Publish,
        "<script>alert(1)</script>",
        "hostile-title",
        "Body.",
    )
    .await;
    let client = site.client().await;

    for uri in ["/hostile-title/", "/"] {
        let (status, body) = get(&client, uri).await;
        assert_eq!(status, Status::Ok, "{body}");
        assert!(
            body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{uri}: {body}"
        );
        assert!(!body.contains("<script>alert(1)"), "{uri}: {body}");
    }
}

#[rocket::async_test]
async fn a_theme_template_cannot_include_a_file_outside_the_theme() {
    let site = Site::new().await;
    site.published("Hello world", "hello-world").await;
    for hostile in [
        "@include('../../secret')",
        "@include('../secret')",
        "@include('/etc/hosts')",
        "@extends('other::layouts.app')",
    ] {
        let dir = copy_of_default_theme();
        std::fs::write(dir.path().join("secret.propel"), "TOP-SECRET-MARKER").unwrap();
        std::fs::write(dir.path().join("themes/secret.propel"), "TOP-SECRET-MARKER").unwrap();
        std::fs::write(dir.path().join("themes/default/single.propel"), hostile).unwrap();
        let client = site.client_with_themes(dir.path().join("themes")).await;

        let (status, body) = get(&client, "/hello-world/").await;
        assert_eq!(status, Status::InternalServerError, "{hostile}: {body}");
        assert!(!body.contains("TOP-SECRET-MARKER"), "{hostile}: {body}");
        assert!(!body.contains("localhost"), "{hostile}: {body}");
    }

    // A link inside the theme that leads out of it is refused too.
    #[cfg(unix)]
    {
        let dir = copy_of_default_theme();
        let secret = dir.path().join("secret.propel");
        std::fs::write(&secret, "TOP-SECRET-MARKER").unwrap();
        std::os::unix::fs::symlink(
            &secret,
            dir.path().join("themes/default/partials/leak.propel"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("themes/default/single.propel"),
            "@include('partials.leak')",
        )
        .unwrap();
        let client = site.client_with_themes(dir.path().join("themes")).await;

        let (status, body) = get(&client, "/hello-world/").await;
        assert_eq!(status, Status::InternalServerError, "{body}");
        assert!(!body.contains("TOP-SECRET-MARKER"), "{body}");
    }
}

#[rocket::async_test]
async fn templates_render_without_a_vite_build_and_load_it_when_present() {
    let site = Site::new().await;
    let dir = copy_of_default_theme();
    let client = site.client_with_themes(dir.path().join("themes")).await;
    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(!body.contains("/themes/default/build/"), "{body}");
    drop(client);

    let build = dir.path().join("themes/default/public/build");
    std::fs::create_dir_all(build.join("assets")).unwrap();
    std::fs::write(
        build.join("manifest.json"),
        r#"{
            "resources/css/app.css": {"file": "assets/app-1a2b.css", "isEntry": true},
            "resources/js/app.js": {"file": "assets/app-3c4d.js", "isEntry": true}
        }"#,
    )
    .unwrap();
    std::fs::write(build.join("assets/app-1a2b.css"), "body{margin:0}").unwrap();
    let client = site.client_with_themes(dir.path().join("themes")).await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(
        body.contains(
            "<link rel=\"stylesheet\" href=\"/themes/default/build/assets/app-1a2b.css\">"
        ),
        "{body}"
    );
    assert!(
        body.contains("src=\"/themes/default/build/assets/app-3c4d.js\""),
        "{body}"
    );
    let (status, css) = get(&client, "/themes/default/build/assets/app-1a2b.css").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(css, "body{margin:0}");
}

#[rocket::async_test]
async fn a_missing_active_theme_falls_back_to_the_default_theme() {
    let site = Site::new().await;
    site.option("stylesheet", json!("not-installed")).await;
    site.option("template", json!("not-installed")).await;
    let client = site.client().await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("home")), "{body}");
}

#[rocket::async_test]
async fn a_manual_excerpt_is_escaped_on_lists() {
    let site = Site::new().await;
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .expect("creates the category");
    let mut post = site.published("Excerpted", "excerpted").await;
    post.excerpt = "<script>alert('x')</script> Short & sweet".to_owned();
    post.save(site.db()).await.expect("saves the excerpt");
    post.sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .expect("adds the category");
    let client = site.client().await;

    for uri in ["/", "/category/news/"] {
        let (status, body) = get(&client, uri).await;
        assert_eq!(status, Status::Ok, "{uri}: {body}");
        assert!(
            body.contains("&lt;script&gt;alert(&#039;x&#039;)&lt;/script&gt; Short &amp; sweet"),
            "{uri}: {body}"
        );
        assert!(!body.contains("<script>alert("), "{uri}: {body}");
    }
}

#[rocket::async_test]
async fn a_search_query_is_escaped_where_it_is_reflected() {
    let site = Site::new().await;
    let client = site.client().await;

    let (status, body) = get(&client, "/?s=%22%3E%3Cscript%3Ealert(1)%3C/script%3E").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(!body.contains("<script>alert(1)"), "{body}");
    assert!(!body.contains("\"><script"), "{body}");
    assert!(
        body.contains("value=\"&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\""),
        "{body}"
    );
    assert!(
        body.contains(
            "<title>Search Results for “&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;”"
        ),
        "{body}"
    );
}

#[rocket::async_test]
async fn a_javascript_site_address_never_reaches_a_link() {
    let site = Site::new().await;
    site.option("home", json!("javascript:alert(1)")).await;
    site.published("Hello world", "hello-world").await;
    let client = site.client().await;

    for uri in ["/", "/hello-world/"] {
        let (status, body) = get(&client, uri).await;
        assert_eq!(status, Status::Ok, "{uri}: {body}");
        assert!(!body.contains("javascript:"), "{uri}: {body}");
    }
}

#[rocket::async_test]
async fn a_hostile_term_description_is_escaped() {
    let site = Site::new().await;
    let news = Term::create(
        site.db(),
        &Taxonomy::CATEGORY,
        NewTerm {
            description: "<img src=x onerror=alert(1)>".to_owned(),
            ..NewTerm::new("News")
        },
    )
    .await
    .expect("creates the category");
    let post = site.published("In the news", "in-the-news").await;
    post.sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    let client = site.client().await;

    let (status, body) = get(&client, "/category/news/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(!body.contains("<img src=x"), "{body}");
    assert!(
        body.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "{body}"
    );
}

#[rocket::async_test]
async fn a_page_with_a_custom_template_gets_its_body_classes() {
    let site = Site::new().await;
    let wide = site
        .post("page", PostStatus::Publish, "Wide", "wide", "Wide page.")
        .await;
    mission_control_core::content::set_post_meta(
        site.db(),
        wide.id,
        "_wp_page_template",
        &json!("templates/full-width.php"),
    )
    .await
    .expect("sets the page template");
    site.post("page", PostStatus::Publish, "Plain", "plain", "Plain page.")
        .await;
    let client = site.client().await;

    let (status, body) = get(&client, "/wide/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(
        body.contains(
            "class=\"page-template page-template-templates page-template-full-width \
             page-template-templatesfull-width-php page"
        ),
        "{body}"
    );
    assert!(!body.contains("page-template-default"), "{body}");

    let (_, body) = get(&client, "/plain/").await;
    assert!(
        body.contains("class=\"page-template-default page"),
        "{body}"
    );
}
