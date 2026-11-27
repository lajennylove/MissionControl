//! The defects end-to-end QA found in the admin, each answered as WordPress answers it:
//!
//! 1. The default category is never deleted (`wp_delete_term`): no Delete on its row, for an
//!    administrator or a super-admin, and a direct request is refused (403) with "Sorry, you
//!    are not allowed to delete this item."; its posts keep it.
//! 2. Settings > Writing: a blank Default Post Category is refused on the field (422).
//! 3. A restored post's slug is made unique when another post took it meanwhile.
//! 4. A page or a category is not offered as its own parent, and a save naming it is refused on
//!    the field.
//! 5. The featured image is an image (`set_post_thumbnail`): a PDF is refused with "Invalid
//!    featured media ID.".
//! 6. A custom menu link's URL must be a link: "Please enter a valid URL.".
//! 7. The lost password form: an empty or malformed email is answered on the form (422).
//! 8. Settings > Reading: a static homepage needs a page, the posts page is not the same page,
//!    and "Blog pages show at most" below 1 is refused.
//! 9. A profile picture over 1 MB gets Ignite's message on the profile, not a bare 413.
//! 10. A new post's Status is Draft, with no blank choice.
//! 11. Parent Category has one "None", no "Select an option".
//! 12. A sticky post cannot have a password: refused with the REST API's message.
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so the flow runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of its own
//! ([`scratch_database`]) with the installer's migrations, dropped at the end; otherwise it says
//! why it is skipped and passes. Ignite reads its database from `DATABASE_URL`, so the flow sets
//! it for the process; every test here holds [`LOCK`] so none runs while it changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, get_option, set_option,
};
use mission_control_core::theme::{get_nav_menu, menu_items};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::{Database, Model};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::json;

/// Held by every test here: the flow sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
const PASSWORD: &str = "correct horse battery staple";

/// The site's address in the tests.
const HOME: &str = "http://localhost";

/// `MC_TEST_DATABASE_URL` when it names MySQL or MariaDB.
fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
}

/// The themes shipped with Mission Control.
fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

fn config(database_url: &str) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: 1024 * 1024,
        themes_dir: shipped_themes(),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment: Environment::Local,
        trusted_proxies: TrustedProxies::None,
    }
}

/// A form body, URL-encoded.
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

/// The CSRF token in a page's form.
fn token_in(html: &str) -> String {
    let start = html
        .find("name=\"_token\" value=\"")
        .unwrap_or_else(|| panic!("a form token in: {html}"))
        + "name=\"_token\" value=\"".len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
}

fn location(response: &LocalResponse<'_>) -> String {
    response
        .headers()
        .get_one("Location")
        .unwrap_or_default()
        .to_owned()
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

/// The session's CSRF token: from the dashboard when signed in, else from the sign-in page.
async fn token(client: &Client) -> String {
    let (status, html) = get(client, "/mc-admin").await;
    if status == Status::Ok {
        return token_in(&html);
    }
    token_in(&get(client, "/mc-admin/login").await.1)
}

/// Posts `fields` with the session's CSRF token: the status, the location and the body.
async fn post(client: &Client, uri: &str, fields: &[(&str, &str)]) -> (Status, String, String) {
    let csrf = token(client).await;
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(&all))
        .dispatch()
        .await;
    let status = response.status();
    let to = location(&response);
    (status, to, response.into_string().await.unwrap_or_default())
}

