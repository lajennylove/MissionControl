//! Each admin screen is gated by the capability WordPress checks for it: the post types by
//! their `edit_posts` (`edit.php`), the media library by `upload_files` (`upload.php`: an
//! author has it, a contributor does not), Categories and Tags by `manage_categories`, Menus by
//! `edit_theme_options`, the settings screens by `manage_options`, the Users screens by
//! `list_users` and the Roles screen by `promote_users`. A user without the capability sees no
//! sidebar entry for the screen and is refused it (403). The capabilities are the roles' stored
//! ones: a change on the Roles screen applies from the next request.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{get, run, sidebar, sign_in, sign_out, user};
use ignite_panel::authz;
use orbit_orm::Database;
use rocket::http::Status;
use rocket::local::asynchronous::Client;

/// Each screen: its list address and the sidebar link to it.
const SCREENS: [&str; 9] = [
    "post",
    "page",
    "attachment",
    "category",
    "nav-menus",
    "options-general",
    "options-reading",
    "users",
    "roles",
];

/// The screens of `SCREENS` each role may open, in order.
fn allowed(role: &str) -> [bool; 9] {
    match role {
        "administrator" => [true; 9],
        "editor" => [true, true, true, true, false, false, false, false, false],
        "author" => [true, false, true, false, false, false, false, false, false],
        "contributor" => [true, false, false, false, false, false, false, false, false],
        _ => [false; 9],
    }
}

/// Checks that the signed-in user, of `role`, sees and opens exactly the screens `expected`.
async fn check(client: &Client, role: &str, expected: [bool; 9]) {
    let (status, dashboard) = get(client, "/mc-admin").await;
    assert_eq!(status, Status::Ok, "{role}: {dashboard}");
    let nav = sidebar(&dashboard);
    for (slug, allowed) in SCREENS.iter().zip(expected) {
        let link = format!("href=\"/mc-admin/{slug}\"");
        assert_eq!(
            nav.contains(&link),
            allowed,
            "{role}: the {slug} entry in {nav}"
        );
        // A settings screen's list is a redirect to its one record's form.
        let path = if slug.starts_with("options-") {
            format!("/mc-admin/{slug}/1/edit")
        } else {
            format!("/mc-admin/{slug}")
        };
        let (status, body) = get(client, &path).await;
        let want = if allowed {
            Status::Ok
        } else {
            Status::Forbidden
        };
        assert_eq!(status, want, "{role}: {path}: {body}");
    }
}

#[test]
fn every_screen_needs_wordpresss_capability() {
    run("gating", |client, db| async move {
        for role in [
            "administrator",
            "editor",
            "author",
            "contributor",
            "subscriber",
        ] {
            let (_, email) = user(&db, &format!("{role}_user"), role).await;
            sign_in(&client, &email).await;
            check(&client, role, allowed(role)).await;
            if role == "administrator" {
                let (_, dashboard) = get(&client, "/mc-admin").await;
                let nav = sidebar(&dashboard);
                for expected in [
                    ">Add New User<",
                    "href=\"/mc-admin/your-profile\"",
                    ">Your Profile<",
                    ">Roles<",
                ] {
                    assert!(nav.contains(expected), "`{expected}` in {nav}");
                }
                let response = client.get("/mc-admin/your-profile").dispatch().await;
                assert_eq!(response.status(), Status::SeeOther);
                assert_eq!(
                    response.headers().get_one("Location"),
                    Some("/mc-admin/profile")
                );
            }
            sign_out(&client).await;
        }
        a_change_on_a_role_applies_to_its_users(&client, &db).await;
    });
}

/// A change on a role applies to its users from the next request.
async fn a_change_on_a_role_applies_to_its_users(client: &Client, db: &Database) {
    {
        let (_, author) = user(db, "aubrey", "author").await;
        sign_in(client, &author).await;
        assert_eq!(
            get(client, "/mc-admin/options-general/1/edit").await.0,
            Status::Forbidden,
            "the author cannot open Settings"
        );
        // An administrator grants `manage_options` and takes `edit_posts` away.
        authz::grant_permission(db, "author", "manage_options")
            .await
            .unwrap();
        authz::revoke_permission(db, "author", "edit_posts")
            .await
            .unwrap();
        let mut expected = allowed("author");
        expected[0] = false;
        expected[5] = true;
        expected[6] = true;
        check(client, "author", expected).await;
        sign_out(client).await;
    }
}
