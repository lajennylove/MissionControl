//! The Menus screen's menu builder (WordPress's `nav-menus.php`): the "Add menu items" column
//! and its quick search, the rows the builder writes into the items field (pages, a custom link,
//! nested items, a new item under a new item, an item moved up and made a sub item), the Menu
//! Settings (theme location, "Automatically add new top-level pages"), Delete Menu, and a save
//! refused without a name.
//!
//! The builder's script runs in the browser; these tests post the rows it writes, read them back
//! from the edit form (what the builder shows), and check what is stored and rendered.
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so the flow runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of its own; otherwise
//! it says why it is skipped and passes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, get_option, set_option,
};
use mission_control_core::theme::{
    MenuItemObject, NAV_MENU_OPTIONS, StoredMenuItem, get_nav_menu, menu_items, menu_locations,
};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::Database;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// Held by every test here: the flow sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

const PASSWORD: &str = "correct horse battery staple";
const HOME: &str = "http://localhost";

fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
}

fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

fn config(database_url: &str) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: shipped_themes(),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment: Environment::Local,
        trusted_proxies: TrustedProxies::None,
    }
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, NON_ALPHANUMERIC),
                utf8_percent_encode(v, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn token_in(html: &str) -> String {
    let start = html
        .find("name=\"_token\" value=\"")
        .unwrap_or_else(|| panic!("a form token in: {html}"))
        + "name=\"_token\" value=\"".len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

async fn post<'c>(client: &'c Client, uri: &str, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(fields))
        .dispatch()
        .await
}

/// Posts `fields` and answers the status and the body.
async fn send(client: &Client, uri: &str, fields: &[(&str, &str)]) -> (Status, String) {
    let response = post(client, uri, fields).await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

async fn sign_in(client: &Client, email: &str) {
    let (_, html) = get(client, "/mc-admin/login").await;
    let csrf = token_in(&html);
    let response = post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
}

async fn user(db: &Database, name: &str, role: &str) -> (i64, String) {
    let email = format!("{name}@example.test");
    let created = users::create_user(db, name, &email, PASSWORD)
        .await
        .expect("creates the user");
    authz::assign_role(db, &email, role)
        .await
        .expect("gives the role");
    (created.id, email)
}

async fn page(db: &Database, author: i64, title: &str, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: title.to_owned(),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .expect("creates a page")
}

/// The JSON the items field holds on a form page: the builder's hidden input (HTML-escaped in
/// its value). The page has no JSON textarea.
fn items_field(html: &str) -> Vec<Value> {
    assert!(
        !html.contains("<textarea"),
        "no textarea for the items: {html}"
    );
    let marker = "<input type=\"hidden\" id=\"items\" name=\"items\" value=\"";
    let start = html
        .find(marker)
        .map(|at| at + marker.len())
        .unwrap_or_else(|| panic!("the items field in: {html}"));
    let end = html[start..].find('"').unwrap() + start;
    let text = html[start..end]
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// The (item id, parent id) of each stored item, in order.
fn structure(items: &[StoredMenuItem]) -> Vec<(i64, Option<i64>)> {
    items.iter().map(|item| (item.id, item.parent)).collect()
}

async fn quick_search(client: &Client, query: &str) -> (Status, Value) {
    let (status, body) = get(client, &format!("/mc-admin/nav-menus/quick-search?{query}")).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}")),
    )
}