async fn sign_in(client: &Client, email: &str) {
    let (status, _, body) = post(
        client,
        "/mc-admin/login",
        &[("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "signs {email} in: {body}");
}

async fn sign_out(client: &Client) {
    let (status, _, _) = post(client, "/mc-admin/logout", &[]).await;
    assert_eq!(status, Status::SeeOther);
}

/// An Ignite user with the Ignite role `role`. Returns the id and the email.
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

/// Saves the New screen of `post_type` with `fields`; returns the new post's id.
async fn new_post(client: &Client, post_type: &str, fields: &[(&str, &str)]) -> i64 {
    let (status, to, body) = post(client, &format!("/mc-admin/{post_type}"), fields).await;
    assert_eq!(status, Status::SeeOther, "creates: {body}");
    to.strip_prefix(&format!("/mc-admin/{post_type}/"))
        .and_then(|rest| rest.strip_suffix("/edit"))
        .and_then(|id| id.parse().ok())
        .expect("redirects to the edit form")
}

async fn find(db: &Database, id: i64) -> Post {
    Post::query()
        .find(db, id)
        .await
        .expect("reads the post")
        .expect("the post")
}

/// The `<select>` named `name` in `html`, from its tag to `</select>`.
fn select<'h>(html: &'h str, name: &str) -> &'h str {
    let marker = format!("name=\"{name}\"");
    let at = html
        .find(&marker)
        .unwrap_or_else(|| panic!("a {name} field in: {html}"));
    let start = html[..at].rfind("<select").expect("a select");
    let end = at + html[at..].find("</select>").expect("its end");
    &html[start..end]
}

/// Asserts a refused form: 422 with `message` in the page.
fn assert_on_form(status: Status, body: &str, message: &str) {
    assert_eq!(status, Status::UnprocessableEntity, "{message}: {body}");
    assert!(body.contains(message), "{message}: {body}");
}

#[test]
fn the_admin_answers_as_wordpress_does() {
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
        .block_on(scratch_database("fixes_e2e"));
    // SAFETY: no other thread reads or writes the environment now. Every test of this process
    // holds `LOCK`, and the runtime above is gone; the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(flows(&scratch));
    // SAFETY: as above: the runtime of the flows has ended, and `LOCK` is held.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

async fn flows(scratch: &ScratchDatabase) {
    run_migrations(scratch.url()).await.expect("migrates");
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
    for (name, value) in [
        ("blogname", json!("Fixes Test")),
        ("home", json!(HOME)),
        ("siteurl", json!(HOME)),
    ] {
        set_option(&db, name, &value).await.unwrap();
    }
    let uncategorized = Term::create(&db, &Taxonomy::CATEGORY, NewTerm::new("Uncategorized"))
        .await
        .expect("creates the default category");
    set_option(&db, "default_category", &json!(uncategorized.term_id))
        .await
        .unwrap();
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
    let (_, root) = user(&db, "root", authz::SUPER_ADMIN).await;

    lost_password_needs_an_email(&client).await;

    sign_in(&client, &admin).await;
    default_category_is_kept(&client, &db, &uncategorized).await;
    writing_needs_a_default_category(&client, &db, &uncategorized).await;
    restore_makes_the_slug_unique(&client, &db).await;
    nothing_is_its_own_parent(&client, &db).await;
    featured_image_is_an_image(&client, &db, admin_id).await;
    custom_links_need_a_url(&client, &db).await;
    reading_checks_its_pages(&client, &db).await;
    profile_picture_too_large(&client).await;
    new_post_is_a_draft(&client, &db).await;
    sticky_posts_have_no_password(&client, &db).await;

    // The second QA round.
    new_posts_are_not_trashed(&client, &db).await;
    term_names_are_checked(&client, &db).await;
    reading_needs_a_homepage(&client, &db).await;
    settings_screens_have_their_names(&client).await;
    custom_links_match_wordpress_s_pattern(&client).await;
    custom_permalinks_need_a_tag(&client, &db).await;
    sign_out(&client).await;

    // A super-admin passes every policy, and the default category still has no Delete.
    sign_in(&client, &root).await;
    let (status, list) = get(&client, "/mc-admin/category").await;
    assert_eq!(status, Status::Ok, "{list}");
    assert!(
        !list.contains(&format!(
            "/mc-admin/category/{}/delete",
            uncategorized.term_id
        )),
        "{list}"
    );
    let (status, _, body) = post(
        &client,
        &format!("/mc-admin/category/{}/delete", uncategorized.term_id),
        &[],
    )
    .await;
    assert_eq!(status, Status::Forbidden, "{body}");
    assert!(
        body.contains("Sorry, you are not allowed to delete this item."),
        "{body}"
    );
    assert!(
        Term::find(&db, uncategorized.term_taxonomy_id)
            .await
            .unwrap()
            .is_some()
    );
    sign_out(&client).await;
    drop(client);
    db.close().await;
}

/// Defect 1: The default category: no Delete on its row, a direct delete refused with WordPress's
/// message, and its post keeps it. Another category is deleted.
async fn default_category_is_kept(client: &Client, db: &Database, uncategorized: &Term) {
    let id = uncategorized.term_id.to_string();
    let post_id = new_post(
        client,
        "post",
        &[
            ("title", "Filed"),
            ("status", "publish"),
            ("tax_category[]", &id),
        ],
    )
    .await;
    let (_, list) = get(client, "/mc-admin/category").await;
    assert!(
        !list.contains(&format!("/mc-admin/category/{id}/delete")),
        "{list}"
    );
    let (status, _, body) = post(client, &format!("/mc-admin/category/{id}/delete"), &[]).await;
    assert_eq!(status, Status::Forbidden, "{body}");
    assert!(
        body.contains("Sorry, you are not allowed to delete this item."),
        "{body}"
    );
    let kept = Term::find(db, uncategorized.term_taxonomy_id)
        .await
        .unwrap()
        .expect("the default category is kept");
    assert_eq!(kept.count, 1, "its post keeps it");
    assert_eq!(find(db, post_id).await.status, PostStatus::Publish);

    let other = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("Gone"))
        .await
        .unwrap();
    let (_, list) = get(client, "/mc-admin/category").await;
    assert!(
        list.contains(&format!("/mc-admin/category/{}/delete", other.term_id)),
        "{list}"
    );
    let (status, _, body) = post(
        client,
        &format!("/mc-admin/category/{}/delete", other.term_id),
        &[],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert!(
        Term::find(db, other.term_taxonomy_id)
            .await
            .unwrap()
            .is_none()
    );
}

/// Defect 2: Settings > Writing: a blank or unknown Default Post Category is refused on the field,
/// and the option is kept.
async fn writing_needs_a_default_category(client: &Client, db: &Database, uncategorized: &Term) {
    // A blank category is required; an id that is no category is not one of the choices.
    for (value, message) in [
        ("", "Default Post Category is required."),
        ("987654", "Choose one of the options."),
    ] {
        let (status, _, body) = post(
            client,
            "/mc-admin/options-writing/1",
            &[("default_category", value)],
        )
        .await;
        assert_on_form(status, &body, message);
        assert!(!body.contains("Changes saved."), "{body}");
        assert_eq!(
            get_option(db, "default_category").await.unwrap(),
            Some(json!(uncategorized.term_id)),
            "{value:?}"
        );
    }
}

/// Defect 3: A restored post whose slug another post took meanwhile gets the next free one.
async fn restore_makes_the_slug_unique(client: &Client, db: &Database) {
    let a = new_post(
        client,
        "post",
        &[
            ("title", "Custom slug"),
            ("slug", "custom-slug-edited"),
            ("status", "publish"),
        ],
    )
    .await;
    let (status, _, _) = post(
        client,
        &format!("/mc-admin/post/{a}/actions/trash"),
        &[("_back", "/mc-admin/post")],
    )
    .await;
    assert_eq!(status, Status::SeeOther);
    let b = new_post(
        client,
        "post",
        &[("title", "Custom Slug Edited"), ("status", "publish")],
    )
    .await;
    assert_eq!(find(db, b).await.slug, "custom-slug-edited");
    let (status, _, _) = post(
        client,
        &format!("/mc-admin/post/{a}/actions/untrash"),
        &[("_back", "/mc-admin/post?tab=trash")],
    )
    .await;
    assert_eq!(status, Status::SeeOther);
    let restored = find(db, a).await;
    assert_eq!(restored.status, PostStatus::Draft);
    assert_eq!(restored.slug, "custom-slug-edited-2");
    assert_eq!(find(db, b).await.slug, "custom-slug-edited");
}

/// Defect 4 and 11: A page's and a category's parent dropdown leave out the item and its
/// descendants, Parent Category has one "None", and a save naming the item is refused.
async fn nothing_is_its_own_parent(client: &Client, db: &Database) {
    let parent = new_post(
        client,
        "page",
        &[("title", "Parent page"), ("status", "publish")],
    )
    .await;
    let parent_id = parent.to_string();
    let child = new_post(
        client,
        "page",
        &[
            ("title", "Child page"),
            ("status", "publish"),
            ("parent", &parent_id),
        ],
    )
    .await;
    let other = new_post(
        client,
        "page",
        &[("title", "Other page"), ("status", "publish")],
    )
    .await;
    assert_eq!(find(db, child).await.parent_id, Some(parent));
    let (status, form) = get(client, &format!("/mc-admin/page/{parent}/edit")).await;
    assert_eq!(status, Status::Ok, "{form}");
    let parents = select(&form, "parent");
    assert!(
        !parents.contains(&format!("value=\"{parent}\"")),
        "{parents}"
    );
    assert!(
        !parents.contains(&format!("value=\"{child}\"")),
        "{parents}"
    );
    assert!(parents.contains(&format!("value=\"{other}\"")), "{parents}");
    let (status, _, body) = post(
        client,
        &format!("/mc-admin/page/{parent}"),
        &[
            ("title", "Parent page"),
            ("status", "publish"),
            ("parent", &parent_id),
        ],
    )
    .await;
    assert_on_form(status, &body, "A page cannot be its own parent.");
    assert!(
        !select(&body, "parent").contains(&format!("value=\"{parent}\"")),
        "{body}"
    );
    assert_eq!(find(db, parent).await.parent_id, None);

    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let local = Term::create(
        db,
        &Taxonomy::CATEGORY,
        NewTerm {
            parent_id: Some(news.term_id),
            ..NewTerm::new("Local")
        },
    )
    .await
    .unwrap();
    for uri in [
        "/mc-admin/category/new".to_owned(),
        format!("/mc-admin/category/{}/edit", news.term_id),
    ] {
        let (status, form) = get(client, &uri).await;
        assert_eq!(status, Status::Ok, "{form}");
        let parents = select(&form, "parent");
        assert_eq!(parents.matches(">None</option>").count(), 1, "{parents}");
        assert!(!parents.contains("Select an option"), "{parents}");
    }
    let (_, form) = get(client, &format!("/mc-admin/category/{}/edit", news.term_id)).await;
    let parents = select(&form, "parent");
    for excluded in [news.term_id, local.term_id] {
        assert!(
            !parents.contains(&format!("value=\"{excluded}\"")),
            "{parents}"
        );
    }
    let (status, _, body) = post(
        client,
        &format!("/mc-admin/category/{}", news.term_id),
        &[
            ("name", "News"),
            ("slug", "news"),
            ("parent", &news.term_id.to_string()),
            ("description", ""),
        ],
    )
    .await;
    assert_on_form(status, &body, "A category cannot be its own parent.");
    let (_, form) = get(
        client,
        &format!("/mc-admin/category/{}/edit", local.term_id),
    )
    .await;
    let parents = select(&form, "parent");
    assert!(
        parents.contains(&format!("value=\"{}\" selected", news.term_id)),
        "{parents}"
    );
}

/// Defect 5: The featured image is an image: a PDF's id is refused with WordPress's message.
async fn featured_image_is_an_image(client: &Client, db: &Database, author: i64) {
    let attachment = async |title: &str, mime: &str| {
        Post::create(
            db,
            NewPost {
                status: PostStatus::Inherit,
                title: title.to_owned(),
                mime_type: mime.to_owned(),
                ..NewPost::new(author, "attachment")
            },
        )
        .await
        .expect("creates the attachment")
        .id
    };
    let photo = attachment("Photo", "image/jpeg").await;
    let report = attachment("Report", "application/pdf").await;
    let id = new_post(
        client,
        "post",
        &[("title", "Featured"), ("status", "draft")],
    )
    .await;
    let save = async |media: i64| {
        post(
            client,
            &format!("/mc-admin/post/{id}"),
            &[
                ("title", "Featured"),
                ("status", "draft"),
                ("featured_media", &media.to_string()),
            ],
        )
        .await
    };
    let (status, _, body) = save(report).await;
    assert_on_form(status, &body, "Invalid featured media ID.");
    let (status, _, body) = save(photo).await;
    assert_eq!(status, Status::SeeOther, "{body}");
}

/// Defect 6: A custom link whose URL is not a link is refused, with the message.
async fn custom_links_need_a_url(client: &Client, db: &Database) {
    let rows = |url: &str| {
        json!([{"id": "", "title": "Docs", "type": "custom", "object_id": "", "url": url,
                "parent": 0, "classes": "", "new_tab": false}])
        .to_string()
    };
    let (status, _, body) = post(
        client,
        "/mc-admin/nav-menus",
        &[("name", "Bad links"), ("items", &rows("not a url"))],
    )
    .await;
    assert_on_form(status, &body, "Please enter a valid URL.");
    let (status, _, body) = post(
        client,
        "/mc-admin/nav-menus",
        &[
            ("name", "Good links"),
            ("items", &rows("https://example.com/docs")),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (_, list) = get(client, "/mc-admin/nav-menus").await;
    let id = list
        .split("/mc-admin/nav-menus/")
        .filter_map(|rest| rest.split('/').next()?.parse::<i64>().ok())
        .next()
        .expect("the menu's edit link");
    let menu = get_nav_menu(db, id).await.unwrap().expect("the menu");
    assert_eq!(menu_items(db, &menu).await.unwrap().len(), 1);
    // The builder checks the URL when a link is added and before the form is saved.
    let script = get(client, "/mc-includes/js/nav-menus.js?ver=1").await.1;
    assert!(script.contains("Please enter a valid URL."), "{script}");
}

/// Defect 7: The lost password form: an empty or malformed email is answered on the form (422)
/// under the field; a valid address gets Ignite's answer.
async fn lost_password_needs_an_email(client: &Client) {
    let (status, page) = get(client, "/mc-admin/password/forgot").await;
    assert_eq!(status, Status::Ok, "{page}");
    for (email, message) in [
        ("", "Please enter an email address."),
        ("   ", "Please enter an email address."),
        ("not-an-email", "Please enter a valid email address."),
    ] {
        let csrf = token_in(&get(client, "/mc-admin/password/forgot").await.1);
        let response = client
            .post("/mc-admin/password/forgot")
            .header(ContentType::Form)
            .body(form(&[("_token", &csrf), ("email", email)]))
            .dispatch()
            .await;
        let status = response.status();
        let body = response.into_string().await.unwrap_or_default();
        assert_on_form(status, &body, message);
        assert!(!body.contains("we have emailed"), "{body}");
    }
    let csrf = token_in(&get(client, "/mc-admin/password/forgot").await.1);
    let response = client
        .post("/mc-admin/password/forgot")
        .header(ContentType::Form)
        .body(form(&[("_token", &csrf), ("email", "nobody@example.test")]))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::SeeOther);
    let (_, page) = get(client, "/mc-admin/password/forgot").await;
    assert!(page.contains("we have emailed"), "{page}");
}

/// Defect 8: Settings > Reading: a static homepage needs a page, the posts page is not the same
/// page, and "Blog pages show at most" below 1 is refused; nothing is saved.
async fn reading_checks_its_pages(client: &Client, db: &Database) {
    let home = new_post(client, "page", &[("title", "Home"), ("status", "publish")]).await;
    let home_id = home.to_string();
    let save = async |front: &str, posts_page: &str, per_page: &str| {
        post(
            client,
            "/mc-admin/options-reading/1",
            &[
                ("show_on_front", "page"),
                ("page_on_front", front),
                ("page_for_posts", posts_page),
                ("posts_per_page", per_page),
                ("posts_per_rss", "10"),
                ("rss_use_excerpt", "0"),
            ],
        )
        .await
    };
    let (status, _, body) = save("", "", "10").await;
    assert_on_form(status, &body, "Please select a page for your homepage.");
    let (status, _, body) = save(&home_id, &home_id, "10").await;
    assert_on_form(status, &body, "These pages should not be the same!");
    let (status, _, body) = save(&home_id, "", "0").await;
    assert_on_form(
        status,
        &body,
        "Please enter a valid number of posts per page.",
    );
    assert_eq!(get_option(db, "show_on_front").await.unwrap(), None);
    let (status, _, body) = save(&home_id, "", "5").await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(
        get_option(db, "page_on_front").await.unwrap(),
        Some(json!(home))
    );
    assert_eq!(
        get_option(db, "posts_per_page").await.unwrap(),
        Some(json!(5))
    );
}

/// Defect 9: A picture over 1 MB reaches Ignite, which answers with its message on the profile.
async fn profile_picture_too_large(client: &Client) {
    let csrf = token(client).await;
    let boundary = "mc-fixes-boundary";
    let mut picture = b"\x89PNG\r\n\x1a\n".to_vec();
    picture.resize(1536 * 1024, 0);
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"_token\"\r\n\r\n{csrf}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; \
         filename=\"big.png\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(&picture);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = client
        .post("/mc-admin/profile/avatar")
        .header(ContentType::new("multipart", "form-data").with_params(("boundary", boundary)))
        .body(body)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(location(&response), "/mc-admin/profile");
    let (_, profile) = get(client, "/mc-admin/profile").await;
    assert!(
        profile.contains("Choose a PNG, JPEG or WebP picture of up to 1 MB."),
        "{profile}"
    );
}

/// Defect 10: A new post's Status is Draft, with no blank choice; a blank status still saves a
/// draft.
async fn new_post_is_a_draft(client: &Client, db: &Database) {
    let (status, form) = get(client, "/mc-admin/post/new").await;
    assert_eq!(status, Status::Ok, "{form}");
    let statuses = select(&form, "status");
    assert!(
        statuses.contains("<option value=\"draft\" selected>Draft</option>"),
        "{statuses}"
    );
    assert!(!statuses.contains("Select an option"), "{statuses}");
    assert!(!statuses.contains("value=\"\""), "{statuses}");
    let id = new_post(client, "post", &[("title", "Blank status"), ("status", "")]).await;
    assert_eq!(find(db, id).await.status, PostStatus::Draft);
}

/// Defect 12: A sticky post cannot have a password: refused on the field, and the post is not
/// sticky.
async fn sticky_posts_have_no_password(client: &Client, db: &Database) {
    let id = new_post(
        client,
        "post",
        &[("title", "Locked"), ("status", "publish")],
    )
    .await;
    let (status, _, body) = post(
        client,
        &format!("/mc-admin/post/{id}"),
        &[
            ("title", "Locked"),
            ("status", "publish"),
            ("password", "secret"),
            ("sticky", "on"),
        ],
    )
    .await;
    assert_on_form(
        status,
        &body,
        "A post can not be sticky and have a password.",
    );
    assert_eq!(find(db, id).await.password.as_deref().unwrap_or(""), "");
    // Without the password it is sticky.
    let (status, _, body) = post(
        client,
        &format!("/mc-admin/post/{id}"),
        &[("title", "Locked"), ("status", "publish"), ("sticky", "on")],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(
        get_option(db, "sticky_posts").await.unwrap(),
        Some(json!([id]))
    );
}

// The second end-to-end QA round's defects (`flows` runs them after the first round's):
//
// 1. A new post is not created in the trash: the New screen has no Trash, and a create that
//    sends it is refused on the field; an edit that moves a post to the trash goes to the list
//    with "1 post moved to the Trash." (`edit.php?trashed=1&ids=…`), and no "Record created."
//    or "Changes saved." is left queued.
// 2. A term name over 200 characters (the `terms.name` column) is refused on the field.
// 3. A rename to a name another term has is refused on the field, tags without regard to case.
// 4. Settings > Reading: "A static page" with no homepage is refused on the field.
// 5. The settings screens are named as WordPress names them, without "Edit" and "#1".
// 6. A custom link's URL must match WordPress's pattern: `notaurl` is refused.
// 7. Permalinks: an empty Custom Structure is refused.

/// The values of `option`s of the select named `name` in `html`.
fn options_of(html: &str, name: &str) -> Vec<String> {
    select(html, name)
        .split("<option value=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_owned)
        .collect()
}

/// Round 2, defect 1.
async fn new_posts_are_not_trashed(client: &Client, db: &Database) {
    for post_type in ["post", "page"] {
        let (status, page) = get(client, &format!("/mc-admin/{post_type}/new")).await;
        assert_eq!(status, Status::Ok, "{page}");
        let statuses = options_of(&page, "status");
        assert!(!statuses.contains(&"trash".to_owned()), "{statuses:?}");
        assert!(statuses.contains(&"draft".to_owned()), "{statuses:?}");
    }
    let count = async || Post::query().count(db).await.unwrap();
    let before = count().await;
    let (status, _, body) = post(
        client,
        "/mc-admin/post",
        &[("title", "Born in the trash"), ("status", "trash")],
    )
    .await;
    assert_on_form(status, &body, "Choose one of the options.");
    assert_eq!(count().await, before, "no post is left behind");
    // Nothing is queued for a later page.
    let (_, list) = get(client, "/mc-admin/post").await;
    assert!(!list.contains("Record created."), "{list}");

    // An existing post moved to the trash from its edit form: the list, with WordPress's notice.
    for (post_type, notice) in [
        ("post", "1 post moved to the Trash."),
        ("page", "1 page moved to the Trash."),
    ] {
        let id = new_post(
            client,
            post_type,
            &[("title", "Short-lived"), ("status", "publish")],
        )
        .await;
        let (_, edit) = get(client, &format!("/mc-admin/{post_type}/{id}/edit")).await;
        assert!(
            options_of(&edit, "status").contains(&"trash".to_owned()),
            "{edit}"
        );
        let csrf = token(client).await;
        let response = client
            .post(format!("/mc-admin/{post_type}/{id}"))
            .header(ContentType::Form)
            .body(form(&[
                ("_token", &csrf),
                ("title", "Short-lived"),
                ("status", "trash"),
            ]))
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::SeeOther);
        assert_eq!(
            location(&response),
            format!("/mc-admin/{post_type}?trashed=1&ids={id}")
        );
        assert!(
            response
                .headers()
                .get("Set-Cookie")
                .all(|cookie| !cookie.starts_with("ignite_flash=")),
            "Ignite's \"Changes saved.\" is not queued"
        );
        drop(response);
        assert_eq!(find(db, id).await.status, PostStatus::Trash);
        let (status, list) =
            get(client, &format!("/mc-admin/{post_type}?trashed=1&ids={id}")).await;
        assert_eq!(status, Status::Ok, "{list}");
        assert!(list.contains(notice), "{list}");
        assert!(!list.contains("Changes saved."), "{list}");
        let (_, again) = get(client, &format!("/mc-admin/{post_type}")).await;
        assert!(!again.contains(notice), "{again}");
    }
}

/// Round 2, defects 2 and 3.
async fn term_names_are_checked(client: &Client, db: &Database) {
    let long = "n".repeat(300);
    let (status, _, body) = post(client, "/mc-admin/category", &[("name", &long)]).await;
    assert_on_form(status, &body, "Name must not exceed 200 characters.");
    let (status, _, body) = post(client, "/mc-admin/post_tag", &[("name", &long)]).await;
    assert_on_form(status, &body, "Name must not exceed 200 characters.");
    let exactly = "n".repeat(200);
    let (status, _, body) = post(client, "/mc-admin/category", &[("name", &exactly)]).await;
    assert_eq!(status, Status::SeeOther, "200 characters are kept: {body}");

    let a = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("QA Cat A"))
        .await
        .unwrap();
    let b = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("QA Cat B"))
        .await
        .unwrap();
    let rename = async |screen: &str, id: i64, name: &str| {
        post(
            client,
            &format!("/mc-admin/{screen}/{id}"),
            &[("name", name), ("slug", ""), ("description", "")],
        )
        .await
    };
    let (status, _, body) = rename("category", b.term_id, "QA Cat A").await;
    assert_on_form(status, &body, "Name is already taken.");
    let (status, _, body) = rename("category", b.term_id, &long).await;
    assert_on_form(status, &body, "Name must not exceed 200 characters.");
    // Its own name, and a free one, are saved.
    let (status, _, body) = rename("category", a.term_id, "QA Cat A").await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (status, _, body) = rename("category", b.term_id, "QA Cat C").await;
    assert_eq!(status, Status::SeeOther, "{body}");

    let alpha = Term::create(db, &Taxonomy::POST_TAG, NewTerm::new("Alpha"))
        .await
        .unwrap();
    let beta = Term::create(db, &Taxonomy::POST_TAG, NewTerm::new("Beta"))
        .await
        .unwrap();
    let (status, _, body) = rename("post_tag", beta.term_id, "alpha").await;
    assert_on_form(status, &body, "Name is already taken.");
    let (status, _, body) = post(client, "/mc-admin/post_tag", &[("name", "ALPHA")]).await;
    assert_on_form(status, &body, "Name is already taken.");
    let kept = Term::find(db, beta.term_taxonomy_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.name, "Beta");
    assert_eq!(
        Term::find(db, alpha.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .name,
        "Alpha"
    );
}

/// The fields of the form in `html` as a browser sends them: each named input's value (a
/// checked radio or checkbox only), each select's selected option, each textarea's text.
fn form_fields(html: &str) -> Vec<(String, String)> {
    let attribute = |tag: &str, name: &str| {
        let marker = format!(" {name}=\"");
        tag.find(&marker).map(|at| {
            let rest = &tag[at + marker.len()..];
            rest[..rest.find('"').unwrap_or(rest.len())].to_owned()
        })
    };
    let decode = |text: &str| {
        text.replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&#x27;", "'")
            .replace("&#39;", "'")
            .replace("&amp;", "&")
    };
    let mut fields = Vec::new();
    for chunk in html.split("<input").skip(1) {
        let tag = &chunk[..chunk.find('>').unwrap_or(chunk.len())];
        let Some(name) = attribute(tag, "name") else {
            continue;
        };
        let kind = attribute(tag, "type").unwrap_or_default();
        if matches!(kind.as_str(), "radio" | "checkbox")
            && !tag.split([' ', '"']).any(|t| t == "checked")
        {
            continue;
        }
        if matches!(kind.as_str(), "submit" | "button" | "file") {
            continue;
        }
        fields.push((name, decode(&attribute(tag, "value").unwrap_or_default())));
    }
    for chunk in html.split("<select").skip(1) {
        let tag = &chunk[..chunk.find('>').unwrap_or(chunk.len())];
        let Some(name) = attribute(tag, "name") else {
            continue;
        };
        let body = &chunk[..chunk.find("</select>").unwrap_or(chunk.len())];
        let selected = body
            .split("<option")
            .skip(1)
            .find(|option| {
                let tag = &option[..option.find('>').unwrap_or(option.len())];
                tag.split([' ', '"']).any(|t| t == "selected")
            })
            .or_else(|| body.split("<option").nth(1))
            .and_then(|option| attribute(option, "value"))
            .unwrap_or_default();
        fields.push((name, decode(&selected)));
    }
    for chunk in html.split("<textarea").skip(1) {
        let tag = &chunk[..chunk.find('>').unwrap_or(chunk.len())];
        let Some(name) = attribute(tag, "name") else {
            continue;
        };
        let text = &chunk[chunk.find('>').map_or(0, |at| at + 1)..];
        let text = &text[..text.find("</textarea>").unwrap_or(text.len())];
        fields.push((name, decode(text)));
    }
    fields
}

/// Posts the form of the page at `page` to `action` as a browser does, with `changes` in place
/// of the form's values.
async fn submit(
    client: &Client,
    page: &str,
    action: &str,
    changes: &[(&str, &str)],
) -> (Status, String, String) {
    let (status, html) = get(client, page).await;
    assert_eq!(status, Status::Ok, "{html}");
    let start = html
        .find(&format!("action=\"{action}\""))
        .unwrap_or_else(|| panic!("a form posting to {action}: {html}"));
    let form_html = &html[start
        ..html[start..]
            .find("</form>")
            .map_or(html.len(), |e| start + e)];
    let mut fields = form_fields(form_html);
    // A changed field's other values go, except the empty value of the hidden input Ignite puts
    // before a radio group or a checkbox, which a browser sends first.
    let mut seen = std::collections::HashSet::new();
    fields.retain(|(name, value)| {
        let changed = changes.iter().any(|(changed, _)| changed == name);
        let first = seen.insert(name.clone());
        !changed
            || (first
                && value.is_empty()
                && form_html.contains(&format!(
                    "<input type=\"hidden\" name=\"{name}\" value=\"\">"
                )))
    });
    fields.extend(
        changes
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let response = client
        .post(action.to_owned())
        .header(ContentType::Form)
        .body(form(&pairs))
        .dispatch()
        .await;
    let status = response.status();
    let to = location(&response);
    (status, to, response.into_string().await.unwrap_or_default())
}

/// Round 2, defect 4: the Reading form as a browser sends it, with "A static page" and no
/// homepage, is refused on the Homepage field; the same page twice still is.
async fn reading_needs_a_homepage(client: &Client, db: &Database) {
    let home = new_post(client, "page", &[("title", "Home"), ("status", "publish")]).await;
    let page = "/mc-admin/options-reading/1/edit";
    let action = "/mc-admin/options-reading/1";
    // From "Your latest posts", as a site starts.
    let (status, _, body) = submit(
        client,
        page,
        action,
        &[
            ("show_on_front", "posts"),
            ("page_on_front", ""),
            ("page_for_posts", ""),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (status, _, body) = submit(
        client,
        page,
        action,
        &[("show_on_front", "page"), ("page_on_front", "")],
    )
    .await;
    assert_on_form(status, &body, "Please select a page for your homepage.");
    let id = home.to_string();
    let (status, _, body) = submit(
        client,
        page,
        action,
        &[
            ("show_on_front", "page"),
            ("page_on_front", &id),
            ("page_for_posts", &id),
        ],
    )
    .await;
    assert_on_form(status, &body, "These pages should not be the same!");
    let (status, _, body) = submit(
        client,
        page,
        action,
        &[("show_on_front", "page"), ("page_on_front", &id)],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    // A homepage is stored: clearing it is refused too.
    let (status, _, body) = submit(client, page, action, &[("page_on_front", "")]).await;
    assert_on_form(status, &body, "Please select a page for your homepage.");
    assert_eq!(
        get_option(db, "page_on_front").await.unwrap(),
        Some(json!(home))
    );
}

/// Round 2, defect 5.
async fn settings_screens_have_their_names(client: &Client) {
    for (slug, name) in [
        ("options-general", "General Settings"),
        ("options-writing", "Writing Settings"),
        ("options-reading", "Reading Settings"),
        ("options-discussion", "Discussion Settings"),
        ("options-media", "Media Settings"),
        ("options-permalink", "Permalink Settings"),
        ("options-privacy", "Privacy"),
    ] {
        let (status, page) = get(client, &format!("/mc-admin/{slug}/1/edit")).await;
        assert_eq!(status, Status::Ok, "{slug}: {page}");
        assert!(!page.contains(" #1"), "{slug}: {page}");
        assert!(!page.contains(&format!("Edit {name}")), "{slug}: {page}");
        let heading = page
            .split_once("<h1")
            .and_then(|(_, rest)| rest.split_once("</h1>"))
            .map(|(h1, _)| {
                h1.split_once('>')
                    .map_or(h1, |(_, text)| text)
                    .trim()
                    .to_owned()
            })
            .unwrap_or_default();
        assert_eq!(heading, name, "{slug}");
        let title = page
            .split_once("<title>")
            .and_then(|(_, rest)| rest.split_once("</title>"))
            .map(|(title, _)| title.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        assert!(title.starts_with(&format!("{name} ")), "{slug}: {title}");
    }
    // A record screen keeps its number.
    let id = new_post(client, "post", &[("title", "Numbered")]).await;
    let (_, edit) = get(client, &format!("/mc-admin/post/{id}/edit")).await;
    assert!(edit.contains(&format!("Edit Post #{id}")), "{edit}");
}

/// Round 2, defect 6.
async fn custom_links_match_wordpress_s_pattern(client: &Client) {
    let rows = |url: &str| {
        json!([{"id": "", "title": "Bad link", "type": "custom", "object_id": "", "url": url,
                "parent": 0, "classes": "", "new_tab": false}])
        .to_string()
    };
    for url in ["notaurl", "example.com", "javascript:alert(1)", "https://"] {
        let (status, _, body) = post(
            client,
            "/mc-admin/nav-menus",
            &[("name", "Bad links"), ("items", &rows(url))],
        )
        .await;
        assert_on_form(status, &body, "Please enter a valid URL.");
    }
    let script = get(client, "/mc-includes/js/nav-menus.js?ver=1").await.1;
    assert!(
        script.contains(r"var URL_PATTERN = /^((\w+:)?\/\/\w.*|\w+:(?!\/\/$)|\/|\?|#)/;"),
        "{script}"
    );
    let (status, _, body) = post(
        client,
        "/mc-admin/nav-menus",
        &[
            ("name", "More links"),
            ("items", &rows("https://example.com/")),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
}

/// Round 2 (U11): an empty Custom Structure is refused with WordPress's message, and the stored
/// structure is kept.
async fn custom_permalinks_need_a_tag(client: &Client, db: &Database) {
    let page = "/mc-admin/options-permalink/1/edit";
    let action = "/mc-admin/options-permalink/1";
    let (status, _, body) =
        submit(client, page, action, &[("permalink_selection", "postname")]).await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (status, _, body) = submit(
        client,
        page,
        action,
        &[
            ("permalink_selection", "custom"),
            ("permalink_structure", ""),
        ],
    )
    .await;
    assert_on_form(
        status,
        &body,
        "A structure tag is required when using custom permalinks.",
    );
    assert_eq!(
        get_option(db, "permalink_structure").await.unwrap(),
        Some(json!("/%postname%/"))
    );
}
