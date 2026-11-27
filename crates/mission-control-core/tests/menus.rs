//! Navigation menus, stored as WordPress stores them: a menu is a `nav_menu` term, its items are
//! `nav_menu_item` posts with `_menu_item_*` meta, and the theme's locations are assigned in the
//! `theme_mods_{stylesheet}` option. Then the tree `wp_nav_menu` would render for a request.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::fs;

use chrono::{FixedOffset, NaiveDate};
use mission_control_core::content::{
    NewPost, NewTerm, PermalinkStructure, Post, PostStatus, Taxonomy, Term, get_option,
    get_post_meta, set_option,
};
use mission_control_core::front::{Query, Resolved, resolve};
use mission_control_core::migrations::migrator;
use mission_control_core::theme::{
    MenuError, MenuItemInput, MenuItemObject, NAV_MENU_ITEM, NAV_MENU_OPTIONS, NavMenu,
    NavMenuItem, NewMenuItem, PageRequest, Theme, add_menu_item, auto_add_menus, create_nav_menu,
    load_theme, menu_items, menu_locations, nav_menu, save_nav_menu_with, set_menu_location,
    theme_menus,
};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use serde_json::json;

const HOME: &str = "https://example.com";

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
    set_option(db, "permalink_structure", &json!("/%postname%/"))
        .await
        .unwrap();
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the author id")
}

/// A theme with a `primary` and a `footer` menu location, in a temporary folder.
fn theme() -> (tempfile::TempDir, Theme) {
    let dir = tempfile::tempdir().unwrap();
    let theme_dir = dir.path().join("menus-theme");
    fs::create_dir_all(&theme_dir).unwrap();
    fs::write(
        theme_dir.join("theme.toml"),
        "name = \"Menus\"\n[menus]\nprimary = \"Primary\"\nfooter = \"Footer\"\n",
    )
    .unwrap();
    fs::write(theme_dir.join("index.propel"), "").unwrap();
    let theme = load_theme(&theme_dir).unwrap();
    (dir, theme)
}

async fn page(db: &Database, author: i64, title: &str, slug: &str, status: PostStatus) -> Post {
    page_under(db, author, title, slug, status, None).await
}

async fn page_under(
    db: &Database,
    author: i64,
    title: &str,
    slug: &str,
    status: PostStatus,
    parent: Option<&Post>,
) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: title.to_owned(),
            slug: slug.to_owned(),
            parent_id: parent.map(|p| p.id),
            published_at: Some(
                NaiveDate::from_ymd_opt(2026, 3, 7)
                    .unwrap()
                    .and_hms_opt(8, 0, 0)
                    .unwrap(),
            ),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .expect("creates the page")
}

async fn item(db: &Database, author: i64, menu: &Term, new: NewMenuItem) -> Post {
    add_menu_item(db, menu, author, new)
        .await
        .expect("adds the menu item")
}

/// The menu as it renders for the request `url` (a path, with an optional query string).
async fn menu_for(db: &Database, menu: &Term, url: &str) -> NavMenu {
    let structure: PermalinkStructure = "/%postname%/".parse().unwrap();
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let resolved = resolve(db, path, &Query::parse(query), &structure)
        .await
        .unwrap();
    let request = request(&resolved, path, query, &structure);
    nav_menu(db, menu.term_id, &request)
        .await
        .expect("reads the menu")
        .expect("the menu exists")
}

fn request<'a>(
    resolved: &'a Resolved,
    path: &'a str,
    query: &'a str,
    structure: &'a PermalinkStructure,
) -> PageRequest<'a> {
    PageRequest {
        resolved,
        home: HOME,
        path,
        query,
        structure,
        utc_offset: FixedOffset::east_opt(0).unwrap(),
        search_query: "",
    }
}

fn titles(items: &[NavMenuItem]) -> Vec<&str> {
    items.iter().map(|i| i.title.as_str()).collect()
}

fn has_class(item: &NavMenuItem, class: &str) -> bool {
    item.classes.split(' ').any(|c| c == class)
}

