//! The third end-to-end QA round's defects, each as WordPress answers it:
//!
//! - A. A date stored with seconds can be changed to a whole minute (the date input takes any
//!   minute), and a date left as it is keeps its seconds.
//! - B. A user added without a role gets the New User Default Role (Settings > General,
//!   `default_role`), which Add New User's Role select starts at.
//! - C. Add New User refuses a username `validate_username` refuses and an email `is_email`
//!   refuses; Edit User refuses the email.
//! - D. Saving a post or a page stays on its edit form with "Post updated." or "Page updated.".
//! - E, F. Quick Draft keeps its refusal and "Draft saved." in its box, and lists "Your Recent
//!   Drafts".
//! - G. "Your Profile" opens the profile page of the user's admin theme.
//! - H. The top bar and the posts list's Author column show the display name.
//! - I. A refused profile picture's message is an error notice.
//! - J. An address nothing answers is Ignite's 404 page in the panel's layout of each admin
//!   theme, with a link back.
//! - K. No Delete on the user's own row, nor on the administrator role.
//! - L. Settings > Reading shows no homepage while "Your latest posts" is chosen.
//! - M. A category counts its published posts only.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use std::sync::Arc;

use admin_support::{
    choose_theme, form, get, location, new_post, post, run, sign_in, sign_out, token, user,
};
use ignite_panel::authz;
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, set_option, set_user_meta,
};
use orbit_orm::{Database, raw};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::Client;
use serde_json::json;

/// The values of the inputs, selects and textareas of `html` as a browser sends them.
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
/// of the form's values: `(status, location, body)`.
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

/// The `<select name="{name}">` of `html`, to its end.
fn select<'h>(html: &'h str, name: &str) -> &'h str {
    let start = html
        .find(&format!("name=\"{name}\""))
        .unwrap_or_else(|| panic!("a select {name} in {html}"));
    let end = html[start..]
        .find("</select>")
        .map_or(html.len(), |e| start + e);
    &html[start..end]
}

async fn published_at(db: &Database, id: i64) -> String {
    raw::fetch_scalar(
        db,
        "SELECT DATE_FORMAT(published_at, '%Y-%m-%d %H:%i:%s') FROM posts WHERE id = ?",
        &[id.into()],
    )
    .await
    .unwrap()
}

async fn user_id(db: &Database, name: &str) -> i64 {
    raw::fetch_scalar(db, "SELECT id FROM users WHERE name = ?", &[name.into()])
        .await
        .unwrap()
}

