//! Users' pictures through the media library: the picture is an attachment, uploaded with the
//! media library's uploader and chosen in its picker, in both admin themes; the top bar and the
//! Users list show the attachment; the media library lists it with the "Avatar" label; only an
//! image the user may read in the library is accepted; and a picture Ignite stored before
//! becomes an attachment the first time it is shown.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use std::path::Path;

use admin_support::{choose_theme, get, location, post, run, sign_in, sign_out, token_in, user};
use ignite_panel::users;
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use mission_control_core::content::Post;
use mission_control_core::rest::media::UPLOADS_PATH;
use mission_control_core::users::avatar::avatar_attachment;
use orbit_orm::Model;
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::Client;
use serde_json::Value;

const PROFILE: &str = "/mc-admin/profile";

/// The multipart boundary the uploads send.
const BOUNDARY: &str = "mc-avatars-boundary-41";

/// A small JPEG, under the test app's 1 KB upload limit.
fn jpeg() -> Vec<u8> {
    let image = RgbImage::from_fn(4, 4, |x, y| Rgb([(x * 60) as u8, (y * 60) as u8, 128]));
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 50)
        .encode_image(&image)
        .unwrap();
    out
}

/// Uploads `bytes` as `name` through the media library's uploader, as the picker's "Upload
/// Files" tab does; the new attachment's JSON.
async fn upload(client: &Client, csrf: &str, name: &str, bytes: &[u8]) -> Value {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"async-upload\"; \
         filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let response = client
        .post("/mc-admin/async-upload.php")
        .header(ContentType::new("multipart", "form-data").with_params(("boundary", BOUNDARY)))
        .header(Header::new("X-CSRF-TOKEN", csrf.to_owned()))
        .header(Header::new("Accept", "application/json"))
        .body(body)
        .dispatch()
        .await;
    let status = response.status();
    let json: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert_eq!(status, Status::Ok, "{json}");
    json["data"].clone()
}

/// The profile page, as the signed-in user sees it.
async fn page(client: &Client) -> String {
    let (status, html) = get(client, PROFILE).await;
    assert_eq!(status, Status::Ok, "{html}");
    html
}

