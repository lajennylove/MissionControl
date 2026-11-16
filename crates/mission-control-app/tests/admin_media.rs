//! The admin's media library (Task 6.4, Rulings R127 to R129): the `attachment` screen, uploads
//! with the admin session and Ignite's CSRF token (`/mc-admin/async-upload.php` and the upload
//! page), the picker's query (`/mc-admin/admin-ajax.php`), and Ignite's media fields.
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
use mission_control_core::content::{Post, get_post_meta, set_option};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::{Database, Model};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// Held by every test here: the flow sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of every test user.
const PASSWORD: &str = "correct horse battery staple";

/// The upload limit of the test app: 512 KiB.
const UPLOAD_MAX: u64 = 512 * 1024;

/// The multipart boundary the tests send.
const BOUNDARY: &str = "mc-test-boundary-7d1f";

fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
}

fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

fn config(database_url: &str, uploads_dir: PathBuf) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: UPLOAD_MAX,
        themes_dir: shipped_themes(),
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

/// A `multipart/form-data` body with `fields` and, when given, the file part `async-upload`.
fn multipart(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some((file_name, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"async-upload\"; \
                 filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn multipart_type() -> ContentType {
    ContentType::new("multipart", "form-data").with_params(("boundary", BOUNDARY))
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

async fn token(client: &Client) -> String {
    let (status, html) = get(client, "/mc-admin/login").await;
    assert_eq!(status, Status::Ok, "{html}");
    token_in(&html)
}

async fn sign_in(client: &Client, email: &str) {
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
}

async fn sign_out(client: &Client) {
    let csrf = token(client).await;
    let response = post(client, "/mc-admin/logout", &[("_token", &csrf)]).await;
    assert_eq!(response.status(), Status::SeeOther);
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

/// A JSON answer: its status, body and `X-MC-Total`.
async fn json_of(response: LocalResponse<'_>) -> (Status, Value, String) {
    let status = response.status();
    assert_eq!(
        header(&response, "Content-Type"),
        "application/json",
        "{status}"
    );
    assert_eq!(header(&response, "Cache-Control"), "no-store");
    let total = header(&response, "X-MC-Total");
    let body = response.into_string().await.unwrap_or_default();
    let value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    (status, value, total)
}

/// Uploads `file` to `async-upload.php` with the token in the header (when given) and the
/// form's other `fields`.
async fn upload<'c>(
    client: &'c Client,
    header_token: Option<&str>,
    fields: &[(&str, &str)],
    file: Option<(&str, &[u8])>,
) -> LocalResponse<'c> {
    let mut request = client
        .post("/mc-admin/async-upload.php")
        .header(multipart_type())
        .body(multipart(fields, file));
    if let Some(token) = header_token {
        request = request.header(Header::new("X-CSRF-TOKEN", token.to_owned()));
    }
    request.dispatch().await
}

async fn attachments(db: &Database) -> i64 {
    Post::query()
        .where_eq("post_type", "attachment")
        .count(db)
        .await
        .unwrap()
}

#[test]
fn the_media_library_uploads_lists_edits_and_deletes() {
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
        .block_on(scratch_database("media"));
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
    let author = user(&db, "arthur", "author").await;
    let subscriber = user(&db, "sue", "subscriber").await;
    let jpeg = jpeg_bytes(800, 600);
    assert!((jpeg.len() as u64) < UPLOAD_MAX, "{}", jpeg.len());

    // A visitor: JSON 401 from the endpoints, the sign-in page from the screens.
    let (status, body, _) = json_of(upload(&client, None, &[], Some(("a.jpg", &jpeg))).await).await;
    assert_eq!(status, Status::Unauthorized);
    assert_eq!(body["success"], false);
    let response = client
        .get("/mc-admin/admin-ajax.php?action=query-attachments")
        .dispatch()
        .await;
    assert_eq!(json_of(response).await.0, Status::Unauthorized);
    for uri in ["/mc-admin/attachment", "/mc-admin/attachment/new"] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::SeeOther, "{uri}");
        assert_eq!(header(&response, "Location"), "/mc-admin/login", "{uri}");
    }
    assert_eq!(attachments(&db).await, 0);

    sign_in(&client, &editor).await;
    let (status, dashboard) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::Ok);
    assert!(dashboard.contains("Media"), "{dashboard}");
    let csrf = token(&client).await;

    // A post to attach uploads to (the editor's Add Media sends its id).
    let edit = header(
        &client.get("/mc-admin/post/new").dispatch().await,
        "Location",
    );
    let post_id: i64 = edit
        .strip_prefix("/mc-admin/post/")
        .and_then(|rest| rest.strip_suffix("/edit"))
        .unwrap()
        .parse()
        .unwrap();
    let post_id_text = post_id.to_string();

    // Without the token, or with a wrong one: 419, nothing stored.
    let response = upload(&client, None, &[], Some(("photo.jpg", &jpeg))).await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status.code, 419, "{body}");
    let response = upload(&client, Some("wrong"), &[], Some(("photo.jpg", &jpeg))).await;
    assert_eq!(json_of(response).await.0.code, 419);
    let response = upload(
        &client,
        None,
        &[("_token", "wrong")],
        Some(("photo.jpg", &jpeg)),
    )
    .await;
    assert_eq!(json_of(response).await.0.code, 419);
    assert_eq!(attachments(&db).await, 0);

    // With the token in the header: the attachment with its sizes, attached to the post.
    let response = upload(
        &client,
        Some(&csrf),
        &[("post_id", &post_id_text)],
        Some(("Harbour Boats.jpg", &jpeg)),
    )
    .await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_eq!(body["success"], true);
    let item = &body["data"];
    let photo = item["id"].as_i64().expect("an id");
    assert_eq!(item["title"], "Harbour Boats");
    assert_eq!(item["filename"], "Harbour-Boats.jpg");
    assert_eq!(item["uploadedTo"], post_id);
    assert_eq!(item["type"], "image");
    assert_eq!(
        item["editLink"],
        format!("/mc-admin/attachment/{photo}/edit")
    );
    let url = item["url"].as_str().unwrap().to_owned();
    assert!(
        url.starts_with("http://localhost/mc-content/uploads/")
            && url.ends_with("/Harbour-Boats.jpg"),
        "{url}"
    );
    let medium = item["sizes"]["medium"]["url"].as_str().unwrap().to_owned();
    assert!(medium.ends_with("/Harbour-Boats-300x225.jpg"), "{medium}");
    let thumbnail = item["sizes"]["thumbnail"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let file = url
        .strip_prefix("http://localhost/mc-content/uploads/")
        .unwrap()
        .to_owned();
    let folder = file.rsplit_once('/').unwrap().0.to_owned();
    let on_disk = |name: &str| uploads.path().join(&folder).join(name);
    for name in [
        "Harbour-Boats.jpg",
        "Harbour-Boats-300x225.jpg",
        "Harbour-Boats-150x150.jpg",
    ] {
        assert!(on_disk(name).exists(), "{name}");
    }
    assert!(
        thumbnail.ends_with("/Harbour-Boats-150x150.jpg"),
        "{thumbnail}"
    );
    // The file is served from the uploads, as data.
    let response = client
        .get(format!("/mc-content/uploads/{file}"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(header(&response, "X-Content-Type-Options"), "nosniff");
    let stored = Post::query().find(&*db, photo).await.unwrap().unwrap();
    assert_eq!(stored.parent_id, Some(post_id));
    assert_eq!(stored.post_type, "attachment");

    // With the token in the form instead: a PDF, not attached.
    let pdf: &[u8] =
        b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";
    let response = upload(
        &client,
        None,
        &[("_token", &csrf)],
        Some(("Annual Report.pdf", pdf)),
    )
    .await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status, Status::Ok, "{body}");
    let report = body["data"]["id"].as_i64().unwrap();
    assert_eq!(body["data"]["uploadedTo"], 0);
    assert_eq!(body["data"]["type"], "application");

    // Refused files: a type that is not allowed, a lie about the type, no file, too large.
    let response = upload(
        &client,
        Some(&csrf),
        &[],
        Some(("shell.php", b"<?php echo 1;")),
    )
    .await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status, Status::BadRequest);
    assert_eq!(
        body["data"]["message"],
        "Sorry, you are not allowed to upload this file type."
    );
    assert_eq!(body["data"]["filename"], "shell.php");
    let response = upload(&client, Some(&csrf), &[], Some(("fake.jpg", b"not a jpeg"))).await;
    assert_eq!(json_of(response).await.0, Status::BadRequest);
    let response = upload(&client, Some(&csrf), &[], None).await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status, Status::BadRequest);
    assert_eq!(body["data"]["message"], "No file was uploaded.");
    let big = vec![b'a'; (UPLOAD_MAX + 1) as usize];
    let response = upload(&client, Some(&csrf), &[], Some(("big.txt", &big))).await;
    assert_eq!(json_of(response).await.0, Status::PayloadTooLarge);
    let huge = vec![b'a'; (UPLOAD_MAX + 2 * 1024 * 1024) as usize];
    let response = upload(&client, Some(&csrf), &[], Some(("huge.txt", &huge))).await;
    assert_eq!(json_of(response).await.0, Status::PayloadTooLarge);
    let response = client
        .post("/mc-admin/async-upload.php")
        .header(ContentType::Form)
        .header(Header::new("X-CSRF-TOKEN", csrf.clone()))
        .body("a=b")
        .dispatch()
        .await;
    assert_eq!(json_of(response).await.0, Status::BadRequest);
    // A post the editor cannot attach to: none with that id.
    let response = upload(
        &client,
        Some(&csrf),
        &[("post_id", "999999999")],
        Some(("photo.jpg", &jpeg)),
    )
    .await;
    assert_eq!(json_of(response).await.0, Status::Forbidden);
    assert_eq!(attachments(&db).await, 2);

    // The picker's query: newest first, searched, paged, with WordPress's total.
    let response = client
        .get("/mc-admin/admin-ajax.php?action=query-attachments")
        .dispatch()
        .await;
    let (status, body, total) = json_of(response).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(total, "2");
    let ids: Vec<i64> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [report, photo]);
    let response = client
        .get("/mc-admin/admin-ajax.php?action=query-attachments&search=harbour&page=1&per_page=500")
        .dispatch()
        .await;
    let (_, body, total) = json_of(response).await;
    assert_eq!(total, "1");
    assert_eq!(body["data"][0]["id"], photo);
    let response = client
        .get("/mc-admin/admin-ajax.php?action=query-attachments&type=image&page=x")
        .dispatch()
        .await;
    assert_eq!(json_of(response).await.2, "1");
    let response = client
        .get("/mc-admin/admin-ajax.php?action=delete-post")
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(response.into_string().await.unwrap(), "0");

    // The screen: listed with the file name and type, searchable.
    let (status, list) = get(&client, "/mc-admin/attachment").await;
    assert_eq!(status, Status::Ok);
    assert!(list.contains("Harbour Boats"), "{list}");
    assert!(list.contains("Harbour-Boats.jpg"), "{list}");
    assert!(list.contains("application/pdf"), "{list}");
    assert!(
        list.contains(&format!("/mc-admin/attachment/{photo}/edit")),
        "{list}"
    );

    // The edit form saves the title, alt text, caption and description.
    let (status, form_page) = get(&client, &format!("/mc-admin/attachment/{photo}/edit")).await;
    assert_eq!(status, Status::Ok, "{form_page}");
    assert!(form_page.contains("name=\"alt\""), "{form_page}");
    let response = post(
        &client,
        &format!("/mc-admin/attachment/{photo}"),
        &[
            ("_token", &csrf),
            ("title", "Boats at dawn"),
            ("alt", "Fishing <b>boats</b> in the harbour"),
            ("caption", "Taken at <em>six</em>"),
            ("description", "<p>A long description</p>"),
        ],
    )
    .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "{body}");
    let saved = Post::query().find(&*db, photo).await.unwrap().unwrap();
    assert_eq!(saved.title, "Boats at dawn");
    assert_eq!(saved.excerpt, "Taken at <em>six</em>");
    assert_eq!(saved.content, "<p>A long description</p>");
    assert_eq!(
        get_post_meta(&db, photo, "_wp_attachment_image_alt")
            .await
            .unwrap(),
        Some(json!("Fishing boats in the harbour"))
    );
    let response = client
        .get("/mc-admin/admin-ajax.php?action=query-attachments&search=dawn")
        .dispatch()
        .await;
    let (_, body, _) = json_of(response).await;
    assert_eq!(body["data"][0]["alt"], "Fishing boats in the harbour");
    let (_, list) = get(&client, "/mc-admin/attachment?q=dawn").await;
    assert!(list.contains("Boats at dawn"), "{list}");
    assert!(!list.contains("Annual Report"), "{list}");

    // Ignite's create is refused: files are uploaded.
    let response = post(
        &client,
        "/mc-admin/attachment",
        &[("_token", &csrf), ("title", "x")],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);

    // The upload page (Ruling R129): a form that works without JavaScript.
    let (status, page) = get(&client, "/mc-admin/attachment/new").await;
    assert_eq!(status, Status::Ok, "{page}");
    assert!(page.contains("enctype=\"multipart/form-data\""), "{page}");
    assert!(page.contains("name=\"async-upload\""), "{page}");
    assert!(
        page.contains("action=\"/mc-admin/attachment/new\""),
        "{page}"
    );
    assert!(page.contains("Maximum upload file size: 512 KB."), "{page}");
    assert!(
        page.contains(&format!(
            "/mc-includes/css/editor.css?ver={}",
            editor_version()
        )),
        "{page}"
    );
    let page_token = token_in(&page);
    assert_eq!(page_token, csrf);
    let browser_upload = |fields: Vec<(&'static str, String)>, file: (&'static str, Vec<u8>)| {
        let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        client
            .post("/mc-admin/attachment/new")
            .header(multipart_type())
            .body(multipart(&fields, Some((file.0, &file.1))))
    };
    let response = browser_upload(
        vec![("_token", csrf.clone())],
        ("Lighthouse.jpg", jpeg.clone()),
    )
    .dispatch()
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    let to = header(&response, "Location");
    let lighthouse: i64 = to
        .strip_prefix("/mc-admin/attachment/")
        .and_then(|rest| rest.strip_suffix("/edit"))
        .unwrap_or_else(|| panic!("to the edit form: {to}"))
        .parse()
        .unwrap();
    assert_eq!(get(&client, &to).await.0, Status::Ok);
    let response = browser_upload(vec![], ("Lighthouse.jpg", jpeg.clone()))
        .dispatch()
        .await;
    assert_eq!(response.status().code, 419);
    let body = response.into_string().await.unwrap();
    assert!(
        body.contains("The link you followed has expired."),
        "{body}"
    );
    let response = browser_upload(
        vec![("_token", csrf.clone())],
        ("<b>evil</b>.exe", b"MZ".to_vec()),
    )
    .dispatch()
    .await;
    assert_eq!(response.status(), Status::BadRequest);
    let body = response.into_string().await.unwrap();
    assert!(body.contains("&lt;b&gt;evil&lt;/b&gt;.exe"), "{body}");
    assert!(!body.contains("<b>evil"), "{body}");
    assert_eq!(attachments(&db).await, 3);

    // HEAD on the new routes creates nothing and sends no body: the upload page and the query
    // answer as their GET does; `async-upload.php` has no GET, so HEAD finds no route (Ignite's
    // list route has no resource of that name).
    for (uri, expected) in [
        ("/mc-admin/attachment/new", Status::Ok),
        (
            "/mc-admin/admin-ajax.php?action=query-attachments",
            Status::Ok,
        ),
        ("/mc-admin/async-upload.php", Status::NotFound),
    ] {
        let response = client.head(uri).dispatch().await;
        assert_eq!(response.status(), expected, "{uri}");
        assert!(
            response.into_bytes().await.unwrap_or_default().is_empty(),
            "{uri}"
        );
    }
    assert_eq!(attachments(&db).await, 3);

    // Ignite's Image field (the featured image): an attachment's id is accepted and shown,
    // anything else refused.
    let response = post(
        &client,
        &format!("/mc-admin/post/{post_id}"),
        &[
            ("_token", &csrf),
            ("title", "With a picture"),
            ("status", "draft"),
            ("featured_media", &post_id_text),
        ],
    )
    .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::UnprocessableEntity, "{body}");
    assert!(body.contains("must be the id of a media item"), "{body}");
    let response = post(
        &client,
        &format!("/mc-admin/post/{post_id}"),
        &[
            ("_token", &csrf),
            ("title", "With a picture"),
            ("status", "draft"),
            ("featured_media", &photo.to_string()),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    assert_eq!(
        get_post_meta(&db, post_id, "_thumbnail_id").await.unwrap(),
        Some(json!(photo))
    );
    let (_, edit_page) = get(&client, &edit).await;
    assert!(edit_page.contains("Boats at dawn"), "{edit_page}");
    assert!(
        edit_page.contains("href=\"/mc-admin/attachment\""),
        "{edit_page}"
    );
    // The editor's form loads editor.js, which loads the picker, served from the bundle.
    let response = client
        .get(format!("/mc-includes/js/media.js?ver={}", editor_version()))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        header(&response, "Content-Type"),
        "text/javascript; charset=utf-8"
    );
    let script = response.into_string().await.unwrap();
    assert!(script.contains("query-attachments"), "the picker script");

    // Delete removes the attachment and its files for good, and the featured image.
    let response = post(
        &client,
        &format!("/mc-admin/attachment/{photo}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther);
    assert!(Post::query().find(&*db, photo).await.unwrap().is_none());
    for name in [
        "Harbour-Boats.jpg",
        "Harbour-Boats-300x225.jpg",
        "Harbour-Boats-150x150.jpg",
    ] {
        assert!(!on_disk(name).exists(), "{name} is deleted");
    }
    assert_eq!(
        get_post_meta(&db, post_id, "_thumbnail_id").await.unwrap(),
        None
    );
    assert_eq!(
        get(&client, &format!("/mc-admin/attachment/{photo}/edit"))
            .await
            .0,
        Status::NotFound
    );
    // A post is not an attachment: its id is no media screen's.
    assert_eq!(
        get(&client, &format!("/mc-admin/attachment/{post_id}/edit"))
            .await
            .0,
        Status::NotFound
    );
    sign_out(&client).await;

    // An author may upload (upload_files) but not use the screens (Ruling R127), nor attach to
    // the editor's post.
    sign_in(&client, &author).await;
    let csrf = token(&client).await;
    let response = upload(&client, Some(&csrf), &[], Some(("mine.jpg", &jpeg))).await;
    assert_eq!(json_of(response).await.0, Status::Ok);
    let response = upload(
        &client,
        Some(&csrf),
        &[("post_id", &post_id_text)],
        Some(("theirs.jpg", &jpeg)),
    )
    .await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status, Status::Forbidden);
    assert_eq!(
        body["data"]["message"],
        "Sorry, you are not allowed to attach files to this post."
    );
    for uri in [
        "/mc-admin/attachment".to_owned(),
        "/mc-admin/attachment/new".to_owned(),
        format!("/mc-admin/attachment/{lighthouse}/edit"),
    ] {
        let (status, body) = get(&client, &uri).await;
        assert_eq!(status, Status::Forbidden, "{uri}: {body}");
        if uri.ends_with("/new") {
            // `media-new.php`'s refusal.
            assert!(
                body.contains("Sorry, you are not allowed to upload files."),
                "{uri}: {body}"
            );
        }
    }
    let response = post(
        &client,
        &format!("/mc-admin/attachment/{lighthouse}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::Forbidden);
    assert!(
        Post::query()
            .find(&*db, lighthouse)
            .await
            .unwrap()
            .is_some()
    );
    sign_out(&client).await;

    // A subscriber may not upload or list.
    sign_in(&client, &subscriber).await;
    let csrf = token(&client).await;
    let before = attachments(&db).await;
    let response = upload(&client, Some(&csrf), &[], Some(("nope.jpg", &jpeg))).await;
    let (status, body, _) = json_of(response).await;
    assert_eq!(status, Status::Forbidden);
    assert_eq!(
        body["data"]["message"],
        "Sorry, you are not allowed to upload files."
    );
    let response = client
        .get("/mc-admin/admin-ajax.php?action=query-attachments")
        .dispatch()
        .await;
    assert_eq!(json_of(response).await.0, Status::Forbidden);
    assert_eq!(attachments(&db).await, before);

    // Fail closed: a second app in the process finds Ignite's `attachment` taken by the first;
    // it does not serve the media screens, and refuses them instead of letting Ignite's coarse
    // grants decide.
    let second = Client::tracked(rocket_with_options(
        config(scratch.url(), uploads.path().to_path_buf()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the second app ignites");
    sign_in(&second, &editor).await;
    for uri in [
        "/mc-admin/attachment".to_owned(),
        "/mc-admin/attachment/new".to_owned(),
        format!("/mc-admin/attachment/{lighthouse}/edit"),
    ] {
        let (status, body) = get(&second, &uri).await;
        assert_eq!(status, Status::ServiceUnavailable, "{uri}: {body}");
        assert!(body.contains("not available"), "{uri}: {body}");
    }
    let csrf = token(&second).await;
    let response = post(
        &second,
        &format!("/mc-admin/attachment/{lighthouse}/delete"),
        &[("_token", &csrf)],
    )
    .await;
    assert_eq!(response.status(), Status::ServiceUnavailable);
    assert!(
        Post::query()
            .find(&*db, lighthouse)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(get(&second, "/mc-admin").await.0, Status::Ok);
    db.close().await;
}

#[test]
fn the_endpoints_are_not_mounted_without_the_panel() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    rocket::async_test(on_sqlite());
}

async fn on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("media.db").display()
    );
    let db = Arc::new(Database::connect(&url).await.unwrap());
    mission_control_testing::create_users_table(&db).await;
    mission_control_core::migrations::migrator()
        .unwrap()
        .run(&db)
        .await
        .unwrap();
    let client = Client::tracked(rocket_with_options(
        config(&url, dir.path().join("uploads")),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    // The admin's 503 page answers every media path.
    for uri in [
        "/mc-admin/attachment",
        "/mc-admin/attachment/new",
        "/mc-admin/admin-ajax.php?action=query-attachments",
    ] {
        let (status, body) = get(&client, uri).await;
        assert_eq!(status, Status::ServiceUnavailable, "{uri}");
        assert!(body.contains("MySQL or MariaDB"), "{uri}: {body}");
    }
    let response = client
        .post("/mc-admin/async-upload.php")
        .header(multipart_type())
        .body(multipart(&[], None))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::ServiceUnavailable);
    db.close().await;
}
