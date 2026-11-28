//! The WordPress admin theme's profile page (Task 8.5), as WordPress's `profile.php`: every field
//! is shown, saved (as `edit_user` saves it) and shown again after a reload; "Display name
//! publicly as" lists the names the user has entered; an invalid email or website is refused
//! with WordPress's message on the field; the password, sessions and application passwords work;
//! and the Ignite theme keeps Ignite's profile page.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{
    PASSWORD, choose_theme, get, location, post, run, sign_in, sign_out, token_in, try_sign_in,
    user,
};
use mission_control_core::content::get_user_meta;
use mission_control_core::users::app_password;
use orbit_orm::{Database, raw};
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use serde_json::{Value, json};

const PROFILE: &str = "/mc-admin/profile";

/// The profile page, as the signed-in user sees it.
async fn page(client: &Client) -> String {
    let (status, html) = get(client, PROFILE).await;
    assert_eq!(status, Status::Ok, "{html}");
    html
}

/// Posts the profile form with `fields` and the page's token; the answer's address.
async fn save(client: &Client, fields: &[(&str, &str)]) -> String {
    let csrf = token_in(&page(client).await);
    let mut all = vec![("_token", csrf.as_str())];
    all.extend_from_slice(fields);
    let response = post(client, PROFILE, &all).await;
    assert_eq!(response.status(), Status::SeeOther);
    location(&response)
}

/// The form's fields when nothing is changed, for user `name` with `email`.
fn unchanged<'a>(name: &'a str, email: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("nickname", name),
        ("display_name", name),
        ("email", email),
        ("admin_theme", "wordpress"),
        ("admin_bar_front", "1"),
    ]
}

/// The `<select>` of `id`'s options: (label, selected).
fn options(html: &str, id: &str) -> Vec<(String, bool)> {
    let start = html
        .find(&format!("<select name=\"{id}\" id=\"{id}\">"))
        .unwrap_or_else(|| panic!("the {id} select in {html}"));
    let select = &html[start..];
    let select = &select[..select.find("</select>").unwrap()];
    select
        .split("<option")
        .skip(1)
        .map(|option| {
            let (attributes, rest) = option.split_once('>').unwrap();
            let label = rest.trim_end_matches("</option>").to_owned();
            (label, attributes.contains("selected"))
        })
        .collect()
}

/// The value of the input `id`.
fn value(html: &str, id: &str) -> String {
    let tag = input(html, id);
    let start = tag
        .find(" value=\"")
        .unwrap_or_else(|| panic!("a value in {tag}"))
        + " value=\"".len();
    tag[start..start + tag[start..].find('"').unwrap()].to_owned()
}

/// The input tag of `id`.
fn input<'h>(html: &'h str, id: &str) -> &'h str {
    let at = html
        .find(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("{id} in {html}"));
    let start = html[..at].rfind('<').unwrap();
    &html[start..at + html[at..].find('>').unwrap() + 1]
}

async fn meta(db: &Database, id: i64, key: &str) -> Option<Value> {
    get_user_meta(db, id, key).await.unwrap()
}

