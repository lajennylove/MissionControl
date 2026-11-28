//! Twenty Twenty-Six, the 2026 default theme built from WordPress's Twenty Twenty-Five, rendered
//! through the front-end request path: the home page, a post, a page, an archive and a 404, with
//! Twenty Twenty-Five's structure, its theme.json presets as WordPress's `--wp--preset--*`
//! variables and its `has-*` classes. Also the template the hierarchy picks in the theme for
//! each kind of request, and the theme's manifest.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use mission_control::rocket_with_database;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, delete_option, set_option, set_post_meta,
};
use mission_control_core::front::{Archive, Resolved};
use mission_control_core::migrations::migrator;
use mission_control_core::theme::{
    Manifest, NewMenuItem, Support, Theme, ThemeImageSize, add_menu_item, create_nav_menu,
    load_theme, set_menu_location, templates_for,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use serde_json::json;

/// The theme's slug: its folder in `MissionControl/themes`.
const SLUG: &str = "twentytwentysix";

fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

fn theme() -> Theme {
    load_theme(&shipped_themes().join(SLUG)).expect("the theme loads")
}

/// A site with Twenty Twenty-Six active: the core tables, a site named `Test <Site>` with
/// `/%postname%/` links, and an author.
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
            &["Ann <b>".into(), unique_email("ann").into(), "hash".into()],
        )
        .await
        .expect("inserts the author");
        let author = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
            .await
            .expect("reads the author id");
        let site = Self { test, author };
        site.option("blogname", json!("Test &lt;Site&gt;")).await;
        site.option("blogdescription", json!("A tagline")).await;
        site.option("permalink_structure", json!("/%postname%/"))
            .await;
        site.option("stylesheet", json!(SLUG)).await;
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

    async fn post(&self, post_type: &str, title: &str, slug: &str, content: &str) -> Post {
        Post::create(
            self.db(),
            NewPost {
                status: PostStatus::Publish,
                title: title.to_owned(),
                slug: slug.to_owned(),
                content: content.to_owned(),
                published_at: Some(at(2026, 3, 7)),
                ..NewPost::new(self.author, post_type)
            },
        )
        .await
        .expect("creates the post")
    }

    async fn client(&self) -> Client {
        let config = Config {
            database_url: "unused: the database is passed in".to_owned(),
            secret_key: "unused".to_owned(),
            debug: false,
            upload_max_bytes: 1024,
            themes_dir: shipped_themes(),
            uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
            plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
            environment: Environment::Production,
            trusted_proxies: TrustedProxies::None,
        };
        Client::tracked(rocket_with_database(config, self.test.shared()))
            .await
            .expect("the app ignites")
    }
}

fn at(y: i32, m: u32, d: u32) -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(8, 5, 9)
        .unwrap()
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

fn marker(template: &str) -> String {
    format!("data-template=\"{template}\"")
}