#[tokio::test]
async fn a_menu_is_a_nav_menu_term_and_its_items_are_posts_with_wordpress_meta() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let about = page(db, author, "About", "about", PostStatus::Publish).await;

    let menu = create_nav_menu(db, "Main menu").await.unwrap();
    assert_eq!(menu.taxonomy, "nav_menu");
    assert_eq!(menu.slug, "main-menu");
    assert_eq!(Taxonomy::NAV_MENU.name, "nav_menu");
    const { assert!(!Taxonomy::NAV_MENU.hierarchical) };
    assert_eq!(Taxonomy::builtin("nav_menu"), Some(Taxonomy::NAV_MENU));

    let home = item(
        db,
        author,
        &menu,
        NewMenuItem {
            classes: vec!["home-link".to_owned(), "bad class!".to_owned()],
            target: "_blank".to_owned(),
            xfn: "me".to_owned(),
            ..NewMenuItem::custom("Home", "https://example.com/")
        },
    )
    .await;
    let about_item = item(
        db,
        author,
        &menu,
        NewMenuItem {
            parent: Some(home.id),
            ..NewMenuItem::post(&about)
        },
    )
    .await;

    // The items are posts of type nav_menu_item, published, ordered by menu_order.
    assert_eq!(home.post_type, NAV_MENU_ITEM);
    assert_eq!(home.status, PostStatus::Publish);
    assert_eq!(home.title, "Home");
    assert_eq!(home.menu_order, 1);
    assert_eq!(about_item.menu_order, 2);
    assert_eq!(
        about_item.title, "",
        "an object's item keeps no title of its own"
    );

    let meta = async |id: i64, key: &str| get_post_meta(db, id, key).await.unwrap();
    assert_eq!(
        meta(home.id, "_menu_item_type").await,
        Some(json!("custom"))
    );
    assert_eq!(
        meta(home.id, "_menu_item_object").await,
        Some(json!("custom"))
    );
    assert_eq!(
        meta(home.id, "_menu_item_object_id").await,
        Some(json!(home.id.to_string()))
    );
    assert_eq!(
        meta(home.id, "_menu_item_url").await,
        Some(json!("https://example.com/"))
    );
    assert_eq!(
        meta(home.id, "_menu_item_menu_item_parent").await,
        Some(json!("0"))
    );
    assert_eq!(
        meta(home.id, "_menu_item_target").await,
        Some(json!("_blank"))
    );
    assert_eq!(
        meta(home.id, "_menu_item_classes").await,
        Some(json!(["home-link", "bad", "class"]))
    );
    assert_eq!(meta(home.id, "_menu_item_xfn").await, Some(json!("me")));

    assert_eq!(
        meta(about_item.id, "_menu_item_type").await,
        Some(json!("post_type"))
    );
    assert_eq!(
        meta(about_item.id, "_menu_item_object").await,
        Some(json!("page"))
    );
    assert_eq!(
        meta(about_item.id, "_menu_item_object_id").await,
        Some(json!(about.id.to_string()))
    );
    assert_eq!(meta(about_item.id, "_menu_item_url").await, Some(json!("")));
    assert_eq!(
        meta(about_item.id, "_menu_item_menu_item_parent").await,
        Some(json!(home.id.to_string()))
    );

    // Both items are related to the menu term.
    let terms = about_item.terms(db, &Taxonomy::NAV_MENU).await.unwrap();
    assert_eq!(terms.len(), 1);
    assert_eq!(terms[0].term_taxonomy_id, menu.term_taxonomy_id);
    let menu = Term::find(db, menu.term_taxonomy_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(menu.count, 2);
}

#[tokio::test]
async fn menu_names_are_unique_and_items_need_a_menu_and_a_parent_in_it() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let menu = create_nav_menu(db, "Main").await.unwrap();
    let other = create_nav_menu(db, "Other").await.unwrap();
    assert!(matches!(
        create_nav_menu(db, "main").await,
        Err(MenuError::Term(_))
    ));

    let category = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    assert!(matches!(
        add_menu_item(db, &category, author, NewMenuItem::custom("X", "/")).await,
        Err(MenuError::NotAMenu)
    ));

    let in_other = item(db, author, &other, NewMenuItem::custom("Other", "/")).await;
    let result = add_menu_item(
        db,
        &menu,
        author,
        NewMenuItem {
            parent: Some(in_other.id),
            ..NewMenuItem::custom("Child", "/child/")
        },
    )
    .await;
    assert!(
        matches!(result, Err(MenuError::InvalidParent)),
        "{result:?}"
    );
}