#[test]
fn every_field_is_saved_and_shown_again() {
    run("profile_wordpress_fields", |client, db| async move {
        let (id, email) = user(&db, "ada", "administrator").await;
        sign_in(&client, &email).await;
        choose_theme(&client, "wordpress").await;

        // WordPress's page, in its order, with WordPress's defaults.
        let html = page(&client).await;
        let order = [
            "<h2>Personal Options</h2>",
            ">Visual Editor</th>",
            ">Keyboard Shortcuts</th>",
            ">Toolbar</th>",
            "<h2>Name</h2>",
            "<label for=\"user_login\">Username</label>",
            "<label for=\"first_name\">First Name</label>",
            "<label for=\"last_name\">Last Name</label>",
            "<label for=\"nickname\">Nickname <span class=\"description\">(required)</span></label>",
            "<label for=\"display_name\">Display name publicly as</label>",
            "<h2>Contact Info</h2>",
            "<label for=\"email\">Email <span class=\"description\">(required)</span></label>",
            "<label for=\"url\">Website</label>",
            "<h2>About Yourself</h2>",
            "<label for=\"description\">Biographical Info</label>",
            "<th>Profile Picture</th>",
            "<h2>Account Management</h2>",
            ">Set New Password</button>",
            ">Generate Password</button>",
            "<label for=\"pass2\">Repeat New Password</label>",
            "<th>Sessions</th>",
            "<h2>Application Passwords</h2>",
            "value=\"Update Profile\"",
        ];
        let mut at = 0;
        for expected in order {
            let found = html[at..]
                .find(expected)
                .unwrap_or_else(|| panic!("`{expected}` after {at} in {html}"));
            at += found;
        }
        assert!(
            input(&html, "user_login").contains("disabled=\"disabled\""),
            "{html}"
        );
        assert!(html.contains("Usernames cannot be changed."), "{html}");
        assert_eq!(value(&html, "nickname"), "ada");
        assert_eq!(options(&html, "display_name"), [("ada".to_owned(), true)]);
        assert!(!input(&html, "rich_editing").contains("checked"), "{html}");
        assert!(!input(&html, "comment_shortcuts").contains("checked"));
        assert!(input(&html, "admin_bar_front").contains("checked"));
        assert!(!html.contains("Profile updated."), "{html}");
        // No language besides English is installed: no Language row, as WordPress.
        assert!(!html.contains("user-language-wrap"), "{html}");

        // Every field saved.
        let address = save(
            &client,
            &[
                ("rich_editing", "false"),
                ("comment_shortcuts", "true"),
                ("admin_theme", "wordpress"),
                ("first_name", "Ada"),
                ("last_name", "Lovelace"),
                ("nickname", "Countess"),
                ("display_name", "Ada Lovelace"),
                ("email", "countess@example.test"),
                ("url", "example.com/ada"),
                ("description", "Wrote the first program."),
            ],
        )
        .await;
        assert_eq!(address, "/mc-admin/profile?updated=1");

        // ... and shown after a reload, with WordPress's message.
        let (_, html) = get(&client, &address).await;
        assert!(
            html.contains("<div id=\"message\" class=\"updated notice notice-success is-dismissible\"><p><strong>Profile updated.</strong></p></div>"),
            "{html}"
        );
        assert!(input(&html, "rich_editing").contains("checked"), "{html}");
        assert!(input(&html, "comment_shortcuts").contains("checked"));
        assert!(!input(&html, "admin_bar_front").contains("checked"));
        assert_eq!(value(&html, "user_login"), "ada");
        assert_eq!(value(&html, "first_name"), "Ada");
        assert_eq!(value(&html, "last_name"), "Lovelace");
        assert_eq!(value(&html, "nickname"), "Countess");
        assert_eq!(value(&html, "email"), "countess@example.test");
        assert_eq!(value(&html, "url"), "http://example.com/ada");
        assert!(
            html.contains(">Wrote the first program.</textarea>"),
            "{html}"
        );
        let selected: Vec<String> = options(&html, "display_name")
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| name)
            .collect();
        assert_eq!(selected, ["Ada Lovelace"]);
        // A reload without `updated=1` shows no message.
        assert!(!page(&client).await.contains("Profile updated."));

        // Stored as WordPress stores them: user meta, and the email on the user.
        for (key, expected) in [
            ("first_name", "Ada"),
            ("last_name", "Lovelace"),
            ("nickname", "Countess"),
            ("display_name", "Ada Lovelace"),
            ("user_url", "http://example.com/ada"),
            ("description", "Wrote the first program."),
            ("rich_editing", "false"),
            ("comment_shortcuts", "true"),
            ("show_admin_bar_front", "false"),
        ] {
            assert_eq!(meta(&db, id, key).await, Some(json!(expected)), "{key}");
        }
        let stored: String =
            raw::fetch_scalar(&*db, "SELECT email FROM users WHERE id = ?", &[id.into()])
                .await
                .unwrap();
        assert_eq!(stored, "countess@example.test");

        // The boxes turned back, and the website cleared.
        let mut fields = unchanged("Countess", "countess@example.test");
        fields.push(("url", ""));
        save(&client, &fields).await;
        let html = page(&client).await;
        assert!(!input(&html, "rich_editing").contains("checked"));
        assert!(!input(&html, "comment_shortcuts").contains("checked"));
        assert!(input(&html, "admin_bar_front").contains("checked"));
        assert_eq!(value(&html, "url"), "");
        assert_eq!(meta(&db, id, "rich_editing").await, Some(json!("true")));
        assert_eq!(meta(&db, id, "comment_shortcuts").await, Some(json!("")));
        sign_out(&client).await;
    });
}

#[test]
fn the_display_name_choices_are_the_names_entered() {
    run("profile_wordpress_names", |client, db| async move {
        let (_, email) = user(&db, "grace", "author").await;
        sign_in(&client, &email).await;
        choose_theme(&client, "wordpress").await;
        save(
            &client,
            &[
                ("first_name", "Grace"),
                ("last_name", "Hopper"),
                ("nickname", "Amazing Grace"),
                ("display_name", "grace"),
                ("email", &email),
                ("admin_theme", "wordpress"),
            ],
        )
        .await;
        let names: Vec<String> = options(&page(&client).await, "display_name")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "Amazing Grace",
                "grace",
                "Grace",
                "Hopper",
                "Grace Hopper",
                "Hopper Grace"
            ]
        );
        // Without a last name, the combined names go.
        save(
            &client,
            &[
                ("first_name", "Grace"),
                ("nickname", "Amazing Grace"),
                ("display_name", "Grace"),
                ("email", &email),
                ("admin_theme", "wordpress"),
            ],
        )
        .await;
        let choices = options(&page(&client).await, "display_name");
        assert_eq!(
            choices,
            [
                ("Amazing Grace".to_owned(), false),
                ("grace".to_owned(), false),
                ("Grace".to_owned(), true),
            ]
        );
    });
}