/// What every page of the theme has: Twenty Twenty-Five's frame, theme.json's presets with
/// WordPress's names, and Mission Control's credit, not WordPress's.
fn assert_frame(body: &str) {
    let expected = [
        "<style id=\"global-styles-inline-css\">",
        // The palette.
        "--wp--preset--color--base: #FFFFFF;",
        "--wp--preset--color--contrast: #111111;",
        "--wp--preset--color--accent-1: #FFEE58;",
        "--wp--preset--color--accent-2: #F6CFF4;",
        "--wp--preset--color--accent-3: #503AA8;",
        "--wp--preset--color--accent-4: #686868;",
        "--wp--preset--color--accent-5: #FBFAF3;",
        "--wp--preset--color--accent-6: color-mix(in srgb, currentColor 20%, transparent);",
        // The fonts and the font sizes, fluid ones as WordPress computes them.
        "--wp--preset--font-family--manrope: Manrope, sans-serif;",
        "--wp--preset--font-family--fira-code: \"Fira Code\", monospace;",
        "--wp--preset--font-size--small: 0.875rem;",
        "--wp--preset--font-size--medium: clamp(1rem, 1rem + ((1vw - 0.2rem) * 0.196), 1.125rem);",
        "--wp--preset--font-size--large: clamp(1.125rem, 1.125rem + ((1vw - 0.2rem) * 0.392), 1.375rem);",
        "--wp--preset--font-size--x-large: clamp(1.75rem, 1.75rem + ((1vw - 0.2rem) * 0.392), 2rem);",
        "--wp--preset--font-size--xx-large: clamp(2.15rem, 2.15rem + ((1vw - 0.2rem) * 1.333), 3rem);",
        "--wp--preset--spacing--50: clamp(30px, 5vw, 50px);",
        // The layout.
        "--wp--style--global--content-size: 645px;",
        "--wp--style--global--wide-size: 1340px;",
        "--wp--style--root--padding-right: var(--wp--preset--spacing--50);",
        ".is-layout-constrained > .alignwide{max-width: var(--wp--style--global--wide-size);}",
        // The body and the elements.
        "body{background-color: var(--wp--preset--color--base);color: var(--wp--preset--color--contrast);font-family: var(--wp--preset--font-family--manrope);font-size: var(--wp--preset--font-size--large);font-weight: 300;",
        ":root :where(h1){font-size: var(--wp--preset--font-size--xx-large);}",
        // The preset classes.
        ".has-accent-3-color{color: var(--wp--preset--color--accent-3) !important;}",
        ".has-base-background-color{background-color: var(--wp--preset--color--base) !important;}",
        ".has-small-font-size{font-size: var(--wp--preset--font-size--small) !important;}",
        ".has-manrope-font-family{font-family: var(--wp--preset--font-family--manrope) !important;}",
        // The frame.
        "<div class=\"wp-site-blocks\">",
        "<header class=\"wp-block-template-part\">",
        "<footer class=\"wp-block-template-part\">",
        "id=\"wp--skip-link--target\"",
        "Proudly powered by <a href=\"https://rocketboost.rs\" rel=\"nofollow\">Mission Control</a>",
    ];
    for text in expected {
        assert!(body.contains(text), "missing `{text}` in {body}");
    }
    assert!(!body.contains("WordPress"), "{body}");
    // `heading` comes before `h6`, so `h6` keeps its weight, as WordPress orders them.
    let heading = body.find(":root :where(h1, h2, h3, h4, h5, h6){").unwrap();
    let h6 = body.find(":root :where(h6){").unwrap();
    assert!(heading < h6, "{body}");
    // The site's name is decoded from its stored form and escaped once.
    assert!(body.contains(">Test &lt;Site&gt;</a>"), "{body}");
}

#[rocket::async_test]
async fn front_home_lists_posts_in_the_query_loop() {
    let site = Site::new().await;
    site.post("post", "Hello <world>", "hello-world", "<p>First body.</p>")
        .await;
    site.post("post", "Second post", "second-post", "<p>Second body.</p>")
        .await;
    let client = site.client().await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_frame(&body);
    assert!(body.contains(&marker("home")), "{body}");
    // The front page's site title is its `h1`.
    assert!(
        body.contains("<h1 class=\"wp-block-site-title\"><a href=\"/\" target=\"_self\" rel=\"home\" aria-current=\"page\">"),
        "{body}"
    );
    assert!(
        body.contains("<h1 class=\"wp-block-heading has-text-align-left\">Blog</h1>"),
        "{body}"
    );
    // Each post: the title (x-large, linked and escaped), the full content, the date.
    assert!(
        body.contains(
            "<h2 class=\"wp-block-post-title has-x-large-font-size\"><a href=\"/hello-world/\" \
             target=\"_self\">Hello &lt;world&gt;</a></h2>"
        ),
        "{body}"
    );
    assert!(body.contains("<p>First body.</p>"), "{body}");
    assert!(body.contains("<p>Second body.</p>"), "{body}");
    assert!(
        body.contains("wp-block-post-content has-medium-font-size"),
        "{body}"
    );
    assert!(
        body.contains("<div class=\"wp-block-post-date has-small-font-size\""),
        "{body}"
    );
    assert!(body.contains(">March 7, 2026</a></time>"), "{body}");
}

/// Twenty Twenty-Six is the front end's default theme: a site with no active theme (no
/// `stylesheet` option, as before the first theme switch) renders with it, as WordPress renders
/// its yearly theme, `WP_DEFAULT_THEME`. The owner chose it over the older `default` theme.
#[rocket::async_test]
async fn front_default_theme_is_twenty_twenty_six() {
    assert_eq!(mission_control_core::front::DEFAULT_THEME, SLUG);
    let site = Site::new().await;
    delete_option(site.db(), "stylesheet")
        .await
        .expect("deletes the option");
    let client = site.client().await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains("<div class=\"wp-site-blocks\">"), "{body}");
    assert!(body.contains("Sorry, but nothing was found."), "{body}");
}

