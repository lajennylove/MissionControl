//! The media controller (Task 10.4): `/wp-json/wp/v2/media`, as WordPress's
//! `WP_REST_Attachments_Controller` answers it, and the uploads served at
//! `/wp-content/uploads/`.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.
//! Each site stores its uploads in its own temporary folder.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{NewPost, Post, get_post_meta, set_option};
use mission_control_core::migrations::migrator;
use mission_control_core::users::app_password::{self, PlainPassword};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, InMemoryUserRoles, RoleAssignment, UserRoles,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The keys of an attachment in `context=view`, as WordPress 6.7's `wp/v2/media` gives them.
const VIEW_KEYS: &[&str] = &[
    "id",
    "date",
    "date_gmt",
    "guid",
    "modified",
    "modified_gmt",
    "slug",
    "status",
    "type",
    "link",
    "title",
    "author",
    "comment_status",
    "ping_status",
    "template",
    "meta",
    "class_list",
    "description",
    "caption",
    "alt_text",
    "media_type",
    "mime_type",
    "media_details",
    "post",
    "source_url",
    "_links",
];

/// What `context=edit` adds to the view keys.
const EDIT_EXTRA_KEYS: &[&str] = &[
    "permalink_template",
    "generated_slug",
    "missing_image_sizes",
];

/// The site address the tests set.
const HOME: &str = "http://example.test";

/// The upload limit the tests set: 512 KiB.
const UPLOAD_LIMIT: u64 = 512 * 1024;

/// A signed-in test user.
struct User {
    id: i64,
    email: String,
    password: PlainPassword,
}

/// One test site: its database, its uploads folder, its users by role, and a client.
struct Site {
    test: TestDatabase,
    uploads: TempDir,
    client: Client,
    users: BTreeMap<&'static str, User>,
}

impl Site {
    fn db(&self) -> &Database {
        self.test.db()
    }

    fn id(&self, name: &str) -> i64 {
        self.users[name].id
    }

    fn auth(&self, name: &str) -> Header<'static> {
        let user = &self.users[name];
        Header::new(
            "Authorization",
            format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{}", user.email, user.password.as_str()))
            ),
        )
    }

    async fn get(&self, uri: &str, as_user: Option<&str>) -> LocalResponse<'_> {
        let mut request = self.client.get(uri.to_owned());
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        request.dispatch().await
    }

    /// A raw upload: the body is the file, named by `Content-Disposition`.
    async fn upload_raw(
        &self,
        as_user: Option<&str>,
        file_name: &str,
        content_type: &str,
        bytes: Vec<u8>,
        query: &str,
    ) -> LocalResponse<'_> {
        let mut request = self
            .client
            .post(format!("/wp-json/wp/v2/media{query}"))
            .header(Header::new("Content-Type", content_type.to_owned()))
            .header(Header::new(
                "Content-Disposition",
                format!("attachment; filename=\"{file_name}\""),
            ))
            .body(bytes);
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        request.dispatch().await
    }

    /// A multipart upload: a `file` part and text `fields`.
    async fn upload_multipart(
        &self,
        as_user: Option<&str>,
        file_name: &str,
        bytes: &[u8],
        fields: &[(&str, &str)],
    ) -> LocalResponse<'_> {
        let boundary = "mc-test-boundary-7f3a";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let mut request = self
            .client
            .post("/wp-json/wp/v2/media")
            .header(Header::new(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            ))
            .body(body);
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        request.dispatch().await
    }

    async fn send_json(
        &self,
        method: &str,
        uri: &str,
        as_user: Option<&str>,
        body: Value,
    ) -> LocalResponse<'_> {
        let request = match method {
            "POST" => self.client.post(uri.to_owned()),
            "PUT" => self.client.put(uri.to_owned()),
            "PATCH" => self.client.patch(uri.to_owned()),
            "DELETE" => self.client.delete(uri.to_owned()),
            other => panic!("unexpected method {other}"),
        };
        let mut request = request.header(ContentType::JSON).body(body.to_string());
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        request.dispatch().await
    }

    /// The file at `path` under the uploads folder, if it is there.
    fn stored(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.uploads.path().join(path)).ok()
    }

    /// The path of `url` on the site, without the address.
    fn path_of(url: &Value) -> String {
        url.as_str()
            .unwrap_or_else(|| panic!("a URL: {url}"))
            .strip_prefix(HOME)
            .unwrap_or_else(|| panic!("{url} is on {HOME}"))
            .to_owned()
    }
}

