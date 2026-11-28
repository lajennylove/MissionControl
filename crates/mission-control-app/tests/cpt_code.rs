//! Post types, taxonomies and field groups defined in the site's code (Task 5.7, S5): built
//! with `mission_control_core::types::code` (as Poet and ACF Composer define them) and given to
//! the app in `AppOptions::definitions`, they are registered at start, serve their screens, and
//! the managers list them locked, "Defined in code": their forms are read-only and say why, and
//! their save and Delete are refused. A manager cannot take their keys.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{get, location, post, run_with, sidebar, sign_in, sign_out, token, user};
use mission_control::AppOptions;
use mission_control_core::types::code::{Field, FieldGroup, PostType, Taxonomy};
use mission_control_core::types::{Definition, LocationOperator, LocationParam};
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

/// The site's code: an Event post type, a Venue Type taxonomy on it, and an Event details
/// field group.
fn definitions() -> Vec<Definition> {
    vec![
        PostType::new("event")
            .public(true)
            .menu_icon("dashicons-calendar")
            .build()
            .unwrap()
            .into(),
        Taxonomy::new("venue_type")
            .object_type("event")
            .public(true)
            .build()
            .unwrap()
            .into(),
        FieldGroup::new("event")
            .title("Event details")
            .field(Field::text("venue"))
            .location(LocationParam::PostType, LocationOperator::Is, "event")
            .build()
            .unwrap()
            .into(),
    ]
}

#[test]
fn definitions_in_code_are_listed_locked() {
    let options = AppOptions {
        definitions: definitions(),
        ..AppOptions::default()
    };
    run_with("cpt_code", options, |client, db| async move {
        let (_, admin) = user(&db, "ada", "administrator").await;
        sign_in(&client, &admin).await;

        // The code's type is served: in the sidebar, with its taxonomy and field group.
        let (_, page) = get(&client, "/mc-admin/post").await;
        assert!(
            sidebar(&page).contains("href=\"/mc-admin/event\""),
            "{page}"
        );
        assert!(
            sidebar(&page).contains("href=\"/mc-admin/venue_type\""),
            "{page}"
        );
        let (status, html) = get(&client, "/mc-admin/event/new").await;
        assert_eq!(status, Status::Ok, "{html}");
        assert!(html.contains("Event details"), "{html}");

        for (manager, key, label, fields) in [
            (
                "post-types",
                "event",
                "Events",
                &[("plural_label", "Shows"), ("singular_label", "Show")][..],
            ),
            (
                "taxonomies",
                "venue_type",
                "Venue Types",
                &[("plural_label", "Kinds"), ("singular_label", "Kind")][..],
            ),
            (
                "field-groups",
                "group_event",
                "Event details",
                &[("title", "Renamed")][..],
            ),
        ] {
            // Listed, "Defined in code".
            let (status, list) = get(&client, &format!("/mc-admin/{manager}")).await;
            assert_eq!(status, Status::Ok, "{list}");
            assert!(list.contains(label), "{manager}: {list}");
            assert!(list.contains("Defined in code"), "{manager}: {list}");
            // The form is read-only and says why.
            let (status, form) = get(&client, &format!("/mc-admin/{manager}/{key}/edit")).await;
            assert_eq!(status, Status::Ok, "{form}");
            assert!(form.contains("is defined in code"), "{manager}: {form}");
            // Its save and Delete are refused, and nothing changes.
            let (status, _, _) = send(&client, &format!("/mc-admin/{manager}/{key}"), fields).await;
            assert_eq!(status, Status::Forbidden, "{manager}: save");
            let (status, _, _) =
                send(&client, &format!("/mc-admin/{manager}/{key}/delete"), &[]).await;
            assert_eq!(status, Status::Forbidden, "{manager}: delete");
            let (_, list) = get(&client, &format!("/mc-admin/{manager}")).await;
            assert!(list.contains(label), "{manager}: still {label}: {list}");
        }

        // The managers cannot take the code's keys.
        let (status, _, body) = send(
            &client,
            "/mc-admin/post-types",
            &[
                ("plural_label", "Events"),
                ("singular_label", "Event"),
                ("key", "event"),
            ],
        )
        .await;
        assert_eq!(status, Status::UnprocessableEntity, "{body}");
        let (status, _, body) = send(
            &client,
            "/mc-admin/taxonomies",
            &[
                ("plural_label", "Venue Types"),
                ("singular_label", "Venue Type"),
                ("key", "venue_type"),
            ],
        )
        .await;
        assert_eq!(status, Status::UnprocessableEntity, "{body}");
        sign_out(&client).await;
    });
}