#[rocket::async_test]
async fn front_home_with_no_posts_says_nothing_was_found() {
    let site = Site::new().await;
    let client = site.client().await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains("Sorry, but nothing was found."), "{body}");
    assert!(
        !body.contains("<ul class=\"alignfull wp-block-post-template"),
        "{body}"
    );
}

#[rocket::async_test]
async fn front_single_has_the_title_the_author_and_the_content() {
    let site = Site::new().await;
    site.post(
        "post",
        "A <b>bold</b> post",
        "bold",
        "<p>Content with <em>markup</em>.</p>",
    )
    .await;
    let client = site.client().await;

    let (status, body) = get(&client, "/bold/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_frame(&body);
    assert!(body.contains(&marker("single")), "{body}");
    assert!(
        body.contains("<p class=\"wp-block-site-title\">"),
        "the site title is not a heading off the front page: {body}"
    );
    assert!(
        body.contains("<h1 class=\"wp-block-post-title\">A &lt;b&gt;bold&lt;/b&gt; post</h1>"),
        "{body}"
    );
    assert!(
        body.contains(
            "class=\"wp-block-group has-accent-4-color has-text-color has-link-color \
             has-small-font-size"
        ),
        "{body}"
    );
    assert!(
        body.contains("<div class=\"wp-block-post-author-name\">Ann &lt;b&gt;</div>"),
        "{body}"
    );
    // The post content is the one raw output.
    assert!(
        body.contains("<p>Content with <em>markup</em>.</p>"),
        "{body}"
    );
    assert!(body.contains("wp-block-post-content"), "{body}");
}

#[rocket::async_test]
async fn front_page_and_page_without_title() {
    let site = Site::new().await;
    site.post("page", "About us", "about", "<p>Who we are.</p>")
        .await;
    let bare = site
        .post("page", "Hidden title", "bare", "<p>Only content.</p>")
        .await;
    set_post_meta(
        site.db(),
        bare.id,
        "_wp_page_template",
        &json!("templates/page-no-title"),
    )
    .await
    .expect("sets the page template");
    let client = site.client().await;

    let (status, body) = get(&client, "/about/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_frame(&body);
    assert!(body.contains(&marker("page")), "{body}");
    assert!(
        body.contains("<h1 class=\"wp-block-post-title\">About us</h1>"),
        "{body}"
    );
    assert!(body.contains("<p>Who we are.</p>"), "{body}");

    let (status, body) = get(&client, "/bare/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("page-no-title")), "{body}");
    assert!(body.contains("<p>Only content.</p>"), "{body}");
    assert!(
        !body.contains("<h1 class=\"wp-block-post-title\""),
        "{body}"
    );
}

#[rocket::async_test]
async fn front_category_archive_has_its_title_and_description() {
    let site = Site::new().await;
    let news = Term::create(
        site.db(),
        &Taxonomy::CATEGORY,
        NewTerm {
            description: "Latest <i>news</i>".to_owned(),
            ..NewTerm::new("News")
        },
    )
    .await
    .expect("creates the category");
    let in_news = site
        .post("post", "In the news", "in-the-news", "<p>News body.</p>")
        .await;
    in_news
        .sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .expect("adds the category");
    site.post("post", "Elsewhere", "elsewhere", "<p>Other.</p>")
        .await;
    let client = site.client().await;

    let (status, body) = get(&client, "/category/news/").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_frame(&body);
    assert!(body.contains(&marker("archive")), "{body}");
    assert!(
        body.contains("<h1 class=\"wp-block-query-title\">Category: News</h1>"),
        "{body}"
    );
    assert!(
        body.contains(
            "<div class=\"wp-block-term-description\"><p>Latest &lt;i&gt;news&lt;/i&gt;</p></div>"
        ),
        "{body}"
    );
    assert!(body.contains("In the news"), "{body}");
    assert!(!body.contains("Elsewhere"), "{body}");
}

#[rocket::async_test]
async fn front_not_found_is_a_404_with_the_search_form() {
    let site = Site::new().await;
    let client = site.client().await;

    let (status, body) = get(&client, "/no-such-page/").await;
    assert_eq!(status, Status::NotFound, "{body}");
    assert_frame(&body);
    assert!(body.contains(&marker("404")), "{body}");
    assert!(
        body.contains("<h1 class=\"wp-block-heading\">Page not found</h1>"),
        "{body}"
    );
    assert!(
        body.contains(
            "class=\"wp-block-search__button-outside wp-block-search__text-button wp-block-search\""
        ),
        "{body}"
    );
    assert!(body.contains("wp-block-columns alignwide"), "{body}");
}

#[rocket::async_test]
async fn front_search_reflects_the_terms_escaped() {
    let site = Site::new().await;
    site.post("post", "Rockets are fast", "rockets", "<p>Fast.</p>")
        .await;
    let client = site.client().await;

    let (status, body) = get(&client, "/?s=%22%3E%3Cscript%3Erockets").await;
    assert_eq!(status, Status::Ok, "{body}");
    assert!(body.contains(&marker("search")), "{body}");
    assert!(!body.contains("<script>rockets"), "{body}");
    assert!(
        body.contains("Search results for: “&quot;&gt;&lt;script&gt;rockets”"),
        "{body}"
    );
}

#[rocket::async_test]
async fn front_primary_menu_shows_in_the_header_navigation() {
    let site = Site::new().await;
    let db = site.db();
    let about = site.post("page", "About", "about", "<p>About.</p>").await;
    let menu = create_nav_menu(db, "Main").await.unwrap();
    add_menu_item(db, &menu, site.author, NewMenuItem::post(&about))
        .await
        .unwrap();
    add_menu_item(
        db,
        &menu,
        site.author,
        NewMenuItem::custom("Fish & chips", "/fish/"),
    )
    .await
    .unwrap();
    set_menu_location(db, &theme(), "primary", Some(&menu))
        .await
        .unwrap();
    let client = site.client().await;

    let (status, body) = get(&client, "/").await;
    assert_eq!(status, Status::Ok, "{body}");
    let header = &body[body.find("<header").unwrap()..body.find("</header>").unwrap()];
    assert!(header.contains("wp-block-navigation"), "{header}");
    assert!(
        header.contains(
            "<a class=\"wp-block-navigation-item__content\" href=\"/about/\"><span \
             class=\"wp-block-navigation-item__label\">About</span></a>"
        ),
        "{header}"
    );
    assert!(
        header.contains("<span class=\"wp-block-navigation-item__label\">Fish &amp; chips</span>"),
        "{header}"
    );
}

fn post(id: i64, post_type: &str, slug: &str) -> Post {
    Post {
        id,
        author_id: 1,
        post_type: post_type.to_owned(),
        status: PostStatus::Publish,
        title: slug.to_owned(),
        slug: slug.to_owned(),
        content: String::new(),
        excerpt: String::new(),
        parent_id: None,
        menu_order: 0,
        published_at: Some(at(2026, 3, 1)),
        modified_at: at(2026, 3, 1),
        password: None,
        mime_type: String::new(),
    }
}

fn term(id: i64, taxonomy: &str, slug: &str) -> Term {
    Term {
        term_id: id,
        term_taxonomy_id: id,
        name: slug.to_owned(),
        slug: slug.to_owned(),
        term_group: 0,
        taxonomy: taxonomy.to_owned(),
        description: String::new(),
        parent_id: None,
        count: 0,
    }
}

/// A request, its page template, the candidates and the template found.
type Case = (
    &'static str,
    Resolved,
    Option<&'static str>,
    &'static [&'static str],
    &'static str,
);

/// The hierarchy's candidates for each kind of request, and the file Twenty Twenty-Six answers
/// with, as WordPress's `locate_template` picks it from Twenty Twenty-Five's templates.
#[test]
fn front_template_hierarchy_table() {
    let theme = theme();
    let archive = |archive| Resolved::Archive { archive, paged: 1 };
    let cases: Vec<Case> = vec![
        (
            "a post",
            Resolved::Single(post(7, "post", "hello")),
            None,
            &[
                "single-post-hello",
                "single-post",
                "single",
                "singular",
                "index",
            ],
            "single",
        ),
        (
            "a custom post type's entry",
            Resolved::Single(post(8, "book", "dune")),
            None,
            &[
                "single-book-dune",
                "single-book",
                "single",
                "singular",
                "index",
            ],
            "single",
        ),
        (
            "a page",
            Resolved::Page(post(9, "page", "about")),
            None,
            &["page-about", "page-9", "page", "singular", "index"],
            "page",
        ),
        (
            "a page with Page No Title",
            Resolved::Page(post(10, "page", "bare")),
            Some("templates/page-no-title"),
            &[
                "templates/page-no-title",
                "page-bare",
                "page-10",
                "page",
                "singular",
                "index",
            ],
            "templates/page-no-title",
        ),
        (
            "a post type archive",
            archive(Archive::PostType("book".to_owned())),
            None,
            &["archive-book", "archive", "index"],
            "archive",
        ),
        (
            "a category",
            archive(Archive::Category(term(3, "category", "news"))),
            None,
            &[
                "category-news",
                "category-3",
                "category",
                "archive",
                "index",
            ],
            "archive",
        ),
        (
            "a search",
            Resolved::Search {
                terms: "rocket".to_owned(),
                paged: 1,
            },
            None,
            &["search", "index"],
            "search",
        ),
        ("a 404", Resolved::NotFound, None, &["404", "index"], "404"),
    ];
    for (what, resolved, page_template, expected, file) in cases {
        let names = templates_for(&resolved, page_template);
        assert_eq!(names, expected, "{what}");
        let found = theme.locate_template(&names).expect("a template is found");
        assert_eq!(found.name, file, "{what}");
        assert_eq!(found.theme, SLUG, "{what}");
    }
}

/// The blog on the front page, and on a posts page: `home`, as Twenty Twenty-Five has it.
#[test]
fn front_template_hierarchy_home() {
    let theme = theme();
    let names = templates_for(
        &Resolved::FrontPage {
            page: None,
            paged: 1,
        },
        None,
    );
    assert_eq!(names, ["front-page", "home", "index"]);
    let found = theme.locate_template(&names).unwrap();
    assert_eq!(found.name, "home");
}

#[test]
fn front_theme_manifest_parses() {
    let theme = theme();
    let manifest = theme.manifest();
    assert_eq!(manifest.name, "Twenty Twenty-Six");
    // 0.1.0 until the team has checked it against WordPress's theme standards (the owner's rule).
    assert_eq!(manifest.version.as_deref(), Some("0.1.0"));
    assert_eq!(manifest.requires.as_deref(), Some("0.1"));
    assert_eq!(manifest.tested.as_deref(), Some("0.1"));
    assert_eq!(manifest.license.as_deref(), Some("GPL-2.0-or-later"));
    assert_eq!(manifest.template, None);
    for feature in [
        Support::Menus,
        Support::PostThumbnails,
        Support::CustomLogo,
        Support::Widgets,
        Support::TitleTag,
    ] {
        assert!(manifest.supports(feature), "{feature:?}");
    }
    let locations: Vec<&str> = manifest.menus.iter().map(|m| m.location.as_str()).collect();
    assert_eq!(locations, ["primary", "footer_1", "footer_2"]);
    // Twenty Twenty-Five registers no image sizes of its own.
    assert!(manifest.image_sizes.is_empty());
    let templates = theme.page_templates();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "templates/page-no-title");
    assert_eq!(templates[0].title, "Page No Title");
}

#[test]
fn front_theme_manifest_image_sizes() {
    let path = Path::new("theme.toml");
    let manifest = Manifest::parse(
        "name = \"X\"\ntested = \"0.2\"\n[image_sizes]\nfeatured = { width = 1200, height = 800, \
         crop = true }\nwide-banner = { width = 1340 }\n",
        path,
    )
    .expect("parses");
    assert_eq!(manifest.tested.as_deref(), Some("0.2"));
    assert_eq!(
        manifest.image_sizes,
        [
            ThemeImageSize {
                name: "featured".to_owned(),
                width: 1200,
                height: 800,
                crop: true,
            },
            ThemeImageSize {
                name: "wide-banner".to_owned(),
                width: 1340,
                height: 0,
                crop: false,
            },
        ]
    );
    for bad in [
        "name = \"X\"\n[image_sizes]\n\"Bad Name\" = { width = 10 }\n",
        "name = \"X\"\n[image_sizes]\nempty = { crop = true }\n",
        "name = \"X\"\n[image_sizes]\nodd = { width = 10, depth = 3 }\n",
        "name = \"X\"\n[image_sizes]\nnegative = { width = -1 }\n",
    ] {
        assert!(Manifest::parse(bad, path).is_err(), "{bad}");
    }
}
