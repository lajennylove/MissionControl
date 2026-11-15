//! The types, taxonomies, statuses and settings routes (Task 10.8): `/mc-json/mc/v2/types`,
//! `/taxonomies`, `/statuses` and `/settings`, as WordPress's controllers answer them.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.
//! Users sign in with application passwords; their roles come from an in-memory provider.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mission_control::{AppOptions, rocket_with_options};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{get_option, set_option};
use mission_control_core::migrations::migrator;
use mission_control_core::rest::posts::{PostType, PostTypes, Supports};
use mission_control_core::rest::settings::{Setting, SettingType, Settings};
use mission_control_core::rest::terms::{Taxonomies, TaxonomySpec};
use mission_control_core::users::app_password::{self, PlainPassword};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, InMemoryUserRoles, RoleAssignment, UserRoles,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// A post type's keys in `context=view`, as WordPress 6.7's `wp/v2/types` gives them.
const TYPE_VIEW_KEYS: &[&str] = &[
    "description",
    "hierarchical",
    "has_archive",
    "name",
    "slug",
    "icon",
    "taxonomies",
    "rest_base",
    "rest_namespace",
    "template",
    "template_lock",
    "_links",
];

/// A post type's keys in `context=edit`.
const TYPE_EDIT_KEYS: &[&str] = &[
    "capabilities",
    "description",
    "hierarchical",
    "has_archive",
    "visibility",
    "viewable",
    "labels",
    "name",
    "slug",
    "icon",
    "supports",
    "taxonomies",
    "rest_base",
    "rest_namespace",
    "template",
    "template_lock",
    "_links",
];

/// A post type's keys in `context=embed`.
const TYPE_EMBED_KEYS: &[&str] = &[
    "name",
    "slug",
    "icon",
    "rest_base",
    "rest_namespace",
    "_links",
];

/// A taxonomy's keys in `context=view`, as WordPress 6.7's `wp/v2/taxonomies` gives them.
const TAXONOMY_VIEW_KEYS: &[&str] = &[
    "name",
    "slug",
    "description",
    "types",
    "hierarchical",
    "rest_base",
    "rest_namespace",
    "_links",
];

/// A taxonomy's keys in `context=edit`.
const TAXONOMY_EDIT_KEYS: &[&str] = &[
    "name",
    "slug",
    "capabilities",
    "description",
    "labels",
    "types",
    "show_cloud",
    "hierarchical",
    "rest_base",
    "rest_namespace",
    "visibility",
    "_links",
];

/// A status's keys in `context=view`, as WordPress 6.7's `wp/v2/statuses` gives them.
const STATUS_VIEW_KEYS: &[&str] = &[
    "name",
    "public",
    "queryable",
    "slug",
    "date_floating",
    "_links",
];

/// A status's keys in `context=edit`.
const STATUS_EDIT_KEYS: &[&str] = &[
    "name",
    "private",
    "protected",
    "public",
    "queryable",
    "show_in_list",
    "slug",
    "date_floating",
    "_links",
];

/// The settings, as WordPress 6.7's `wp/v2/settings` gives them on a single site.
const SETTINGS_KEYS: &[&str] = &[
    "title",
    "description",
    "url",
    "email",
    "timezone",
    "date_format",
    "time_format",
    "start_of_week",
    "language",
    "use_smilies",
    "default_category",
    "default_post_format",
    "posts_per_page",
    "show_on_front",
    "page_on_front",
    "page_for_posts",
    "default_ping_status",
    "default_comment_status",
    "site_logo",
    "site_icon",
];

/// The site address the tests set.
const HOME: &str = "http://example.test";

/// A signed-in test user.
struct User {
    email: String,
    password: PlainPassword,
}

/// One test site: its database, its users by role, and a client.
struct Site {
    test: TestDatabase,
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

