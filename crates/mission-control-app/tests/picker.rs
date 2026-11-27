//! The featured image picker of the post and page editors: WordPress's Featured image box
//! (`post_thumbnail_meta_box`, `_wp_post_thumbnail_html`) and its media frame, whose two tabs are
//! "Uploaded to this post" (`query-attachments` with `uploaded_to`, the media modal's
//! `uploadedTo`) and "Media Library". The box reads the stored image with `get-attachment`
//! (`wp_ajax_get_attachment`), and the pick is stored in the editor's `featured_media` field,
//! which the server still checks.
//!
//! The panel runs on MySQL and MariaDB only (Ruling R119), so the flow runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of its own
//! ([`scratch_database`]) with the installer's migrations, dropped at the end; otherwise it says
//! why it is skipped and passes. Ignite reads its database from `DATABASE_URL`, so the flow sets
//! it for the process; every test here holds [`LOCK`] so none runs while it changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use mission_control::{AppOptions, editor_version, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{get_post_meta, set_option};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::Database;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// Held by every test here: the flow sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
const PASSWORD: &str = "correct horse battery staple";

/// The multipart boundary the tests send.
const BOUNDARY: &str = "mc-picker-boundary-41a9";

fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
}

fn config(database_url: &str, uploads_dir: PathBuf) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: 512 * 1024,
        themes_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes"),
        uploads_dir,
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment: Environment::Local,
        trusted_proxies: TrustedProxies::None,
    }
}

fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 80)
        .encode_image(&image)
        .unwrap();
    out
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, NON_ALPHANUMERIC),
                utf8_percent_encode(v, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn token_in(html: &str) -> String {
    let start = html
        .find("name=\"_token\" value=\"")
        .unwrap_or_else(|| panic!("a form token in: {html}"))
        + "name=\"_token\" value=\"".len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
}

fn header(response: &LocalResponse<'_>, name: &str) -> String {
    response
        .headers()
        .get_one(name)
        .unwrap_or_default()
        .to_owned()
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

async fn post<'c>(client: &'c Client, uri: &str, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(fields))
        .dispatch()
        .await
}

