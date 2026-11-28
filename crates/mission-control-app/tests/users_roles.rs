//! WordPress's users, roles and capabilities in the admin: the five default roles with their
//! capabilities (`populate_roles`), stored as Ignite roles and abilities; the Users screens
//! (`users.php`, `user-new.php`, `user-edit.php`): the list with its columns, search and role
//! views with their counts, "Change role to", Add New User and Edit User; and the Roles screen,
//! where an administrator changes a role's capabilities.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use std::collections::BTreeSet;

use admin_support::{get, location, post, run, sign_in, sign_out, token, try_sign_in, user};
use ignite_panel::authz;
use mission_control_core::content::get_user_meta;
use mission_control_core::users::{default_roles, is_capability_name};
use orbit_orm::{Database, raw};
use rocket::http::Status;
use serde_json::json;

/// The capabilities the role `role` has stored.
async fn stored_caps(db: &Database, role: &str) -> BTreeSet<String> {
    authz::role_permissions(db, role)
        .await
        .expect("reads the role")
        .into_iter()
        .filter(|name| is_capability_name(name))
        .collect()
}

async fn role_id(db: &Database, role: &str) -> i64 {
    raw::fetch_scalar(db, "SELECT id FROM roles WHERE name = ?", &[role.into()])
        .await
        .unwrap()
}

async fn user_id(db: &Database, name: &str) -> i64 {
    raw::fetch_scalar(db, "SELECT id FROM users WHERE name = ?", &[name.into()])
        .await
        .unwrap()
}