#[tokio::test]
async fn locations_are_assigned_in_the_theme_mods_option() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (_dir, theme) = theme();
    let menu = create_nav_menu(db, "Main").await.unwrap();
    set_option(
        db,
        "theme_mods_menus-theme",
        &json!({"custom_logo": 12, "nav_menu_locations": {"footer": 99}}),
    )
    .await
    .unwrap();

    set_menu_location(db, &theme, "primary", Some(&menu))
        .await
        .unwrap();

    let mods = get_option(db, "theme_mods_menus-theme")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        mods,
        json!({
            "custom_logo": 12,
            "nav_menu_locations": {"footer": 99, "primary": menu.term_id}
        })
    );
    let locations = menu_locations(db, theme.slug()).await.unwrap();
    assert_eq!(locations.get("primary"), Some(&menu.term_id));

    set_menu_location(db, &theme, "footer", None).await.unwrap();
    let locations = menu_locations(db, theme.slug()).await.unwrap();
    assert_eq!(locations.get("footer"), None);
    assert_eq!(locations.get("primary"), Some(&menu.term_id));

    let result = set_menu_location(db, &theme, "sidebar", Some(&menu)).await;
    assert!(
        matches!(result, Err(MenuError::UnknownLocation { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn the_menu_renders_as_a_tree_with_links_titles_and_current_classes() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let about = page(db, author, "About us", "about", PostStatus::Publish).await;
    let team = page_under(
        db,
        author,
        "Team",
        "team",
        PostStatus::Publish,
        Some(&about),
    )
    .await;
    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();

    let menu = create_nav_menu(db, "Main").await.unwrap();
    let home = item(
        db,
        author,
        &menu,
        NewMenuItem::custom("Home", "https://example.com/"),
    )
    .await;
    let about_item = item(
        db,
        author,
        &menu,
        NewMenuItem {
            title: "About".to_owned(),
            ..NewMenuItem::post(&about)
        },
    )
    .await;
    let team_item = item(
        db,
        author,
        &menu,
        NewMenuItem {
            parent: Some(about_item.id),
            classes: vec!["highlight".to_owned()],
            ..NewMenuItem::post(&team)
        },
    )
    .await;
    let news_item = item(db, author, &menu, NewMenuItem::term(&news)).await;

    let rendered = menu_for(db, &menu, "/about/team/").await;
    assert_eq!(rendered.name, "Main");
    assert_eq!(titles(&rendered.items), ["Home", "About", "News"]);
    let about_view = &rendered.items[1];
    assert_eq!(about_view.id, about_item.id);
    assert_eq!(about_view.url, "https://example.com/about/");
    assert_eq!(titles(&about_view.children), ["Team"]);
    let team_view = &about_view.children[0];
    assert_eq!(team_view.id, team_item.id);
    // The object's title, when the item has none of its own.
    assert_eq!(team_view.title, "Team");
    assert_eq!(team_view.url, "https://example.com/about/team/");
    assert!(team_view.current);
    assert_eq!(team_view.aria_current, "page");
    for class in [
        "highlight",
        "menu-item",
        "menu-item-type-post_type",
        "menu-item-object-page",
        "current-menu-item",
        "page_item",
        "current_page_item",
    ] {
        assert!(
            has_class(team_view, class),
            "{class}: {}",
            team_view.classes
        );
    }
    assert!(
        team_view
            .classes
            .ends_with(&format!("menu-item-{}", team_item.id)),
        "{}",
        team_view.classes
    );
    assert!(team_view.classes.starts_with("highlight "));
    assert!(!about_view.current);
    for class in [
        "current-menu-ancestor",
        "current-menu-parent",
        "current_page_parent",
        "current_page_ancestor",
        "menu-item-has-children",
    ] {
        assert!(
            has_class(about_view, class),
            "{class}: {}",
            about_view.classes
        );
    }
    assert!(!has_class(about_view, "current-menu-item"));
    let home_view = &rendered.items[0];
    assert_eq!(home_view.id, home.id);
    assert!(has_class(home_view, "menu-item-home"));
    assert!(!home_view.current);
    let news_view = &rendered.items[2];
    assert_eq!(news_view.id, news_item.id);
    assert_eq!(news_view.url, "https://example.com/category/news/");
    assert!(has_class(news_view, "menu-item-type-taxonomy"));
    assert!(has_class(news_view, "menu-item-object-category"));

    // On the front page, the home link is the current item.
    let rendered = menu_for(db, &menu, "/").await;
    assert!(rendered.items[0].current);
    assert!(has_class(&rendered.items[0], "current-menu-item"));
    assert!(!rendered.items[1].current);
    // On the category archive, the term's item is.
    let rendered = menu_for(db, &menu, "/category/news/").await;
    assert!(rendered.items[2].current);
    assert!(has_class(&rendered.items[2], "current-menu-item"));
}

#[tokio::test]
async fn a_custom_url_with_a_script_scheme_is_neutralised() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let menu = create_nav_menu(db, "Main").await.unwrap();
    for (title, url) in [
        ("Script", "javascript:alert(1)"),
        ("Tabbed", "java\tscript:alert(1)"),
        ("Upper", " JAVASCRIPT:alert(1)"),
        ("Data", "data:text/html,<script>alert(1)</script>"),
        ("Vb", "vbscript:msgbox"),
        ("Web", "https://rocketboost.rs/docs?a=1#top"),
        ("Plain", "http://example.org"),
        ("Mail", "mailto:ann@example.com"),
        ("Phone", "tel:+15551234"),
        ("Relative", "/contact/"),
        ("Fragment", "#main"),
        ("Query", "?s=rocket"),
    ] {
        item(db, author, &menu, NewMenuItem::custom(title, url)).await;
    }
    // The URL is stored empty (`esc_url_raw`); one written straight to the meta, as an importer
    // could, is neutralised when the menu renders.
    let script = Post::of_type(NAV_MENU_ITEM)
        .where_eq("title", "Script")
        .first(db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        get_post_meta(db, script.id, "_menu_item_url")
            .await
            .unwrap(),
        Some(json!(""))
    );
    let imported = item(db, author, &menu, NewMenuItem::custom("Imported", "/x/")).await;
    mission_control_core::content::set_post_meta(
        db,
        imported.id,
        "_menu_item_url",
        &json!("javascript:alert(document.cookie)"),
    )
    .await
    .unwrap();

    let rendered = menu_for(db, &menu, "/").await;
    let urls: Vec<(&str, &str)> = rendered
        .items
        .iter()
        .map(|i| (i.title.as_str(), i.url.as_str()))
        .collect();
    assert_eq!(
        urls,
        [
            ("Script", ""),
            ("Tabbed", ""),
            ("Upper", ""),
            ("Data", ""),
            ("Vb", ""),
            ("Web", "https://rocketboost.rs/docs?a=1#top"),
            ("Plain", "http://example.org"),
            ("Mail", "mailto:ann@example.com"),
            ("Phone", "tel:+15551234"),
            ("Relative", "/contact/"),
            ("Fragment", "#main"),
            ("Query", "?s=rocket"),
            ("Imported", ""),
        ]
    );
}

#[tokio::test]
async fn items_whose_object_is_not_published_are_skipped() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let draft = page(db, author, "Draft", "draft", PostStatus::Draft).await;
    let private = page(db, author, "Private", "private", PostStatus::Private).await;
    let mut trashed = page(db, author, "Trashed", "trashed", PostStatus::Publish).await;
    let shown = page(db, author, "Shown", "shown", PostStatus::Publish).await;
    let menu = create_nav_menu(db, "Main").await.unwrap();
    let draft_item = item(db, author, &menu, NewMenuItem::post(&draft)).await;
    item(db, author, &menu, NewMenuItem::post(&private)).await;
    item(db, author, &menu, NewMenuItem::post(&trashed)).await;
    item(db, author, &menu, NewMenuItem::post(&shown)).await;
    // A child of a skipped item is shown at the top level, as WordPress's walker shows orphans.
    item(
        db,
        author,
        &menu,
        NewMenuItem {
            parent: Some(draft_item.id),
            ..NewMenuItem::custom("Orphan", "/orphan/")
        },
    )
    .await;
    // An item pointing at a term that is gone, and one whose own row is a draft (a pending
    // change), are skipped too.
    let gone = Term::create(db, &Taxonomy::POST_TAG, NewTerm::new("Gone"))
        .await
        .unwrap();
    item(db, author, &menu, NewMenuItem::term(&gone)).await;
    gone.delete(db).await.unwrap();
    let mut pending = item(db, author, &menu, NewMenuItem::custom("Pending", "/p/")).await;
    pending.status = PostStatus::Draft;
    pending.save(db).await.unwrap();
    trashed.trash(db).await.unwrap();

    let rendered = menu_for(db, &menu, "/").await;
    assert_eq!(titles(&rendered.items), ["Shown", "Orphan"]);
}

#[tokio::test]
async fn parent_cycles_and_deep_nesting_never_loop() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let menu = create_nav_menu(db, "Main").await.unwrap();
    let a = item(db, author, &menu, NewMenuItem::custom("A", "/a/")).await;
    let b = item(
        db,
        author,
        &menu,
        NewMenuItem {
            parent: Some(a.id),
            ..NewMenuItem::custom("B", "/b/")
        },
    )
    .await;
    // Make A a child of B: a cycle, written as an importer could.
    mission_control_core::content::set_post_meta(
        db,
        a.id,
        "_menu_item_menu_item_parent",
        &json!(b.id.to_string()),
    )
    .await
    .unwrap();
    let rendered = menu_for(db, &menu, "/").await;
    let mut all = Vec::new();
    fn walk<'a>(items: &'a [NavMenuItem], out: &mut Vec<&'a str>) {
        for item in items {
            out.push(&item.title);
            walk(&item.children, out);
        }
    }
    walk(&rendered.items, &mut all);
    all.sort_unstable();
    assert_eq!(all, ["A", "B"], "each item shows once");
}