/// A, D, H (the Author column) and M.
async fn posts_dates_saves_authors_and_counts(client: &Client, db: &Database) {
    let (ada, email) = user(db, "ada", "administrator").await;
    sign_in(client, &email).await;

    // A. A date stored with seconds: the input takes any minute.
    let id = new_post(
        client,
        "post",
        &[("title", "Seconds"), ("status", "publish")],
    )
    .await;
    raw::execute(
        db,
        "UPDATE posts SET published_at = ? WHERE id = ?",
        &["2026-11-20 02:43:22".into(), id.into()],
    )
    .await
    .unwrap();
    let edit = format!("/mc-admin/post/{id}/edit");
    let (_, html) = get(client, &edit).await;
    assert!(
        html.contains("type=\"datetime-local\" step=\"1\""),
        "{html}"
    );
    let action = format!("/mc-admin/post/{id}");
    // Left as it is, the date keeps its seconds.
    let (status, to, body) = submit(client, &edit, &action, &[]).await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(published_at(db, id).await, "2026-11-20 02:43:22");
    // D. The save stays on the edit form, with WordPress's message.
    assert_eq!(to, format!("{edit}?message=1"));
    let (_, html) = get(client, &to).await;
    assert!(html.contains("Post updated."), "{html}");
    assert!(!html.contains("Changes saved."), "{html}");
    // A whole minute is taken.
    let (status, _, body) = submit(client, &edit, &action, &[("date", "2026-11-20T10:00")]).await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(published_at(db, id).await, "2026-11-20 10:00:00");

    // D. A page: "Page updated.".
    let page = new_post(client, "page", &[("title", "About")]).await;
    let edit = format!("/mc-admin/page/{page}/edit");
    let (status, to, body) = submit(client, &edit, &format!("/mc-admin/page/{page}"), &[]).await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(to, format!("{edit}?message=1"));
    assert!(get(client, &to).await.1.contains("Page updated."));

    // D. The other screens' saves stay on their edit form too (Ignite's `RedirectAfter::Edit`),
    // with the panel's message: a category, as `term.php`, and General Settings.
    let term = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("Stays"))
        .await
        .unwrap();
    let edit = format!("/mc-admin/category/{}/edit", term.term_id);
    let (status, to, body) = submit(
        client,
        &edit,
        &format!("/mc-admin/category/{}", term.term_id),
        &[],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(to, edit);
    assert!(get(client, &to).await.1.contains("Changes saved."));
    let edit = "/mc-admin/options-general/1/edit";
    let (status, to, body) = submit(
        client,
        edit,
        "/mc-admin/options-general/1",
        &[
            ("siteurl", "http://example.test"),
            ("home", "http://example.test"),
            ("admin_email", "ada@example.test"),
        ],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    assert_eq!(to, edit);

    // H. The Author column and the top bar show the display name.
    set_user_meta(db, ada, "display_name", &json!("Ada Lovelace"))
        .await
        .unwrap();
    let (_, list) = get(client, "/mc-admin/post").await;
    assert!(list.contains("Ada Lovelace"), "{list}");
    let (_, dashboard) = get(client, "/mc-admin").await;
    assert!(dashboard.contains("Ada Lovelace</a>"), "{dashboard}");
    choose_theme(client, "wordpress").await;
    let (_, dashboard) = get(client, "/mc-admin").await;
    assert!(
        dashboard.contains("<span class=\"display-name\">Ada Lovelace</span>"),
        "{dashboard}"
    );
    assert!(
        !dashboard.contains("<span class=\"display-name\">ada</span>"),
        "{dashboard}"
    );

    // M. A category counts its published posts, not its drafts (`_update_post_term_count`).
    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    for status in [PostStatus::Publish, PostStatus::Draft] {
        let post_row = Post::create(
            db,
            NewPost {
                status,
                title: "Counted".to_owned(),
                ..NewPost::new(ada, "post")
            },
        )
        .await
        .unwrap();
        post_row
            .attach_terms(db, &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
            .await
            .unwrap();
    }
    let news = Term::find(db, news.term_taxonomy_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(news.count, 1);
}

/// B, C, K and L.
async fn users_roles_and_settings(client: &Client, db: &Database) {
    let (uma, email) = user(db, "uma", "administrator").await;
    let (_, _) = user(db, "bob", "editor").await;
    sign_in(client, &email).await;
    let csrf = token(client).await;

    // The installer's General options.
    for (name, value) in [
        ("siteurl", "https://example.test"),
        ("home", "https://example.test"),
        ("admin_email", "uma@example.test"),
    ] {
        set_option(db, name, &json!(value)).await.unwrap();
    }
    // B. General's New User Default Role, Subscriber at first.
    let general = "/mc-admin/options-general/1/edit";
    let (_, html) = get(client, general).await;
    assert!(html.contains(">New User Default Role"), "{html}");
    assert!(
        select(&html, "default_role").contains("<option value=\"subscriber\" selected>"),
        "{html}"
    );
    // Add New User's Role starts there, without a blank choice.
    let (_, new) = get(client, "/mc-admin/users/new").await;
    let role = select(&new, "role");
    assert!(
        role.contains("<option value=\"subscriber\" selected>"),
        "{new}"
    );
    assert!(!role.contains("<option value=\"\""), "{new}");
    // A user added without a role gets it.
    let add = |name: &'static str, email: &'static str, role: Option<&'static str>| {
        let mut fields = vec![
            ("name", name),
            ("email", email),
            ("password", "a new password 1"),
        ];
        if let Some(role) = role {
            fields.push(("role", role));
        }
        fields
    };
    let mut sent = vec![("_token", csrf.as_str())];
    sent.extend(add("carol", "carol@example.test", None));
    let response = post(client, "/mc-admin/users", &sent).await;
    assert_eq!(response.status(), Status::SeeOther);
    drop(response);
    let carol = user_id(db, "carol").await;
    assert_eq!(authz::roles_of(db, carol).await.unwrap(), ["subscriber"]);
    // The default changed on General is the new default.
    let (status, _, body) = submit(
        client,
        general,
        "/mc-admin/options-general/1",
        &[("default_role", "author")],
    )
    .await;
    assert_eq!(status, Status::SeeOther, "{body}");
    let (_, new) = get(client, "/mc-admin/users/new").await;
    assert!(
        select(&new, "role").contains("<option value=\"author\" selected>"),
        "{new}"
    );
    let mut sent = vec![("_token", csrf.as_str())];
    sent.extend(add("dave", "dave@example.test", Some("")));
    let response = post(client, "/mc-admin/users", &sent).await;
    assert_eq!(response.status(), Status::SeeOther);
    drop(response);
    let dave = user_id(db, "dave").await;
    assert_eq!(authz::roles_of(db, dave).await.unwrap(), ["author"]);
    // A role that is none of the site's is refused.
    let (status, _, body) = submit(
        client,
        general,
        "/mc-admin/options-general/1",
        &[("default_role", "pirate")],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");

    // C. WordPress's username and email checks.
    let mut sent = vec![("_token", csrf.as_str())];
    sent.extend(add("qa tester 1!", "qa@example.test", Some("editor")));
    let response = post(client, "/mc-admin/users", &sent).await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap_or_default();
    assert!(
        body.contains(
            "This username is invalid because it uses illegal characters. Please enter a \
                 valid username."
        ),
        "{body}"
    );
    let mut sent = vec![("_token", csrf.as_str())];
    sent.extend(add("qa.tester-1", "a@b", Some("editor")));
    let response = post(client, "/mc-admin/users", &sent).await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
    let body = response.into_string().await.unwrap_or_default();
    assert!(body.contains("The email address is not correct."), "{body}");
    let n: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM users WHERE name IN ('qa tester 1!', 'qa.tester-1')",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(n, 0);
    let (status, _, body) = submit(
        client,
        &format!("/mc-admin/users/{carol}/edit"),
        &format!("/mc-admin/users/{carol}"),
        &[("email", "a@b")],
    )
    .await;
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    assert!(body.contains("The email address is not correct."), "{body}");

    // K. No Delete on the user's own row; the others keep theirs.
    let (_, list) = get(client, "/mc-admin/users").await;
    assert!(
        !list.contains(&format!("/mc-admin/users/{uma}/actions/delete")),
        "{list}"
    );
    assert!(
        list.contains(&format!("/mc-admin/users/{carol}/actions/delete")),
        "{list}"
    );
    // Nor on the administrator role.
    let (_, roles) = get(client, "/mc-admin/roles").await;
    let administrator: i64 = raw::fetch_scalar(
        db,
        "SELECT id FROM roles WHERE name = ?",
        &["administrator".into()],
    )
    .await
    .unwrap();
    let editor: i64 = raw::fetch_scalar(
        db,
        "SELECT id FROM roles WHERE name = ?",
        &["editor".into()],
    )
    .await
    .unwrap();
    assert!(
        !roles.contains(&format!("/mc-admin/roles/{administrator}/actions/delete")),
        "{roles}"
    );
    assert!(
        roles.contains(&format!("/mc-admin/roles/{editor}/actions/delete")),
        "{roles}"
    );

    // L. "Your latest posts": the Homepage shows no page, whatever id is stored.
    let home = new_post(client, "page", &[("title", "Home"), ("status", "publish")]).await;
    set_option(db, "show_on_front", &json!("posts"))
        .await
        .unwrap();
    set_option(db, "page_on_front", &json!(home)).await.unwrap();
    let reading = "/mc-admin/options-reading/1/edit";
    let (_, html) = get(client, reading).await;
    assert!(
        !select(&html, "page_on_front").contains(&format!("value=\"{home}\" selected")),
        "{html}"
    );
    set_option(db, "show_on_front", &json!("page"))
        .await
        .unwrap();
    let (_, html) = get(client, reading).await;
    assert!(
        select(&html, "page_on_front").contains(&format!("value=\"{home}\" selected")),
        "{html}"
    );
}

/// Posts Quick Draft as the dashboard's form does.
async fn quick_draft<'c>(
    client: &'c Client,
    title: &str,
    content: &str,
) -> rocket::local::asynchronous::LocalResponse<'c> {
    let csrf = token(client).await;
    post(
        client,
        "/mc-admin/post",
        &[
            ("_token", &csrf),
            ("_back", "/mc-admin"),
            ("status", "draft"),
            ("title", title),
            ("content", content),
        ],
    )
    .await
}

/// E, F and G.
async fn quick_draft_and_your_profile(client: &Client, db: &Database) {
    let (_, email) = user(db, "quinn", "administrator").await;
    sign_in(client, &email).await;
    for theme in ["", "wordpress"] {
        if !theme.is_empty() {
            choose_theme(client, theme).await;
        }
        // E. A draft without a title stays on the dashboard, refused in the box.
        let response = quick_draft(client, "", "An idea worth keeping").await;
        assert_eq!(response.status(), Status::SeeOther, "{theme}");
        assert_eq!(location(&response), "/mc-admin");
        drop(response);
        let (status, html) = get(client, "/mc-admin").await;
        assert_eq!(status, Status::Ok);
        let card = &html[html.find(">Quick Draft</h2>").expect("the box")..];
        assert!(card.contains("Title is required."), "{theme}: {html}");
        assert!(card.contains("An idea worth keeping</textarea>"), "{html}");
        assert!(!html.contains("Record created."), "{html}");
        // Once only.
        assert!(
            !get(client, "/mc-admin")
                .await
                .1
                .contains("Title is required.")
        );

        // F. A saved draft: "Draft saved." and Your Recent Drafts.
        let title = format!(
            "Idea for {}",
            if theme.is_empty() { "ignite" } else { theme }
        );
        let response = quick_draft(client, &title, "Some words").await;
        assert_eq!(response.status(), Status::SeeOther);
        assert_eq!(location(&response), "/mc-admin");
        drop(response);
        let id: i64 = raw::fetch_scalar(
            db,
            "SELECT id FROM posts WHERE title = ? AND status = 'draft'",
            &[title.as_str().into()],
        )
        .await
        .unwrap();
        let (_, html) = get(client, "/mc-admin").await;
        let card = &html[html.find(">Quick Draft</h2>").expect("the box")..];
        assert!(card.contains("Draft saved."), "{html}");
        assert!(!html.contains("Record created."), "{html}");
        assert!(card.contains("Your Recent Drafts"), "{html}");
        assert!(
            card.contains(&format!("href=\"/mc-admin/post/{id}/edit\"")),
            "{html}"
        );
        assert!(card.contains(&format!(">{title}</a>")), "{html}");
    }

    // G. "Your Profile" opens the profile page, in the user's admin theme.
    let response = client.get("/mc-admin/your-profile").dispatch().await;
    assert_eq!(location(&response), "/mc-admin/profile");
    drop(response);
    let (_, profile) = get(client, "/mc-admin/profile").await;
    assert!(profile.contains("id=\"your-profile\""), "{profile}");
    choose_theme(client, "ignite").await;
    let (_, profile) = get(client, "/mc-admin/profile").await;
    assert!(profile.contains(">Update Profile</button>"), "{profile}");
}

/// Uploads `bytes` as the profile picture.
async fn upload_picture(client: &Client, bytes: &[u8]) {
    let csrf = token(client).await;
    let boundary = "mc-round3-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"_token\"\r\n\r\n{csrf}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; \
         filename=\"fake.png\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = client
        .post("/mc-admin/profile/avatar")
        .header(ContentType::new("multipart", "form-data").with_params(("boundary", boundary)))
        .body(body)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::SeeOther);
}