async fn sign_in(client: &Client, email: &str) {
    let (_, login) = get(client, "/mc-admin/login").await;
    let response = post(
        client,
        "/mc-admin/login",
        &[
            ("_token", &token_in(&login)),
            ("email", email),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
}

async fn user(db: &Database, name: &str, role: &str) -> String {
    let email = format!("{name}@example.test");
    users::create_user(db, name, &email, PASSWORD)
        .await
        .expect("creates the user");
    authz::assign_role(db, &email, role)
        .await
        .expect("gives the role");
    email
}

/// Uploads `name` to `async-upload.php`, attached to `post_id` when given; the new id.
async fn upload(
    client: &Client,
    csrf: &str,
    post_id: Option<i64>,
    name: &str,
    bytes: &[u8],
) -> i64 {
    let mut body = Vec::new();
    if let Some(post_id) = post_id {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"post_id\"\r\n\r\n\
                 {post_id}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"async-upload\"; \
             filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let response = client
        .post("/mc-admin/async-upload.php")
        .header(ContentType::new("multipart", "form-data").with_params(("boundary", BOUNDARY)))
        .header(Header::new("X-CSRF-TOKEN", csrf.to_owned()))
        .body(body)
        .dispatch()
        .await;
    let (status, json, _) = json_of(response).await;
    assert_eq!(status, Status::Ok, "{json}");
    json["data"]["id"].as_i64().expect("an id")
}

/// A JSON answer: its status, body and `X-MC-Total`.
async fn json_of(response: LocalResponse<'_>) -> (Status, Value, String) {
    let status = response.status();
    assert_eq!(header(&response, "Content-Type"), "application/json");
    let total = header(&response, "X-MC-Total");
    let body = response.into_string().await.unwrap_or_default();
    let value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    (status, value, total)
}

/// The ids of a `query-attachments` answer, and its total.
async fn query(client: &Client, params: &str) -> (Vec<i64>, String) {
    let response = client
        .get(format!(
            "/mc-admin/admin-ajax.php?action=query-attachments{params}"
        ))
        .dispatch()
        .await;
    let (status, body, total) = json_of(response).await;
    assert_eq!(status, Status::Ok, "{body}");
    let ids = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect();
    (ids, total)
}

/// A post of `post_type` created through its New screen's save; its id.
async fn new_post(client: &Client, csrf: &str, post_type: &str, title: &str) -> i64 {
    let response = post(
        client,
        &format!("/mc-admin/{post_type}"),
        &[("_token", csrf), ("title", title), ("status", "draft")],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "{post_type}");
    header(&response, "Location")
        .strip_prefix(&format!("/mc-admin/{post_type}/"))
        .and_then(|rest| rest.strip_suffix("/edit"))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn the_featured_image_is_picked_from_the_media_library() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(_) = mysql_test_url() else {
        eprintln!(
            "skipped: the admin runs on MySQL or MariaDB only (Ruling R119); set \
             MC_TEST_DATABASE_URL to a mysql:// URL to run this test"
        );
        return;
    };
    let scratch = rocket::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scratch_database("picker"));
    // SAFETY: no other thread reads or writes the environment now. Every test of this process
    // holds `LOCK`, and the runtime above is gone; the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(flow(&scratch));
    // SAFETY: as above: the runtime of the flow has ended, and `LOCK` is held.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

async fn flow(scratch: &ScratchDatabase) {
    run_migrations(scratch.url()).await.expect("migrates");
    let uploads = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
    set_option(&db, "home", &json!("http://localhost"))
        .await
        .unwrap();
    let client = Client::tracked(rocket_with_options(
        config(scratch.url(), uploads.path().to_path_buf()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    let editor = user(&db, "edna", "editor").await;
    let subscriber = user(&db, "sue", "subscriber").await;

    // A visitor gets no attachment.
    let response = client
        .get("/mc-admin/admin-ajax.php?action=get-attachment&id=1")
        .dispatch()
        .await;
    assert_eq!(json_of(response).await.0, Status::Unauthorized);

    sign_in(&client, &editor).await;
    let (_, dashboard) = get(&client, "/mc-admin").await;
    let csrf = token_in(&dashboard);
    let post_id = new_post(&client, &csrf, "post", "With a featured image").await;
    let page_id = new_post(&client, &csrf, "page", "About").await;
    let jpeg = jpeg_bytes(640, 480);

    // Two images uploaded to the post, one to nothing, a PDF to the post.
    let first = upload(&client, &csrf, Some(post_id), "Harbour.jpg", &jpeg).await;
    let second = upload(&client, &csrf, Some(post_id), "Lighthouse.jpg", &jpeg).await;
    let loose = upload(&client, &csrf, None, "Elsewhere.jpg", &jpeg).await;
    let pdf: &[u8] =
        b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";
    let report = upload(&client, &csrf, Some(post_id), "Report.pdf", pdf).await;

    // "Uploaded to this post": everything attached to the post, newest first; "Media
    // Library": every item, of every type, as WordPress's featured image modal lists them.
    let (ids, total) = query(&client, &format!("&uploaded_to={post_id}")).await;
    assert_eq!(ids, [report, second, first]);
    assert_eq!(total, "3");
    let (ids, total) = query(&client, "").await;
    assert_eq!(ids, [report, loose, second, first]);
    assert_eq!(total, "4");
    // Nothing is uploaded to the page; `uploaded_to` that is not a post's id lists all.
    let (ids, total) = query(&client, &format!("&uploaded_to={page_id}")).await;
    assert!(ids.is_empty());
    assert_eq!(total, "0");
    for odd in ["0", "-4", "x"] {
        let (ids, _) = query(&client, &format!("&uploaded_to={odd}")).await;
        assert_eq!(ids.len(), 4, "{odd}");
    }
    // The Media Library tab's search and its pages ("Load more" asks for the next one).
    let (ids, total) = query(&client, "&search=lighthouse").await;
    assert_eq!((ids, total.as_str()), (vec![second], "1"));
    let (ids, total) = query(&client, "&per_page=3&page=1").await;
    assert_eq!((ids, total.as_str()), (vec![report, loose, second], "4"));
    let (ids, _) = query(&client, "&per_page=3&page=2").await;
    assert_eq!(ids, [first]);

    // `get-attachment`: one item, as the box shows it; 404 for anything that is not one.
    let response = client
        .get(format!(
            "/mc-admin/admin-ajax.php?action=get-attachment&id={first}"
        ))
        .dispatch()
        .await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["id"], first);
    assert_eq!(body["data"]["type"], "image");
    assert!(
        body["data"]["sizes"]["medium"]["url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/Harbour-300x225.jpg")),
        "{body}"
    );
    assert!(body["data"]["filesizeHumanReadable"].is_string(), "{body}");
    assert_eq!(
        body["data"]["editLink"],
        format!("/mc-admin/attachment/{first}/edit")
    );
    for id in [
        post_id.to_string(),
        "0".to_owned(),
        "x".to_owned(),
        String::new(),
    ] {
        let response = client
            .get(format!(
                "/mc-admin/admin-ajax.php?action=get-attachment&id={id}"
            ))
            .dispatch()
            .await;
        let (status, body, _) = json_of(response).await;
        assert_eq!(status, Status::NotFound, "{id}");
        assert_eq!(body, json!({ "success": false }), "{id}");
    }

    // The post and page editors have the featured image field, which the editor script turns
    // into the box; its script is served with WordPress's wording, tabs in WordPress's order.
    for (post_type, id) in [("post", post_id), ("page", page_id)] {
        let (status, form) = get(&client, &format!("/mc-admin/{post_type}/{id}/edit")).await;
        assert_eq!(status, Status::Ok, "{form}");
        assert!(
            form.contains("<div data-ignite-field=\"featured_media\">"),
            "{post_type}: {form}"
        );
        assert!(
            form.contains("name=\"featured_media\" type=\"number\""),
            "{form}"
        );
    }
    let response = client
        .get(format!(
            "/mc-includes/js/featured-image.js?ver={}",
            editor_version()
        ))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        header(&response, "Content-Type"),
        "text/javascript; charset=utf-8"
    );
    let script = response.into_string().await.unwrap();
    for text in [
        "Set featured image",
        "Remove featured image",
        "Click the image to edit or update",
        "Featured image",
        "Load more",
        "action=get-attachment",
        "mc-media-icon",
        "uploaded_to=",
    ] {
        assert!(script.contains(text), "{text}");
    }
    let attached = script
        .find("}, 'Uploaded to this post')")
        .expect("the first tab");
    let library = script.find("}, 'Media Library')").expect("the second tab");
    assert!(attached < library, "Uploaded to this post comes first");

    // Picking stores the id in the field the editor already saves (`_thumbnail_id`).
    let save = |id: String| {
        let csrf = csrf.clone();
        let client = &client;
        async move {
            post(
                client,
                &format!("/mc-admin/post/{post_id}"),
                &[
                    ("_token", csrf.as_str()),
                    ("title", "With a featured image"),
                    ("status", "draft"),
                    ("featured_media", id.as_str()),
                ],
            )
            .await
        }
    };
    let response = save(second.to_string()).await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(
        get_post_meta(&db, post_id, "_thumbnail_id").await.unwrap(),
        Some(json!(second))
    );
    // The form shows the stored image (which the box reads with `get-attachment`).
    let (_, form) = get(&client, &format!("/mc-admin/post/{post_id}/edit")).await;
    assert!(form.contains(&format!("value=\"{second}\"")), "{form}");
    // The server refuses an id that is not a media item with WordPress's message on the
    // field, not Ignite's media check.
    for refused in [post_id.to_string(), "999999999".to_owned(), "-3".to_owned()] {
        let response = save(refused.clone()).await;
        let status = response.status();
        let body = response.into_string().await.unwrap_or_default();
        assert_eq!(status, Status::UnprocessableEntity, "{refused}: {body}");
        assert!(
            body.contains("Invalid featured media ID."),
            "{refused}: {body}"
        );
        assert!(!body.contains("must be the id of a media item"), "{body}");
    }
    assert_eq!(
        get_post_meta(&db, post_id, "_thumbnail_id").await.unwrap(),
        Some(json!(second))
    );
    // A PDF is a media item too: WordPress's featured image modal lists every type.
    let response = save(report.to_string()).await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(
        get_post_meta(&db, post_id, "_thumbnail_id").await.unwrap(),
        Some(json!(report))
    );
    // "Remove featured image" empties the field: the save deletes the meta.
    let response = save(String::new()).await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(
        get_post_meta(&db, post_id, "_thumbnail_id").await.unwrap(),
        None
    );

    // A subscriber may not read the library.
    let (_, dashboard) = get(&client, "/mc-admin").await;
    let response = post(
        &client,
        "/mc-admin/logout",
        &[("_token", &token_in(&dashboard))],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    sign_in(&client, &subscriber).await;
    let response = client
        .get(format!(
            "/mc-admin/admin-ajax.php?action=get-attachment&id={first}"
        ))
        .dispatch()
        .await;
    assert_eq!(json_of(response).await.0, Status::Forbidden);
    db.close().await;
}
