//! The Taxonomies manager (Task 5.7, design `cpt-field-manager-design.md` 2.2): an
//! administrator creates a taxonomy at `/mc-admin/taxonomies` attached to a post type made in
//! the Post Types manager; its term screen is under that post type (Books) on the next page
//! load, as ACF Pro places its taxonomies, without a restart, a term can be added there, and the post type's form assigns it. WordPress's own
//! categories and tags are listed locked.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{get, location, new_post, post, run, sidebar, sign_in, sign_out, token, user};
use mission_control_core::content::{Taxonomy, Term};
use orbit_orm::{Database, raw};
use rocket::http::Status;
use rocket::local::asynchronous::Client;

/// Posts `fields` to `uri` with the session's form token: the status and the redirect.
async fn send(client: &Client, uri: &str, fields: &[(&str, &str)]) -> (Status, String, String) {
    let csrf = token(client).await;
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = post(client, uri, &all).await;
    let status = response.status();
    let to = location(&response);
    (status, to, response.into_string().await.unwrap_or_default())
}

#[test]
fn an_administrator_creates_a_taxonomy_and_its_terms() {
    run("cpt_taxonomies", |client, db| async move {
        let (_, admin) = user(&db, "ada", "administrator").await;
        sign_in(&client, &admin).await;

        // Books, from the Post Types manager.
        let (status, _, body) = send(
            &client,
            "/mc-admin/post-types",
            &[
                ("plural_label", "Books"),
                ("singular_label", "Book"),
                ("key", "book"),
                ("public", "on"),
            ],
        )
        .await;
        assert_eq!(status, Status::SeeOther, "{body}");

        // Categories and Tags are listed, built in; their form says why it cannot be changed.
        let (status, list) = get(&client, "/mc-admin/taxonomies").await;
        assert_eq!(status, Status::Ok, "{list}");
        for text in ["Categories", "Tags", "Built in"] {
            assert!(list.contains(text), "{text}: {list}");
        }
        let (_, form) = get(&client, "/mc-admin/taxonomies/category/edit").await;
        assert!(form.contains("built into WordPress"), "{form}");
        let (status, _, _) = send(
            &client,
            "/mc-admin/taxonomies/category",
            &[("plural_label", "Topics"), ("singular_label", "Topic")],
        )
        .await;
        assert_eq!(status, Status::Forbidden);

        // A key in use by a post type is refused on the field.
        let (status, _, body) = send(
            &client,
            "/mc-admin/taxonomies",
            &[
                ("plural_label", "Books"),
                ("singular_label", "Book"),
                ("key", "book"),
            ],
        )
        .await;
        assert_eq!(status, Status::UnprocessableEntity, "{body}");
        assert!(body.contains("already in use"), "{body}");

        // Genres, attached to Books.
        let (status, html) = get(&client, "/mc-admin/taxonomies/new").await;
        assert_eq!(status, Status::Ok, "{html}");
        let (status, to, body) = send(
            &client,
            "/mc-admin/taxonomies",
            &[
                ("plural_label", "Genres"),
                ("singular_label", "Genre"),
                ("key", "genre"),
                ("post_types[]", "book"),
                ("public", "on"),
                ("hierarchical", "on"),
                ("show_admin_column", "on"),
            ],
        )
        .await;
        assert_eq!(status, Status::SeeOther, "{body}");
        assert_eq!(to, "/mc-admin/taxonomies/genre/edit");
        let stored: i64 = raw::fetch_scalar(
            &*db,
            "SELECT COUNT(*) FROM taxonomies WHERE slug = ?",
            &["genre".into()],
        )
        .await
        .unwrap();
        assert_eq!(stored, 1);

        // Its term screen is under Books on the next page load, not under Posts: ACF Pro
        // registers a taxonomy for its post types, and WordPress's menu lists the term screen
        // in each attached post type's submenu.
        let (_, page) = get(&client, "/mc-admin/post").await;
        let nav = sidebar(&page);
        let books = nav.find("</i>Books</a>").expect("the Books entry");
        // A child entry (Ignite's submenu link), after Books, the last entry of Content.
        let genre = nav
            .find("pl-11 text-[13px]\" href=\"/mc-admin/genre\"")
            .expect("the Genres child entry");
        assert!(genre > books, "Genres is listed under Books: {nav}");
        let posts = nav.find("</i>Posts</a>").expect("the Posts entry");
        let pages = nav.find("</i>Pages</a>").expect("the Pages entry");
        assert!(
            !nav[posts..pages].contains("href=\"/mc-admin/genre\""),
            "Genres is not under Posts: {nav}"
        );

        // A term is added on it.
        let (status, html) = get(&client, "/mc-admin/genre/new").await;
        assert_eq!(status, Status::Ok, "{html}");
        assert!(html.contains("Parent Genre"), "{html}");
        let (status, _, body) = send(
            &client,
            "/mc-admin/genre",
            &[
                ("name", "Science Fiction"),
                ("slug", ""),
                ("description", ""),
            ],
        )
        .await;
        assert_eq!(status, Status::SeeOther, "{body}");
        let genre = Taxonomy::new("genre", true, vec!["book".to_owned()]);
        let term = Term::find_by_slug(&db, &genre, "science-fiction")
            .await
            .unwrap()
            .expect("the term is saved");
        let (_, list) = get(&client, "/mc-admin/genre").await;
        assert!(list.contains("Science Fiction"), "{list}");

        // The book form assigns it.
        let (status, html) = get(&client, "/mc-admin/book/new").await;
        assert_eq!(status, Status::Ok, "{html}");
        assert!(html.contains("tax_genre"), "{html}");
        let id = term.term_id.to_string();
        let book = new_post(
            &client,
            "book",
            &[("title", "Dune"), ("tax_genre[]", id.as_str())],
        )
        .await;
        assert_eq!(assigned(&db, book).await, 1, "Dune is science fiction");

        // Deleting the taxonomy removes its screen; its terms stay.
        let (status, _, body) = send(&client, "/mc-admin/taxonomies/genre/delete", &[]).await;
        assert_eq!(status, Status::SeeOther, "{body}");
        let (_, page) = get(&client, "/mc-admin/post").await;
        assert!(
            !sidebar(&page).contains("href=\"/mc-admin/genre\""),
            "{page}"
        );
        assert!(
            Term::find_by_slug(&db, &genre, "science-fiction")
                .await
                .unwrap()
                .is_some()
        );
        sign_out(&client).await;
    });
}

/// How many `genre` terms the post `id` has.
async fn assigned(db: &Database, id: i64) -> i64 {
    raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM term_relationships tr \
         INNER JOIN term_taxonomy tt ON tt.id = tr.term_taxonomy_id \
         WHERE tr.object_id = ? AND tt.taxonomy = ?",
        &[id.into(), "genre".into()],
    )
    .await
    .unwrap()
}