fn config(uploads: &Path) -> Config {
    Config {
        database_url: "unused: the database is passed in".to_owned(),
        secret_key: "unused".to_owned(),
        debug: false,
        upload_max_bytes: UPLOAD_LIMIT,
        themes_dir: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../themes")
            .to_path_buf(),
        uploads_dir: uploads.to_path_buf(),
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment: Environment::Local,
        trusted_proxies: TrustedProxies::None,
    }
}

/// Inserts a user named `name` and returns its id and email.
async fn new_user(db: &Database, name: &str) -> (i64, String) {
    let email = unique_email(name);
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &[name.into(), email.clone().into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    let id = raw::fetch_scalar(
        db,
        "SELECT id FROM users WHERE email = ?",
        &[email.clone().into()],
    )
    .await
    .expect("reads the user id");
    (id, email)
}

/// A site with an administrator, an editor, two authors and a contributor, and its uploads in a
/// temporary folder.
async fn site() -> Site {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    for (name, value) in [
        ("home", json!(HOME)),
        // Uploads are served by the app at `home`, whatever `siteurl` says.
        ("siteurl", json!(format!("{HOME}/cms"))),
        ("permalink_structure", json!("/%postname%/")),
    ] {
        set_option(db, name, &value).await.expect("sets the option");
    }
    let roles = InMemoryUserRoles::new();
    let mut users = BTreeMap::new();
    for (name, role) in [
        ("admin", ADMINISTRATOR),
        ("editor", EDITOR),
        ("author", AUTHOR),
        ("author2", AUTHOR),
        ("contributor", CONTRIBUTOR),
    ] {
        let (id, email) = new_user(db, name).await;
        let (password, _) = app_password::create(db, id, "Tests", None)
            .await
            .expect("creates the application password");
        roles.assign(id, RoleAssignment::new([role], BTreeMap::new()));
        users.insert(
            name,
            User {
                id,
                email,
                password,
            },
        );
    }
    let uploads = tempfile::tempdir().expect("creates the uploads folder");
    let user_roles: Arc<dyn UserRoles> = Arc::new(roles);
    let options = AppOptions {
        database: Some(test.shared()),
        user_roles: Some(user_roles),
        ..AppOptions::default()
    };
    let client = Client::tracked(rocket_with_options(config(uploads.path()), options))
        .await
        .expect("the app ignites");
    Site {
        test,
        uploads,
        client,
        users,
    }
}

async fn body(response: LocalResponse<'_>) -> (Status, Value) {
    let status = response.status();
    let text = response.into_string().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{status}: {e}: {text}"));
    (status, json)
}

/// Asserts that `response` is WordPress's error `code` with `status`.
async fn assert_error(response: LocalResponse<'_>, status: Status, code: &str) {
    let (got, json) = body(response).await;
    assert_eq!((got, json["code"].as_str()), (status, Some(code)), "{json}");
    assert_eq!(json["data"]["status"], json!(status.code));
}

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("an object: {value}"))
        .keys()
        .cloned()
        .collect()
}

fn set(lists: &[&[&str]]) -> BTreeSet<String> {
    lists
        .iter()
        .flat_map(|list| list.iter())
        .map(|s| (*s).to_owned())
        .collect()
}

/// A gradient JPEG, so its resized copies are real images.
fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 80)
        .encode_image(&image)
        .unwrap();
    out
}

/// Uploads a 400×300 JPEG as `as_user` and returns the attachment's JSON.
async fn uploaded(site: &Site, as_user: &str, file_name: &str) -> Value {
    let response = site
        .upload_raw(Some(as_user), file_name, "image/jpeg", jpeg(400, 300), "")
        .await;
    let (status, item) = body(response).await;
    assert_eq!(status, Status::Created, "{item}");
    item
}