#[tokio::test]
async fn theme_menus_has_every_location_with_null_for_an_unassigned_one() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (_dir, theme) = theme();
    let menu = create_nav_menu(db, "Main").await.unwrap();
    item(db, author, &menu, NewMenuItem::custom("Home", "/")).await;
    set_menu_location(db, &theme, "primary", Some(&menu))
        .await
        .unwrap();

    let structure: PermalinkStructure = "/%postname%/".parse().unwrap();
    let resolved = resolve(db, "/", &Query::parse(""), &structure)
        .await
        .unwrap();
    let menus = theme_menus(db, &theme, &request(&resolved, "/", "", &structure))
        .await
        .unwrap();
    assert_eq!(menus.len(), 2);
    assert_eq!(
        menus.get("primary").unwrap().as_ref().map(|m| m.id),
        Some(menu.term_id)
    );
    assert!(menus.get("footer").unwrap().is_none());

    // A location assigned a menu that was deleted is empty too.
    menu.delete(db).await.unwrap();
    let menus = theme_menus(db, &theme, &request(&resolved, "/", "", &structure))
        .await
        .unwrap();
    assert!(menus.get("primary").unwrap().is_none());
}

#[tokio::test]
async fn a_chain_deeper_than_the_cap_is_shown_flat_past_it() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let menu = create_nav_menu(db, "Deep").await.unwrap();
    let depth = mission_control_core::theme::MAX_MENU_DEPTH + 4;
    let mut parent = None;
    for level in 0..depth {
        let item = item(
            db,
            author,
            &menu,
            NewMenuItem {
                parent,
                ..NewMenuItem::custom(&format!("L{level}"), &format!("/l{level}/"))
            },
        )
        .await;
        parent = Some(item.id);
    }

    let rendered = menu_for(db, &menu, "/").await;
    fn deepest(items: &[NavMenuItem]) -> usize {
        items
            .iter()
            .map(|i| 1 + deepest(&i.children))
            .max()
            .unwrap_or(0)
    }
    fn count(items: &[NavMenuItem]) -> usize {
        items.iter().map(|i| 1 + count(&i.children)).sum()
    }
    assert_eq!(
        deepest(&rendered.items),
        mission_control_core::theme::MAX_MENU_DEPTH
    );
    assert_eq!(count(&rendered.items), depth, "each item shows once");
    // The first item holds the capped tree; the rest follow at the top level, in order.
    assert_eq!(rendered.items.len(), 1 + 4);
    assert_eq!(rendered.items[1].title, format!("L{}", depth - 4));
}