#[test]
fn an_invalid_email_or_website_is_refused_on_its_field() {
    run("profile_wordpress_errors", |client, db| async move {
        let (id, email) = user(&db, "linus", "editor").await;
        let (_, taken) = user(&db, "other", "subscriber").await;
        sign_in(&client, &email).await;
        choose_theme(&client, "wordpress").await;

        let address = save(
            &client,
            &[
                ("first_name", "Linus"),
                ("nickname", "linus"),
                ("email", "not-an-email"),
                ("url", "javascript:alert(1)"),
                ("admin_theme", "wordpress"),
            ],
        )
        .await;
        assert_eq!(address, PROFILE);
        let html = page(&client).await;
        assert!(
            html.contains("<p><strong>Error:</strong> The email address is not correct.</p>"),
            "{html}"
        );
        assert!(
            html.contains("<p class=\"description mc-field-error\" id=\"email-error\">The email address is not correct.</p>"),
            "{html}"
        );
        assert!(input(&html, "email").contains("aria-invalid=\"true\""));
        assert!(
            html.contains("<p class=\"description mc-field-error\" id=\"url-error\">Please enter a valid URL.</p>"),
            "{html}"
        );
        // The values typed are kept, and nothing is saved.
        assert_eq!(value(&html, "email"), "not-an-email");
        assert_eq!(value(&html, "first_name"), "Linus");
        assert_eq!(meta(&db, id, "first_name").await, None);
        // The errors show once.
        assert!(!page(&client).await.contains("id=\"email-error\""));

        // An email another user has, an empty one, and an empty nickname.
        save(
            &client,
            &[
                ("nickname", "linus"),
                ("email", &taken),
                ("admin_theme", "wordpress"),
            ],
        )
        .await;
        let html = page(&client).await;
        assert!(
            html.contains("id=\"email-error\">This email is already registered. Please choose another one.</p>"),
            "{html}"
        );
        save(&client, &[("email", ""), ("admin_theme", "wordpress")]).await;
        let html = page(&client).await;
        assert!(
            html.contains("id=\"email-error\">Please enter an email address.</p>"),
            "{html}"
        );
        assert!(
            html.contains("id=\"nickname-error\">Please enter a nickname.</p>"),
            "{html}"
        );
        let stored: String =
            raw::fetch_scalar(&*db, "SELECT email FROM users WHERE id = ?", &[id.into()])
                .await
                .unwrap();
        assert_eq!(stored, email);
    });
}