#[rocket::async_test]
async fn a_raw_upload_makes_an_attachment_with_its_sizes() {
    let site = site().await;
    let bytes = jpeg(400, 300);
    let response = site
        .upload_raw(
            Some("author"),
            "Harbour View.jpg",
            "image/jpeg",
            bytes.clone(),
            "",
        )
        .await;
    assert_eq!(response.status(), Status::Created);
    let location = response
        .headers()
        .get_one("Location")
        .map(str::to_owned)
        .expect("a Location header");
    let (_, item) = body(response).await;
    let id = item["id"].as_i64().unwrap();
    assert_eq!(location, format!("{HOME}/wp-json/wp/v2/media/{id}"));
    assert_eq!(keys(&item), set(&[VIEW_KEYS, EDIT_EXTRA_KEYS]));

    // WordPress's REST rule: the stored file's name without its extension.
    assert_eq!(item["title"]["raw"], "Harbour-View");
    assert_eq!(item["type"], "attachment");
    assert_eq!(item["status"], "inherit");
    assert_eq!(item["author"], json!(site.id("author")));
    assert_eq!(item["media_type"], "image");
    assert_eq!(item["mime_type"], "image/jpeg");
    assert_eq!(item["post"], Value::Null);
    assert_eq!(item["alt_text"], "");
    assert_eq!(item["missing_image_sizes"], json!([]));
    assert_eq!(
        item["class_list"],
        json!([
            format!("post-{id}"),
            "attachment",
            "type-attachment",
            "status-inherit",
            "hentry"
        ])
    );
    let source = item["source_url"].as_str().unwrap().to_owned();
    let file = item["media_details"]["file"].as_str().unwrap().to_owned();
    assert!(file.ends_with("/Harbour-View.jpg"), "{file}");
    assert_eq!(source, format!("{HOME}/wp-content/uploads/{file}"));
    assert_eq!(item["guid"]["raw"], json!(source));
    assert_eq!(item["media_details"]["width"], 400);
    assert_eq!(item["media_details"]["height"], 300);
    let sizes = item["media_details"]["sizes"].as_object().unwrap();
    assert_eq!(
        sizes.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["full", "medium", "thumbnail"])
    );
    assert_eq!(sizes["thumbnail"]["width"], 150);
    assert_eq!(sizes["thumbnail"]["mime_type"], "image/jpeg");
    assert_eq!(sizes["full"]["source_url"], json!(source));
    assert!(
        !item["_links"]
            .as_object()
            .unwrap()
            .contains_key("wp:attachment")
    );

    // The file and its sizes are on disk and served, with the stored type and nosniff.
    assert_eq!(site.stored(&file), Some(bytes.clone()));
    let response = site.get(&Site::path_of(&item["source_url"]), None).await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.content_type(), Some(ContentType::JPEG));
    assert_eq!(
        response.headers().get_one("X-Content-Type-Options"),
        Some("nosniff")
    );
    assert_eq!(
        response.headers().get_one("Content-Security-Policy"),
        Some("sandbox")
    );
    assert_eq!(response.into_bytes().await, Some(bytes));
    let thumbnail = site
        .get(&Site::path_of(&sizes["thumbnail"]["source_url"]), None)
        .await;
    assert_eq!(thumbnail.status(), Status::Ok);

    // A visitor reads it in the view context.
    let (status, view) = body(site.get(&format!("/wp-json/wp/v2/media/{id}"), None).await).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(keys(&view), set(&[VIEW_KEYS]));
    assert!(view["title"].get("raw").is_none());
    assert_eq!(view["title"]["rendered"], "Harbour-View");
}