/// Posts the Ignite theme's picture form with `value`; the answer's status and body.
async fn save_picture(client: &Client, value: &str) -> (Status, String) {
    let csrf = token_in(&page(client).await);
    let response = post(
        client,
        "/mc-admin/profile/picture",
        &[("_token", &csrf), ("mc_avatar", value)],
    )
    .await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

#[test]
fn the_picture_is_an_attachment_of_the_media_library() {
    run("avatars", |client, db| async move {
        let (id, email) = user(&db, "ada", "administrator").await;
        sign_in(&client, &email).await;

        // The Ignite theme: the picture card is the media library's picker.
        let html = page(&client).await;
        assert!(
            html.contains("action=\"/mc-admin/profile/picture\""),
            "{html}"
        );
        assert!(
            html.contains("name=\"mc_avatar\"") && html.contains("data-ignite-media=\"image\""),
            "{html}"
        );
        assert!(
            html.contains("/mc-includes/js/featured-image.js?ver="),
            "{html}"
        );
        assert!(
            !html.contains("name=\"avatar\""),
            "no upload of Ignite's own: {html}"
        );

        // An image uploaded with the picker's "Upload Files" tab (the media library's uploader),
        // then chosen.
        let csrf = token_in(&html);
        let item = upload(&client, &csrf, "portrait.jpg", &jpeg()).await;
        let attachment = item["id"].as_i64().unwrap();
        let url = item["url"].as_str().unwrap().to_owned();
        assert!(url.contains(UPLOADS_PATH), "{url}");
        let (status, body) = save_picture(&client, &attachment.to_string()).await;
        assert_eq!(status, Status::SeeOther, "{body}");
        assert_eq!(avatar_attachment(&db, id).await.unwrap(), Some(attachment));

        // The top bar and the card show the attachment.
        let html = page(&client).await;
        assert!(html.contains("Picture saved."), "{html}");
        assert!(html.contains(&format!("src=\"{url}\"")), "{html}");
        let (_, dashboard) = get(&client, "/mc-admin").await;
        assert!(dashboard.contains(&format!("src=\"{url}\"")), "{dashboard}");

        // The Users list shows it before the username.
        let (_, list) = get(&client, "/mc-admin/users").await;
        assert!(
            list.contains(&format!("<td class=\"mc-list-avatar\"><img src=\"{url}\"")),
            "{list}"
        );

        // The media library lists it with the other files, labelled "Avatar".
        let response = client
            .get("/mc-admin/admin-ajax.php?action=query-attachments")
            .header(Header::new("Accept", "application/json"))
            .dispatch()
            .await;
        let json: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
        let listed = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"].as_i64() == Some(attachment))
            .cloned()
            .unwrap_or_else(|| panic!("the picture in the library: {json}"));
        assert_eq!(listed["avatar"], Value::Bool(true), "{listed}");
        let (_, edit) = get(&client, &format!("/mc-admin/attachment/{attachment}/edit")).await;
        assert!(edit.contains("mc-avatar-label\">Avatar</span>"), "{edit}");
        // Another file is not labelled.
        let other = upload(&client, &csrf, "other.jpg", &jpeg()).await;
        let other_url = other["url"].as_str().unwrap().to_owned();
        let other = other["id"].as_i64().unwrap();
        let (_, edit) = get(&client, &format!("/mc-admin/attachment/{other}/edit")).await;
        assert!(!edit.contains("mc-avatar-label"), "{edit}");

        // Only an image of the library: a text file and an id that is no attachment are refused.
        let text = upload(&client, &csrf, "notes.txt", b"just some notes").await;
        let text = text["id"].as_i64().unwrap();
        let (status, body) = save_picture(&client, &text.to_string()).await;
        assert_eq!(status, Status::UnprocessableEntity);
        assert!(
            body.contains("Choose an image from the media library."),
            "{body}"
        );
        let (status, _) = save_picture(&client, "999999").await;
        assert_eq!(status, Status::UnprocessableEntity);
        assert_eq!(avatar_attachment(&db, id).await.unwrap(), Some(attachment));

        // The WordPress theme: the same picker in the Profile Picture row, saved with "Update
        // Profile", and the admin bar shows the picture.
        choose_theme(&client, "wordpress").await;
        let html = page(&client).await;
        assert!(
            html.contains(&format!(
                "<input class=\"ig-input\" id=\"mc_avatar\" name=\"mc_avatar\" type=\"number\" \
                 min=\"1\" step=\"1\" value=\"{attachment}\" data-ignite-media=\"image\""
            )),
            "{html}"
        );
        assert!(
            html.contains("/mc-includes/js/featured-image.js?ver="),
            "{html}"
        );
        assert!(!html.contains("mc-avatar-form"), "{html}");
        let csrf = token_in(&html);
        let other_string = other.to_string();
        let fields = [
            ("_token", csrf.as_str()),
            ("nickname", "ada"),
            ("display_name", "ada"),
            ("email", email.as_str()),
            ("admin_theme", "wordpress"),
            ("mc_avatar", other_string.as_str()),
            // The marker of the row, which the form sends with it.
            ("mc_avatar_shown", "1"),
        ];
        let response = post(&client, PROFILE, &fields).await;
        assert_eq!(location(&response), format!("{PROFILE}?updated=1"));
        drop(response);
        assert_eq!(avatar_attachment(&db, id).await.unwrap(), Some(other));
        let (_, dashboard) = get(&client, "/mc-admin").await;
        assert!(dashboard.contains("id=\"wpadminbar\""), "{dashboard}");
        assert!(
            dashboard.contains(&format!("src=\"{other_url}\"")),
            "{dashboard}"
        );
        // A refused picture: WordPress's error notice, the picture unchanged.
        let mut refused = fields;
        refused[5] = ("mc_avatar", "999999");
        let response = post(&client, PROFILE, &refused).await;
        assert_eq!(location(&response), PROFILE);
        drop(response);
        let html = page(&client).await;
        assert!(
            html.contains("Choose an image from the media library."),
            "{html}"
        );
        assert_eq!(avatar_attachment(&db, id).await.unwrap(), Some(other));
        // No picture: the initials again.
        let mut none = fields;
        none[5] = ("mc_avatar", "");
        let response = post(&client, PROFILE, &none).await;
        drop(response);
        assert_eq!(avatar_attachment(&db, id).await.unwrap(), None);
        choose_theme(&client, "ignite").await;
        sign_out(&client).await;

        // A user who may not upload (a subscriber) cannot choose a library image.
        let (sub, sub_email) = user(&db, "sam", "subscriber").await;
        sign_in(&client, &sub_email).await;
        let (status, _) = save_picture(&client, &attachment.to_string()).await;
        assert_eq!(status, Status::UnprocessableEntity);
        assert_eq!(avatar_attachment(&db, sub).await.unwrap(), None);
    });
}

#[test]
fn a_picture_stored_before_becomes_an_attachment() {
    let storage = Path::new("storage");
    let had_storage = storage.exists();
    run("avatars_migration", |client, db| async move {
        let (id, email) = user(&db, "lin", "author").await;
        // A picture Ignite's own upload stored, before the media library kept them.
        users::set_avatar(&db, id, "image/jpeg", &jpeg())
            .await
            .unwrap();
        assert!(users::avatar(&db, id).await.unwrap().is_some());
        assert_eq!(avatar_attachment(&db, id).await.unwrap(), None);

        // The first page shown moves it to the media library.
        sign_in(&client, &email).await;
        let (_, dashboard) = get(&client, "/mc-admin").await;
        let attachment = avatar_attachment(&db, id)
            .await
            .unwrap()
            .expect("the picture is an attachment");
        let post_row = Post::query().find(&*db, attachment).await.unwrap().unwrap();
        assert_eq!(post_row.post_type, "attachment");
        assert_eq!(post_row.author_id, id);
        assert_eq!(post_row.title, "Profile picture");
        // Ignite's copy is gone, and the top bar shows the attachment.
        assert!(users::avatar(&db, id).await.unwrap().is_none());
        assert!(
            !dashboard.contains("/mc-admin/profile/avatar"),
            "{dashboard}"
        );
        assert!(
            dashboard.contains(&format!("src=\"{UPLOADS_PATH}/")),
            "{dashboard}"
        );
        // Once: the next page keeps the same attachment.
        let _ = get(&client, "/mc-admin").await;
        assert_eq!(avatar_attachment(&db, id).await.unwrap(), Some(attachment));
    });
    // Ignite's storage folder, emptied by the move, is not left in the crate.
    if !had_storage {
        let _ = std::fs::remove_dir(storage.join("avatars"));
        let _ = std::fs::remove_dir(storage);
    }
}