#[test]
fn the_password_sessions_and_application_passwords_work() {
    run("profile_wordpress_account", |client, db| async move {
        let (id, email) = user(&db, "margaret", "administrator").await;
        sign_in(&client, &email).await;
        choose_theme(&client, "wordpress").await;

        // Passwords that differ are refused on the field.
        let mut fields = unchanged("margaret", &email);
        fields.extend([("pass1", "a new password"), ("pass2", "another one")]);
        save(&client, &fields).await;
        let html = page(&client).await;
        assert!(
            html.contains("id=\"pass1-error\">Passwords do not match. Please enter the same password in both password fields.</p>"),
            "{html}"
        );

        // Another sign-in, elsewhere: "Log Out Everywhere Else" is offered.
        raw::execute(
            &*db,
            "INSERT INTO sessions (id, user_id, expires_at) VALUES ('elsewhere', ?, ?)",
            &[
                id.into(),
                (chrono::Utc::now().naive_utc() + chrono::Duration::hours(1)).into(),
            ],
        )
        .await
        .unwrap();
        let html = page(&client).await;
        assert!(
            html.contains("<button type=\"submit\" class=\"button\" id=\"destroy-sessions\" form=\"mc-destroy-sessions-form\">Log Out Everywhere Else</button>"),
            "{html}"
        );
        let csrf = token_in(&html);
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
        let others: i64 = raw::fetch_scalar(
            &*db,
            "SELECT COUNT(*) FROM sessions WHERE id = 'elsewhere'",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(others, 0);

        // A new password: saved, and it signs in.
        let mut fields = unchanged("margaret", &email);
        fields.extend([
            ("pass1", "a brand new secret"),
            ("pass2", "a brand new secret"),
        ]);
        assert_eq!(save(&client, &fields).await, "/mc-admin/profile?updated=1");
        sign_out(&client).await;
        assert_ne!(
            try_sign_in(&client, &email, PASSWORD).await,
            Status::SeeOther
        );
        assert_eq!(
            try_sign_in(&client, &email, "a brand new secret").await,
            Status::SeeOther
        );

        // Application passwords: a name is required; a new one is shown once, then listed.
        let csrf = token_in(&page(&client).await);
        let create = "/mc-admin/profile/application-passwords";
        let response = post(
            &client,
            create,
            &[("_token", &csrf), ("new_application_password_name", "")],
        )
        .await;
        assert_eq!(
            location(&response),
            "/mc-admin/profile#application-passwords-section"
        );
        drop(response);
        let html = page(&client).await;
        assert!(
            html.contains("An application name is required to create an application password."),
            "{html}"
        );
        let response = post(
            &client,
            create,
            &[
                ("_token", &csrf),
                ("new_application_password_name", "Laptop"),
            ],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        drop(response);
        let html = page(&client).await;
        assert!(
            html.contains("<label for=\"new-application-password-value\">Your new password for Laptop is:</label>"),
            "{html}"
        );
        assert!(
            html.contains(
                "Be sure to save this in a safe location. You will not be able to retrieve it."
            ),
            "{html}"
        );
        let shown = value(&html, "new-application-password-value");
        assert_eq!(shown.len(), 29, "{shown}");
        let items = app_password::list(&db, id).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(app_password::check_password(
            &shown.replace(' ', ""),
            &raw::fetch_scalar::<String>(
                &*db,
                "SELECT password FROM application_passwords WHERE id = ?",
                &[items[0].id.into()]
            )
            .await
            .unwrap()
        ));
        let html = page(&client).await;
        assert!(
            !html.contains("new-application-password-value"),
            "shown once"
        );
        assert!(
            html.contains("<td class=\"name column-name column-primary\">Laptop</td>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"last_used column-last_used\">\u{2014}</td>"),
            "{html}"
        );

        // Revoked.
        let response = post(
            &client,
            "/mc-admin/profile/application-passwords/revoke",
            &[("_token", &csrf), ("uuid", &items[0].uuid)],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        drop(response);
        let html = page(&client).await;
        assert!(html.contains("Application password revoked."), "{html}");
        assert!(app_password::list(&db, id).await.unwrap().is_empty());
        for name in ["One", "Two"] {
            post(
                &client,
                create,
                &[("_token", &csrf), ("new_application_password_name", name)],
            )
            .await;
        }
        let response = post(
            &client,
            "/mc-admin/profile/application-passwords/revoke-all",
            &[("_token", &csrf)],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        drop(response);
        assert!(
            page(&client)
                .await
                .contains("All application passwords revoked.")
        );
        assert!(app_password::list(&db, id).await.unwrap().is_empty());
    });
}

#[test]
fn the_ignite_theme_keeps_ignites_profile_page() {
    run("profile_wordpress_themes", |client, db| async move {
        let (id, email) = user(&db, "alan", "administrator").await;
        sign_in(&client, &email).await;

        // The Ignite theme: Ignite's page and its save.
        let html = page(&client).await;
        assert!(!html.contains("id=\"your-profile\""), "{html}");
        assert!(!html.contains("mc-wordpress-profile"), "{html}");
        assert!(html.contains(">Save details</button>"), "{html}");
        let csrf = token_in(&html);
        let response = post(
            &client,
            PROFILE,
            &[("_token", &csrf), ("name", "alan"), ("email", &email)],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        assert_eq!(location(&response), PROFILE);
        drop(response);
        assert!(page(&client).await.contains("Profile saved."));

        // The WordPress theme: WordPress's page, in the theme's chrome.
        choose_theme(&client, "wordpress").await;
        let html = page(&client).await;
        assert!(html.contains("<body class=\"wp-admin\">"), "{html}");
        assert!(
            html.contains("<form id=\"your-profile\" action=\"/mc-admin/profile\" method=\"post\""),
            "{html}"
        );
        assert!(!html.contains(">Save details</button>"), "{html}");
        assert!(!html.contains("<!-- mc-wordpress-profile -->"), "{html}");
        // "Your Profile" under Users leads here.
        let response = client.get("/mc-admin/your-profile").dispatch().await;
        assert_eq!(location(&response), PROFILE);
        drop(response);

        // Choosing the Ignite theme on WordPress's page goes back to Ignite's.
        let mut fields = unchanged("alan", &email);
        fields.retain(|(name, _)| *name != "admin_theme");
        fields.push(("admin_theme", "ignite"));
        save(&client, &fields).await;
        let html = page(&client).await;
        assert!(html.contains(">Save details</button>"), "{html}");
        let _ = id;
    });
}
