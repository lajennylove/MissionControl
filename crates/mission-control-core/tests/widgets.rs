//! Classic widgets, stored as WordPress stores them: each widget type's instances in the option
//! `widget_{id_base}` (keyed by number, with `_multiwidget`), and the areas' widgets in
//! `sidebars_widgets`. Then the areas as the theme renders them.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::fs;

use chrono::{FixedOffset, NaiveDate};
use mission_control_core::content::{
    NewPost, NewTerm, PermalinkStructure, Post, PostStatus, Taxonomy, Term, get_option, set_option,
};
use mission_control_core::front::{Query, Resolved, resolve};
use mission_control_core::migrations::migrator;
use mission_control_core::theme::{
    PageRequest, SidebarView, Theme, WidgetError, load_theme, save_widget, set_sidebar_widgets,
    theme_sidebars,
};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use serde_json::json;

const HOME: &str = "https://example.com";

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
    set_option(db, "permalink_structure", &json!("/%postname%/"))
        .await
        .unwrap();
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the author id")
}

/// A theme with a `sidebar` and a `footer` widget area.
fn theme() -> (tempfile::TempDir, Theme) {
    let dir = tempfile::tempdir().unwrap();
    let theme_dir = dir.path().join("widgets-theme");
    fs::create_dir_all(&theme_dir).unwrap();
    fs::write(
        theme_dir.join("theme.toml"),
        "name = \"Widgets\"\n[sidebars]\nsidebar = \"Sidebar\"\nfooter = \"Footer\"\n",
    )
    .unwrap();
    fs::write(theme_dir.join("index.propel"), "").unwrap();
    let theme = load_theme(&theme_dir).unwrap();
    (dir, theme)
}

async fn post(db: &Database, author: i64, title: &str, day: u32, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: title.to_owned(),
            published_at: Some(
                NaiveDate::from_ymd_opt(2026, 3, day)
                    .unwrap()
                    .and_hms_opt(8, 0, 0)
                    .unwrap(),
            ),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("creates the post")
}

/// The theme's areas, as they render for the request `path`.
async fn sidebars(db: &Database, theme: &Theme, path: &str) -> Vec<SidebarView> {
    let structure: PermalinkStructure = "/%postname%/".parse().unwrap();
    let resolved: Resolved = resolve(db, path, &Query::parse(""), &structure)
        .await
        .unwrap();
    let request = PageRequest {
        resolved: &resolved,
        home: HOME,
        path,
        query: "",
        structure: &structure,
        utc_offset: FixedOffset::east_opt(0).unwrap(),
        search_query: "rock & <roll>",
    };
    theme_sidebars(db, theme, &request)
        .await
        .expect("reads the widget areas")
}

#[tokio::test]
async fn widgets_are_stored_as_wordpress_stores_them() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    let first = save_widget(db, "text", None, &json!({"title": "Hi", "text": "One"}))
        .await
        .unwrap();
    let second = save_widget(db, "text", None, &json!({"title": "Yo", "text": "Two"}))
        .await
        .unwrap();
    // WordPress numbers a type's instances from 2.
    assert_eq!(first, "text-2");
    assert_eq!(second, "text-3");
    let replaced = save_widget(db, "text", Some(2), &json!({"title": "Hey", "text": "One"}))
        .await
        .unwrap();
    assert_eq!(replaced, "text-2");
    assert_eq!(
        get_option(db, "widget_text").await.unwrap().unwrap(),
        json!({
            "2": {"title": "Hey", "text": "One"},
            "3": {"title": "Yo", "text": "Two"},
            "_multiwidget": 1
        })
    );

    set_option(
        db,
        "sidebars_widgets",
        &json!({"wp_inactive_widgets": ["text-9"], "array_version": 3}),
    )
    .await
    .unwrap();
    set_sidebar_widgets(db, "footer", &["text-3", "text-2"])
        .await
        .unwrap();
    assert_eq!(
        get_option(db, "sidebars_widgets").await.unwrap().unwrap(),
        json!({
            "wp_inactive_widgets": ["text-9"],
            "footer": ["text-3", "text-2"],
            "array_version": 3
        })
    );

    for (id_base, instance) in [("Text", json!({})), ("", json!({})), ("text", json!("x"))] {
        assert!(
            save_widget(db, id_base, None, &instance).await.is_err(),
            "{id_base:?} {instance}"
        );
    }
    assert!(matches!(
        set_sidebar_widgets(db, "footer", &["text"]).await,
        Err(WidgetError::InvalidWidgetId)
    ));
    assert!(matches!(
        set_sidebar_widgets(db, "", &["text-2"]).await,
        Err(WidgetError::InvalidSidebarId)
    ));
}