/// "Automatically add new top-level pages to this menu" (`nav_menu_options`' `auto_add`, as
/// `wp_nav_menu_update_menu_items` saves it): a page with no parent that becomes `publish` is
/// added at the end of the menu, once, as `_wp_auto_add_pages_to_menu` does; a draft, a child
/// page, a post and a page already in the menu are not.
#[tokio::test]
async fn new_top_level_pages_are_added_to_the_menus_that_ask_for_them() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let menu = create_nav_menu(db, "Main").await.unwrap();
    let other = create_nav_menu(db, "Other").await.unwrap();
    let about = page(db, author, "About", "about", PostStatus::Publish).await;
    let rows = [MenuItemInput {
        id: None,
        object: MenuItemObject::Post {
            post_type: "page".into(),
            id: about.id,
        },
        title: String::new(),
        parent: None,
        target: String::new(),
        classes: Vec::new(),
        attr_title: Some("About us".into()),
        description: Some("Who we are".into()),
        xfn: Some("me friend!".into()),
    }];
    save_nav_menu_with(db, &menu, author, Some(&rows), None, Some(true))
        .await
        .unwrap();
    assert_eq!(auto_add_menus(db).await.unwrap(), [menu.term_id]);
    let stored = menu_items(db, &menu).await.unwrap();
    assert_eq!(stored[0].attr_title, "About us");
    assert_eq!(stored[0].description, "Who we are");
    assert_eq!(stored[0].xfn, "me friend");

    // A row without them keeps them.
    let kept = [MenuItemInput {
        id: Some(stored[0].id),
        attr_title: None,
        description: None,
        xfn: None,
        ..rows[0].clone()
    }];
    save_nav_menu_with(db, &menu, author, Some(&kept), None, None)
        .await
        .unwrap();
    let stored = menu_items(db, &menu).await.unwrap();
    assert_eq!(stored[0].attr_title, "About us");
    assert_eq!(stored[0].xfn, "me friend");

    let mut draft = page(db, author, "Draft", "draft", PostStatus::Draft).await;
    let _child = page_under(
        db,
        author,
        "Team",
        "team",
        PostStatus::Publish,
        Some(&about),
    )
    .await;
    Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "A post".into(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();
    let contact = page(db, author, "Contact", "contact", PostStatus::Publish).await;
    let objects = |items: &[mission_control_core::theme::StoredMenuItem]| {
        items
            .iter()
            .map(|item| item.object.clone())
            .collect::<Vec<_>>()
    };
    let page_of = |id: i64| MenuItemObject::Post {
        post_type: "page".into(),
        id,
    };
    assert_eq!(
        objects(&menu_items(db, &menu).await.unwrap()),
        [page_of(about.id), page_of(contact.id)]
    );
    assert!(menu_items(db, &other).await.unwrap().is_empty());

    // Publishing the draft adds it; saving it again does not add it twice.
    draft.status = PostStatus::Publish;
    draft.save(db).await.unwrap();
    draft.title = "Drafted".into();
    draft.save(db).await.unwrap();
    let items = menu_items(db, &menu).await.unwrap();
    assert_eq!(
        objects(&items),
        [page_of(about.id), page_of(contact.id), page_of(draft.id)]
    );
    assert_eq!(items[2].parent, None);
    assert_eq!(items[2].title, "", "the item shows the page's title");

    // Turned off, nothing more is added.
    save_nav_menu_with(db, &menu, author, None, None, Some(false))
        .await
        .unwrap();
    assert!(auto_add_menus(db).await.unwrap().is_empty());
    assert_eq!(
        get_option(db, NAV_MENU_OPTIONS).await.unwrap(),
        Some(json!({"auto_add": []}))
    );
    page(db, author, "Later", "later", PostStatus::Publish).await;
    assert_eq!(menu_items(db, &menu).await.unwrap().len(), 3);
}