/// I and J.
async fn refused_picture_and_not_found(client: &Client, db: &Database) {
    let (_, email) = user(db, "cleo", "administrator").await;
    sign_in(client, &email).await;
    let refused = "Choose a PNG, JPEG or WebP picture of up to 1 MB.";

    // I. A fake picture: the message is an error, in each theme.
    upload_picture(client, b"not a picture").await;
    let (_, profile) = get(client, "/mc-admin/profile").await;
    assert!(
        profile.contains(&format!(
            "border-error bg-lighterror px-4 py-3 text-error-emphasis\" role=\"alert\">{refused}"
        )),
        "{profile}"
    );
    choose_theme(client, "wordpress").await;
    upload_picture(client, b"not a picture").await;
    let (_, profile) = get(client, "/mc-admin/profile").await;
    assert!(
        profile.contains(&format!(
            "<div class=\"notice notice-error\" role=\"alert\"><p>{refused}</p></div>"
        )),
        "{profile}"
    );

    // J. Unknown addresses: Ignite's 404 page, in the panel's layout of each admin theme (the
    // menu and the top bar), with the way back.
    let response = client
        .get("/mc-includes/images/mission-control-logo-dark.webp")
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.content_type(), Some(ContentType::WEBP));
    drop(response);
    for theme in ["wordpress", "ignite"] {
        choose_theme(client, theme).await;
        for path in [
            "/mc-admin/no-such-screen",
            "/mc-admin/post/999999/edit",
            "/mc-admin/a/b/c/d/e",
        ] {
            let (status, html) = get(client, path).await;
            assert_eq!(status, Status::NotFound, "{path}");
            assert!(html.contains("data-ignite-not-found"), "{path}: {html}");
            assert!(
                html.contains("The page you are looking for could not be found."),
                "{html}"
            );
            assert!(
                html.contains("href=\"/mc-admin\">Go to the dashboard</a>"),
                "{html}"
            );
            // The layout: the menu with its Posts entry, and the theme's chrome.
            assert!(
                html.contains("href=\"/mc-admin/post\""),
                "{theme} {path}: {html}"
            );
            if theme == "wordpress" {
                assert!(html.contains("id=\"wpadminbar\""), "{html}");
            } else {
                assert!(!html.contains("id=\"wpadminbar\""), "{html}");
                // The Ignite theme's dark mode shows the logo's light-on-dark variant.
                assert!(
                    html.contains("/mc-includes/images/mission-control-logo-dark.webp"),
                    "{html}"
                );
            }
        }
    }
}

/// Each defect's flow, on one app: the admin's screens are registered once per process.
#[test]
fn the_third_qa_rounds_defects_are_fixed() {
    run("round3", |client, db: Arc<Database>| async move {
        posts_dates_saves_authors_and_counts(&client, &db).await;
        sign_out(&client).await;
        users_roles_and_settings(&client, &db).await;
        sign_out(&client).await;
        quick_draft_and_your_profile(&client, &db).await;
        sign_out(&client).await;
        refused_picture_and_not_found(&client, &db).await;
    });
}
