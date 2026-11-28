//! The Field Groups manager (Task 5.7, design `cpt-field-manager-design.md` 2.3 and 5): an
//! administrator creates a field group with a text field and the location "Post Type is Book",
//! on ACF's one page (the fields a repeater of field rows, the location rules a repeater of rule
//! groups); the field is on the Book form on the next page load, without a restart, its value is
//! stored in post meta as ACF stores it (under the field's name, `_name` holding its key), and
//! shows again after a reload.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{get, location, new_post, post, run, sign_in, sign_out, token, user};
use mission_control_core::content::get_post_meta;
use orbit_orm::{Database, raw};
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use serde_json::{Value, json};

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

/// The stored field group `key`, as JSON.
async fn stored(db: &Database, key: &str) -> Value {
    let rows: Vec<(String,)> = raw::fetch_all(
        db,
        "SELECT definition FROM field_groups WHERE group_key = ?",
        &[key.into()],
    )
    .await
    .unwrap();
    serde_json::from_str(&rows.first().expect("the group is stored").0).unwrap()
}

#[test]
fn a_field_group_shows_its_field_on_the_post_types_form() {
    run("cpt_field_groups", |client, db| async move {
        let (_, admin) = user(&db, "ada", "administrator").await;
        let (_, editor) = user(&db, "edna", "editor").await;
        sign_in(&client, &editor).await;
        assert_eq!(
            get(&client, "/mc-admin/field-groups").await.0,
            Status::Forbidden
        );
        sign_out(&client).await;

        sign_in(&client, &admin).await;
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

        let (status, html) = get(&client, "/mc-admin/field-groups").await;
        assert_eq!(status, Status::Ok, "{html}");
        let (status, html) = get(&client, "/mc-admin/field-groups/new").await;
        assert_eq!(status, Status::Ok, "{html}");
        for text in [
            "Field Group Title",
            "Fields",
            "Location Rules",
            "Presentation",
        ] {
            assert!(html.contains(text), "{text}: {html}");
        }

        // A rule's value must be one of its parameter's choices.
        let (status, _, body) = send(
            &client,
            "/mc-admin/field-groups",
            &[
                ("title", "Wrong"),
                ("location[0][rules][0][param]", "post_type"),
                ("location[0][rules][0][operator]", "=="),
                ("location[0][rules][0][value]", "no_such_type"),
            ],
        )
        .await;
        assert_eq!(status, Status::UnprocessableEntity, "{body}");

        // "Book details": a text field, shown when the post type is Book.
        let (status, to, body) = send(
            &client,
            "/mc-admin/field-groups",
            &[
                ("title", "Book details"),
                ("fields[0][label]", "Publisher"),
                ("fields[0][name]", "publisher"),
                ("fields[0][type]", "text"),
                ("fields[0][required]", "on"),
                ("fields[0][placeholder]", "Who published it"),
                ("fields[1][label]", "Format"),
                ("fields[1][name]", "format"),
                ("fields[1][type]", "select"),
                (
                    "fields[1][choices]",
                    "hardcover : Hardcover\npaperback : Paperback",
                ),
                ("location[0][rules][0][param]", "post_type"),
                ("location[0][rules][0][operator]", "=="),
                ("location[0][rules][0][value]", "book"),
                ("position", "side"),
            ],
        )
        .await;
        assert_eq!(status, Status::SeeOther, "{body}");
        let key = to
            .strip_prefix("/mc-admin/field-groups/")
            .and_then(|rest| rest.strip_suffix("/edit"))
            .expect("the new group's edit form")
            .to_owned();
        assert!(key.starts_with("group_"), "{key}");
        let group = stored(&db, &key).await;
        assert_eq!(group["title"], "Book details");
        assert_eq!(group["position"], "side");
        assert_eq!(
            group["location"],
            json!([[{"param": "post_type", "operator": "==", "value": "book"}]])
        );
        let publisher = &group["fields"][0];
        assert_eq!(publisher["type"], "text");
        assert_eq!(publisher["required"], true);
        assert_eq!(publisher["placeholder"], "Who published it");
        let field_key = publisher["key"].as_str().unwrap().to_owned();
        assert!(field_key.starts_with("field_"), "{field_key}");
        assert_eq!(
            group["fields"][1]["choices"],
            json!({"hardcover": "Hardcover", "paperback": "Paperback"})
        );
        let rows: i64 = raw::fetch_scalar(
            &*db,
            "SELECT COUNT(*) FROM field_group_locations WHERE group_key = ?",
            &[key.as_str().into()],
        )
        .await
        .unwrap();
        assert_eq!(rows, 1, "the location rule's row");

        // The edit form shows the rows, and the rule's value among its choices.
        let (status, html) = get(&client, &to).await;
        assert_eq!(status, Status::Ok, "{html}");
        assert!(html.contains("Publisher"), "{html}");
        assert!(html.contains("value=\"book\""), "{html}");
        let (_, list) = get(&client, "/mc-admin/field-groups").await;
        assert!(list.contains("Post Type is book"), "{list}");

        // The Book form shows the field on the next page load; its value saves.
        let (status, html) = get(&client, "/mc-admin/book/new").await;
        assert_eq!(status, Status::Ok, "{html}");
        assert!(html.contains("Book details"), "{html}");
        assert!(html.contains(&format!("name=\"{field_key}\"")), "{html}");
        let id = new_post(
            &client,
            "book",
            &[("title", "Dune"), (field_key.as_str(), "Chilton Books")],
        )
        .await;
        assert_eq!(
            get_post_meta(&db, id, "publisher").await.unwrap(),
            Some(json!("Chilton Books"))
        );
        assert_eq!(
            get_post_meta(&db, id, "_publisher").await.unwrap(),
            Some(json!(field_key))
        );
        let (_, html) = get(&client, &format!("/mc-admin/book/{id}/edit")).await;
        assert!(html.contains("Chilton Books"), "{html}");

        // Renaming the field keeps its key, so the value stays its own.
        let (status, _, body) = send(
            &client,
            &format!("/mc-admin/field-groups/{key}"),
            &[
                ("title", "Book details"),
                ("fields[0][label]", "Publishing house"),
                ("fields[0][name]", "publisher"),
                ("fields[0][type]", "text"),
                ("fields[0][key]", &field_key),
                ("location[0][rules][0][param]", "post_type"),
                ("location[0][rules][0][operator]", "=="),
                ("location[0][rules][0][value]", "book"),
            ],
        )
        .await;
        assert_eq!(status, Status::SeeOther, "{body}");
        let group = stored(&db, &key).await;
        assert_eq!(group["fields"][0]["label"], "Publishing house");
        assert_eq!(group["fields"][0]["key"], field_key.as_str());
        assert_eq!(group["fields"].as_array().unwrap().len(), 1);
        let (_, html) = get(&client, &format!("/mc-admin/book/{id}/edit")).await;
        assert!(html.contains("Publishing house"), "{html}");
        assert!(html.contains("Chilton Books"), "{html}");

        // Deleting the group takes the field off the form; the value stays.
        let (status, _, body) = send(
            &client,
            &format!("/mc-admin/field-groups/{key}/delete"),
            &[],
        )
        .await;
        assert_eq!(status, Status::SeeOther, "{body}");
        let (_, html) = get(&client, "/mc-admin/book/new").await;
        assert!(!html.contains("Book details"), "{html}");
        assert!(get_post_meta(&db, id, "publisher").await.unwrap().is_some());
        sign_out(&client).await;
    });
}