#[tokio::test]
async fn an_area_renders_its_widgets_in_order_and_skips_unknown_ones() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (_dir, theme) = theme();
    post(db, author, "Older", 1, PostStatus::Publish).await;
    post(db, author, "Newest", 5, PostStatus::Publish).await;
    post(db, author, "Middle", 3, PostStatus::Publish).await;
    post(db, author, "Unpublished", 9, PostStatus::Draft).await;
    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let empty = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("Empty"))
        .await
        .unwrap();
    let older = Post::of_type("post")
        .where_eq("title", "Older")
        .first(db)
        .await
        .unwrap()
        .unwrap();
    older
        .sync_terms(db, &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();

    let search = save_widget(db, "search", None, &json!({"title": ""}))
        .await
        .unwrap();
    let recent = save_widget(
        db,
        "recent-posts",
        None,
        &json!({"title": "", "number": 2, "show_date": true}),
    )
    .await
    .unwrap();
    let text = save_widget(
        db,
        "text",
        None,
        &json!({"title": "About", "text": "Hello"}),
    )
    .await
    .unwrap();
    let categories = save_widget(
        db,
        "categories",
        None,
        &json!({"title": "Topics", "count": true}),
    )
    .await
    .unwrap();
    // A widget of a type Mission Control does not have, one whose instance is gone, and an id
    // that is not a widget id.
    set_option(
        db,
        "widget_calendar",
        &json!({"2": {"title": "Cal"}, "_multiwidget": 1}),
    )
    .await
    .unwrap();
    set_sidebar_widgets(
        db,
        "footer",
        &[
            &recent,
            "calendar-2",
            &text,
            "text-99",
            &categories,
            &search,
        ],
    )
    .await
    .unwrap();

    let areas = sidebars(db, &theme, "/").await;
    let ids: Vec<&str> = areas.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["sidebar", "footer"], "in the manifest's order");
    let sidebar = &areas[0];
    assert!(!sidebar.active);
    assert!(sidebar.widgets.is_empty());
    let footer = &areas[1];
    assert_eq!(footer.name, "Footer");
    assert!(footer.active);
    let rendered: Vec<(&str, &str, &str)> = footer
        .widgets
        .iter()
        .map(|w| (w.id.as_str(), w.classes.as_str(), w.title.as_str()))
        .collect();
    assert_eq!(
        rendered,
        [
            (recent.as_str(), "widget_recent_entries", "Recent Posts"),
            (text.as_str(), "widget_text", "About"),
            (categories.as_str(), "widget_categories", "Topics"),
            (search.as_str(), "widget_search", ""),
        ]
    );

    let recent_html = &footer.widgets[0].content;
    let newest = recent_html.find("Newest").expect("lists the newest post");
    let middle = recent_html.find("Middle").expect("lists the second post");
    assert!(newest < middle, "{recent_html}");
    assert!(!recent_html.contains("Older"), "two posts: {recent_html}");
    assert!(!recent_html.contains("Unpublished"), "{recent_html}");
    assert!(
        recent_html.contains("href=\"https://example.com/newest/\""),
        "{recent_html}"
    );
    assert!(
        recent_html.contains("<span class=\"post-date\">March 5, 2026</span>"),
        "{recent_html}"
    );

    assert_eq!(
        footer.widgets[1].content,
        "<div class=\"textwidget\">Hello</div>"
    );

    let categories_html = &footer.widgets[2].content;
    assert!(
        categories_html.contains(&format!(
            "<li class=\"cat-item cat-item-{}\"><a href=\"https://example.com/category/news/\">News</a> (1)</li>",
            news.term_id
        )),
        "{categories_html}"
    );
    assert!(
        !categories_html.contains(&format!("cat-item-{}\"", empty.term_id)),
        "an empty category is hidden: {categories_html}"
    );

    let search_html = &footer.widgets[3].content;
    assert!(
        search_html.contains("action=\"https://example.com/\""),
        "{search_html}"
    );
    assert!(
        search_html.contains("value=\"rock &amp; &lt;roll&gt;\""),
        "{search_html}"
    );
}

#[tokio::test]
async fn widget_text_and_titles_are_escaped() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (_dir, theme) = theme();
    post(db, author, "<b>Bold</b> & co", 2, PostStatus::Publish).await;
    let text = save_widget(
        db,
        "text",
        None,
        &json!({
            "title": "<script>alert('t')</script>",
            "text": "<img src=x onerror=alert(1)>\n\nSecond \"para\"\nline",
            "filter": true
        }),
    )
    .await
    .unwrap();
    let recent = save_widget(db, "recent-posts", None, &json!({"title": "Latest"}))
        .await
        .unwrap();
    set_sidebar_widgets(db, "sidebar", &[&text, &recent])
        .await
        .unwrap();

    let areas = sidebars(db, &theme, "/").await;
    let widgets = &areas[0].widgets;
    // The title is text: the template escapes it with `{{ }}`.
    assert_eq!(widgets[0].title, "<script>alert('t')</script>");
    assert_eq!(
        widgets[0].content,
        "<div class=\"textwidget\"><p>&lt;img src=x onerror=alert(1)&gt;</p>\n\
         <p>Second &quot;para&quot;<br />\nline</p></div>"
    );
    assert!(
        widgets[1]
            .content
            .contains(">&lt;b&gt;Bold&lt;/b&gt; &amp; co</a>"),
        "{}",
        widgets[1].content
    );
}

