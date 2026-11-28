//! The Post Types manager (Task 5.7, design `cpt-field-manager-design.md` 2.1): an
//! administrator creates a post type at `/mc-admin/post-types`, it is in the sidebar under
//! Content below Posts and Pages on the next page load, without a restart, its New screen saves
//! a post, its key is locked after the first save, editing the label renames the entry, and
//! deleting it removes the entry and keeps its posts. WordPress's own `post` and `page` are
//! listed locked: they cannot be edited or deleted, and say why.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{get, location, new_post, post, run, sidebar, sign_in, sign_out, token, user};
use mission_control_core::content::Post;
use orbit_orm::{Database, Model, raw};
use rocket::http::Status;
use rocket::local::asynchronous::Client;

/// Posts `fields` to `uri` with the session's form token: the status and the redirect.
pub async fn send(client: &Client, uri: &str, fields: &[(&str, &str)]) -> (Status, String, String) {
    let csrf = token(client).await;
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = post(client, uri, &all).await;
    let status = response.status();
    let to = location(&response);
    (status, to, response.into_string().await.unwrap_or_default())
}

/// The stored definition of the post type `slug`, as JSON, if there is one.
async fn stored(db: &Database, slug: &str) -> Option<serde_json::Value> {
    let rows: Vec<(String,)> = raw::fetch_all(
        db,
        "SELECT definition FROM post_types WHERE slug = ?",
        &[slug.into()],
    )
    .await
    .expect("reads the post types");
    rows.first()
        .map(|(json,)| serde_json::from_str(json).expect("the definition is JSON"))
}

#[test]
fn an_administrator_creates_edits_and_deletes_a_post_type() {
    run("cpt_post_types", |client, db| async move {
        let (_, admin) = user(&db, "ada", "administrator").await;
        let (_, editor) = user(&db, "edna", "editor").await;

        // The managers need `manage_options`: an editor neither sees nor opens them.
        sign_in(&client, &editor).await;
        let (_, dashboard) = get(&client, "/mc-admin").await;
        assert!(
            !sidebar(&dashboard).contains("href=\"/mc-admin/post-types\""),
            "{dashboard}"
        );
        for uri in ["/mc-admin/post-types", "/mc-admin/post-types/new"] {
            assert_eq!(get(&client, uri).await.0, Status::Forbidden, "{uri}");
        }
        sign_out(&client).await;

        sign_in(&client, &admin).await;
        // The Site group lists Post Types above Settings.
        let (_, dashboard) = get(&client, "/mc-admin").await;
        let nav = sidebar(&dashboard);
        let manager = nav
            .find("href=\"/mc-admin/post-types\"")
            .expect("the Post Types entry");
        let settings = nav.find(">Settings<").expect("the Settings entry");
        assert!(manager < settings, "Post Types above Settings: {nav}");

        // `post` and `page` are listed, built in.
        let (status, list) = get(&client, "/mc-admin/post-types").await;
        assert_eq!(status, Status::Ok, "{list}");
        for text in ["Posts", "Pages", "Built in", "Key", "Taxonomies"] {
            assert!(list.contains(text), "{text}: {list}");
        }

        create(&client, &db).await;
        key_locked(&client, &db).await;
        built_in_locked(&client).await;
        delete(&client, &db).await;
        sign_out(&client).await;
    });
}

