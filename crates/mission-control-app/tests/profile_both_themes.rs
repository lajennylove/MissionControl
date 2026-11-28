//! The profile in both admin themes (Task 8.5): the Ignite theme's profile page shows WordPress's
//! fields in WordPress's sections (Personal Options, Name with the username read only, Contact
//! Info, About Yourself, Account Management with Sessions, then Application Passwords), saves
//! them as the WordPress theme's form does (the same user meta and the same checks), and the
//! WordPress theme shows what the Ignite theme saved.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{choose_theme, get, location, post, run, sign_in, token_in, user};
use ignite_panel::users;
use mission_control_core::users::app_password;
use mission_control_core::users::profile::Profile;
use orbit_orm::raw;
use rocket::http::Status;
use rocket::local::asynchronous::Client;

const PROFILE: &str = "/mc-admin/profile";

/// The profile page, as the signed-in user sees it.
async fn page(client: &Client) -> String {
    let (status, html) = get(client, PROFILE).await;
    assert_eq!(status, Status::Ok, "{html}");
    html
}

/// The Ignite theme's "Your details" form: everything it sends when nothing is changed, with
/// `changes` in place.
fn details<'a>(
    username: &'a str,
    email: &'a str,
    changes: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("name", username),
        ("email", email),
        ("admin_bar_front", "on"),
        ("first_name", ""),
        ("last_name", ""),
        ("nickname", username),
        ("display_name", username),
        ("url", ""),
        ("description", ""),
    ];
    for (name, value) in changes {
        fields.retain(|(n, _)| n != name);
        if !value.is_empty() || !matches!(*name, "rich_editing" | "comment_shortcuts") {
            fields.push((name, value));
        }
    }
    fields
}

/// Posts the Ignite theme's "Your details" form with `fields`.
async fn save_details<'c>(
    client: &'c Client,
    fields: &[(&str, &str)],
) -> rocket::local::asynchronous::LocalResponse<'c> {
    let csrf = token_in(&page(client).await);
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    post(client, PROFILE, &all).await
}

/// The options of the select `name`: (value, selected).
fn select_options(html: &str, name: &str) -> Vec<(String, bool)> {
    let start = html
        .find(&format!("name=\"{name}\""))
        .and_then(|at| {
            html[..at]
                .rfind("<select")
                .filter(|s| html[*s..at].len() < 80)
        })
        .unwrap_or_else(|| panic!("the {name} select in {html}"));
    let select = &html[start..];
    let select = &select[..select.find("</select>").unwrap()];
    select
        .split("<option")
        .skip(1)
        .map(|option| {
            let (attributes, _) = option.split_once('>').unwrap();
            let value = attributes
                .split("value=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default()
                .to_owned();
            (value, attributes.contains("selected"))
        })
        .collect()
}

/// The tag of the input named `name`.
fn input<'h>(html: &'h str, name: &str) -> &'h str {
    let at = html
        .find(&format!("name=\"{name}\""))
        .unwrap_or_else(|| panic!("{name} in {html}"));
    let start = html[..at].rfind('<').unwrap();
    &html[start..at + html[at..].find('>').unwrap() + 1]
}