/// Writes `text` as option `name`'s stored value, valid JSON or not, as a broken import could.
async fn raw_option(db: &Database, name: &str, text: &str) {
    set_option(db, name, &json!(null)).await.unwrap();
    raw::execute(
        db,
        "UPDATE options SET value = ? WHERE name = ?",
        &[text.into(), name.into()],
    )
    .await
    .unwrap();
}

fn li_count(html: &str) -> usize {
    html.matches("<li>").count()
}

#[tokio::test]
async fn malformed_widget_data_from_the_database_is_skipped() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (_dir, theme) = theme();
    for day in 1..=6 {
        post(db, author, &format!("Post {day}"), day, PostStatus::Publish).await;
    }

    // Options that are not JSON: nothing renders, nothing fails, and a save starts afresh.
    raw_option(db, "sidebars_widgets", "{not json").await;
    raw_option(db, "widget_text", "not json either").await;
    let areas = sidebars(db, &theme, "/").await;
    assert!(areas.iter().all(|a| !a.active && a.widgets.is_empty()));
    assert_eq!(
        save_widget(db, "text", None, &json!({"text": "ok"}))
            .await
            .unwrap(),
        "text-2"
    );

    // Instances that are not objects are skipped; numbers that are not numbers fall back.
    set_option(
        db,
        "widget_text",
        &json!({"2": "a string", "3": [1, 2], "4": {"title": "Kept", "text": "t"}, "_multiwidget": 1}),
    )
    .await
    .unwrap();
    set_option(
        db,
        "widget_recent-posts",
        &json!({
            "2": {"number": "abc"},
            "3": {"number": -1},
            "4": {"number": 1e9},
            "5": {"number": "3"},
            "_multiwidget": 1
        }),
    )
    .await
    .unwrap();
    set_option(
        db,
        "sidebars_widgets",
        &json!({
            "footer": "text-4",
            "sidebar": [
                "text-2", "text-3", "text-4", 7, "text-99999999999999999999",
                "recent-posts-2", "recent-posts-3", "recent-posts-4", "recent-posts-5"
            ],
            "array_version": 3
        }),
    )
    .await
    .unwrap();
    let areas = sidebars(db, &theme, "/").await;
    let sidebar = &areas[0];
    let footer = &areas[1];
    assert!(!footer.active, "an area that is not a list is empty");
    let ids: Vec<&str> = sidebar.widgets.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "text-4",
            "recent-posts-2",
            "recent-posts-3",
            "recent-posts-4",
            "recent-posts-5"
        ]
    );
    let counts: Vec<usize> = sidebar.widgets[1..]
        .iter()
        .map(|w| li_count(&w.content))
        .collect();
    // "abc" and -1 give the default 5; 1e9 is capped (all six posts); "3" is 3.
    assert_eq!(counts, [5, 5, 6, 3]);
}

#[tokio::test]
async fn a_new_widget_number_never_overflows() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let stored = json!({
        "2": {"text": "first"},
        "18446744073709551615": {"text": "last"},
        "_multiwidget": 1
    });
    set_option(db, "widget_text", &stored).await.unwrap();

    let result = save_widget(db, "text", None, &json!({"text": "new"})).await;
    assert!(
        matches!(result, Err(WidgetError::NoFreeNumber)),
        "{result:?}"
    );
    assert_eq!(
        get_option(db, "widget_text").await.unwrap().unwrap(),
        stored
    );
    // A key past u64 is not a number, so it is ignored.
    set_option(
        db,
        "widget_text",
        &json!({"99999999999999999999": {"text": "x"}, "_multiwidget": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        save_widget(db, "text", None, &json!({})).await.unwrap(),
        "text-2"
    );
}

#[tokio::test]
async fn reserved_and_overlong_ids_are_refused() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    for area in ["wp_inactive_widgets", "array_version"] {
        assert!(matches!(
            set_sidebar_widgets(db, area, &["text-2"]).await,
            Err(WidgetError::InvalidSidebarId)
        ));
    }
    assert!(matches!(
        set_sidebar_widgets(db, "footer", &["text-99999999999999999999"]).await,
        Err(WidgetError::InvalidWidgetId)
    ));
}