fn titles(found: &Value) -> Vec<String> {
    found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["title"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_menu_builder_creates_nests_moves_and_assigns_menus() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(_) = mysql_test_url() else {
        eprintln!(
            "skipped: the admin runs on MySQL or MariaDB only (Ruling R119); set \
             MC_TEST_DATABASE_URL to a mysql:// URL to run this test"
        );
        return;
    };
    let scratch = rocket::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scratch_database("menus_builder"));
    // SAFETY: no other thread reads or writes the environment now. Every test of this process
    // holds `LOCK`, and the runtime above is gone; the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(flow(&scratch));
    // SAFETY: as above: the runtime of the flow has ended, and `LOCK` is held.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

async fn flow(scratch: &ScratchDatabase) {
    run_migrations(scratch.url()).await.expect("migrates");
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
    for (name, value) in [
        ("blogname", json!("Menus Test")),
        ("home", json!(HOME)),
        ("siteurl", json!(HOME)),
        ("permalink_structure", json!("/%postname%/")),
    ] {
        set_option(&db, name, &value).await.unwrap();
    }
    let client = Client::tracked(rocket_with_options(
        config(scratch.url()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    let (admin_id, admin) = user(&db, "ada", "administrator").await;
    let (_, editor) = user(&db, "edna", "editor").await;

    let about = page(&db, admin_id, "About", PostStatus::Publish).await;
    let contact = page(&db, admin_id, "Contact", PostStatus::Publish).await;
    let draft = page(&db, admin_id, "Draft page", PostStatus::Draft).await;
    for n in 1..=52 {
        page(&db, admin_id, &format!("Page {n:02}"), PostStatus::Publish).await;
    }
    let news = Term::create(&db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();

    sign_in(&client, &admin).await;
    builder_page(&client).await;
    item_sources(&client, &about, &contact, &draft).await;
    let menu_id = create_menu(&client, &db, &about, &contact).await;
    move_and_nest(&client, &db, menu_id, &news).await;
    refused_without_a_name(&client, &db, menu_id).await;
    auto_add_and_locations(&client, &db, admin_id, menu_id).await;
    switch_menus(&client, &db, menu_id).await;
    delete_menu(&client, &db, menu_id).await;

    // An editor may not list the boxes' items either.
    let (_, html) = get(&client, "/mc-admin").await;
    let csrf = token_in(&html);
    post(&client, "/mc-admin/logout", &[("_token", &csrf)]).await;
    sign_in(&client, &editor).await;
    let (status, _) = get(
        &client,
        "/mc-admin/nav-menus/quick-search?kind=post_type&object=page",
    )
    .await;
    assert_eq!(status, Status::Forbidden);
    db.close().await;
}

/// The New form: the "Add menu items" column (hidden until the script shows it), in WordPress's
/// order, the script and its stylesheet, the items field as the builder's hidden value (no raw
/// JSON textarea, nor Ignite 0.6.7's repeater sub-fields), and the Menu Settings.
async fn builder_page(client: &Client) {
    let (status, html) = get(client, "/mc-admin/nav-menus/new").await;
    assert_eq!(status, Status::Ok);
    assert!(
        html.contains("id=\"mc-menu-builder\" class=\"mc-menu-builder\" hidden"),
        "{html}"
    );
    let boxes = [
        "<summary>Pages</summary>",
        "<summary>Posts</summary>",
        "<summary>Custom Links</summary>",
        "<summary>Categories</summary>",
        "<summary>Tags</summary>",
    ];
    let mut last = 0;
    for summary in boxes {
        let at = html
            .find(summary)
            .unwrap_or_else(|| panic!("{summary}: {html}"));
        assert!(at > last, "{summary} in WordPress's order: {html}");
        last = at;
    }
    for text in [
        "Select All",
        "Add to Menu",
        "Link Text",
        "Load more",
        "/mc-includes/js/nav-menus.js?ver=",
        "/mc-includes/css/nav-menus.css?ver=",
        "<input type=\"hidden\" id=\"items\" name=\"items\" value=\"\">",
        "Automatically add new top-level pages to this menu",
        "loc_primary",
    ] {
        assert!(html.contains(text), "{text}: {html}");
    }
    assert!(!html.contains("Delete Menu"), "{html}");
    assert!(!html.contains("<textarea"), "{html}");
    assert!(!html.contains("items[0]"), "{html}");
    assert!(!html.contains("for=\"items\""), "{html}");
    // The script and the stylesheet are served.
    let script = html
        .split("src=\"")
        .find_map(|rest| {
            rest.starts_with("/mc-includes/js/nav-menus.js")
                .then(|| rest.split('"').next().unwrap().to_owned())
        })
        .expect("the script's URL");
    let response = client.get(script).dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        response.headers().get_one("Cache-Control"),
        Some("public, max-age=31536000, immutable")
    );
    let body = response.into_string().await.unwrap();
    assert!(body.contains("Out from under "), "the script");
    let (status, css) = get(client, "/mc-includes/css/nav-menus.css").await;
    assert_eq!(status, Status::Ok);
    assert!(css.contains(".mc-menu-columns"), "{css}");
}

/// The boxes' items: the published pages a page of 50 at a time, a search, the categories, the
/// titles of chosen ids whatever their status, and no other post type.
async fn item_sources(client: &Client, about: &Post, contact: &Post, draft: &Post) {
    let (status, found) = quick_search(client, "kind=post_type&object=page").await;
    assert_eq!(status, Status::Ok);
    let first = titles(&found);
    assert_eq!(first.len(), 50, "{found}");
    assert_eq!(found["more"], true);
    assert_eq!(first[0], "About", "pages by title");
    assert!(!first.contains(&"Draft page".to_owned()), "{found}");
    let (_, found) = quick_search(client, "kind=post_type&object=page&page=2").await;
    assert_eq!(titles(&found).len(), 4, "{found}");
    assert_eq!(found["more"], false);
    let (_, found) = quick_search(client, "kind=post_type&object=page&q=CONT").await;
    assert_eq!(titles(&found), ["Contact"]);
    assert_eq!(found["items"][0]["id"], contact.id);
    let (_, found) = quick_search(client, "kind=taxonomy&object=category").await;
    assert_eq!(titles(&found), ["News"]);
    let (_, found) = quick_search(
        client,
        &format!(
            "kind=post_type&object=page&include={},{},987654",
            about.id, draft.id
        ),
    )
    .await;
    let mut read: Vec<(String, String)> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["title"].as_str().unwrap().to_owned(),
                item["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    read.sort();
    assert_eq!(
        read,
        [
            ("About".to_owned(), "publish".to_owned()),
            ("Draft page".to_owned(), "draft".to_owned())
        ]
    );
    for refused in [
        "kind=post_type&object=nav_menu_item",
        "kind=post_type&object=attachment",
        "kind=custom&object=custom",
    ] {
        let (status, _) = quick_search(client, refused).await;
        assert_eq!(status, Status::NotFound, "{refused}");
    }
}

/// A menu made of two pages (Contact under About, both new: the parent is About's row number)
/// and a custom link with its settings, shown in the primary location.
async fn create_menu(client: &Client, db: &Database, about: &Post, contact: &Post) -> i64 {
    let rows = json!([
        {"id": "", "title": "", "type": "page", "object_id": about.id, "url": "", "parent": 0,
         "classes": "", "new_tab": false, "attr_title": "", "xfn": "", "description": ""},
        {"id": "", "title": "", "type": "page", "object_id": contact.id, "url": "",
         "parent": 0, "parent_row": 1, "classes": "", "new_tab": false, "attr_title": "",
         "xfn": "", "description": ""},
        {"id": "", "title": "Docs", "type": "custom", "object_id": "",
         "url": "https://rocketboost.rs/docs/", "parent": 0, "classes": "docs wide",
         "new_tab": true, "attr_title": "The docs", "xfn": "me", "description": "Read them"},
    ])
    .to_string();
    let (_, html) = get(client, "/mc-admin/nav-menus/new").await;
    let csrf = token_in(&html);
    let (status, body) = send(
        client,
        "/mc-admin/nav-menus",
        &[
            ("_token", &csrf),
            ("name", "Main"),
            ("loc_primary", "on"),
            ("items", &rows),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (_, list) = get(client, "/mc-admin/nav-menus").await;
    assert!(list.contains("Main"), "{list}");
    assert!(list.contains("Primary menu"), "{list}");
    let id = list
        .split("/mc-admin/nav-menus/")
        .filter_map(|rest| rest.split('/').next()?.parse::<i64>().ok())
        .next()
        .expect("the menu's edit link");
    let menu = get_nav_menu(db, id).await.unwrap().expect("the menu");
    let items = menu_items(db, &menu).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(
        items[0].object,
        MenuItemObject::Post {
            post_type: "page".into(),
            id: about.id
        }
    );
    assert_eq!(items[1].parent, Some(items[0].id), "Contact under About");
    assert_eq!(items[2].parent, None);
    assert_eq!(items[2].target, "_blank");
    assert_eq!(items[2].classes, ["docs", "wide"]);
    assert_eq!(items[2].attr_title, "The docs");
    assert_eq!(items[2].xfn, "me");
    assert_eq!(items[2].description, "Read them");
    assert_eq!(
        menu_locations(db, "default").await.unwrap().get("primary"),
        Some(&id)
    );

    // The edit form shows what was saved: the rows, each parent as its item's id, the settings
    // and Delete Menu.
    let (status, html) = get(client, &format!("/mc-admin/nav-menus/{id}/edit")).await;
    assert_eq!(status, Status::Ok);
    let shown = items_field(&html);
    assert_eq!(shown.len(), 3);
    assert_eq!(shown[1]["parent"], items[0].id);
    assert_eq!(shown[2]["xfn"], "me");
    assert_eq!(shown[2]["description"], "Read them");
    assert_eq!(shown[2]["attr_title"], "The docs");
    assert!(html.contains("id=\"mc-menu-builder\""), "{html}");
    assert!(
        html.contains(&format!("action=\"/mc-admin/nav-menus/{id}/delete\"")),
        "{html}"
    );
    assert!(html.contains(">Delete Menu</button>"), "{html}");

    // The site's header shows it nested.
    let (_, home) = get(client, "/").await;
    assert!(home.contains("id=\"menu-main\""), "{home}");
    let sub = home.find("class=\"sub-menu\"").expect("a sub menu");
    let contact_at = home.find(">Contact</a>").expect("Contact");
    assert!(contact_at > sub, "{home}");
    assert!(
        home.contains("title=\"The docs\"") && home.contains("rel=\"me\""),
        "{home}"
    );
    id
}

/// The builder's moves, as it writes them: Docs moved up one (above About and its sub item),
/// then About made a sub item of Docs (Under Docs), and the News category added under Docs (a
/// new item under an existing one). The saved structure is what was shown, and the edit form
/// shows it back.
async fn move_and_nest(client: &Client, db: &Database, id: i64, news: &Term) {
    let menu = get_nav_menu(db, id).await.unwrap().unwrap();
    let before = menu_items(db, &menu).await.unwrap();
    let (about, contact, docs) = (before[0].id, before[1].id, before[2].id);
    let (_, html) = get(client, &format!("/mc-admin/nav-menus/{id}/edit")).await;
    let csrf = token_in(&html);
    let mut shown = items_field(&html);
    // Up one: the Docs row first; Under Docs: About's parent is Docs, Contact stays under About.
    let docs_row = shown.remove(2);
    shown.insert(0, docs_row);
    shown[1]["parent"] = json!(docs);
    shown[1]["parent_row"] = json!(1);
    shown[2]["parent_row"] = json!(2);
    shown.push(
        json!({"id": "", "title": "", "type": "category", "object_id": news.term_id,
        "url": "", "parent": docs, "parent_row": 1, "classes": "", "new_tab": false,
        "attr_title": "", "xfn": "", "description": ""}),
    );
    let rows = Value::Array(shown).to_string();
    let (status, body) = send(
        client,
        &format!("/mc-admin/nav-menus/{id}"),
        &[
            ("_token", &csrf),
            ("name", "Main"),
            ("loc_primary", "on"),
            ("items", &rows),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let after = menu_items(db, &menu).await.unwrap();
    assert_eq!(after.len(), 4);
    let category = after[3].id;
    assert_eq!(
        structure(&after),
        [
            (docs, None),
            (about, Some(docs)),
            (contact, Some(about)),
            (category, Some(docs))
        ]
    );
    assert_eq!(
        after[3].object,
        MenuItemObject::Term {
            taxonomy: "category".into(),
            term_id: news.term_id
        }
    );
    let (_, html) = get(client, &format!("/mc-admin/nav-menus/{id}/edit")).await;
    let shown: Vec<(i64, i64)> = items_field(&html)
        .iter()
        .map(|row| (row["id"].as_i64().unwrap(), row["parent"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        shown,
        [(docs, 0), (about, docs), (contact, about), (category, docs)]
    );
    // The header follows: Docs first, About in its sub menu.
    let (_, home) = get(client, "/").await;
    let docs_at = home.find(">Docs</a>").expect("Docs");
    let about_at = home.find(">About</a>").expect("About");
    let news_at = home.find(">News</a>").expect("News");
    assert!(docs_at < about_at && about_at < news_at, "{home}");
}

/// A save without a name is refused with WordPress's message under the name, on the form with
/// the builder and the typed rows; nothing is created or changed.
async fn refused_without_a_name(client: &Client, db: &Database, id: i64) {
    let (_, html) = get(client, "/mc-admin/nav-menus/new").await;
    let csrf = token_in(&html);
    let rows =
        json!([{"id": "", "title": "Typed", "type": "custom", "url": "/typed/"}]).to_string();
    let (status, body) = send(
        client,
        "/mc-admin/nav-menus",
        &[("_token", &csrf), ("name", "  "), ("items", &rows)],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    let field = body
        .find("data-ignite-field=\"name\"")
        .expect("the name field");
    let message = body
        .find("Please enter a valid menu name.")
        .expect("WordPress's message");
    let next_field = body[field + 1..]
        .find("data-ignite-field=")
        .map_or(body.len(), |at| at + field + 1);
    assert!(
        message > field && message < next_field,
        "under the name: {body}"
    );
    assert!(body.contains("id=\"mc-menu-builder\""), "{body}");
    assert_eq!(items_field(&body)[0]["title"], "Typed");
    let (_, list) = get(client, "/mc-admin/nav-menus").await;
    assert!(!list.contains("Typed"), "{list}");

    // Editing: a blank name refuses the whole save; the menu keeps its name and items.
    let menu = get_nav_menu(db, id).await.unwrap().unwrap();
    let (status, body) = send(
        client,
        &format!("/mc-admin/nav-menus/{id}"),
        &[("_token", &csrf), ("name", ""), ("items", "[]")],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    assert!(body.contains("Please enter a valid menu name."), "{body}");
    assert!(body.contains("Delete Menu"), "{body}");
    let kept = get_nav_menu(db, id).await.unwrap().unwrap();
    assert_eq!(kept.name, "Main");
    assert_eq!(menu_items(db, &menu).await.unwrap().len(), 4);
}

/// The Menu Settings: "Automatically add new top-level pages to this menu" adds a new published
/// page at the end; the primary location moves to another menu when that one takes it.
async fn auto_add_and_locations(client: &Client, db: &Database, author: i64, id: i64) {
    let (_, html) = get(client, &format!("/mc-admin/nav-menus/{id}/edit")).await;
    let csrf = token_in(&html);
    let rows = Value::Array(items_field(&html)).to_string();
    let (status, body) = send(
        client,
        &format!("/mc-admin/nav-menus/{id}"),
        &[
            ("_token", &csrf),
            ("name", "Main"),
            ("auto_add", "on"),
            ("loc_primary", "on"),
            ("items", &rows),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(
        get_option(db, NAV_MENU_OPTIONS).await.unwrap(),
        Some(json!({"auto_add": [id]}))
    );
    let (_, html) = get(client, &format!("/mc-admin/nav-menus/{id}/edit")).await;
    let checked = html
        .find("name=\"auto_add\" value=\"on\" checked")
        .is_some();
    assert!(checked, "{html}");
    let careers = page(db, author, "Careers", PostStatus::Publish).await;
    let menu = get_nav_menu(db, id).await.unwrap().unwrap();
    let items = menu_items(db, &menu).await.unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(
        items[4].object,
        MenuItemObject::Post {
            post_type: "page".into(),
            id: careers.id
        }
    );
    assert_eq!(items[4].parent, None);

    // A second menu takes the primary location: the list shows it there, with its item count.
    let (status, body) = send(
        client,
        "/mc-admin/nav-menus",
        &[
            ("_token", &csrf),
            ("name", "Footer links"),
            ("loc_primary", "on"),
            (
                "items",
                &json!([{"title": "Home", "type": "custom", "url": "/"}]).to_string(),
            ),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (_, list) = get(client, "/mc-admin/nav-menus").await;
    // The table's rows (the dropdown above the table names the menus too).
    let list = &list[list.find("<tbody").expect("the table")..];
    let footer_row = list
        .split("<tr")
        .find(|row| row.contains("Footer links"))
        .expect("the new menu's row");
    assert!(footer_row.contains("Primary menu"), "{footer_row}");
    assert!(footer_row.contains(">1<"), "{footer_row}");
    let main_row = list
        .split("<tr")
        .find(|row| row.contains(">Main<"))
        .expect("the first menu's row");
    assert!(!main_row.contains("Primary menu"), "{main_row}");
    assert!(main_row.contains(">5<"), "{main_row}");
}

/// `nav-menus.php`'s top of the screen: the list and each form show the "Edit Menus" and
/// "Create New Menu" tabs and "Select a menu to edit" with every menu; Select loads the chosen
/// menu's form with its items; the list links each menu to its edit form; Create New Menu opens
/// New, which creates a menu.
async fn switch_menus(client: &Client, db: &Database, main: i64) {
    let footer = Term::find_by_slug(db, &Taxonomy::NAV_MENU, "footer-links")
        .await
        .unwrap()
        .expect("the second menu")
        .term_id;
    let (status, list) = get(client, "/mc-admin/nav-menus").await;
    assert_eq!(status, Status::Ok);
    for text in [
        "Select a menu to edit:",
        "action=\"/mc-admin/nav-menus/switch\"",
        "href=\"/mc-admin/nav-menus/new\" class=\"mc-menu-tab\">Create New Menu</a>",
        "aria-current=\"page\">Edit Menus</a>",
        // Every menu, by name.
        &format!("<option value=\"{footer}\">Footer links</option>"),
        &format!("<option value=\"{main}\">Main</option>"),
        // Each row's edit link.
        &format!("href=\"/mc-admin/nav-menus/{footer}/edit\""),
        &format!("href=\"/mc-admin/nav-menus/{main}/edit\""),
    ] {
        assert!(list.contains(text), "{text}: {list}");
    }
    let footer_at = list.find("Footer links</option>").unwrap();
    let main_at = list.find(">Main</option>").unwrap();
    assert!(footer_at < main_at, "by name: {list}");

    // Select sends the browser to the chosen menu's form, which shows its items and has it
    // chosen in the dropdown.
    let response = client
        .get(format!("/mc-admin/nav-menus/switch?menu={footer}"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::SeeOther);
    let to = response.headers().get_one("Location").unwrap().to_owned();
    assert_eq!(to, format!("/mc-admin/nav-menus/{footer}/edit"));
    let (status, html) = get(client, &to).await;
    assert_eq!(status, Status::Ok);
    assert!(
        html.contains(&format!(
            "<option value=\"{footer}\" selected>Footer links</option>"
        )),
        "{html}"
    );
    let rows = items_field(&html);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "Home");
    assert!(html.contains("value=\"Footer links\""), "{html}");
    // And back to the first menu.
    let response = client
        .get(format!("/mc-admin/nav-menus/switch?menu={main}"))
        .dispatch()
        .await;
    let to = response.headers().get_one("Location").unwrap().to_owned();
    let (_, html) = get(client, &to).await;
    assert_eq!(items_field(&html).len(), 5);
    assert!(
        html.contains(&format!("<option value=\"{main}\" selected>Main</option>")),
        "{html}"
    );

    // Create New Menu: the New form, its tab current; it creates a menu, listed with the others.
    let (status, html) = get(client, "/mc-admin/nav-menus/new").await;
    assert_eq!(status, Status::Ok);
    assert!(
        html.contains("aria-current=\"page\">Create New Menu</a>"),
        "{html}"
    );
    assert!(html.contains("Select a menu to edit:"), "{html}");
    let csrf = token_in(&html);
    let (status, body) = send(
        client,
        "/mc-admin/nav-menus",
        &[("_token", &csrf), ("name", "Sidebar"), ("items", "[]")],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (_, list) = get(client, "/mc-admin/nav-menus").await;
    assert!(list.contains(">Sidebar</option>"), "{list}");
    // Without a menu, Select opens New.
    let response = client.get("/mc-admin/nav-menus/switch").dispatch().await;
    assert_eq!(
        response.headers().get_one("Location"),
        Some("/mc-admin/nav-menus/new")
    );
}

/// Delete Menu, the form under an existing menu's form, deletes the menu and its items.
async fn delete_menu(client: &Client, db: &Database, id: i64) {
    let (_, html) = get(client, &format!("/mc-admin/nav-menus/{id}/edit")).await;
    let action = format!("action=\"/mc-admin/nav-menus/{id}/delete\"");
    let at = html.find(&action).expect("the Delete Menu form");
    let csrf = token_in(&html[at..]);
    let (status, body) = send(
        client,
        &format!("/mc-admin/nav-menus/{id}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert!(get_nav_menu(db, id).await.unwrap().is_none());
}