#[test]
fn the_ignite_theme_shows_and_saves_wordpresss_profile_fields() {
    run("profile_both_themes", |client, db| async move {
        let (id, email) = user(&db, "ada", "administrator").await;
        sign_in(&client, &email).await;

        // WordPress's sections and fields, in its order, in Ignite's page.
        let html = page(&client).await;
        let order = [
            ">Personal Options</h3>",
            "Disable the visual editor when writing",
            "Enable keyboard shortcuts for comment moderation.",
            "Show Toolbar when viewing site",
            ">Name</h3>",
            ">Username</label>",
            "Usernames cannot be changed.",
            ">First Name</label>",
            ">Last Name</label>",
            ">Nickname (required)</label>",
            ">Display name publicly as</label>",
            ">Contact Info</h3>",
            ">Email *</label>",
            ">Website</label>",
            ">About Yourself</h3>",
            ">Biographical Info</label>",
            ">Update Profile</button>",
            "<h2 class=\"mb-5\">Account Management</h2>",
            ">New Password</h3>",
            ">Set New Password</button>",
            ">Sessions</h3>",
            "You are only logged in at this location.",
            "<h2 class=\"m-0\">Picture</h2>",
            "<h2 class=\"m-0\">Application Passwords</h2>",
            ">Add Application Password</button>",
        ];
        let mut at = 0;
        for expected in order {
            let found = html[at..]
                .find(expected)
                .unwrap_or_else(|| panic!("`{expected}` after {at} in {html}"));
            at += found;
        }
        assert!(input(&html, "name").contains(" readonly"), "{html}");
        assert!(!html.contains(">Save details</button>"), "{html}");
        assert_eq!(
            select_options(&html, "display_name"),
            [("ada".to_owned(), true)]
        );

        // The save: the same meta, and the same rules, as the WordPress theme's.
        let response = save_details(
            &client,
            &details(
                "someone-else",
                &email,
                &[
                    ("rich_editing", "on"),
                    ("comment_shortcuts", "on"),
                    ("admin_bar_front", ""),
                    ("first_name", "Ada"),
                    ("last_name", "Lovelace"),
                    ("nickname", "Countess"),
                    ("display_name", "ada"),
                    ("url", "example.com/ada"),
                    ("description", "Hi <script>x</script><em>there</em>"),
                ],
            ),
        )
        .await;
        let status = response.status();
        let to = location(&response);
        let body = response.into_string().await.unwrap_or_default();
        assert_eq!(status, Status::SeeOther, "{body}");
        assert_eq!(to, PROFILE);
        let row = users::find_by_id(&*db, id).await.unwrap().unwrap();
        // The username is not changed from the profile.
        assert_eq!(row.name, "ada");
        let saved = Profile::load(&db, id, &row.name, &row.email).await.unwrap();
        assert_eq!(saved.first_name, "Ada");
        assert_eq!(saved.last_name, "Lovelace");
        assert_eq!(saved.nickname, "Countess");
        assert_eq!(saved.url, "http://example.com/ada");
        assert_eq!(saved.description, "Hi <em>there</em>");
        assert!(!saved.rich_editing && saved.comment_shortcuts && !saved.admin_bar_front);
        let html = page(&client).await;
        assert!(html.contains("Profile saved."), "{html}");
        assert!(
            input(&html, "first_name").contains("value=\"Ada\""),
            "{html}"
        );

        // "Display name publicly as" offers the names entered, as WordPress builds them.
        let names: Vec<String> = select_options(&html, "display_name")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "Countess",
                "ada",
                "Ada",
                "Lovelace",
                "Ada Lovelace",
                "Lovelace Ada"
            ]
        );
        let response = save_details(
            &client,
            &details(
                "ada",
                &email,
                &[
                    ("first_name", "Ada"),
                    ("last_name", "Lovelace"),
                    ("nickname", "Countess"),
                    ("display_name", "Ada Lovelace"),
                ],
            ),
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        drop(response);
        let saved = Profile::load(&db, id, "ada", &email).await.unwrap();
        assert_eq!(saved.display_name, "Ada Lovelace");
        // A name that is not one of them is refused.
        let response = save_details(
            &client,
            &details(
                "ada",
                &email,
                &[("nickname", "Countess"), ("display_name", "Mallory")],
            ),
        )
        .await;
        assert_eq!(response.status(), Status::UnprocessableEntity);
        drop(response);

        // WordPress's messages, on their fields, with nothing saved.
        let response = save_details(
            &client,
            &details(
                "ada",
                &email,
                &[("nickname", ""), ("url", "javascript:alert(1)")],
            ),
        )
        .await;
        assert_eq!(response.status(), Status::UnprocessableEntity);
        let html = response.into_string().await.unwrap();
        assert!(html.contains("Please enter a nickname."), "{html}");
        // The refused page keeps WordPress's sections.
        assert!(html.contains(">Personal Options</h3>"), "{html}");
        let saved = Profile::load(&db, id, "ada", &email).await.unwrap();
        assert_eq!(saved.nickname, "Countess");
        let response = save_details(
            &client,
            &details(
                "ada",
                &email,
                &[("nickname", "Countess"), ("url", "javascript:alert(1)")],
            ),
        )
        .await;
        assert_eq!(response.status(), Status::UnprocessableEntity);
        let html = response.into_string().await.unwrap();
        assert!(html.contains("Please enter a valid URL."), "{html}");

        // The WordPress theme shows what the Ignite theme saved.
        choose_theme(&client, "wordpress").await;
        let html = page(&client).await;
        assert!(html.contains("id=\"your-profile\""), "{html}");
        assert!(
            html.contains("name=\"first_name\" id=\"first_name\" value=\"Ada\""),
            "{html}"
        );
        assert!(
            html.contains("<option  selected=\"selected\">Ada Lovelace</option>"),
            "{html}"
        );
        assert!(
            html.contains("name=\"nickname\" id=\"nickname\" value=\"Countess\""),
            "{html}"
        );
        choose_theme(&client, "ignite").await;
    });
}