/// "Books", saved on the New screen, is stored and in the sidebar under Content after Pages on
/// the next page load; its New screen saves a book.
async fn create(client: &Client, db: &Database) {
    let (status, html) = get(client, "/mc-admin/post-types/new").await;
    assert_eq!(status, Status::Ok, "{html}");
    for label in [
        "Plural Label",
        "Singular Label",
        "Post Type Key",
        "Supports",
    ] {
        assert!(html.contains(label), "{label}: {html}");
    }
    // A key WordPress keeps is refused on the field.
    let (status, _, body) = send(
        client,
        "/mc-admin/post-types",
        &[
            ("plural_label", "Pages Again"),
            ("singular_label", "Page"),
            ("key", "page"),
        ],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    assert!(body.contains("already in use"), "{body}");

    let (status, to, body) = send(
        client,
        "/mc-admin/post-types",
        &[
            ("plural_label", "Books"),
            ("singular_label", "Book"),
            ("key", "book"),
            ("public", "on"),
            ("supports[]", "title"),
            ("supports[]", "editor"),
            ("supports[]", "thumbnail"),
            ("menu_icon", "dashicons-book"),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(to, "/mc-admin/post-types/book/edit");
    let def = stored(db, "book").await.expect("the post type is stored");
    assert_eq!(def["labels"]["name"], "Books");
    assert_eq!(def["public"], true);
    assert_eq!(def["menu_icon"], "dashicons-book");
    assert_eq!(
        def["supports"],
        serde_json::json!(["title", "editor", "thumbnail"])
    );

    // The next page load lists Books under Content, after Posts and Pages, before the managers.
    let (_, page) = get(client, "/mc-admin/post").await;
    let nav = sidebar(&page);
    let at = |href: &str| {
        nav.find(&format!("href=\"{href}\""))
            .unwrap_or_else(|| panic!("{href} in {nav}"))
    };
    assert!(at("/mc-admin/post") < at("/mc-admin/book"), "{nav}");
    assert!(at("/mc-admin/page") < at("/mc-admin/book"), "{nav}");
    assert!(at("/mc-admin/book") < at("/mc-admin/post-types"), "{nav}");
    assert!(nav.contains(">Books<") || nav.contains("Books</"), "{nav}");

    // Its New screen saves a book.
    let (status, html) = get(client, "/mc-admin/book/new").await;
    assert_eq!(status, Status::Ok, "{html}");
    let id = new_post(client, "book", &[("title", "Dune"), ("content", "Spice.")]).await;
    let book = Post::query().find(db, id).await.unwrap().expect("the book");
    assert_eq!(book.post_type, "book");
    assert_eq!(book.title, "Dune");

    // The list counts it.
    let (_, list) = get(client, "/mc-admin/post-types").await;
    assert!(list.contains("Books"), "{list}");
}

/// The key is locked after the first save: the edit form shows it disabled, and a save that
/// sends another keeps it. Editing the plural label renames the sidebar entry.
async fn key_locked(client: &Client, db: &Database) {
    let (status, html) = get(client, "/mc-admin/post-types/book/edit").await;
    assert_eq!(status, Status::Ok, "{html}");
    // A disabled field is in a disabled fieldset, so the browser sends nothing for it.
    let at = html
        .find("data-ignite-field=\"key\"")
        .expect("the key field");
    let field = &html[at..at + html[at..].find("name=\"key\"").unwrap()];
    assert!(field.contains("<fieldset disabled"), "{field}");

    let (status, to, body) = send(
        client,
        "/mc-admin/post-types/book",
        &[
            ("plural_label", "Novels"),
            ("singular_label", "Novel"),
            ("key", "novel"),
            ("public", "on"),
            ("supports[]", "title"),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(to, "/mc-admin/post-types/book/edit");
    assert!(
        stored(db, "novel").await.is_none(),
        "the key did not change"
    );
    let def = stored(db, "book").await.expect("still stored as book");
    assert_eq!(def["labels"]["name"], "Novels");
    assert_eq!(def["menu_icon"], "dashicons-book", "kept: not on the form");

    let (_, page) = get(client, "/mc-admin/post").await;
    let nav = sidebar(&page);
    assert!(nav.contains("Novels"), "{nav}");
    assert!(nav.contains("href=\"/mc-admin/book\""), "{nav}");
}

/// WordPress's `post` cannot be changed or deleted here, and its form says why.
async fn built_in_locked(client: &Client) {
    let (status, html) = get(client, "/mc-admin/post-types/post/edit").await;
    assert_eq!(status, Status::Ok, "{html}");
    assert!(html.contains("built into WordPress"), "{html}");
    let (status, _, _) = send(
        client,
        "/mc-admin/post-types/post",
        &[("plural_label", "Articles"), ("singular_label", "Article")],
    )
    .await;
    assert_eq!(status, Status::Forbidden);
    let (status, _, _) = send(client, "/mc-admin/post-types/post/delete", &[]).await;
    assert_eq!(status, Status::Forbidden);
}

/// Deleting the type removes it from the sidebar; its posts stay, as WordPress and ACF keep
/// them.
async fn delete(client: &Client, db: &Database) {
    let (status, _, body) = send(client, "/mc-admin/post-types/book/delete", &[]).await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert!(stored(db, "book").await.is_none());
    let (_, page) = get(client, "/mc-admin/post").await;
    assert!(
        !sidebar(&page).contains("href=\"/mc-admin/book\""),
        "{page}"
    );
    let left: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM posts WHERE post_type = ?",
        &["book".into()],
    )
    .await
    .unwrap();
    assert_eq!(left, 1, "the book stays");
}