/// The list's column headings.
fn headings(html: &str) -> Vec<String> {
    html.split("<th>")
        .skip(1)
        .map(|cell| {
            let cell = &cell[..cell.find("</th>").unwrap_or(cell.len())];
            let mut text = String::new();
            let mut in_tag = false;
            for c in cell.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => text.push(c),
                    _ => {}
                }
            }
            text.trim().to_owned()
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// The text of the tab `label` of the Users list, with its count.
fn tab(label: &str, count: usize) -> String {
    format!("{label} <span class=\"ig-tab-count\">({count})</span>")
}

/// The five roles, as `populate_roles` makes them, are stored with their capabilities.
async fn the_five_roles_have_wordpresss_capabilities(db: &Database) {
    for role in default_roles() {
        let expected: BTreeSet<String> = role.granted().map(str::to_owned).collect();
        assert_eq!(stored_caps(db, &role.name).await, expected, "{}", role.name);
    }
    let author = stored_caps(db, "author").await;
    assert!(author.contains("publish_posts") && author.contains("upload_files"));
    assert!(!author.contains("manage_options") && !author.contains("edit_others_posts"));
    assert_eq!(
        stored_caps(db, "subscriber").await,
        BTreeSet::from(["read".to_owned(), "level_0".to_owned()])
    );
    let administrator = stored_caps(db, "administrator").await;
    for cap in [
        "manage_options",
        "list_users",
        "create_users",
        "edit_users",
        "delete_users",
        "promote_users",
        "edit_theme_options",
    ] {
        assert!(administrator.contains(cap), "{cap}");
    }
    assert!(!stored_caps(db, "editor").await.contains("list_users"));
}

#[test]
fn the_roles_and_the_users_screens() {
    run("users_roles", |client, db| async move {
        the_five_roles_have_wordpresss_capabilities(&db).await;
        let (_, admin) = user(&db, "ada", "administrator").await;
        let (_, author) = user(&db, "aubrey", "author").await;
        sign_in(&client, &admin).await;

        // The list: WordPress's columns, and the role views with their counts.
        let (status, list) = get(&client, "/mc-admin/users").await;
        assert_eq!(status, Status::Ok, "{list}");
        assert_eq!(
            headings(&list),
            ["Username", "Name", "Email", "Role", "Posts"],
            "{list}"
        );
        for expected in [
            "aubrey@example.test".to_owned(),
            tab("All", 2),
            tab("Administrator", 1),
            tab("Author", 1),
        ] {
            assert!(list.contains(&expected), "`{expected}` in {list}");
        }
        assert!(!list.contains(&tab("Editor", 0)), "{list}");
        assert!(list.contains(">Change role to Editor<"), "{list}");
        // A search and a role view.
        let (_, found) = get(&client, "/mc-admin/users?q=aubrey").await;
        assert!(found.contains("aubrey@example.test"), "{found}");
        assert!(!found.contains("ada@example.test"), "{found}");
        let (_, authors) = get(&client, "/mc-admin/users?tab=author").await;
        assert!(authors.contains("aubrey@example.test"), "{authors}");
        assert!(!authors.contains("ada@example.test"), "{authors}");

        // Add New User: WordPress's fields, its "Generate password" and the role select.
        let (status, new) = get(&client, "/mc-admin/users/new").await;
        assert_eq!(status, Status::Ok, "{new}");
        for expected in [
            ">Username *<",
            ">Email *<",
            ">First Name<",
            ">Last Name<",
            ">Website<",
            ">Password *<",
            ">Role<",
            ">Generate password</button>",
            "<option value=\"editor\">Editor</option>",
            // The New User Default Role (Subscriber) is chosen, with no blank choice.
            "<option value=\"subscriber\" selected>Subscriber</option>",
        ] {
            assert!(new.contains(expected), "`{expected}` in {new}");
        }
        assert!(!new.contains("Super Admin"), "{new}");
        let csrf = token(&client).await;
        let fields = |name: &'static str, email: &'static str| {
            vec![
                ("name", name),
                ("email", email),
                ("first_name", "Eddie"),
                ("last_name", "Editor"),
                ("url", "https://eddie.example"),
                ("password", "a new password 1"),
                ("role", "editor"),
            ]
        };
        let mut sent = vec![("_token", csrf.as_str())];
        sent.extend(fields("eddie", "eddie@example.test"));
        let response = post(&client, "/mc-admin/users", &sent).await;
        assert_eq!(response.status(), Status::SeeOther, "adds the user");
        drop(response);
        let eddie = user_id(&db, "eddie").await;
        assert_eq!(authz::roles_of(&db, eddie).await.unwrap(), ["editor"]);
        assert_eq!(
            get_user_meta(&db, eddie, "first_name").await.unwrap(),
            Some(json!("Eddie"))
        );
        assert_eq!(
            get_user_meta(&db, eddie, "user_url").await.unwrap(),
            Some(json!("https://eddie.example"))
        );
        // A username already taken is refused, as WordPress refuses it.
        let mut again = vec![("_token", csrf.as_str())];
        again.extend(fields("eddie", "other@example.test"));
        let response = post(&client, "/mc-admin/users", &again).await;
        assert_eq!(response.status(), Status::UnprocessableEntity);
        let body = response.into_string().await.unwrap_or_default();
        assert!(
            body.contains("Sorry, that username already exists!"),
            "{body}"
        );
        let (_, list) = get(&client, "/mc-admin/users").await;
        assert!(list.contains(">Eddie Editor<"), "{list}");
        assert!(list.contains(&tab("Editor", 1)), "{list}");

        // Edit User: the same fields with the stored values, the username kept.
        let (status, edit) = get(&client, &format!("/mc-admin/users/{eddie}/edit")).await;
        assert_eq!(status, Status::Ok, "{edit}");
        for expected in [
            "value=\"Eddie\"",
            "value=\"https://eddie.example\"",
            "<option value=\"editor\" selected>Editor</option>",
            ">Generate password</button>",
        ] {
            assert!(edit.contains(expected), "`{expected}` in {edit}");
        }
        let response = post(
            &client,
            &format!("/mc-admin/users/{eddie}"),
            &[
                ("_token", &csrf),
                ("name", "renamed"),
                ("email", "eddie@example.test"),
                ("first_name", "Edward"),
                ("last_name", "Editor"),
                ("url", ""),
                ("password", ""),
                ("role", "author"),
            ],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther, "saves the user");
        drop(response);
        assert_eq!(user_id(&db, "eddie").await, eddie, "the username stays");
        assert_eq!(authz::roles_of(&db, eddie).await.unwrap(), ["author"]);
        assert_eq!(
            get_user_meta(&db, eddie, "first_name").await.unwrap(),
            Some(json!("Edward"))
        );

        // "Change role to": the row action of the list.
        let response = post(
            &client,
            &format!("/mc-admin/users/{eddie}/actions/role-contributor"),
            &[("_token", &csrf)],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        assert!(location(&response).starts_with("/mc-admin/users"));
        drop(response);
        assert_eq!(authz::roles_of(&db, eddie).await.unwrap(), ["contributor"]);
        // The administrator cannot give themselves a role without user editing.
        let ada = user_id(&db, "ada").await;
        let response = post(
            &client,
            &format!("/mc-admin/users/{ada}/actions/role-editor"),
            &[("_token", &csrf)],
        )
        .await;
        drop(response);
        assert_eq!(authz::roles_of(&db, ada).await.unwrap(), ["administrator"]);
        sign_out(&client).await;

        // The new user signs in with the password given, with their role's capabilities.
        assert_eq!(
            try_sign_in(&client, "eddie@example.test", "a new password 1").await,
            Status::SeeOther
        );
        assert_eq!(get(&client, "/mc-admin/post").await.0, Status::Ok);
        assert_eq!(get(&client, "/mc-admin/users").await.0, Status::Forbidden);
        sign_out(&client).await;

        // The Roles screen: each role with its capabilities, which an administrator changes.
        sign_in(&client, &admin).await;
        let (status, roles) = get(&client, "/mc-admin/roles").await;
        assert_eq!(status, Status::Ok, "{roles}");
        for label in [
            "Administrator",
            "Editor",
            "Author",
            "Contributor",
            "Subscriber",
        ] {
            assert!(roles.contains(label), "{label} in {roles}");
        }
        let author_role = role_id(&db, "author").await;
        let (status, form) = get(&client, &format!("/mc-admin/roles/{author_role}/edit")).await;
        assert_eq!(status, Status::Ok, "{form}");
        assert!(form.contains("value=\"manage_options\""), "{form}");
        let mut caps: Vec<String> = stored_caps(&db, "author").await.into_iter().collect();
        caps.push("manage_options".to_owned());
        let mut sent: Vec<(&str, &str)> = vec![("_token", &csrf), ("name", "author")];
        sent.extend(caps.iter().map(|cap| ("abilities[]", cap.as_str())));
        let response = post(&client, &format!("/mc-admin/roles/{author_role}"), &sent).await;
        assert_eq!(response.status(), Status::SeeOther, "saves the role");
        drop(response);
        assert!(stored_caps(&db, "author").await.contains("manage_options"));
        sign_out(&client).await;

        // The author now opens Settings, as WordPress lets a role with manage_options.
        sign_in(&client, &author).await;
        let (status, settings) = get(&client, "/mc-admin/options-general/1/edit").await;
        assert_eq!(status, Status::Ok, "{settings}");
        sign_out(&client).await;
    });
}