#[rocket::async_test]
async fn a_multipart_upload_takes_its_fields() {
    let site = site().await;
    let post = Post::create(
        site.db(),
        NewPost {
            title: "A draft".to_owned(),
            ..NewPost::new(site.id("author"), "post")
        },
    )
    .await
    .unwrap();
    let post_id = post.id.to_string();
    let response = site
        .upload_multipart(
            Some("author"),
            "Boats.jpg",
            &jpeg(200, 150),
            &[
                ("title", "Boats at dawn"),
                ("alt_text", "Two boats <b>moored</b>"),
                ("caption", "On the water"),
                ("post", &post_id),
            ],
        )
        .await;
    let (status, item) = body(response).await;
    assert_eq!(status, Status::Created, "{item}");
    assert_eq!(item["title"]["raw"], "Boats at dawn");
    assert_eq!(item["alt_text"], "Two boats moored");
    assert_eq!(item["caption"]["raw"], "On the water");
    assert_eq!(item["caption"]["rendered"], "<p>On the water</p>\n");
    assert_eq!(item["post"], json!(post.id));

    // Listed under its post, by type, by MIME type and by file name.
    for query in [
        format!("parent={}", post.id),
        "media_type=image".to_owned(),
        "mime_type=image/jpeg".to_owned(),
        "search=boats.jpg".to_owned(),
    ] {
        let response = site
            .get(&format!("/wp-json/wp/v2/media?{query}"), Some("author"))
            .await;
        let (status, list) = body(response).await;
        assert_eq!(status, Status::Ok, "{query}: {list}");
        assert_eq!(list[0]["id"], item["id"], "{query}");
    }
    let (_, none) = body(
        site.get("/wp-json/wp/v2/media?media_type=video", Some("author"))
            .await,
    )
    .await;
    assert_eq!(none, json!([]));
}

#[rocket::async_test]
async fn refused_uploads_get_wordpresss_codes() {
    let site = site().await;
    // A type that is not allowed, by raw body and by multipart.
    assert_error(
        site.upload_raw(
            Some("author"),
            "shell.php",
            "application/x-php",
            b"<?php echo 1;".to_vec(),
            "",
        )
        .await,
        Status::BadRequest,
        "rest_upload_sideload_error",
    )
    .await;
    assert_error(
        site.upload_multipart(Some("author"), "tool.exe", b"MZ\x90\x00", &[])
            .await,
        Status::BadRequest,
        "rest_upload_unknown_error",
    )
    .await;
    // Bytes that are not what the name says.
    assert_error(
        site.upload_raw(
            Some("author"),
            "fake.jpg",
            "image/jpeg",
            b"plain text".to_vec(),
            "",
        )
        .await,
        Status::BadRequest,
        "rest_upload_sideload_error",
    )
    .await;
    // Over the limit, by raw body and by multipart.
    let large = vec![b'a'; UPLOAD_LIMIT as usize + 1];
    assert_error(
        site.upload_raw(Some("author"), "big.txt", "text/plain", large.clone(), "")
            .await,
        Status::BadRequest,
        "rest_upload_file_too_big",
    )
    .await;
    assert_error(
        site.upload_multipart(Some("author"), "big.txt", &large, &[])
            .await,
        Status::BadRequest,
        "rest_upload_file_too_big",
    )
    .await;
    // A raw body needs its name and type.
    let response = site
        .client
        .post("/wp-json/wp/v2/media")
        .header(Header::new("Content-Type", "image/jpeg"))
        .header(site.auth("author"))
        .body(jpeg(20, 20))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::BadRequest,
        "rest_upload_no_content_disposition",
    )
    .await;
    let response = site
        .client
        .post("/wp-json/wp/v2/media")
        .header(Header::new("Content-Type", "image/jpeg"))
        .header(Header::new("Content-Disposition", "attachment"))
        .header(site.auth("author"))
        .body(jpeg(20, 20))
        .dispatch()
        .await;
    assert_error(
        response,
        Status::BadRequest,
        "rest_upload_invalid_disposition",
    )
    .await;
    assert_error(
        site.upload_multipart(Some("author"), "", b"", &[("title", "No file")])
            .await,
        Status::BadRequest,
        "rest_upload_unknown_error",
    )
    .await;
    // A `Content-MD5` that does not match.
    let response = site
        .client
        .post("/wp-json/wp/v2/media")
        .header(Header::new("Content-Type", "text/plain"))
        .header(Header::new(
            "Content-Disposition",
            "attachment; filename=notes.txt",
        ))
        .header(Header::new(
            "Content-MD5",
            "00000000000000000000000000000000",
        ))
        .header(site.auth("author"))
        .body("hello")
        .dispatch()
        .await;
    assert_error(
        response,
        Status::PreconditionFailed,
        "rest_upload_hash_mismatch",
    )
    .await;

    // Users who may not upload: a contributor lacks `upload_files`; a visitor is nobody.
    assert_error(
        site.upload_raw(
            Some("contributor"),
            "photo.jpg",
            "image/jpeg",
            jpeg(20, 20),
            "",
        )
        .await,
        Status::Forbidden,
        "rest_cannot_create",
    )
    .await;
    assert_error(
        site.upload_raw(None, "photo.jpg", "image/jpeg", jpeg(20, 20), "")
            .await,
        Status::Unauthorized,
        "rest_cannot_create",
    )
    .await;
    // Attaching to a post the user may not edit.
    let others = Post::create(
        site.db(),
        NewPost {
            title: "Someone else's".to_owned(),
            ..NewPost::new(site.id("author2"), "post")
        },
    )
    .await
    .unwrap();
    assert_error(
        site.upload_raw(
            Some("author"),
            "photo.jpg",
            "image/jpeg",
            jpeg(20, 20),
            &format!("?post={}", others.id),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_edit",
    )
    .await;

    // Nothing was stored.
    let count: i64 = raw::fetch_scalar(
        site.db(),
        "SELECT COUNT(*) FROM posts WHERE post_type = ?",
        &["attachment".into()],
    )
    .await
    .unwrap();
    assert_eq!(count, 0);
}