#[test]
fn the_ignite_theme_has_the_sessions_and_application_passwords() {
    run("profile_both_themes_extras", |client, db| async move {
        let (id, email) = user(&db, "grace", "editor").await;
        sign_in(&client, &email).await;

        // Add an application password: shown once, then listed.
        let csrf = token_in(&page(&client).await);
        let response = post(
            &client,
            "/mc-admin/profile/application-passwords",
            &[
                ("_token", &csrf),
                ("new_application_password_name", "Deploy bot"),
            ],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        drop(response);
        let html = page(&client).await;
        assert!(
            html.contains("Your new password for Deploy bot is:"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"py-2 pr-3 font-semibold\">Deploy bot</td>"),
            "{html}"
        );
        let html = page(&client).await;
        assert!(
            !html.contains("Your new password for"),
            "shown once: {html}"
        );

        // A blank name: WordPress's message.
        let response = post(
            &client,
            "/mc-admin/profile/application-passwords",
            &[("_token", &csrf), ("new_application_password_name", "")],
        )
        .await;
        drop(response);
        let html = page(&client).await;
        assert!(
            html.contains("An application name is required to create an application password."),
            "{html}"
        );

        // Revoke it.
        let uuid = app_password::list(&db, id).await.unwrap()[0].uuid.clone();
        let response = post(
            &client,
            "/mc-admin/profile/application-passwords/revoke",
            &[("_token", &csrf), ("uuid", &uuid)],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        drop(response);
        let html = page(&client).await;
        assert!(html.contains("Application password revoked."), "{html}");
        assert!(app_password::list(&db, id).await.unwrap().is_empty());

        // Sessions: a sign-in elsewhere, then "Log Out Everywhere Else".
        raw::execute(
            &*db,
            "INSERT INTO sessions (id, user_id, expires_at) VALUES ('elsewhere', ?, ?)",
            &[
                id.into(),
                (chrono::Utc::now() + chrono::Duration::hours(1))
                    .naive_utc()
                    .into(),
            ],
        )
        .await
        .unwrap();
        let html = page(&client).await;
        assert!(html.contains("id=\"destroy-sessions\""), "{html}");
        let response = post(
            &client,
            "/mc-admin/profile/destroy-sessions",
            &[("_token", &csrf)],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        drop(response);
        let html = page(&client).await;
        assert!(
            html.contains("You are now logged out everywhere else."),
            "{html}"
        );
        assert!(
            html.contains("You are only logged in at this location."),
            "{html}"
        );
    });
}
