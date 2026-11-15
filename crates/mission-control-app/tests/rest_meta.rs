//! Registered meta in the REST API (Task 10.6): the keys in `MetaKeys` show under `meta` on
//! posts, pages, media, terms and users, and creates and updates write them, as WordPress's
//! `WP_REST_Meta_Fields` does.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.
//! Users sign in with application passwords; their roles come from an in-memory provider.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, get_post_meta, get_post_meta_values,
    get_term_meta, set_option, set_post_meta, set_user_meta,
};
use mission_control_core::migrations::migrator;
use mission_control_core::rest::meta::{
    MAX_META_VALUE_BYTES, MAX_META_VALUES, MetaAction, MetaAuth, MetaKey, MetaKeys, MetaType,
};
use mission_control_core::users::app_password::{self, PlainPassword};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, EDITOR, InMemoryUserRoles, RoleAssignment, UserRoles,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The site address the tests set.
const HOME: &str = "http://example.test";

/// A signed-in test user.
struct User {
    id: i64,
    email: String,
    password: PlainPassword,
}

/// One test site: its database, its uploads folder, its users by role, and a client.
struct Site {
    test: TestDatabase,
    _uploads: TempDir,
    client: Client,
    users: BTreeMap<&'static str, User>,
}

impl Site {
    fn db(&self) -> &Database {
        self.test.db()
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

    async fn get(&self, uri: &str, as_user: Option<&str>) -> (Status, Value) {
        let mut request = self.client.get(uri.to_owned());
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        body(request.dispatch().await).await
    }

    async fn post(&self, uri: &str, as_user: &str, json: Value) -> (Status, Value) {
        let response = self
            .client
            .post(uri.to_owned())
            .header(self.auth(as_user))
            .header(ContentType::JSON)
            .body(json.to_string())
            .dispatch()
            .await;
        body(response).await
    }

    /// A post of `post_type` by `author`, in `status`.
    async fn post_of(&self, post_type: &str, author: &str, status: PostStatus) -> Post {
        Post::create(
            self.db(),
            NewPost {
                status,
                title: "Original".to_owned(),
                content: "Text".to_owned(),
                ..NewPost::new(self.users[author].id, post_type)
            },
        )
        .await
        .expect("creates the post")
    }

    async fn title_of(&self, id: i64) -> String {
        raw::fetch_scalar(
            self.db(),
            "SELECT title FROM posts WHERE id = ?",
            &[id.into()],
        )
        .await
        .expect("reads the title")
    }
}

fn config(uploads: &Path) -> Config {
    Config {
        database_url: "unused: the database is passed in".to_owned(),
        secret_key: "unused".to_owned(),
        debug: false,
        upload_max_bytes: 512 * 1024,
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

/// The keys the tests register, as a theme would.
fn register(keys: &MetaKeys) {
    let post = |key: &str, meta_type| MetaKey::post(key, meta_type).subtype("post");
    let registrations = [
        post("subtitle", MetaType::String)
            .single()
            .default(json!("None yet"))
            .show_in_rest(),
        post("rating", MetaType::Integer).single().show_in_rest(),
        post("featured", MetaType::Boolean).single().show_in_rest(),
        post("price", MetaType::Number).single().show_in_rest(),
        post("labels", MetaType::Array)
            .single()
            .rest_schema(json!({"items": {"type": "string"}})),
        post("dimensions", MetaType::Object)
            .single()
            .rest_schema(json!({
                "properties": {"width": {"type": "integer"}, "height": {"type": "integer"}},
            })),
        post("reviewers", MetaType::String).show_in_rest(),
        post("_secret", MetaType::String).single().show_in_rest(),
        post("_open", MetaType::String)
            .single()
            .auth(MetaAuth::Allow)
            .show_in_rest(),
        post("hidden", MetaType::String).single(),
        MetaKey::term("color", MetaType::String)
            .subtype("category")
            .single()
            .show_in_rest(),
        MetaKey::post("credit", MetaType::String)
            .subtype("attachment")
            .single()
            .show_in_rest(),
        MetaKey::user("twitter", MetaType::String)
            .single()
            .show_in_rest(),
        // Registered for every post type: posts, pages and attachments.
        MetaKey::post("everywhere", MetaType::String)
            .single()
            .show_in_rest(),
        post("_options", MetaType::String)
            .single()
            .auth(MetaAuth::Capability("manage_options".to_owned()))
            .show_in_rest(),
        // Set by anyone who may edit the post, never deleted.
        post("_append_only", MetaType::String)
            .single()
            .auth(MetaAuth::Callback(Arc::new(|request| {
                request.action == MetaAction::Edit
            })))
            .show_in_rest(),
    ];
    for key in registrations {
        keys.register(key).expect("registers the key");
    }
}

/// A site with an administrator, an editor and an author, the address [`HOME`], pretty
/// permalinks, a default category, and the keys of [`register`].
async fn site() -> Site {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    let uncategorized = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("Uncategorized"))
        .await
        .expect("creates the default category");
    for (name, value) in [
        ("home", json!(HOME)),
        ("siteurl", json!(HOME)),
        ("permalink_structure", json!("/%postname%/")),
        ("default_category", json!(uncategorized.term_id)),
    ] {
        set_option(db, name, &value).await.expect("sets the option");
    }
    let roles = InMemoryUserRoles::new();
    let mut users = BTreeMap::new();
    for (name, role) in [
        ("admin", ADMINISTRATOR),
        ("editor", EDITOR),
        ("author", AUTHOR),
        ("keeper", AUTHOR),
    ] {
        let (id, email) = new_user(db, name).await;
        let (password, _) = app_password::create(db, id, "Tests", None)
            .await
            .expect("creates the application password");
        // The keeper is an author who holds the post meta capabilities by name.
        let extra: BTreeMap<String, bool> = if name == "keeper" {
            [("edit_post_meta", true), ("delete_post_meta", true)]
                .into_iter()
                .map(|(cap, granted)| (cap.to_owned(), granted))
                .collect()
        } else {
            BTreeMap::new()
        };
        roles.assign(id, RoleAssignment::new([role], extra));
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
    register(
        client
            .rocket()
            .state::<Arc<MetaKeys>>()
            .expect("the app manages the meta keys"),
    );
    Site {
        test,
        _uploads: uploads,
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

/// Asserts that the answer is WordPress's error `code` with `status`, and returns the body.
fn assert_error((got, json): (Status, Value), status: Status, code: &str) -> Value {
    assert_eq!((got, json["code"].as_str()), (status, Some(code)), "{json}");
    assert_eq!(json["data"]["status"], json!(status.code));
    json
}

/// A small JPEG, for uploads.
fn jpeg() -> Vec<u8> {
    let image = RgbImage::from_fn(16, 16, |x, y| Rgb([(x * 16) as u8, (y * 16) as u8, 128]));
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 80)
        .encode_image(&image)
        .unwrap();
    out
}

#[rocket::async_test]
async fn registered_keys_show_with_their_defaults_and_nothing_else_shows() {
    let site = site().await;
    let post = site.post_of("post", "author", PostStatus::Publish).await;
    set_post_meta(site.db(), post.id, "unregistered", &json!("never shown"))
        .await
        .unwrap();
    set_post_meta(site.db(), post.id, "hidden", &json!("not in the REST API"))
        .await
        .unwrap();

    let (status, json) = site
        .get(&format!("/wp-json/wp/v2/posts/{}", post.id), None)
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(
        json["meta"],
        json!({
            "subtitle": "None yet",
            "rating": 0,
            "featured": false,
            "price": 0.0,
            "labels": [],
            "dimensions": {},
            "reviewers": [],
            "_secret": "",
            "_open": "",
            "everywhere": "",
            "_options": "",
            "_append_only": "",
        })
    );

    // The keys are registered for posts only: a page shows only the key of every post type.
    let page = site.post_of("page", "admin", PostStatus::Publish).await;
    let (_, json) = site
        .get(&format!("/wp-json/wp/v2/pages/{}", page.id), None)
        .await;
    assert_eq!(json["meta"], json!({"everywhere": ""}));

    // `_fields` reaches into `meta`; the embed context leaves it out.
    let (_, json) = site
        .get(
            &format!("/wp-json/wp/v2/posts/{}?_fields=meta.subtitle", post.id),
            None,
        )
        .await;
    assert_eq!(json, json!({"meta": {"subtitle": "None yet"}}));
    let (_, json) = site
        .get(
            &format!("/wp-json/wp/v2/posts/{}?context=embed", post.id),
            None,
        )
        .await;
    assert!(json.get("meta").is_none(), "{json}");
}

#[rocket::async_test]
async fn each_type_is_written_and_read_back_typed() {
    let site = site().await;
    let (status, json) = site
        .post(
            "/wp-json/wp/v2/posts",
            "admin",
            json!({
                "title": "Typed",
                "status": "publish",
                "meta": {
                    "subtitle": "Hello",
                    "rating": "7",
                    "featured": "true",
                    "price": 9.5,
                    "labels": "red, green",
                    "dimensions": {"width": 10, "height": "20"},
                    "reviewers": ["Ann", "Bob"],
                    "unregistered": "ignored",
                    "hidden": "ignored",
                },
            }),
        )
        .await;
    assert_eq!(status, Status::Created, "{json}");
    let expected = json!({
        "subtitle": "Hello",
        "rating": 7,
        "featured": true,
        "price": 9.5,
        "labels": ["red", "green"],
        "dimensions": {"width": 10, "height": 20},
        "reviewers": ["Ann", "Bob"],
        "_secret": "",
        "_open": "",
        "everywhere": "",
        "_options": "",
        "_append_only": "",
    });
    assert_eq!(json["meta"], expected);
    let id = json["id"].as_i64().unwrap();

    // Stored typed, and a key that is not single as one row per value, as WordPress stores it.
    let db = site.db();
    assert_eq!(
        get_post_meta(db, id, "rating").await.unwrap(),
        Some(json!(7))
    );
    assert_eq!(
        get_post_meta(db, id, "featured").await.unwrap(),
        Some(json!(true))
    );
    assert_eq!(
        get_post_meta_values(db, id, "reviewers").await.unwrap(),
        [json!("Ann"), json!("Bob")]
    );
    assert_eq!(get_post_meta(db, id, "unregistered").await.unwrap(), None);
    assert_eq!(get_post_meta(db, id, "hidden").await.unwrap(), None);

    let (_, json) = site.get(&format!("/wp-json/wp/v2/posts/{id}"), None).await;
    assert_eq!(json["meta"], expected);

    // An update writes only the keys it names.
    let (status, json) = site
        .post(
            &format!("/wp-json/wp/v2/posts/{id}"),
            "admin",
            json!({"meta": {"rating": 3, "reviewers": ["Cy"]}}),
        )
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"]["rating"], json!(3));
    assert_eq!(json["meta"]["reviewers"], json!(["Cy"]));
    assert_eq!(json["meta"]["subtitle"], json!("Hello"));
}

#[rocket::async_test]
async fn a_value_of_the_wrong_type_is_refused_and_nothing_is_written() {
    let site = site().await;
    let post = site.post_of("post", "admin", PostStatus::Draft).await;
    let uri = format!("/wp-json/wp/v2/posts/{}", post.id);

    let json = assert_error(
        site.post(
            &uri,
            "admin",
            json!({"title": "Changed", "meta": {"subtitle": "Kept?", "rating": "seven"}}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_type",
    );
    assert_eq!(
        json["message"],
        json!("meta.rating is not of type integer.")
    );
    assert_eq!(site.title_of(post.id).await, "Original");
    assert_eq!(
        get_post_meta(site.db(), post.id, "subtitle").await.unwrap(),
        None
    );

    for (meta, code) in [
        (json!({"featured": "maybe"}), "rest_invalid_type"),
        (json!({"labels": [1, {"a": 1}]}), "rest_invalid_type"),
        (
            json!({"dimensions": {"width": 1, "depth": 2}}),
            "rest_additional_properties_forbidden",
        ),
        (json!({"subtitle": 12}), "rest_invalid_type"),
    ] {
        assert_error(
            site.post(&uri, "admin", json!({"meta": meta})).await,
            Status::BadRequest,
            code,
        );
    }

    // `meta` itself must be an object.
    let json = assert_error(
        site.post(&uri, "admin", json!({"meta": "subtitle"})).await,
        Status::BadRequest,
        "rest_invalid_param",
    );
    assert_eq!(
        json["data"]["params"]["meta"],
        json!("meta is not of type object.")
    );
}

#[rocket::async_test]
async fn a_protected_key_is_refused_without_the_meta_capability() {
    let site = site().await;
    let post = site.post_of("post", "author", PostStatus::Draft).await;
    let uri = format!("/wp-json/wp/v2/posts/{}", post.id);

    let json = assert_error(
        site.post(
            &uri,
            "author",
            json!({"title": "Changed", "meta": {"_secret": "mine"}}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_update",
    );
    assert_eq!(
        json["message"],
        json!("Sorry, you are not allowed to edit the _secret custom field.")
    );
    assert_eq!(json["data"]["key"], json!("_secret"));
    // Refused before the post was written.
    assert_eq!(site.title_of(post.id).await, "Original");

    // As in WordPress, nobody holds `edit_post_meta` by default, not even an administrator, and
    // deleting is refused as `rest_cannot_delete`.
    assert_error(
        site.post(&uri, "admin", json!({"meta": {"_secret": "x"}}))
            .await,
        Status::Forbidden,
        "rest_cannot_update",
    );
    assert_error(
        site.post(&uri, "admin", json!({"meta": {"_secret": null}}))
            .await,
        Status::Forbidden,
        "rest_cannot_delete",
    );

    // A value equal to the stored one is not written, so it is not checked.
    set_post_meta(site.db(), post.id, "_secret", &json!("same"))
        .await
        .unwrap();
    let (status, json) = site
        .post(&uri, "author", json!({"meta": {"_secret": "same"}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");

    // A protected key registered with an `auth_callback` that allows it.
    let (status, json) = site
        .post(&uri, "author", json!({"meta": {"_open": "allowed"}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"]["_open"], json!("allowed"));

    // Someone else's post: the author may not edit it at all.
    let other = site.post_of("post", "editor", PostStatus::Draft).await;
    assert_error(
        site.post(
            &format!("/wp-json/wp/v2/posts/{}", other.id),
            "author",
            json!({"meta": {"subtitle": "x"}}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_edit",
    );
}

#[rocket::async_test]
async fn null_deletes_a_key() {
    let site = site().await;
    let post = site.post_of("post", "admin", PostStatus::Publish).await;
    let db = site.db();
    set_post_meta(db, post.id, "subtitle", &json!("Set"))
        .await
        .unwrap();
    set_post_meta(db, post.id, "rating", &json!(4))
        .await
        .unwrap();
    let uri = format!("/wp-json/wp/v2/posts/{}", post.id);
    let (status, json) = site
        .post(&uri, "admin", json!({"meta": {"reviewers": ["A", "B"]}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");

    let (status, json) = site
        .post(
            &uri,
            "admin",
            json!({"meta": {"subtitle": null, "rating": null, "reviewers": []}}),
        )
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"]["subtitle"], json!("None yet"));
    assert_eq!(json["meta"]["rating"], json!(0));
    assert_eq!(json["meta"]["reviewers"], json!([]));
    assert_eq!(get_post_meta(db, post.id, "subtitle").await.unwrap(), None);
    assert_eq!(get_post_meta(db, post.id, "rating").await.unwrap(), None);
    assert!(
        get_post_meta_values(db, post.id, "reviewers")
            .await
            .unwrap()
            .is_empty()
    );
}

#[rocket::async_test]
async fn term_meta_is_shown_and_written_through_categories() {
    let site = site().await;
    let (status, json) = site
        .post(
            "/wp-json/wp/v2/categories",
            "editor",
            json!({"name": "News", "meta": {"color": "red", "unknown": "x"}}),
        )
        .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["meta"], json!({"color": "red"}));
    let id = json["id"].as_i64().unwrap();
    assert_eq!(
        get_term_meta(site.db(), id, "color").await.unwrap(),
        Some(json!("red"))
    );

    // An update with nothing but meta still writes it.
    let (status, json) = site
        .post(
            &format!("/wp-json/wp/v2/categories/{id}"),
            "editor",
            json!({"meta": {"color": "blue"}}),
        )
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"], json!({"color": "blue"}));
    let (_, json) = site
        .get(&format!("/wp-json/wp/v2/categories/{id}"), None)
        .await;
    assert_eq!(json["meta"], json!({"color": "blue"}));

    assert_error(
        site.post(
            &format!("/wp-json/wp/v2/categories/{id}"),
            "editor",
            json!({"meta": {"color": ["not", "a", "string"]}}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_type",
    );

    // The key is registered for categories only: a tag shows none.
    let (status, json) = site
        .post("/wp-json/wp/v2/tags", "editor", json!({"name": "Rust"}))
        .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["meta"], json!({}));
}

#[rocket::async_test]
async fn media_meta_is_shown_and_written_through_media() {
    let site = site().await;
    // A raw upload takes its fields from the query string, `meta[credit]` among them.
    let response = site
        .client
        .post("/wp-json/wp/v2/media?meta%5Bcredit%5D=Jane")
        .header(site.auth("author"))
        .header(Header::new("Content-Type", "image/jpeg"))
        .header(Header::new(
            "Content-Disposition",
            "attachment; filename=\"photo.jpg\"",
        ))
        .body(jpeg())
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["meta"], json!({"credit": "Jane", "everywhere": ""}));
    let id = json["id"].as_i64().unwrap();

    let (status, json) = site
        .post(
            &format!("/wp-json/wp/v2/media/{id}"),
            "author",
            json!({"meta": {"credit": "Ann"}}),
        )
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"], json!({"credit": "Ann", "everywhere": ""}));
    let (_, json) = site.get(&format!("/wp-json/wp/v2/media/{id}"), None).await;
    assert_eq!(json["meta"], json!({"credit": "Ann", "everywhere": ""}));
    assert_eq!(
        get_post_meta(site.db(), id, "credit").await.unwrap(),
        Some(json!("Ann"))
    );
}

#[rocket::async_test]
async fn user_meta_is_shown() {
    let site = site().await;
    let admin = site.users["admin"].id;
    set_user_meta(site.db(), admin, "twitter", &json!("@admin"))
        .await
        .unwrap();
    let (status, json) = site
        .get(&format!("/wp-json/wp/v2/users/{admin}"), Some("admin"))
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"], json!({"twitter": "@admin"}));
    let editor = site.users["editor"].id;
    let (_, json) = site
        .get(&format!("/wp-json/wp/v2/users/{editor}"), Some("admin"))
        .await;
    assert_eq!(json["meta"], json!({"twitter": ""}));
}

/// Adds `count` rows `prefix0`, `prefix1`, ... under post meta `key`, as `add_post_meta` would.
async fn add_rows(db: &Database, post_id: i64, key: &str, prefix: &str, count: usize) {
    for start in (0..count).step_by(100) {
        let end = (start + 100).min(count);
        let marks = vec!["(?, ?, ?)"; end - start].join(", ");
        let mut binds = Vec::new();
        for i in start..end {
            binds.push(post_id.into());
            binds.push(key.into());
            binds.push(json!(format!("{prefix}{i}")).to_string().into());
        }
        raw::execute(
            db,
            &format!("INSERT INTO post_meta (post_id, meta_key, meta_value) VALUES {marks}"),
            &binds,
        )
        .await
        .expect("adds the rows");
    }
}

#[rocket::async_test]
async fn writes_over_the_bounds_are_refused_and_reads_are_bounded() {
    let site = site().await;
    let post = site.post_of("post", "admin", PostStatus::Publish).await;
    let uri = format!("/wp-json/wp/v2/posts/{}", post.id);

    let many: Vec<String> = (0..=MAX_META_VALUES).map(|i| format!("r{i}")).collect();
    let json = assert_error(
        site.post(
            &uri,
            "admin",
            json!({"title": "Changed", "meta": {"reviewers": many}}),
        )
        .await,
        Status::PayloadTooLarge,
        "rest_meta_too_large",
    );
    assert_eq!(json["data"]["key"], json!("reviewers"));
    assert_eq!(json["data"]["limit"], json!(MAX_META_VALUES));
    assert_eq!(site.title_of(post.id).await, "Original");

    let big = "x".repeat(MAX_META_VALUE_BYTES);
    let json = assert_error(
        site.post(&uri, "admin", json!({"meta": {"subtitle": big}}))
            .await,
        Status::PayloadTooLarge,
        "rest_meta_too_large",
    );
    assert_eq!(json["data"]["key"], json!("subtitle"));
    assert_eq!(json["data"]["limit"], json!(MAX_META_VALUE_BYTES));
    let json = assert_error(
        site.post(&uri, "admin", json!({"meta": {"reviewers": ["ok", big]}}))
            .await,
        Status::PayloadTooLarge,
        "rest_meta_too_large",
    );
    assert_eq!(json["data"]["key"], json!("reviewers"));
    assert_eq!(
        get_post_meta(site.db(), post.id, "subtitle").await.unwrap(),
        None
    );

    // The size bound is per value: a full list of 2 KB values (2 MB in all) is accepted.
    let wide: Vec<String> = (0..MAX_META_VALUES)
        .map(|i| format!("{i:04}{}", "w".repeat(2048)))
        .collect();
    let (status, json) = site
        .post(&uri, "admin", json!({"meta": {"reviewers": wide}}))
        .await;
    assert_eq!(status, Status::Ok, "{}", json["code"]);
    assert_eq!(
        get_post_meta_values(site.db(), post.id, "reviewers")
            .await
            .unwrap()
            .len(),
        MAX_META_VALUES
    );

    // The most a request may write is written.
    let most: Vec<String> = (0..MAX_META_VALUES).map(|i| format!("r{i}")).collect();
    let (status, json) = site
        .post(&uri, "admin", json!({"meta": {"reviewers": most}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(
        json["meta"]["reviewers"].as_array().unwrap().len(),
        MAX_META_VALUES
    );

    // Rows added past the bound by other code: a response shows the first ones only, and an
    // update with those same values still replaces the whole key.
    add_rows(site.db(), post.id, "reviewers", "extra", 5).await;
    let (_, json) = site.get(&uri, None).await;
    let shown = json["meta"]["reviewers"].as_array().unwrap();
    assert_eq!(shown.len(), MAX_META_VALUES);
    assert_eq!(
        (&shown[0], &shown[MAX_META_VALUES - 1]),
        (&json!("r0"), &json!("r999"))
    );
    let (status, _) = site
        .post(&uri, "admin", json!({"meta": {"reviewers": shown.clone()}}))
        .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(
        get_post_meta_values(site.db(), post.id, "reviewers")
            .await
            .unwrap()
            .len(),
        MAX_META_VALUES
    );
}

#[rocket::async_test]
async fn the_edit_check_comes_before_the_values_are_checked() {
    let site = site().await;
    let other = site.post_of("post", "editor", PostStatus::Draft).await;
    assert_error(
        site.post(
            &format!("/wp-json/wp/v2/posts/{}", other.id),
            "author",
            json!({"meta": {"rating": "not a number"}}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_edit",
    );
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    assert_error(
        site.post(
            &format!("/wp-json/wp/v2/categories/{}", news.term_id),
            "author",
            json!({"meta": {"color": ["not", "a", "string"]}}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_update",
    );
}

#[rocket::async_test]
async fn auth_policies_decide_protected_keys() {
    let site = site().await;
    let post = site.post_of("post", "keeper", PostStatus::Draft).await;
    let uri = format!("/wp-json/wp/v2/posts/{}", post.id);

    // The meta capability held by name allows any key, and deleting it.
    let (status, json) = site
        .post(&uri, "keeper", json!({"meta": {"_secret": "kept"}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"]["_secret"], json!("kept"));
    let (status, json) = site
        .post(&uri, "keeper", json!({"meta": {"_secret": null}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(
        get_post_meta(site.db(), post.id, "_secret").await.unwrap(),
        None
    );

    // `MetaAuth::Capability`: `manage_options`, which an editor lacks and an administrator holds.
    let draft = site.post_of("post", "editor", PostStatus::Draft).await;
    let draft_uri = format!("/wp-json/wp/v2/posts/{}", draft.id);
    assert_error(
        site.post(&draft_uri, "editor", json!({"meta": {"_options": "x"}}))
            .await,
        Status::Forbidden,
        "rest_cannot_update",
    );
    let (status, json) = site
        .post(&draft_uri, "admin", json!({"meta": {"_options": "x"}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"]["_options"], json!("x"));

    // `MetaAuth::Callback`: setting is allowed, deleting is not.
    let (status, json) = site
        .post(
            &draft_uri,
            "editor",
            json!({"meta": {"_append_only": "first"}}),
        )
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"]["_append_only"], json!("first"));
    assert_error(
        site.post(
            &draft_uri,
            "editor",
            json!({"meta": {"_append_only": null}}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_delete",
    );
}

#[rocket::async_test]
async fn deleting_an_invalid_stored_value_is_refused() {
    let site = site().await;
    let post = site.post_of("post", "admin", PostStatus::Draft).await;
    set_post_meta(site.db(), post.id, "rating", &json!("not a number"))
        .await
        .unwrap();
    let uri = format!("/wp-json/wp/v2/posts/{}", post.id);
    // The stored value reads as `null`, as WordPress's `prepare_value` refuses it.
    let (_, json) = site
        .get(&format!("{uri}?context=edit"), Some("admin"))
        .await;
    assert_eq!(json["meta"]["rating"], Value::Null);
    let json = assert_error(
        site.post(&uri, "admin", json!({"meta": {"rating": null}}))
            .await,
        Status::InternalServerError,
        "rest_invalid_stored_value",
    );
    assert_eq!(
        json["message"],
        json!("The rating property has an invalid stored value, and cannot be updated to null.")
    );
    // A new valid value replaces it.
    let (status, json) = site
        .post(&uri, "admin", json!({"meta": {"rating": 5}}))
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["meta"]["rating"], json!(5));
}

#[rocket::async_test]
async fn a_key_registered_for_every_post_type_is_everywhere() {
    let site = site().await;
    for (base, post_type) in [("posts", "post"), ("pages", "page")] {
        let (status, json) = site
            .post(
                &format!("/wp-json/wp/v2/{base}"),
                "admin",
                json!({"title": "T", "meta": {"everywhere": base}}),
            )
            .await;
        assert_eq!(status, Status::Created, "{post_type}: {json}");
        assert_eq!(json["meta"]["everywhere"], json!(base));
    }
    let response = site
        .client
        .post("/wp-json/wp/v2/media")
        .header(site.auth("admin"))
        .header(Header::new("Content-Type", "image/jpeg"))
        .header(Header::new(
            "Content-Disposition",
            "attachment; filename=\"photo.jpg\"",
        ))
        .body(jpeg())
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["meta"], json!({"credit": "", "everywhere": ""}));
}

#[rocket::async_test]
async fn meta_is_shown_on_a_password_protected_post() {
    let site = site().await;
    let post = Post::create(
        site.db(),
        NewPost {
            status: PostStatus::Publish,
            title: "Locked".to_owned(),
            content: "Secret text".to_owned(),
            password: Some("letmein".to_owned()),
            ..NewPost::new(site.users["admin"].id, "post")
        },
    )
    .await
    .unwrap();
    set_post_meta(site.db(), post.id, "subtitle", &json!("Visible"))
        .await
        .unwrap();
    let (status, json) = site
        .get(&format!("/wp-json/wp/v2/posts/{}", post.id), None)
        .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["content"]["rendered"], json!(""));
    assert_eq!(json["meta"]["subtitle"], json!("Visible"));
}

#[rocket::async_test]
async fn discovery_lists_the_registered_keys() {
    let site = site().await;
    let (status, json) = site.get("/wp-json/wp/v2", None).await;
    assert_eq!(status, Status::Ok, "{json}");
    let meta_of = |route: &str, endpoint: usize| {
        json["routes"][route]["endpoints"][endpoint]["args"]["meta"].clone()
    };

    let posts = meta_of("/wp/v2/posts", 1);
    assert_eq!(posts["type"], json!("object"));
    let properties = posts["properties"].as_object().expect("properties");
    assert_eq!(
        properties["subtitle"],
        json!({"type": "string", "description": "", "default": "None yet"})
    );
    assert_eq!(
        properties["reviewers"],
        json!({"type": "array", "items": {"type": "string", "description": "", "default": ""}})
    );
    assert!(properties.contains_key("everywhere"));
    assert!(!properties.contains_key("hidden") && !properties.contains_key("credit"));
    assert_eq!(
        meta_of("/wp/v2/posts/(?P<id>[\\d]+)", 1)["properties"],
        posts["properties"]
    );

    let pages = meta_of("/wp/v2/pages", 1);
    assert_eq!(keys_of(&pages["properties"]), ["everywhere".to_owned()]);
    assert_eq!(
        keys_of(&meta_of("/wp/v2/media", 1)["properties"]),
        ["credit".to_owned(), "everywhere".to_owned()]
    );
    assert_eq!(
        keys_of(&meta_of("/wp/v2/categories", 1)["properties"]),
        ["color".to_owned()]
    );
    assert_eq!(meta_of("/wp/v2/tags", 1)["properties"], json!([]));
}

fn keys_of(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("an object: {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}