#[rocket::async_test]
async fn details_update_through_kses() {
    let site = site().await;
    let item = uploaded(&site, "author", "photo.jpg").await;
    let id = item["id"].as_i64().unwrap();
    let uri = format!("/wp-json/wp/v2/media/{id}");
    let (status, updated) = body(
        site.send_json(
            "POST",
            &uri,
            Some("author"),
            json!({
                "title": "Sunset",
                "alt_text": "The sun <em>setting</em>",
                "caption": {"raw": "Golden hour"},
                "description": "<p>Taken late.</p><script>alert(1)</script>",
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{updated}");
    assert_eq!(updated["title"]["raw"], "Sunset");
    assert_eq!(updated["alt_text"], "The sun setting");
    assert_eq!(updated["caption"]["raw"], "Golden hour");
    // An author has no `unfiltered_html`: the script goes.
    let description = updated["description"]["raw"].as_str().unwrap();
    assert!(description.contains("<p>Taken late.</p>"), "{description}");
    assert!(!description.contains("script"), "{description}");
    assert_eq!(
        get_post_meta(site.db(), id, "_wp_attachment_image_alt")
            .await
            .unwrap(),
        Some(json!("The sun setting"))
    );

    // A PATCH that sends only the alt text keeps the rest.
    let (_, patched) = body(
        site.send_json("PATCH", &uri, Some("author"), json!({"alt_text": "Dusk"}))
            .await,
    )
    .await;
    assert_eq!(patched["alt_text"], "Dusk");
    assert_eq!(patched["title"]["raw"], "Sunset");

    // Attach it to a post, then detach it.
    let post = Post::create(
        site.db(),
        NewPost {
            title: "Mine".to_owned(),
            ..NewPost::new(site.id("author"), "post")
        },
    )
    .await
    .unwrap();
    let (_, attached) = body(
        site.send_json("PUT", &uri, Some("author"), json!({"post": post.id}))
            .await,
    )
    .await;
    assert_eq!(attached["post"], json!(post.id));
    let (_, detached) = body(
        site.send_json("PUT", &uri, Some("author"), json!({"post": 0}))
            .await,
    )
    .await;
    assert_eq!(detached["post"], Value::Null);
    assert_error(
        site.send_json("PUT", &uri, Some("author"), json!({"post": id}))
            .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;

    // Another author may not edit it; a wrong type is refused.
    assert_error(
        site.send_json("POST", &uri, Some("author2"), json!({"title": "Mine now"}))
            .await,
        Status::Forbidden,
        "rest_cannot_edit",
    )
    .await;
    assert_error(
        site.send_json("POST", &uri, Some("author"), json!({"alt_text": 5}))
            .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert_error(
        site.send_json(
            "POST",
            "/wp-json/wp/v2/media/999999999",
            Some("author"),
            json!({"title": "x"}),
        )
        .await,
        Status::NotFound,
        "rest_post_invalid_id",
    )
    .await;
    // The parameters are checked before the attachment is looked up, as WordPress does.
    assert_error(
        site.send_json(
            "POST",
            "/wp-json/wp/v2/media/999999999",
            Some("author"),
            json!({"alt_text": 5}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
}

#[rocket::async_test]
async fn a_forced_delete_removes_the_files() {
    let site = site().await;
    let item = uploaded(&site, "author", "gone.jpg").await;
    let id = item["id"].as_i64().unwrap();
    let file = item["media_details"]["file"].as_str().unwrap().to_owned();
    let folder = file.rsplit_once('/').unwrap().0.to_owned();
    let thumbnail = format!(
        "{folder}/{}",
        item["media_details"]["sizes"]["thumbnail"]["file"]
            .as_str()
            .unwrap()
    );
    assert!(site.stored(&file).is_some() && site.stored(&thumbnail).is_some());
    let uri = format!("/wp-json/wp/v2/media/{id}");

    // No trash for attachments.
    assert_error(
        site.send_json("DELETE", &uri, Some("author"), json!({}))
            .await,
        Status::NotImplemented,
        "rest_trash_not_supported",
    )
    .await;
    assert_error(
        site.send_json(
            "DELETE",
            &format!("{uri}?force=true"),
            Some("author2"),
            json!({}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_delete",
    )
    .await;

    let (status, deleted) = body(
        site.send_json(
            "DELETE",
            &format!("{uri}?force=true"),
            Some("author"),
            json!({}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{deleted}");
    assert_eq!(deleted["deleted"], json!(true));
    assert_eq!(deleted["previous"]["id"], json!(id));
    assert_eq!(site.stored(&file), None);
    assert_eq!(site.stored(&thumbnail), None);
    assert!(Post::query().find(site.db(), id).await.unwrap().is_none());
    assert_error(
        site.get(&uri, Some("author")).await,
        Status::NotFound,
        "rest_post_invalid_id",
    )
    .await;
    let response = site.get(&Site::path_of(&item["source_url"]), None).await;
    assert_eq!(response.status(), Status::NotFound);
}

#[rocket::async_test]
async fn uploads_refuse_dot_files_and_paths_outside_the_folder() {
    let site = site().await;
    let folder = site.uploads.path().join("2026/11");
    std::fs::create_dir_all(&folder).unwrap();
    for name in [".mc-tmp.1.2", ".htaccess", "notes.txt", "script.php"] {
        std::fs::write(folder.join(name), b"secret").unwrap();
    }
    // A file the uploads folder holds by an allowed type is served as text, never sniffed.
    let response = site
        .get("/wp-content/uploads/2026/11/notes.txt", None)
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.content_type(), Some(ContentType::Plain));
    for uri in [
        "/wp-content/uploads/2026/11/.mc-tmp.1.2",
        "/wp-content/uploads/2026/11/.htaccess",
        "/wp-content/uploads/2026/11/script.php",
        "/wp-content/uploads/2026/11/missing.jpg",
        "/wp-content/uploads/../Cargo.toml",
        "/wp-content/uploads/2026/%2e%2e/11/notes.txt",
        "/wp-content/uploads/2026%2F11/notes.txt",
        "/wp-content/uploads/2026/11/",
        "/wp-content/uploads/",
    ] {
        let response = site.get(uri, None).await;
        assert_eq!(response.status(), Status::NotFound, "{uri}");
    }
}

#[rocket::async_test]
async fn media_is_registered_for_discovery() {
    let site = site().await;
    let (_, index) = body(site.get("/wp-json/wp/v2", None).await).await;
    let routes = index["routes"].as_object().unwrap();
    assert_eq!(routes["/wp/v2/media"]["methods"], json!(["GET", "POST"]));
    assert_eq!(
        routes["/wp/v2/media/(?P<id>[\\d]+)"]["methods"],
        json!(["GET", "POST", "PUT", "PATCH", "DELETE"])
    );
    assert_eq!(routes["/wp/v2/users/me"]["methods"], json!(["GET"]));
}

#[rocket::async_test]
async fn a_large_file_streams_whole() {
    let site = site().await;
    // Far larger than any read buffer, under the upload limit.
    let text: Vec<u8> = (0..300_000u32)
        .map(|i| b"abcdefghijklmnopqrstuvwxyz0123456789\n"[(i % 37) as usize])
        .collect();
    let (status, item) = body(
        site.upload_raw(Some("author"), "long.txt", "text/plain", text.clone(), "")
            .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{item}");
    let response = site.get(&Site::path_of(&item["source_url"]), None).await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.content_type(), Some(ContentType::Plain));
    // A sized body: Rocket sends its size as `Content-Length` (the local client keeps it on
    // the body, not in the headers).
    assert_eq!(response.body().preset_size(), Some(text.len()));
    assert_eq!(
        response.headers().get_one("X-Content-Type-Options"),
        Some("nosniff")
    );
    assert_eq!(response.into_bytes().await, Some(text));
}

#[rocket::async_test]
async fn an_attachment_of_a_draft_is_hidden_from_visitors() {
    let site = site().await;
    let draft = Post::create(
        site.db(),
        NewPost {
            title: "Not yet".to_owned(),
            ..NewPost::new(site.id("author"), "post")
        },
    )
    .await
    .unwrap();
    let hidden = site
        .upload_raw(
            Some("author"),
            "hidden.jpg",
            "image/jpeg",
            jpeg(40, 30),
            &format!("?post={}", draft.id),
        )
        .await;
    let (status, hidden) = body(hidden).await;
    assert_eq!(status, Status::Created, "{hidden}");
    let shown = uploaded(&site, "author", "shown.jpg").await;

    assert_error(
        site.get(&format!("/wp-json/wp/v2/media/{}", hidden["id"]), None)
            .await,
        Status::Unauthorized,
        "rest_forbidden",
    )
    .await;
    assert_error(
        site.get(
            &format!("/wp-json/wp/v2/media/{}", hidden["id"]),
            Some("author2"),
        )
        .await,
        Status::Forbidden,
        "rest_forbidden",
    )
    .await;
    let (_, list) = body(site.get("/wp-json/wp/v2/media", None).await).await;
    let ids: Vec<&Value> = list.as_array().unwrap().iter().map(|i| &i["id"]).collect();
    assert_eq!(ids, [&shown["id"]]);
    // Its author still reads it.
    let response = site
        .get(
            &format!("/wp-json/wp/v2/media/{}", hidden["id"]),
            Some("author"),
        )
        .await;
    assert_eq!(response.status(), Status::Ok);
}

#[rocket::async_test]
async fn a_file_part_needs_a_file_name() {
    let site = site().await;
    // A `file` field with no file name is a text field, as PHP puts it in `$_POST`.
    let boundary = "mc-test-boundary-no-file";
    let body_text = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nnot a file\r\n--{boundary}--\r\n"
    );
    let response = site
        .client
        .post("/wp-json/wp/v2/media")
        .header(Header::new(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .header(site.auth("author"))
        .body(body_text)
        .dispatch()
        .await;
    assert_error(response, Status::BadRequest, "rest_upload_no_data").await;
}

#[cfg(unix)]
#[rocket::async_test]
async fn uploads_never_follow_a_link_out_of_the_folder() {
    let site = site().await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    let folder = site.uploads.path().join("2026/11");
    std::fs::create_dir_all(&folder).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), folder.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path(), site.uploads.path().join("2026/out")).unwrap();
    // A FIFO is no file, and reading one never blocks the request.
    let made = std::process::Command::new("mkfifo")
        .arg(folder.join("pipe.txt"))
        .status()
        .unwrap();
    assert!(made.success());
    for uri in [
        "/wp-content/uploads/2026/11/link.txt",
        "/wp-content/uploads/2026/out/secret.txt",
        "/wp-content/uploads/2026/11/pipe.txt",
    ] {
        let response =
            rocket::tokio::time::timeout(std::time::Duration::from_secs(10), site.get(uri, None))
                .await
                .unwrap_or_else(|_| panic!("{uri} answers without hanging"));
        assert_eq!(response.status(), Status::NotFound, "{uri}");
    }
}