    async fn get(&self, uri: &str, as_user: Option<&str>) -> LocalResponse<'_> {
        let mut request = self.client.get(uri.to_owned());
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        request.dispatch().await
    }

    async fn send(&self, method: &str, uri: &str, as_user: &str, body: Value) -> LocalResponse<'_> {
        let request = match method {
            "POST" => self.client.post(uri.to_owned()),
            "PUT" => self.client.put(uri.to_owned()),
            "PATCH" => self.client.patch(uri.to_owned()),
            other => panic!("unexpected method {other}"),
        };
        request
            .header(self.auth(as_user))
            .header(ContentType::JSON)
            .body(body.to_string())
            .dispatch()
            .await
    }

    async fn head(&self, uri: &str, as_user: Option<&str>) -> Status {
        let mut request = self.client.head(uri.to_owned());
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        request.dispatch().await.status()
    }

    async fn option(&self, name: &str) -> Option<Value> {
        get_option(self.db(), name).await.expect("reads the option")
    }
}

fn config() -> Config {
    Config {
        database_url: "unused: the database is passed in".to_owned(),
        secret_key: "unused".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../themes")
            .to_path_buf(),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
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

/// A site with an administrator, an editor, an author and a contributor, the address [`HOME`]
/// and a few options set.
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
        ("siteurl", json!(HOME)),
        ("blogname", json!("Test Site")),
        ("blogdescription", json!("Just another test site")),
        ("admin_email", json!("admin@example.test")),
        ("permalink_structure", json!("/%postname%/")),
        ("gmt_offset", json!("0")),
        ("start_of_week", json!("1")),
        ("use_smilies", json!("1")),
        ("posts_per_page", json!(10)),
        ("default_ping_status", json!("open")),
    ] {
        set_option(db, name, &value).await.expect("sets the option");
    }
    let roles = InMemoryUserRoles::new();
    let mut users = BTreeMap::new();
    for (name, role) in [
        ("admin", ADMINISTRATOR),
        ("editor", EDITOR),
        ("author", AUTHOR),
        ("contributor", CONTRIBUTOR),
    ] {
        let (id, email) = new_user(db, name).await;
        let (password, _) = app_password::create(db, id, "Tests", None)
            .await
            .expect("creates the application password");
        roles.assign(id, RoleAssignment::new([role], BTreeMap::new()));
        users.insert(name, User { email, password });
    }
    let user_roles: Arc<dyn UserRoles> = Arc::new(roles);
    let options = AppOptions {
        database: Some(test.shared()),
        user_roles: Some(user_roles),
        ..AppOptions::default()
    };
    let client = Client::tracked(rocket_with_options(config(), options))
        .await
        .expect("the app ignites");
    Site {
        test,
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

/// Asserts that `response` is WordPress's error `code` with `status`, and returns the body.
async fn assert_error(response: LocalResponse<'_>, status: Status, code: &str) -> Value {
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some("application/json; charset=UTF-8")
    );
    let (got, json) = body(response).await;
    assert_eq!((got, json["code"].as_str()), (status, Some(code)), "{json}");
    assert_eq!(json["data"]["status"], json!(status.code));
    json
}

/// A successful response's body.
async fn ok(response: LocalResponse<'_>) -> Value {
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok, "{json}");
    json
}

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("an object: {value}"))
        .keys()
        .cloned()
        .collect()
}

