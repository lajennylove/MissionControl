//! The users controller (Task 10.4): `/mc-json/mc/v2/users`, `/users/{id}` and `/users/me`, as
//! WordPress's `WP_REST_Users_Controller` answers them.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.
//! The `users` table is shared on a server database, so lists are checked with `include` or
//! against the users each test makes. Users sign in with application passwords; their roles come
//! from an in-memory provider.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mission_control::{AppOptions, rocket_with_options};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{NewPost, Post, PostStatus, set_option, set_user_meta};
use mission_control_core::migrations::migrator;
use mission_control_core::users::app_password::{self, PlainPassword};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, InMemoryUserRoles, RoleAssignment, UserRoles,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use rocket::http::{Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The keys of a user in `context=view`, as WordPress 6.7 gives them.
const VIEW_KEYS: &[&str] = &[
    "id",
    "name",
    "url",
    "description",
    "link",
    "slug",
    "avatar_urls",
    "meta",
    "_links",
];

/// What `context=edit` adds to the view keys.
const EDIT_EXTRA_KEYS: &[&str] = &[
    "username",
    "first_name",
    "last_name",
    "email",
    "locale",
    "nickname",
    "registered_date",
    "roles",
    "capabilities",
    "extra_capabilities",
];

/// The site address the tests set.
const HOME: &str = "http://example.test";

/// A signed-in test user.
struct User {
    id: i64,
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

    /// A post by `author` with `status`.
    async fn post_by(&self, author: &str, status: PostStatus, title: &str) -> Post {
        Post::create(
            self.db(),
            NewPost {
                status,
                title: title.to_owned(),
                content: "Body.".to_owned(),
                published_at: (status == PostStatus::Publish)
                    .then(|| chrono::Utc::now().naive_utc()),
                ..NewPost::new(self.id(author), "post")
            },
        )
        .await
        .expect("creates the post")
    }

    /// `?include=` with every user of this test, so other rows of a shared `users` table are
    /// left out.
    fn include_all(&self) -> String {
        let ids: Vec<String> = self.users.values().map(|u| u.id.to_string()).collect();
        format!("include={}", ids.join(","))
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
async fn new_user(db: &Database, name: &str, local: &str) -> (i64, String) {
    let email = unique_email(local);
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash, created_at) VALUES (?, ?, ?, ?)",
        &[
            name.into(),
            email.clone().into(),
            "hash".into(),
            chrono::NaiveDate::from_ymd_opt(2026, 3, 4)
                .unwrap()
                .and_hms_opt(5, 6, 7)
                .unwrap()
                .into(),
        ],
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

/// A site with an administrator, an editor, two authors and a contributor, and the address
/// [`HOME`].
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
        ("permalink_structure", json!("/%postname%/")),
    ] {
        set_option(db, name, &value).await.expect("sets the option");
    }
    let roles = InMemoryUserRoles::new();
    let mut users = BTreeMap::new();
    for (key, name, role) in [
        ("admin", "Ada Admin", ADMINISTRATOR),
        ("editor", "Ed Editor", EDITOR),
        ("author", "Ann Author", AUTHOR),
        ("author2", "Al Author", AUTHOR),
        ("contributor", "Cy Contributor", CONTRIBUTOR),
    ] {
        let (id, email) = new_user(db, name, key).await;
        let (password, _) = app_password::create(db, id, "Tests", None)
            .await
            .expect("creates the application password");
        let mut own = BTreeMap::new();
        if key == "author2" {
            own.insert("moderate_comments".to_owned(), true);
        }
        roles.assign(id, RoleAssignment::new([role], own));
        users.insert(
            key,
            User {
                id,
                email,
                password,
            },
        );
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

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap_or_else(|| panic!("a list: {list}"))
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect()
}

/// The SHA-256 of `email`, trimmed and lowercased, in hex: Gravatar's hash as WordPress 6.8
/// makes it.
fn gravatar_hash(email: &str) -> String {
    Sha256::digest(email.trim().to_ascii_lowercase().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[rocket::async_test]
async fn me_is_the_signed_in_user_and_a_visitor_is_not_logged_in() {
    let site = site().await;
    assert_error(
        site.get("/mc-json/mc/v2/users/me", None).await,
        Status::Unauthorized,
        "rest_not_logged_in",
    )
    .await;

    let (status, me) = body(site.get("/mc-json/mc/v2/users/me", Some("editor")).await).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(me["id"], json!(site.id("editor")));
    assert_eq!(keys(&me), set(&[VIEW_KEYS]));
    assert_eq!(me["name"], "Ed Editor");

    // A user may see their own edit context without `list_users`.
    let (status, me) = body(
        site.get("/mc-json/mc/v2/users/me?context=edit", Some("author"))
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{me}");
    assert_eq!(keys(&me), set(&[VIEW_KEYS, EDIT_EXTRA_KEYS]));
    let author = &site.users["author"];
    assert_eq!(me["username"], json!(author.email));
    assert_eq!(me["email"], json!(author.email));
    assert_eq!(me["roles"], json!(["author"]));
    assert_eq!(me["capabilities"]["upload_files"], json!(true));
    assert_eq!(me["capabilities"]["author"], json!(true));
    assert_eq!(me["extra_capabilities"], json!({"author": true}));
    assert_eq!(me["registered_date"], "2026-03-04T05:06:07+00:00");
    assert_eq!(me["locale"], "en_US");
}

#[rocket::async_test]
async fn a_user_has_wordpresss_view_fields() {
    let site = site().await;
    site.post_by("author", PostStatus::Publish, "Hello").await;
    let id = site.id("author");
    set_user_meta(site.db(), id, "description", &json!("Writes things."))
        .await
        .unwrap();
    set_user_meta(site.db(), id, "first_name", &json!("Ann"))
        .await
        .unwrap();

    let (status, user) = body(site.get(&format!("/mc-json/mc/v2/users/{id}"), None).await).await;
    assert_eq!(status, Status::Ok, "{user}");
    assert_eq!(keys(&user), set(&[VIEW_KEYS]));
    assert_eq!(user["slug"], "ann-author");
    assert_eq!(user["link"], json!(format!("{HOME}/?author={id}")));
    assert_eq!(user["url"], "");
    assert_eq!(user["description"], "Writes things.");
    let hash = gravatar_hash(&site.users["author"].email);
    for size in ["24", "48", "96"] {
        assert_eq!(
            user["avatar_urls"][size],
            json!(format!(
                "https://secure.gravatar.com/avatar/{hash}?s={size}&d=mm&r=g"
            ))
        );
    }
    assert_eq!(
        user["_links"]["self"][0]["href"],
        json!(format!("{HOME}/mc-json/mc/v2/users/{id}"))
    );
    assert_eq!(
        user["_links"]["collection"][0]["href"],
        json!(format!("{HOME}/mc-json/mc/v2/users"))
    );

    // The edit context needs `list_users`: an administrator has it, an author does not.
    let (status, edit) = body(
        site.get(
            &format!("/mc-json/mc/v2/users/{id}?context=edit"),
            Some("admin"),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(keys(&edit), set(&[VIEW_KEYS, EDIT_EXTRA_KEYS]));
    assert_eq!(edit["first_name"], "Ann");
    assert_eq!(edit["last_name"], "");
    assert_error(
        site.get(
            &format!("/mc-json/mc/v2/users/{id}?context=edit"),
            Some("author2"),
        )
        .await,
        Status::Forbidden,
        "rest_user_cannot_view",
    )
    .await;

    // The embed context drops `meta`; `_fields` keeps what it names.
    let (_, embed) = body(
        site.get(&format!("/mc-json/mc/v2/users/{id}?context=embed"), None)
            .await,
    )
    .await;
    assert!(!keys(&embed).contains("meta"));
    let (_, few) = body(
        site.get(&format!("/mc-json/mc/v2/users/{id}?_fields=id,slug"), None)
            .await,
    )
    .await;
    assert_eq!(few, json!({"id": id, "slug": "ann-author"}));

    // A user's own capabilities are their extra capabilities.
    let other = site.id("author2");
    let (_, edit) = body(
        site.get(
            &format!("/mc-json/mc/v2/users/{other}?context=edit"),
            Some("admin"),
        )
        .await,
    )
    .await;
    assert_eq!(
        edit["extra_capabilities"],
        json!({"author": true, "moderate_comments": true})
    );
    assert_eq!(edit["capabilities"]["moderate_comments"], json!(true));

    assert_error(
        site.get("/mc-json/mc/v2/users/999999999", Some("admin"))
            .await,
        Status::NotFound,
        "rest_user_invalid_id",
    )
    .await;
}

#[rocket::async_test]
async fn visitors_see_only_users_with_published_posts() {
    let site = site().await;
    site.post_by("author", PostStatus::Publish, "Public").await;
    site.post_by("author2", PostStatus::Draft, "Not yet").await;

    let (status, list) = body(site.get("/mc-json/mc/v2/users", None).await).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(ids(&list), [site.id("author")]);

    assert_error(
        site.get(
            &format!("/mc-json/mc/v2/users/{}", site.id("author2")),
            None,
        )
        .await,
        Status::Unauthorized,
        "rest_user_cannot_view",
    )
    .await;
    // A signed-in user without `list_users` sees the same; an editor may edit users? No:
    // editors have neither `list_users` nor `edit_users`.
    assert_error(
        site.get(
            &format!("/mc-json/mc/v2/users/{}", site.id("author2")),
            Some("editor"),
        )
        .await,
        Status::Forbidden,
        "rest_user_cannot_view",
    )
    .await;
    let response = site
        .get(&format!("/mc-json/mc/v2/users/{}", site.id("author")), None)
        .await;
    assert_eq!(response.status(), Status::Ok);

    // What visitors may not ask for.
    assert_error(
        site.get("/mc-json/mc/v2/users?context=edit", None).await,
        Status::Unauthorized,
        "rest_forbidden_context",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/users?roles=editor", Some("author"))
            .await,
        Status::Forbidden,
        "rest_user_cannot_view",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/users?orderby=email", None).await,
        Status::Unauthorized,
        "rest_forbidden_orderby",
    )
    .await;
    assert_error(
        site.get("/mc-json/mc/v2/users?who=authors", None).await,
        Status::Unauthorized,
        "rest_forbidden_who",
    )
    .await;
    // A visitor's search never matches the email (the username here).
    let email = site.users["author"].email.clone();
    let local = email.split('@').next().unwrap().to_owned();
    let (_, found) = body(
        site.get(&format!("/mc-json/mc/v2/users?search={local}"), None)
            .await,
    )
    .await;
    assert_eq!(found, json!([]));
    let (_, found) = body(site.get("/mc-json/mc/v2/users?search=ann", None).await).await;
    assert_eq!(ids(&found), [site.id("author")]);
}

#[rocket::async_test]
async fn administrators_list_filter_and_page_every_user() {
    let site = site().await;
    let include = site.include_all();

    let response = site
        .get(
            &format!("/mc-json/mc/v2/users?{include}&per_page=2&context=edit"),
            Some("admin"),
        )
        .await;
    assert_eq!(response.headers().get_one("X-MC-Total"), Some("5"));
    assert_eq!(response.headers().get_one("X-MC-TotalPages"), Some("3"));
    let links: Vec<&str> = response.headers().get("Link").collect();
    assert_eq!(links.len(), 1, "{links:?}");
    assert!(links[0].ends_with("rel=\"next\""), "{links:?}");
    let (_, page) = body(response).await;
    // By name, ascending: Ada Admin, Al Author.
    assert_eq!(ids(&page), [site.id("admin"), site.id("author2")]);
    assert_eq!(keys(&page[0]), set(&[VIEW_KEYS, EDIT_EXTRA_KEYS]));

    // A page past the end is an empty list.
    let (status, empty) = body(
        site.get(
            &format!("/mc-json/mc/v2/users?{include}&page=9"),
            Some("admin"),
        )
        .await,
    )
    .await;
    assert_eq!((status, empty), (Status::Ok, json!([])));

    let (_, authors) = body(
        site.get(
            &format!("/mc-json/mc/v2/users?{include}&roles=author&orderby=id&order=desc"),
            Some("admin"),
        )
        .await,
    )
    .await;
    let mut expected = vec![site.id("author"), site.id("author2")];
    expected.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(ids(&authors), expected);

    let (_, moderators) = body(
        site.get(
            &format!("/mc-json/mc/v2/users?{include}&capabilities=moderate_comments"),
            Some("admin"),
        )
        .await,
    )
    .await;
    // Administrators and editors moderate comments by role; the second author by their own
    // capability.
    let found: BTreeSet<i64> = ids(&moderators).into_iter().collect();
    let expected: BTreeSet<i64> = [site.id("admin"), site.id("editor"), site.id("author2")]
        .into_iter()
        .collect();
    assert_eq!(found, expected);

    let (_, by_slug) = body(
        site.get(
            &format!(
                "/mc-json/mc/v2/users?{include}&slug=cy-contributor,ed-editor&orderby=include_slugs"
            ),
            Some("admin"),
        )
        .await,
    )
    .await;
    assert_eq!(ids(&by_slug), [site.id("contributor"), site.id("editor")]);

    // `who=authors`: everyone above user level 0, which a contributor may ask for.
    let (status, writers) = body(
        site.get(
            &format!("/mc-json/mc/v2/users?{include}&who=authors"),
            Some("contributor"),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{writers}");
    // A contributor has no `list_users`: only users with published posts, none here.
    assert_eq!(writers, json!([]));
}

#[rocket::async_test]
async fn an_embedded_post_author_is_the_users_embed() {
    let site = site().await;
    let post = site
        .post_by("author", PostStatus::Publish, "Embedded")
        .await;
    let (status, item) = body(
        site.get(
            &format!("/mc-json/mc/v2/posts/{}?_embed=author", post.id),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    let author = &item["_embedded"]["author"][0];
    assert_eq!(author["id"], json!(site.id("author")));
    assert_eq!(author["slug"], "ann-author");
    assert_eq!(
        keys(author),
        set(&[&[
            "id",
            "name",
            "url",
            "description",
            "link",
            "slug",
            "avatar_urls",
            "_links"
        ]])
    );
}

#[rocket::async_test]
async fn an_embedded_author_without_published_posts_needs_list_users() {
    let site = site().await;
    // The second author has only a draft; an editor may read it, but not list users.
    let draft = site.post_by("author2", PostStatus::Draft, "Draft").await;
    let uri = format!("/mc-json/mc/v2/posts/{}?_embed=author", draft.id);
    let (status, item) = body(site.get(&uri, Some("editor")).await).await;
    assert_eq!(status, Status::Ok, "{item}");
    assert_eq!(
        item["_embedded"]["author"][0],
        json!({
            "code": "rest_user_cannot_view",
            "message": "Sorry, you are not allowed to list users.",
            "data": {"status": 403},
        })
    );
    // An administrator lists users; the author is themself.
    for reader in ["admin", "author2"] {
        let (_, item) = body(site.get(&uri, Some(reader)).await).await;
        assert_eq!(
            item["_embedded"]["author"][0]["id"],
            json!(site.id("author2")),
            "{reader}"
        );
    }
}
