//! Capabilities of the post types made in the Post Types manager (Task 5.7, S4): a type with
//! its own capability type ("recipe") has WordPress's capabilities for it (`edit_recipes`,
//! `publish_recipes`, ...), which the save grants to Administrator (the owner's rule; WordPress
//! grants them to no role). An administrator then sees and uses the type's screens at once; an
//! editor, who has none of them, neither sees the type in the sidebar nor opens its screens. A
//! type with the default capability type (`post`) is an editor's, as WordPress maps it to
//! `edit_posts`.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{get, location, new_post, post, run, sidebar, sign_in, sign_out, token, user};
use ignite_panel::authz;
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
fn a_post_types_capabilities_are_administrators() {
    run("cpt_capabilities", |client, db| async move {
        let (_, admin) = user(&db, "ada", "administrator").await;
        let (_, editor) = user(&db, "edna", "editor").await;

        sign_in(&client, &admin).await;
        // The administrator's capabilities are read before the type exists.
        let (status, _) = get(&client, "/mc-admin/post").await;
        assert_eq!(status, Status::Ok);
        for (plural, singular, key, capability_type) in [
            ("Recipes", "Recipe", "recipe", "recipe"),
            ("Notes", "Note", "note", ""),
        ] {
            let (status, _, body) = send(
                &client,
                "/mc-admin/post-types",
                &[
                    ("plural_label", plural),
                    ("singular_label", singular),
                    ("key", key),
                    ("public", "on"),
                    ("capability_type", capability_type),
                ],
            )
            .await;
            assert_eq!(status, Status::SeeOther, "{body}");
        }

        // The recipe capabilities are Administrator's.
        let granted = authz::role_permissions(&db, "administrator").await.unwrap();
        for cap in [
            "edit_recipes",
            "edit_others_recipes",
            "publish_recipes",
            "delete_recipes",
            "read_private_recipes",
        ] {
            assert!(granted.iter().any(|g| g == cap), "{cap}: {granted:?}");
        }
        let editors = authz::role_permissions(&db, "editor").await.unwrap();
        assert!(!editors.iter().any(|g| g == "edit_recipes"), "{editors:?}");

        // The administrator uses the screens on the next request.
        let (_, page) = get(&client, "/mc-admin/post").await;
        assert!(
            sidebar(&page).contains("href=\"/mc-admin/recipe\""),
            "{page}"
        );
        assert_eq!(get(&client, "/mc-admin/recipe").await.0, Status::Ok);
        new_post(&client, "recipe", &[("title", "Bread")]).await;
        sign_out(&client).await;

        // An editor has no recipe capability: no entry, and 403 on its screens. Notes use
        // `post`'s capabilities, which an editor has.
        sign_in(&client, &editor).await;
        let (_, page) = get(&client, "/mc-admin/post").await;
        let nav = sidebar(&page);
        assert!(!nav.contains("href=\"/mc-admin/recipe\""), "{nav}");
        assert!(nav.contains("href=\"/mc-admin/note\""), "{nav}");
        for uri in ["/mc-admin/recipe", "/mc-admin/recipe/new"] {
            assert_eq!(get(&client, uri).await.0, Status::Forbidden, "{uri}");
        }
        let (status, _, _) = send(&client, "/mc-admin/recipe", &[("title", "Sneaky")]).await;
        assert_eq!(status, Status::Forbidden);
        assert_eq!(get(&client, "/mc-admin/note").await.0, Status::Ok);
        sign_out(&client).await;
    });
}