fn set(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[rocket::async_test]
async fn types_have_wordpresss_keys_in_each_context() {
    let site = site().await;
    let types = ok(site.get("/mc-json/mc/v2/types", None).await).await;
    assert_eq!(keys(&types), set(&["post", "page", "attachment"]));
    for (name, item) in types.as_object().unwrap() {
        assert_eq!(keys(item), set(TYPE_VIEW_KEYS), "{name}");
    }
    let post = &types["post"];
    assert_eq!(post["name"], "Posts");
    assert_eq!(post["slug"], "post");
    assert_eq!(post["taxonomies"], json!(["category", "post_tag"]));
    assert_eq!(post["rest_base"], "posts");
    assert_eq!(post["rest_namespace"], "mc/v2");
    assert_eq!(post["hierarchical"], false);
    assert_eq!(post["template"], json!([]));
    assert_eq!(post["template_lock"], false);
    assert_eq!(types["page"]["hierarchical"], true);
    assert_eq!(types["page"]["taxonomies"], json!([]));
    assert_eq!(types["attachment"]["rest_base"], "media");
    assert_eq!(
        post["_links"]["mc:items"][0]["href"],
        format!("{HOME}/mc-json/mc/v2/posts")
    );
    assert_eq!(
        post["_links"]["collection"][0]["href"],
        format!("{HOME}/mc-json/mc/v2/types")
    );

    let page = ok(site
        .get("/mc-json/mc/v2/types/page?context=edit", Some("editor"))
        .await)
    .await;
    assert_eq!(keys(&page), set(TYPE_EDIT_KEYS));
    assert_eq!(page["capabilities"]["edit_post"], "edit_page");
    assert_eq!(page["capabilities"]["edit_posts"], "edit_pages");
    assert_eq!(page["labels"]["singular_name"], "Page");
    assert_eq!(page["supports"]["page-attributes"], true);
    assert_eq!(page["viewable"], true);
    let media = ok(site
        .get(
            "/mc-json/mc/v2/types/attachment?context=edit",
            Some("admin"),
        )
        .await)
    .await;
    assert_eq!(media["capabilities"]["create_posts"], "upload_files");
    assert_eq!(media["visibility"]["show_in_nav_menus"], false);

    let embed = ok(site
        .get("/mc-json/mc/v2/types/post?context=embed", None)
        .await)
    .await;
    assert_eq!(keys(&embed), set(TYPE_EMBED_KEYS));
    let fields = ok(site
        .get("/mc-json/mc/v2/types/post?_fields=slug,name", None)
        .await)
    .await;
    assert_eq!(keys(&fields), set(&["slug", "name"]));
}

#[rocket::async_test]
async fn the_edit_context_of_types_needs_edit_posts() {
    let site = site().await;
    assert_error(
        site.get("/mc-json/mc/v2/types?context=edit", None).await,
        Status::Unauthorized,
        "rest_cannot_view",
    )
    .await;
    // A contributor may edit posts but not pages.
    let types = ok(site
        .get("/mc-json/mc/v2/types?context=edit", Some("contributor"))
        .await)
    .await;
    assert_eq!(keys(&types), set(&["post", "attachment"]));
    for item in types.as_object().unwrap().values() {
        assert_eq!(keys(item), set(TYPE_EDIT_KEYS));
    }
    assert_error(
        site.get(
            "/mc-json/mc/v2/types/page?context=edit",
            Some("contributor"),
        )
        .await,
        Status::Forbidden,
        "rest_forbidden_context",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/types/nope", None).await,
        Status::NotFound,
        "rest_type_invalid",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/types?context=raw", None).await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
}

#[rocket::async_test]
async fn registered_types_are_listed_only_when_shown_in_rest() {
    let site = site().await;
    let post_types = site
        .client
        .rocket()
        .state::<Arc<PostTypes>>()
        .expect("the app manages the post types");
    let book = PostType {
        name: "book".into(),
        rest_base: "books".into(),
        hierarchical: false,
        show_in_rest: false,
        supports: Supports::ALL,
        taxonomies: Vec::new(),
        caps: mission_control_core::users::PostTypeCaps::for_post_type("book"),
    };
    post_types.register(book.clone());
    let types = ok(site.get("/mc-json/mc/v2/types", None).await).await;
    assert!(types.get("book").is_none());
    assert_error(
        site.get("/mc-json/mc/v2/types/book", None).await,
        Status::Unauthorized,
        "rest_cannot_read_type",
    )
    .await;
    post_types.register(PostType {
        show_in_rest: true,
        ..book
    });
    let types = ok(site.get("/mc-json/mc/v2/types", None).await).await;
    assert_eq!(keys(&types["book"]), set(TYPE_VIEW_KEYS));
    assert_eq!(types["book"]["name"], "book");
    assert_eq!(types["book"]["rest_base"], "books");
}

#[rocket::async_test]
async fn taxonomies_have_wordpresss_keys_in_each_context() {
    let site = site().await;
    let taxonomies = ok(site.get("/mc-json/mc/v2/taxonomies", None).await).await;
    assert_eq!(keys(&taxonomies), set(&["category", "post_tag"]));
    for item in taxonomies.as_object().unwrap().values() {
        assert_eq!(keys(item), set(TAXONOMY_VIEW_KEYS));
    }
    let category = &taxonomies["category"];
    assert_eq!(category["name"], "Categories");
    assert_eq!(category["types"], json!(["post"]));
    assert_eq!(category["hierarchical"], true);
    assert_eq!(category["rest_base"], "categories");
    assert_eq!(
        category["_links"]["mc:items"][0]["href"],
        format!("{HOME}/mc-json/mc/v2/categories")
    );
    assert_eq!(taxonomies["post_tag"]["hierarchical"], false);

    let tag = ok(site
        .get(
            "/mc-json/mc/v2/taxonomies/post_tag?context=edit",
            Some("author"),
        )
        .await)
    .await;
    assert_eq!(keys(&tag), set(TAXONOMY_EDIT_KEYS));
    assert_eq!(
        tag["capabilities"],
        json!({
            "manage_terms": "manage_post_tags",
            "edit_terms": "edit_post_tags",
            "delete_terms": "delete_post_tags",
            "assign_terms": "assign_post_tags",
        })
    );
    assert_eq!(tag["labels"]["popular_items"], "Popular Tags");
    assert_eq!(tag["show_cloud"], true);
    let edit = ok(site
        .get(
            "/mc-json/mc/v2/taxonomies?context=edit",
            Some("contributor"),
        )
        .await)
    .await;
    assert_eq!(keys(&edit), set(&["category", "post_tag"]));

    let embed = ok(site
        .get("/mc-json/mc/v2/taxonomies/category?context=embed", None)
        .await)
    .await;
    assert_eq!(
        keys(&embed),
        set(&["name", "slug", "rest_base", "rest_namespace", "_links"])
    );
}

#[rocket::async_test]
async fn taxonomies_filter_by_type_and_check_the_edit_context() {
    let site = site().await;
    let for_pages = ok(site.get("/mc-json/mc/v2/taxonomies?type=page", None).await).await;
    assert_eq!(for_pages, json!({}));
    let for_posts = ok(site.get("/mc-json/mc/v2/taxonomies?type=post", None).await).await;
    assert_eq!(keys(&for_posts), set(&["category", "post_tag"]));
    assert_error(
        site.get("/mc-json/mc/v2/taxonomies?context=edit", None)
            .await,
        Status::Unauthorized,
        "rest_cannot_view",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/taxonomies/category?context=edit", None)
            .await,
        Status::Unauthorized,
        "rest_forbidden_context",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/taxonomies/genre", None).await,
        Status::NotFound,
        "rest_taxonomy_invalid",
    )
    .await;
}

#[rocket::async_test]
async fn statuses_show_only_what_the_user_may_see() {
    let site = site().await;
    let visitor = ok(site.get("/mc-json/mc/v2/statuses", None).await).await;
    assert_eq!(keys(&visitor), set(&["publish"]));
    assert_eq!(keys(&visitor["publish"]), set(STATUS_VIEW_KEYS));
    assert_eq!(visitor["publish"]["name"], "Published");
    assert_eq!(visitor["publish"]["public"], true);
    assert_eq!(visitor["publish"]["queryable"], true);
    assert_eq!(
        visitor["publish"]["_links"]["archives"][0]["href"],
        format!("{HOME}/mc-json/mc/v2/posts")
    );

    let editor = ok(site
        .get("/mc-json/mc/v2/statuses?context=edit", Some("contributor"))
        .await)
    .await;
    assert_eq!(
        keys(&editor),
        set(&["publish", "future", "draft", "pending", "private", "trash"])
    );
    for item in editor.as_object().unwrap().values() {
        assert_eq!(keys(item), set(STATUS_EDIT_KEYS));
    }
    assert_eq!(editor["draft"]["date_floating"], true);
    assert_eq!(editor["private"]["private"], true);
    assert_eq!(editor["future"]["protected"], true);
    assert_eq!(editor["trash"]["show_in_list"], false);
    assert_eq!(
        editor["draft"]["_links"]["archives"][0]["href"],
        format!("{HOME}/mc-json/mc/v2/posts?status=draft")
    );

    let draft = ok(site
        .get("/mc-json/mc/v2/statuses/draft", Some("author"))
        .await)
    .await;
    assert_eq!(keys(&draft), set(STATUS_VIEW_KEYS));
    assert_error(
        site.get("/mc-json/mc/v2/statuses/draft", None).await,
        Status::Unauthorized,
        "rest_cannot_read_status",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/statuses/auto-draft", Some("admin"))
            .await,
        Status::Forbidden,
        "rest_cannot_read_status",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/statuses/nope", None).await,
        Status::NotFound,
        "rest_status_invalid",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/statuses?context=edit", None).await,
        Status::Unauthorized,
        "rest_cannot_view",
    )
    .await;
}

#[rocket::async_test]
async fn settings_are_for_administrators_only() {
    let site = site().await;
    assert_error(
        site.get("/mc-json/mc/v2/settings", None).await,
        Status::Unauthorized,
        "rest_forbidden",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/settings", Some("editor")).await,
        Status::Forbidden,
        "rest_forbidden",
    )
    .await;
    assert_error(
        site.send(
            "POST",
            "/mc-json/mc/v2/settings",
            "editor",
            json!({"title": "Taken"}),
        )
        .await,
        Status::Forbidden,
        "rest_forbidden",
    )
    .await;
    assert_eq!(site.option("blogname").await, Some(json!("Test Site")));

    let settings = ok(site.get("/mc-json/mc/v2/settings", Some("admin")).await).await;
    assert_eq!(keys(&settings), set(SETTINGS_KEYS));
    assert_eq!(settings["title"], "Test Site");
    assert_eq!(settings["description"], "Just another test site");
    assert_eq!(settings["url"], HOME);
    assert_eq!(settings["email"], "admin@example.test");
    // Stored as text, read by the schema's type.
    assert_eq!(settings["start_of_week"], 1);
    assert_eq!(settings["use_smilies"], true);
    assert_eq!(settings["posts_per_page"], 10);
    assert_eq!(settings["default_ping_status"], "open");
    // Not set: the default, or `null` without one.
    assert_eq!(settings["language"], "en_US");
    assert_eq!(settings["timezone"], Value::Null);
    assert_eq!(settings["site_logo"], Value::Null);
}

#[rocket::async_test]
async fn administrators_write_settings_sanitized_as_wordpress() {
    let site = site().await;
    let updated = ok(site
        .send(
            "POST",
            "/mc-json/mc/v2/settings",
            "admin",
            json!({
                "title": "Tom & Jerry <b>",
                "posts_per_page": "5",
                "use_smilies": false,
                "timezone": "America/Toronto",
                "date_format": "<i>Y-m-d</i>",
                "page_on_front": -3,
                "default_comment_status": "closed",
                "unknown": "ignored",
            }),
        )
        .await)
    .await;
    assert_eq!(keys(&updated), set(SETTINGS_KEYS));
    assert_eq!(updated["title"], "Tom &amp; Jerry &lt;b&gt;");
    assert_eq!(updated["posts_per_page"], 5);
    assert_eq!(updated["use_smilies"], false);
    assert_eq!(updated["timezone"], "America/Toronto");
    assert_eq!(updated["date_format"], "Y-m-d");
    assert_eq!(updated["page_on_front"], 3);
    assert_eq!(updated["default_comment_status"], "closed");
    assert_eq!(
        site.option("blogname").await,
        Some(json!("Tom &amp; Jerry &lt;b&gt;"))
    );
    assert_eq!(site.option("unknown").await, None);

    // Saving the escaped title back does not escape it again.
    let again = ok(site
        .send(
            "PUT",
            "/mc-json/mc/v2/settings",
            "admin",
            json!({"title": updated["title"]}),
        )
        .await)
    .await;
    assert_eq!(again["title"], "Tom &amp; Jerry &lt;b&gt;");

    // Values `sanitize_option` refuses keep the stored value.
    let kept = ok(site
        .send(
            "PATCH",
            "/mc-json/mc/v2/settings",
            "admin",
            json!({"url": "ftp://files.example.test", "timezone": "Mars/Olympus", "language": "fr_FR"}),
        )
        .await)
    .await;
    assert_eq!(kept["url"], HOME);
    assert_eq!(kept["timezone"], "America/Toronto");
    assert_eq!(kept["language"], "en_US");

    // `null` deletes the option: the default reads again.
    let reset = ok(site
        .send(
            "POST",
            "/mc-json/mc/v2/settings",
            "admin",
            json!({"posts_per_page": null}),
        )
        .await)
    .await;
    assert_eq!(reset["posts_per_page"], 10);
    assert_eq!(site.option("posts_per_page").await, None);
}

#[rocket::async_test]
async fn invalid_settings_are_refused_before_anything_is_written() {
    let site = site().await;
    let error = assert_error(
        site.send(
            "POST",
            "/mc-json/mc/v2/settings",
            "admin",
            json!({
                "title": "Changed",
                "posts_per_page": "many",
                "email": "not-an-email",
                "default_ping_status": "maybe",
                "use_smilies": "sometimes",
                "description": 42,
            }),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert_eq!(
        keys(&error["data"]["params"]),
        set(&[
            "posts_per_page",
            "email",
            "default_ping_status",
            "use_smilies",
            "description"
        ])
    );
    assert_eq!(
        error["data"]["details"]["posts_per_page"]["code"],
        "rest_invalid_type"
    );
    assert_eq!(
        error["data"]["details"]["email"]["code"],
        "rest_invalid_email"
    );
    assert_eq!(
        error["data"]["details"]["default_ping_status"]["code"],
        "rest_not_in_enum"
    );
    assert_eq!(site.option("blogname").await, Some(json!("Test Site")));
}

#[rocket::async_test]
async fn a_taxonomy_not_shown_in_rest_is_forbidden() {
    let site = site().await;
    let taxonomies = site
        .client
        .rocket()
        .state::<Arc<Taxonomies>>()
        .expect("the app manages the taxonomies");
    taxonomies.register(TaxonomySpec {
        show_in_rest: false,
        ..TaxonomySpec::post_tag()
    });
    assert_error(
        site.get("/mc-json/mc/v2/taxonomies/post_tag", None).await,
        Status::Unauthorized,
        "rest_forbidden",
    )
    .await;
    let listed = ok(site.get("/mc-json/mc/v2/taxonomies", None).await).await;
    assert_eq!(keys(&listed), set(&["category"]));
}

#[rocket::async_test]
async fn settings_refuse_head_and_fields_for_others_and_filter_for_administrators() {
    let site = site().await;
    assert_eq!(
        site.head("/mc-json/mc/v2/settings", None).await,
        Status::Unauthorized
    );
    assert_eq!(
        site.head("/mc-json/mc/v2/settings", Some("editor")).await,
        Status::Forbidden
    );
    assert_eq!(
        site.head("/mc-json/mc/v2/settings", Some("admin")).await,
        Status::Ok
    );
    assert_error(
        site.get("/mc-json/mc/v2/settings?_fields=title", Some("editor"))
            .await,
        Status::Forbidden,
        "rest_forbidden",
    )
    .await;
    let fields = ok(site
        .get("/mc-json/mc/v2/settings?_fields=title,email", Some("admin"))
        .await)
    .await;
    assert_eq!(
        fields,
        json!({"title": "Test Site", "email": "admin@example.test"})
    );
    let updated = ok(site
        .send(
            "POST",
            "/mc-json/mc/v2/settings?_fields=posts_per_page",
            "admin",
            json!({"posts_per_page": 4}),
        )
        .await)
    .await;
    assert_eq!(updated, json!({"posts_per_page": 4}));
}

#[rocket::async_test]
async fn a_setting_is_in_the_rest_api_only_when_it_asks() {
    let site = site().await;
    let settings = site
        .client
        .rocket()
        .state::<Arc<Settings>>()
        .expect("the app manages the settings");
    settings.register(Setting::new("acme_secret", SettingType::String));
    settings.register(Setting::new("acme_greeting", SettingType::String).show_in_rest());
    let read = ok(site.get("/mc-json/mc/v2/settings", Some("admin")).await).await;
    assert!(read.get("acme_secret").is_none());
    assert_eq!(read["acme_greeting"], Value::Null);
    let written = ok(site
        .send(
            "POST",
            "/mc-json/mc/v2/settings",
            "admin",
            json!({"acme_secret": "leaked", "acme_greeting": "Hello"}),
        )
        .await)
    .await;
    assert!(written.get("acme_secret").is_none());
    assert_eq!(written["acme_greeting"], "Hello");
    assert_eq!(site.option("acme_secret").await, None);
    assert_eq!(site.option("acme_greeting").await, Some(json!("Hello")));
}

#[rocket::async_test]
async fn timezones_are_checked_against_the_iana_list() {
    let site = site().await;
    let set = |zone: &str| json!({"timezone": zone});
    let named = ok(site
        .send(
            "POST",
            "/mc-json/mc/v2/settings",
            "admin",
            set("US/Eastern"),
        )
        .await)
    .await;
    assert_eq!(named["timezone"], "US/Eastern");
    let kept = ok(site
        .send(
            "POST",
            "/mc-json/mc/v2/settings",
            "admin",
            set("Europe/Narnia"),
        )
        .await)
    .await;
    assert_eq!(kept["timezone"], "US/Eastern");
    // `UTC+N`, as WordPress's settings screen saves it: an offset, and no timezone.
    let offset = ok(site
        .send("POST", "/mc-json/mc/v2/settings", "admin", set("UTC+5.5"))
        .await)
    .await;
    assert_eq!(offset["timezone"], "");
    assert_eq!(site.option("gmt_offset").await, Some(json!("5.5")));
    ok(site
        .send("POST", "/mc-json/mc/v2/settings", "admin", set("UTC-3"))
        .await)
    .await;
    assert_eq!(site.option("gmt_offset").await, Some(json!("-3")));
    // An offset WordPress's menu does not offer is refused, and nothing changes.
    let refused = ok(site
        .send("POST", "/mc-json/mc/v2/settings", "admin", set("UTC+5.3"))
        .await)
    .await;
    assert_eq!(refused["timezone"], "");
    assert_eq!(site.option("gmt_offset").await, Some(json!("-3")));
    let utc = ok(site
        .send("POST", "/mc-json/mc/v2/settings", "admin", set("UTC"))
        .await)
    .await;
    assert_eq!(utc["timezone"], "UTC");
}

#[rocket::async_test]
async fn editable_settings_list_no_default_in_discovery() {
    let site = site().await;
    let index = ok(site.get("/mc-json/mc/v2", None).await).await;
    let endpoints = index["routes"]["/mc/v2/settings"]["endpoints"]
        .as_array()
        .unwrap();
    let edit = &endpoints[1]["args"];
    assert_eq!(edit["posts_per_page"]["type"], "integer");
    assert!(edit["posts_per_page"].get("default").is_none());
    assert_eq!(edit["email"]["format"], "email");
}
