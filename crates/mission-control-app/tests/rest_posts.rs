//! The posts controller (Task 10.2): `/mc-json/mc/v2/posts` and `/pages`, as WordPress's
//! `WP_REST_Posts_Controller` answers them.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.
//! Users sign in with application passwords; their roles come from an in-memory provider.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{Duration, NaiveDateTime, SubsecRound};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::{
    MAX_FILTERED_BYTES, NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, get_post_meta,
    get_post_revisions, set_option, set_post_meta,
};
use mission_control_core::migrations::migrator;
use mission_control_core::rest::RestRoutes;
use mission_control_core::rest::posts::{PostType, PostTypes, Supports};
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::types::{Origin, PostTypeDef, Registry, TaxonomyDef};
use mission_control_core::users::app_password::{self, PlainPassword};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, InMemoryUserRoles, RoleAssignment, SUBSCRIBER,
    UserRoles,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// The keys of a post in `context=view`, as WordPress 6.7's `wp/v2/posts` gives them.
const POST_VIEW_KEYS: &[&str] = &[
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
    "content",
    "excerpt",
    "author",
    "featured_media",
    "comment_status",
    "ping_status",
    "sticky",
    "template",
    "format",
    "meta",
    "categories",
    "tags",
    "class_list",
    "_links",
];

/// The keys of a page in `context=view`, as WordPress 6.7's `wp/v2/pages` gives them: no
/// `sticky`, `format` or taxonomies; `parent` and `menu_order`.
const PAGE_VIEW_KEYS: &[&str] = &[
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
    "content",
    "excerpt",
    "author",
    "featured_media",
    "parent",
    "menu_order",
    "comment_status",
    "ping_status",
    "template",
    "meta",
    "class_list",
    "_links",
];

/// What `context=edit` adds to the view keys.
const EDIT_EXTRA_KEYS: &[&str] = &["password", "permalink_template", "generated_slug"];

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
    roles: Arc<InMemoryUserRoles>,
}

impl Site {
    fn db(&self) -> &Database {
        self.test.db()
    }

    fn user(&self, name: &str) -> &User {
        &self.users[name]
    }

    fn auth(&self, name: &str) -> Header<'static> {
        let user = self.user(name);
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

    async fn send(
        &self,
        method: &str,
        uri: &str,
        as_user: Option<&str>,
        body: Option<Value>,
        headers: &[(&'static str, String)],
    ) -> LocalResponse<'_> {
        let mut request = match method {
            "POST" => self.client.post(uri.to_owned()),
            "PUT" => self.client.put(uri.to_owned()),
            "PATCH" => self.client.patch(uri.to_owned()),
            "DELETE" => self.client.delete(uri.to_owned()),
            other => panic!("unexpected method {other}"),
        };
        if let Some(name) = as_user {
            request = request.header(self.auth(name));
        }
        for (name, value) in headers {
            request = request.header(Header::new(*name, value.clone()));
        }
        if let Some(body) = body {
            request = request.header(ContentType::JSON).body(body.to_string());
        }
        request.dispatch().await
    }

    async fn post(&self, uri: &str, as_user: Option<&str>, body: Value) -> LocalResponse<'_> {
        self.send("POST", uri, as_user, Some(body), &[]).await
    }

    /// A post of `post_type` by `author`, stored directly.
    async fn create(&self, author: &str, post_type: &str, new: NewPost) -> Post {
        Post::create(
            self.db(),
            NewPost {
                author_id: self.user(author).id,
                post_type: post_type.to_owned(),
                ..new
            },
        )
        .await
        .expect("creates the post")
    }

    async fn published(&self, author: &str, post_type: &str, title: &str) -> Post {
        self.create(
            author,
            post_type,
            NewPost {
                status: PostStatus::Publish,
                title: title.to_owned(),
                content: format!("<p>The body of {title}.</p>"),
                published_at: Some(mission_control_now()),
                ..NewPost::new(0, post_type)
            },
        )
        .await
    }

    /// The `Authorization` header of a new subscriber `name` granted `cap` by name.
    async fn subscriber_with(&self, name: &str, cap: &str) -> Header<'static> {
        let (id, email) = new_user(self.db(), name).await;
        let (password, _) = app_password::create(self.db(), id, "Tests", None)
            .await
            .expect("creates the application password");
        let granted = BTreeMap::from([(cap.to_owned(), true)]);
        self.roles
            .assign(id, RoleAssignment::new([SUBSCRIBER], granted));
        Header::new(
            "Authorization",
            format!(
                "Basic {}",
                STANDARD.encode(format!("{email}:{}", password.as_str()))
            ),
        )
    }

    async fn reload(&self, id: i64) -> Option<Post> {
        Post::query().find(self.db(), id).await.expect("reads")
    }
}

/// Now, in UTC, to the second, as posts store it.
fn mission_control_now() -> NaiveDateTime {
    chrono::Utc::now().naive_utc().trunc_subsecs(0)
}

fn config() -> Config {
    config_with_themes(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes"))
}

fn config_with_themes(themes_dir: PathBuf) -> Config {
    Config {
        database_url: "unused: the database is passed in".to_owned(),
        secret_key: "unused".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir,
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

/// A site with an administrator, an editor, two authors and a contributor, pretty permalinks
/// and the address [`HOME`].
async fn site() -> Site {
    site_with(config()).await
}

/// [`site`] with the configuration `config`.
async fn site_with(config: Config) -> Site {
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
        ("gmt_offset", json!("0")),
    ] {
        set_option(db, name, &value).await.expect("sets the option");
    }
    let roles = Arc::new(InMemoryUserRoles::new());
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
    let user_roles: Arc<dyn UserRoles> = Arc::clone(&roles) as Arc<dyn UserRoles>;
    let options = AppOptions {
        database: Some(test.shared()),
        user_roles: Some(user_roles),
        ..AppOptions::default()
    };
    let client = Client::tracked(rocket_with_options(config, options))
        .await
        .expect("the app ignites");
    Site {
        test,
        client,
        users,
        roles,
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

fn header<'a>(response: &'a LocalResponse<'_>, name: &str) -> Option<&'a str> {
    response.headers().get_one(name)
}

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap_or_else(|| panic!("an array: {list}"))
        .iter()
        .map(|item| item["id"].as_i64().expect("an id"))
        .collect()
}

#[rocket::async_test]
async fn a_post_has_wordpresss_keys() {
    let site = site().await;
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let post = site.published("author", "post", "Hello world").await;
    post.sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();

    let (status, json) = body(
        site.get(&format!("/mc-json/mc/v2/posts/{}", post.id), None)
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(keys(&json), set(POST_VIEW_KEYS));
    assert_eq!(json["id"], json!(post.id));
    assert_eq!(json["type"], json!("post"));
    assert_eq!(json["status"], json!("publish"));
    assert_eq!(json["slug"], json!("hello-world"));
    assert_eq!(json["link"], json!(format!("{HOME}/hello-world/")));
    assert_eq!(json["title"], json!({"rendered": "Hello world"}));
    assert_eq!(
        json["content"],
        // `the_content`'s wpautop, as WordPress renders it (Task A5).
        json!({"rendered": "<p>The body of Hello world.</p>\n", "protected": false})
    );
    assert_eq!(json["excerpt"]["protected"], json!(false));
    assert_eq!(json["author"], json!(site.user("author").id));
    assert_eq!(json["categories"], json!([news.term_id]));
    assert_eq!(json["tags"], json!([]));
    assert_eq!(json["meta"], json!({}));
    assert_eq!(json["sticky"], json!(false));
    assert_eq!(json["format"], json!("standard"));
    assert_eq!(json["featured_media"], json!(0));
    assert_eq!(
        json["guid"],
        json!({"rendered": format!("{HOME}/?p={}", post.id)})
    );
    let date = post
        .published_at
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    assert_eq!(
        (json["date"].clone(), json["date_gmt"].clone()),
        (json!(date), json!(date))
    );
    assert!(
        json["class_list"]
            .as_array()
            .unwrap()
            .contains(&json!("category-news"))
    );
    let links = keys(&json["_links"]);
    for rel in [
        "self",
        "collection",
        "about",
        "author",
        "version-history",
        "mc:attachment",
        "mc:term",
        "curies",
    ] {
        assert!(links.contains(rel), "{rel} in {links:?}");
    }
    assert_eq!(
        json["_links"]["self"][0]["href"],
        json!(format!("{HOME}/mc-json/mc/v2/posts/{}", post.id))
    );
    assert_eq!(
        json["_links"]["self"][0]["targetHints"]["allow"],
        json!(["GET"])
    );

    // The edit context adds the raw fields, for a user who may edit the post.
    let (status, json) = body(
        site.get(
            &format!("/mc-json/mc/v2/posts/{}?context=edit", post.id),
            Some("editor"),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    let mut edit = set(POST_VIEW_KEYS);
    edit.extend(set(EDIT_EXTRA_KEYS));
    assert_eq!(keys(&json), edit);
    assert_eq!(json["title"]["raw"], json!("Hello world"));
    assert_eq!(
        json["content"]["raw"],
        json!("<p>The body of Hello world.</p>")
    );
    assert_eq!(json["content"]["block_version"], json!(0));
    assert_eq!(
        json["permalink_template"],
        json!(format!("{HOME}/%postname%/"))
    );
    assert_eq!(json["generated_slug"], json!("hello-world"));
    assert!(keys(&json["_links"]).contains("mc:action-publish"));

    // Anonymous edit context is refused.
    assert_error(
        site.get(
            &format!("/mc-json/mc/v2/posts/{}?context=edit", post.id),
            None,
        )
        .await,
        Status::Unauthorized,
        "rest_forbidden_context",
    )
    .await;
}

#[rocket::async_test]
async fn a_page_has_wordpresss_keys() {
    let site = site().await;
    let parent = site.published("editor", "page", "About").await;
    let page = site
        .create(
            "editor",
            "page",
            NewPost {
                status: PostStatus::Publish,
                title: "Team".into(),
                parent_id: Some(parent.id),
                menu_order: 3,
                published_at: Some(mission_control_now()),
                ..NewPost::new(0, "page")
            },
        )
        .await;
    set_post_meta(
        site.db(),
        page.id,
        "_wp_page_template",
        &json!("templates/wide.php"),
    )
    .await
    .unwrap();
    let (status, json) = body(
        site.get(&format!("/mc-json/mc/v2/pages/{}", page.id), None)
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(keys(&json), set(PAGE_VIEW_KEYS));
    assert_eq!(json["parent"], json!(parent.id));
    assert_eq!(json["menu_order"], json!(3));
    assert_eq!(json["template"], json!("templates/wide.php"));
    assert_eq!(json["link"], json!(format!("{HOME}/about/team/")));
    assert_eq!(
        json["guid"]["rendered"],
        json!(format!("{HOME}/?page_id={}", page.id))
    );

    // A page is not a post, and a post is not a page.
    assert_error(
        site.get(&format!("/mc-json/mc/v2/posts/{}", page.id), None)
            .await,
        Status::NotFound,
        "rest_post_invalid_id",
    )
    .await;

    // The list of pages filters by parent and orders by menu_order.
    let first = site
        .create(
            "editor",
            "page",
            NewPost {
                status: PostStatus::Publish,
                title: "First".into(),
                parent_id: Some(parent.id),
                menu_order: 1,
                published_at: Some(mission_control_now()),
                ..NewPost::new(0, "page")
            },
        )
        .await;
    let (status, json) = body(
        site.get(
            &format!(
                "/mc-json/mc/v2/pages?parent={}&orderby=menu_order&order=asc",
                parent.id
            ),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(ids(&json), [first.id, page.id]);
}

#[rocket::async_test]
async fn lists_have_pagination_headers() {
    let site = site().await;
    let mut posts = Vec::new();
    for title in ["One", "Two", "Three"] {
        posts.push(site.published("author", "post", title).await);
    }
    // Drafts, pages and other types are not listed.
    site.create(
        "author",
        "post",
        NewPost {
            title: "Draft".into(),
            ..NewPost::new(0, "post")
        },
    )
    .await;
    site.published("editor", "page", "A page").await;

    let response = site.get("/mc-json/mc/v2/posts?per_page=2", None).await;
    assert_eq!(header(&response, "X-MC-Total"), Some("3"));
    assert_eq!(header(&response, "X-MC-TotalPages"), Some("2"));
    let links: Vec<String> = response.headers().get("Link").map(str::to_owned).collect();
    assert_eq!(
        links,
        [format!(
            "<{HOME}/mc-json/mc/v2/posts?per_page=2&page=2>; rel=\"next\""
        )]
    );
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok);
    // Newest first, the id breaking ties within a second.
    assert_eq!(ids(&json), [posts[2].id, posts[1].id]);
    for item in json.as_array().unwrap() {
        assert_eq!(keys(item), set(POST_VIEW_KEYS));
    }

    let response = site
        .get("/mc-json/mc/v2/posts?per_page=2&page=2", None)
        .await;
    let links: Vec<String> = response.headers().get("Link").map(str::to_owned).collect();
    assert_eq!(
        links,
        [format!(
            "<{HOME}/mc-json/mc/v2/posts?per_page=2&page=1>; rel=\"prev\""
        )]
    );
    let (_, json) = body(response).await;
    assert_eq!(ids(&json), [posts[0].id]);

    assert_error(
        site.get("/mc-json/mc/v2/posts?per_page=2&page=3", None)
            .await,
        Status::BadRequest,
        "rest_post_invalid_page_number",
    )
    .await;
    let json = assert_error(
        site.get("/mc-json/mc/v2/posts?per_page=101", None).await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert!(json["data"]["params"]["per_page"].is_string(), "{json}");

    // include and orderby=include keep the order asked for.
    let (_, json) = body(
        site.get(
            &format!(
                "/mc-json/mc/v2/posts?include={},{}&orderby=include",
                posts[0].id, posts[2].id
            ),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(ids(&json), [posts[0].id, posts[2].id]);
}

#[rocket::async_test]
async fn fields_filter_the_keys() {
    let site = site().await;
    let post = site.published("author", "post", "Fields").await;
    let (_, json) = body(
        site.get(
            &format!(
                "/mc-json/mc/v2/posts/{}?_fields=id,title.rendered,slug",
                post.id
            ),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        json,
        json!({"id": post.id, "slug": "fields", "title": {"rendered": "Fields"}})
    );
    let (_, json) = body(site.get("/mc-json/mc/v2/posts?_fields=id", None).await).await;
    assert_eq!(json, json!([{"id": post.id}]));
}

#[rocket::async_test]
async fn search_finds_words_in_the_title_and_content() {
    let site = site().await;
    let a = site.published("author", "post", "Rockets").await;
    let b = site
        .create(
            "author",
            "post",
            NewPost {
                status: PostStatus::Publish,
                title: "Engines".into(),
                content: "All about rockets and fuel.".into(),
                published_at: Some(mission_control_now() - Duration::seconds(10)),
                ..NewPost::new(0, "post")
            },
        )
        .await;
    site.published("author", "post", "Unrelated").await;
    let (_, json) = body(site.get("/mc-json/mc/v2/posts?search=rocket", None).await).await;
    assert_eq!(ids(&json), [a.id, b.id]);
    let (_, json) = body(
        site.get("/mc-json/mc/v2/posts?search=fuel&orderby=relevance", None)
            .await,
    )
    .await;
    assert_eq!(ids(&json), [b.id]);
    assert_error(
        site.get("/mc-json/mc/v2/posts?orderby=relevance", None)
            .await,
        Status::BadRequest,
        "rest_no_search_term_defined",
    )
    .await;
}

#[rocket::async_test]
async fn drafts_are_listed_only_for_editors() {
    let site = site().await;
    let draft = site
        .create(
            "author",
            "post",
            NewPost {
                title: "Secret".into(),
                ..NewPost::new(0, "post")
            },
        )
        .await;
    let json = assert_error(
        site.get("/mc-json/mc/v2/posts?status=draft", None).await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert_eq!(
        json["data"]["details"]["status"]["code"],
        json!("rest_forbidden_status"),
        "{json}"
    );
    let (status, json) = body(
        site.get("/mc-json/mc/v2/posts?status=draft", Some("editor"))
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(ids(&json), [draft.id]);
    // A draft cannot be read by a visitor, nor by another author.
    assert_error(
        site.get(&format!("/mc-json/mc/v2/posts/{}", draft.id), None)
            .await,
        Status::Unauthorized,
        "rest_forbidden",
    )
    .await;
    assert_error(
        site.get(
            &format!("/mc-json/mc/v2/posts/{}", draft.id),
            Some("author2"),
        )
        .await,
        Status::Forbidden,
        "rest_forbidden",
    )
    .await;
    let (status, _) = body(
        site.get(
            &format!("/mc-json/mc/v2/posts/{}", draft.id),
            Some("author"),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
}

#[rocket::async_test]
async fn roles_decide_who_creates_publishes_and_edits() {
    let site = site().await;

    assert_error(
        site.post("/mc-json/mc/v2/posts", None, json!({"title": "Nope"}))
            .await,
        Status::Unauthorized,
        "rest_cannot_create",
    )
    .await;

    // A contributor writes drafts and cannot publish.
    let (status, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("contributor"),
            json!({"title": "My draft", "content": "Words"}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["status"], json!("draft"));
    assert_eq!(json["author"], json!(site.user("contributor").id));
    assert_eq!(json["title"]["raw"], json!("My draft"));
    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("contributor"),
            json!({"title": "Published", "status": "publish"}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_publish",
    )
    .await;

    // An author publishes, with a Location header.
    let response = site
        .post(
            "/mc-json/mc/v2/posts",
            Some("author"),
            json!({"title": "Launch day", "content": "Go", "status": "publish"}),
        )
        .await;
    let location = header(&response, "Location").map(str::to_owned);
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Created, "{json}");
    let id = json["id"].as_i64().unwrap();
    assert_eq!(location, Some(format!("{HOME}/mc-json/mc/v2/posts/{id}")));
    assert_eq!(json["status"], json!("publish"));
    assert_eq!(json["slug"], json!("launch-day"));

    // An author cannot write as someone else.
    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("author"),
            json!({"title": "As editor", "author": site.user("editor").id}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_edit_others",
    )
    .await;

    // The author edits their own post, but not another author's.
    let (status, json) = body(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/posts/{id}"),
            Some("author"),
            Some(json!({"title": "Launch day, again"})),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["title"]["raw"], json!("Launch day, again"));
    // Every update records a revision.
    assert_eq!(get_post_revisions(site.db(), id).await.unwrap().len(), 1);
    assert_error(
        site.send(
            "PATCH",
            &format!("/mc-json/mc/v2/posts/{id}"),
            Some("author2"),
            Some(json!({"title": "Mine now"})),
            &[],
        )
        .await,
        Status::Forbidden,
        "rest_cannot_edit",
    )
    .await;
    assert_error(
        site.send(
            "DELETE",
            &format!("/mc-json/mc/v2/posts/{id}"),
            Some("author2"),
            None,
            &[],
        )
        .await,
        Status::Forbidden,
        "rest_cannot_delete",
    )
    .await;
    // The editor edits anyone's post; POST updates too, as in WordPress.
    let (status, json) = body(
        site.post(
            &format!("/mc-json/mc/v2/posts/{id}"),
            Some("editor"),
            json!({"excerpt": "Short"}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["excerpt"]["raw"], json!("Short"));
    assert_eq!(json["title"]["raw"], json!("Launch day, again"));

    // A form-encoded body works as JSON does.
    let response = site
        .client
        .post(format!("/mc-json/mc/v2/posts/{id}"))
        .header(site.auth("editor"))
        .header(ContentType::Form)
        .body("title=From+a+form&menu_order=2")
        .dispatch()
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["title"]["raw"], json!("From a form"));
}

#[rocket::async_test]
async fn an_updated_slug_is_settled_and_reported() {
    let site = site().await;
    site.published("author", "post", "Taken").await;
    let post = site.published("author", "post", "Other").await;
    let (status, json) = body(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/posts/{}", post.id),
            Some("author"),
            Some(json!({"slug": "taken"})),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["slug"], json!("taken-2"));
}

#[rocket::async_test]
async fn kses_filters_authors_and_not_editors() {
    let site = site().await;
    let html = "<p>Hi</p><script>alert(1)</script>";
    let (_, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("author"),
            json!({"title": "Filtered", "content": html}),
        )
        .await,
    )
    .await;
    let raw = json["content"]["raw"].as_str().unwrap();
    assert!(!raw.contains("<script"), "{raw}");
    assert!(raw.contains("<p>Hi</p>"), "{raw}");
    let id = json["id"].as_i64().unwrap();
    assert!(!site.reload(id).await.unwrap().content.contains("<script"));

    let (_, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "Unfiltered", "content": html}),
        )
        .await,
    )
    .await;
    assert_eq!(json["content"]["raw"], json!(html));
}

#[rocket::async_test]
async fn an_oversized_field_is_refused_for_an_author() {
    let site = site().await;
    let big = "a".repeat(MAX_FILTERED_BYTES + 1);
    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("author"),
            json!({"title": "Big", "content": big}),
        )
        .await,
        Status::PayloadTooLarge,
        "rest_content_too_large",
    )
    .await;
    assert_eq!(Post::of_type("post").count(site.db()).await.unwrap(), 0);
    // An editor is not filtered, so the kses cap does not apply, on every database.
    let (status, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "Big", "content": big}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    let id = json["id"].as_i64().unwrap();
    assert_eq!(site.reload(id).await.unwrap().content, big);
}

#[rocket::async_test]
async fn an_editors_two_megabyte_post_fits_the_request_limit() {
    let site = site().await;
    // Ruling R58: the REST body limit is 8 MiB by default (PHP's `post_max_size`), past
    // Rocket's 1 MiB `json` limit.
    let content = format!("<p>{}</p>", "Grand café 日本. ".repeat(105_000));
    assert!(content.len() > 2_000_000);
    let (status, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "Two megabytes", "content": content}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    let id = json["id"].as_i64().unwrap();
    assert_eq!(site.reload(id).await.unwrap().content, content);

    // Past the limit, the body is refused before it is read in full.
    let huge = "a".repeat(8 * 1024 * 1024 + 1);
    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "Too big", "content": huge}),
        )
        .await,
        Status::PayloadTooLarge,
        "rest_request_too_large",
    )
    .await;
}

#[rocket::async_test]
async fn an_editors_one_megabyte_post_saves_and_reads_back() {
    let site = site().await;
    // Past MySQL's 64 KB `TEXT` and the kses cap: the columns are `LONGTEXT` there, as in
    // WordPress, and an editor (`unfiltered_html`) is not filtered.
    let content = format!("<p>{}</p>", "Grand café 日本. ".repeat(50_500));
    assert!(content.len() > 1_000_000 && content.len() < 1_040_000);
    let (status, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "A megabyte", "content": content}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    let id = json["id"].as_i64().unwrap();
    assert_eq!(site.reload(id).await.unwrap().content, content);
    let (status, json) = body(
        site.get(
            &format!("/mc-json/mc/v2/posts/{id}?context=edit"),
            Some("editor"),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(json["content"]["raw"], json!(content));
}

#[rocket::async_test]
async fn fields_over_64_kb_round_trip_on_every_database() {
    let site = site().await;
    // Under the kses cap for an author, over MySQL's 65,535-byte `TEXT`.
    let content = format!("<p>{}</p>", "a".repeat(70_000));
    let excerpt = "e".repeat(70_000);
    let title = "t".repeat(70_000);
    let (status, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("author"),
            json!({"title": title, "content": content, "excerpt": excerpt}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    let id = json["id"].as_i64().unwrap();
    let stored = site.reload(id).await.unwrap();
    assert_eq!(stored.content, content);
    assert_eq!(stored.excerpt, excerpt);
    assert_eq!(stored.title, title);

    // An update that keeps the long fields (kses runs on the merged post) stores them whole.
    let (status, json) = body(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/posts/{id}"),
            Some("author"),
            Some(json!({"slug": "longer"})),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["content"]["raw"], json!(content));
    assert_eq!(json["excerpt"]["raw"], json!(excerpt));
    assert_eq!(json["title"]["raw"], json!(title));
    assert_eq!(site.reload(id).await.unwrap().content, content);
}

#[rocket::async_test]
async fn if_unmodified_since_refuses_a_stale_update() {
    let site = site().await;
    let post = site.published("author", "post", "Versioned").await;
    let (_, json) = body(
        site.get(&format!("/mc-json/mc/v2/posts/{}", post.id), None)
            .await,
    )
    .await;
    let modified =
        NaiveDateTime::parse_from_str(json["modified_gmt"].as_str().unwrap(), "%Y-%m-%dT%H:%M:%S")
            .unwrap();
    let http_date = |date: NaiveDateTime| date.format("%a, %d %b %Y %H:%M:%S GMT").to_string();

    // A save the client has not seen.
    let mut other = site.reload(post.id).await.unwrap();
    other.title = "Changed elsewhere".into();
    other.save(site.db()).await.unwrap();

    assert_error(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/posts/{}", post.id),
            Some("author"),
            Some(json!({"title": "Mine"})),
            &[("If-Unmodified-Since", http_date(modified))],
        )
        .await,
        Status::PreconditionFailed,
        "rest_post_conflict",
    )
    .await;
    let stored = site.reload(post.id).await.unwrap();
    assert_eq!(stored.title, "Changed elsewhere");

    let (status, json) = body(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/posts/{}", post.id),
            Some("author"),
            Some(json!({"title": "Mine"})),
            &[("If-Unmodified-Since", http_date(stored.modified_at))],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["title"]["raw"], json!("Mine"));

    // Without the header the last write wins, as in WordPress.
    let (status, _) = body(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/posts/{}", post.id),
            Some("author"),
            Some(json!({"title": "Last"})),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
}

#[rocket::async_test]
async fn an_invalid_if_unmodified_since_is_ignored() {
    let site = site().await;
    let post = site.published("author", "post", "Versioned").await;
    // A save the client has not seen: a valid, older header would get a 412.
    let mut other = site.reload(post.id).await.unwrap();
    other.title = "Changed elsewhere".into();
    other.save(site.db()).await.unwrap();

    // RFC 9110, section 13.1.4: a header that is not an HTTP date is ignored, so the update is
    // made without the check, and the last write wins as in WordPress.
    for (i, header) in [
        "not a date",
        "",
        "2026-11-15T10:00:00Z",
        "Sun, 99 Nov 1994 08:49:37 GMT",
    ]
    .into_iter()
    .enumerate()
    {
        let title = format!("Mine {i}");
        let (status, json) = body(
            site.send(
                "PUT",
                &format!("/mc-json/mc/v2/posts/{}", post.id),
                Some("author"),
                Some(json!({"title": title})),
                &[("If-Unmodified-Since", header.to_owned())],
            )
            .await,
        )
        .await;
        assert_eq!(status, Status::Ok, "{header:?}: {json}");
        assert_eq!(site.reload(post.id).await.unwrap().title, title);
    }
}

#[rocket::async_test]
async fn delete_trashes_and_force_deletes() {
    let site = site().await;
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let post = site.published("author", "post", "Short lived").await;
    post.sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    let uri = format!("/mc-json/mc/v2/posts/{}", post.id);

    let (status, json) = body(site.send("DELETE", &uri, Some("author"), None, &[]).await).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["status"], json!("trash"));
    assert_eq!(json["id"], json!(post.id));
    assert_error(
        site.send("DELETE", &uri, Some("author"), None, &[]).await,
        Status::Gone,
        "rest_already_trashed",
    )
    .await;

    let (status, json) = body(
        site.send(
            "DELETE",
            &format!("{uri}?force=true"),
            Some("author"),
            None,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["deleted"], json!(true));
    assert_eq!(json["previous"]["id"], json!(post.id));
    assert!(site.reload(post.id).await.is_none());
    assert!(
        get_post_revisions(site.db(), post.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_error(
        site.get(&uri, Some("author")).await,
        Status::NotFound,
        "rest_post_invalid_id",
    )
    .await;

    // A published post in the category, deleted at once: the count follows.
    let second = site.published("author", "post", "Counted").await;
    second
        .sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    assert_eq!(
        Term::find(site.db(), news.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        1
    );
    let (status, _) = body(
        site.send(
            "DELETE",
            &format!("/mc-json/mc/v2/posts/{}?force=true", second.id),
            Some("editor"),
            None,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(
        Term::find(site.db(), news.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        0
    );
}

#[rocket::async_test]
async fn the_front_page_needs_manage_options_to_delete() {
    let site = site().await;
    let front = site.published("editor", "page", "Home").await;
    set_option(site.db(), "show_on_front", &json!("page"))
        .await
        .unwrap();
    set_option(site.db(), "page_on_front", &json!(front.id))
        .await
        .unwrap();
    let uri = format!("/mc-json/mc/v2/pages/{}", front.id);
    assert_error(
        site.send("DELETE", &uri, Some("editor"), None, &[]).await,
        Status::Forbidden,
        "rest_cannot_delete",
    )
    .await;
    // The editor may still edit it.
    let (status, _) = body(
        site.send(
            "PUT",
            &uri,
            Some("editor"),
            Some(json!({"title": "Welcome"})),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    let (status, _) = body(
        site.send(
            "DELETE",
            &format!("{uri}?force=true"),
            Some("admin"),
            None,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    // WordPress forgets the deleted front page.
    let (_, index) = body(site.get("/mc-json/", None).await).await;
    assert_eq!(index["page_on_front"], json!(0));
    assert_eq!(index["show_on_front"], json!("posts"));
}

#[rocket::async_test]
async fn a_protected_post_hides_its_content() {
    let site = site().await;
    let post = site
        .create(
            "author",
            "post",
            NewPost {
                status: PostStatus::Publish,
                title: "Locked".into(),
                content: "Hidden words".into(),
                password: Some("sesame".into()),
                published_at: Some(mission_control_now()),
                ..NewPost::new(0, "post")
            },
        )
        .await;
    let uri = format!("/mc-json/mc/v2/posts/{}", post.id);
    let (_, json) = body(site.get(&uri, None).await).await;
    assert_eq!(json["content"], json!({"rendered": "", "protected": true}));
    assert_eq!(json["excerpt"], json!({"rendered": "", "protected": true}));
    // `get_the_title`'s prefix (Ruling R70).
    assert_eq!(json["title"]["rendered"], json!("Protected: Locked"));
    let (_, json) = body(site.get(&format!("{uri}?password=sesame"), None).await).await;
    assert_eq!(json["content"]["rendered"], json!("<p>Hidden words</p>\n"));
    assert_eq!(json["title"]["rendered"], json!("Protected: Locked"));
    assert_error(
        site.get(&format!("{uri}?password=wrong"), None).await,
        Status::Forbidden,
        "rest_post_incorrect_password",
    )
    .await;
}

#[rocket::async_test]
async fn a_private_posts_rendered_title_is_marked() {
    let site = site().await;
    let post = site
        .create(
            "author",
            "post",
            NewPost {
                status: PostStatus::Private,
                title: "Secret".into(),
                content: "Inside".into(),
                published_at: Some(mission_control_now()),
                ..NewPost::new(0, "post")
            },
        )
        .await;
    let uri = format!("/mc-json/mc/v2/posts/{}?context=edit", post.id);
    let (status, json) = body(site.get(&uri, Some("editor")).await).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(
        json["title"],
        json!({"raw": "Secret", "rendered": "Private: Secret"})
    );
    let (_, list) = body(
        site.get("/mc-json/mc/v2/posts?status=private", Some("editor"))
            .await,
    )
    .await;
    assert_eq!(list[0]["title"]["rendered"], json!("Private: Secret"));
}

#[rocket::async_test]
async fn only_types_shown_in_rest_have_routes() {
    let site = site().await;
    // A type that is not registered for the REST API (like `show_in_rest` false) has no route
    // and is not listed, even with posts in the database.
    site.published("author", "book", "A book").await;
    assert_error(
        site.get("/mc-json/mc/v2/books", None).await,
        Status::NotFound,
        "rest_no_route",
    )
    .await;
    let (_, json) = body(site.get("/mc-json/mc/v2/posts", None).await).await;
    assert_eq!(json, json!([]));
    let (_, index) = body(site.get("/mc-json/mc/v2", None).await).await;
    let routes = keys(&index["routes"]);
    for route in [
        "/mc/v2/posts",
        "/mc/v2/posts/(?P<id>[\\d]+)",
        "/mc/v2/pages",
        "/mc/v2/pages/(?P<id>[\\d]+)",
    ] {
        assert!(routes.contains(route), "{route} in {routes:?}");
    }
    assert!(!routes.iter().any(|r| r.contains("book")), "{routes:?}");
    assert_eq!(
        index["routes"]["/mc/v2/posts"]["methods"],
        json!(["GET", "POST"])
    );
    // A non-numeric id is no route.
    assert_error(
        site.get("/mc-json/mc/v2/posts/abc", None).await,
        Status::NotFound,
        "rest_no_route",
    )
    .await;
}

#[rocket::async_test]
async fn embed_adds_the_author_and_terms() {
    let site = site().await;
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let post = site.published("author", "post", "Embedded").await;
    post.sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    let (_, json) = body(
        site.get(&format!("/mc-json/mc/v2/posts/{}?_embed", post.id), None)
            .await,
    )
    .await;
    let embedded = &json["_embedded"];
    assert_eq!(embedded["author"][0]["id"], json!(site.user("author").id));
    assert_eq!(embedded["author"][0]["name"], json!("author"));
    assert_eq!(embedded["mc:term"][0][0]["slug"], json!("news"));
    assert_eq!(embedded["mc:term"][0][0]["taxonomy"], json!("category"));
    assert_eq!(embedded["mc:term"][1], json!([]));
    // Only what was asked for.
    let (_, json) = body(
        site.get(
            &format!("/mc-json/mc/v2/posts/{}?_embed=author", post.id),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(keys(&json["_embedded"]), set(&["author"]));
}

#[rocket::async_test]
async fn featured_media_and_categories_are_written() {
    let site = site().await;
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let image = site
        .create(
            "editor",
            "attachment",
            NewPost {
                status: PostStatus::Inherit,
                title: "Photo".into(),
                mime_type: "image/jpeg".into(),
                ..NewPost::new(0, "attachment")
            },
        )
        .await;
    let (status, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({
                "title": "With media",
                "status": "publish",
                "featured_media": image.id,
                "categories": [news.term_id],
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["featured_media"], json!(image.id));
    assert_eq!(json["categories"], json!([news.term_id]));
    let id = json["id"].as_i64().unwrap();
    assert_eq!(
        get_post_meta(site.db(), id, "_thumbnail_id").await.unwrap(),
        Some(json!(image.id))
    );
    assert_eq!(
        Term::find(site.db(), news.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        1
    );
    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "Bad media", "featured_media": id}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_featured_media",
    )
    .await;
    // A file that is not an image is refused too: `set_post_thumbnail` sets images only.
    let pdf = site
        .create(
            "editor",
            "attachment",
            NewPost {
                status: PostStatus::Inherit,
                title: "Report".into(),
                mime_type: "application/pdf".into(),
                ..NewPost::new(0, "attachment")
            },
        )
        .await;
    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "PDF media", "featured_media": pdf.id}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_featured_media",
    )
    .await;
}

#[rocket::async_test]
async fn a_visitors_search_never_matches_a_protected_post() {
    let site = site().await;
    let locked = site
        .create(
            "author",
            "post",
            NewPost {
                status: PostStatus::Publish,
                title: "Vault".into(),
                content: "The secret word is xylophonic.".into(),
                password: Some("sesame".into()),
                published_at: Some(mission_control_now()),
                ..NewPost::new(0, "post")
            },
        )
        .await;
    let open = site
        .create(
            "author",
            "post",
            NewPost {
                status: PostStatus::Publish,
                title: "Open door".into(),
                content: "Nothing hidden: xylophonic.".into(),
                published_at: Some(mission_control_now() - Duration::seconds(5)),
                ..NewPost::new(0, "post")
            },
        )
        .await;
    let response = site
        .get("/mc-json/mc/v2/posts?search=xylophonic", None)
        .await;
    assert_eq!(header(&response, "X-MC-Total"), Some("1"));
    let (_, json) = body(response).await;
    assert_eq!(ids(&json), [open.id]);
    // A signed-in user's search matches it, as WordPress's does.
    let (_, json) = body(
        site.get("/mc-json/mc/v2/posts?search=xylophonic", Some("author2"))
            .await,
    )
    .await;
    assert_eq!(ids(&json), [locked.id, open.id]);

    // The front end's search is a visitor's too.
    let response = site.client.get("/?s=xylophonic").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let page = response.into_string().await.unwrap();
    assert!(page.contains("Open door"), "{page}");
    assert!(!page.contains("Vault"), "{page}");
}

#[rocket::async_test]
async fn force_delete_leaves_no_meta_or_relationships() {
    let site = site().await;
    let news = Term::create(site.db(), &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let post = site.published("author", "post", "Gone").await;
    post.sync_terms(site.db(), &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    set_post_meta(site.db(), post.id, "subtitle", &json!("x"))
        .await
        .unwrap();
    let (status, _) = body(
        site.send(
            "DELETE",
            &format!("/mc-json/mc/v2/posts/{}?force=true", post.id),
            Some("author"),
            None,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
    for (table, column) in [
        ("post_meta", "post_id"),
        ("term_relationships", "object_id"),
    ] {
        let count: i64 = raw::fetch_scalar(
            site.db(),
            &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?"),
            &[post.id.into()],
        )
        .await
        .unwrap();
        assert_eq!(count, 0, "{table}");
    }
}

#[rocket::async_test]
async fn edit_context_lists_need_edit_posts() {
    let site = site().await;
    site.published("author", "post", "Listed").await;
    // WordPress 6.7 lets anyone with the type's `edit_posts` list in the edit context (R51).
    let (status, json) = body(
        site.get("/mc-json/mc/v2/posts?context=edit", Some("contributor"))
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert!(json[0]["content"]["raw"].is_string());
    assert_error(
        site.get("/mc-json/mc/v2/posts?context=edit", None).await,
        Status::Unauthorized,
        "rest_forbidden_context",
    )
    .await;
    // A status outside the enum is named before any permission check.
    let json = assert_error(
        site.get("/mc-json/mc/v2/posts?status=nonsense", None).await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert_eq!(
        json["data"]["details"]["status"]["code"],
        json!("rest_not_in_enum")
    );
}

#[rocket::async_test]
async fn writes_check_sticky_parents_media_and_templates() {
    let site = site().await;
    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("contributor"),
            json!({"title": "Pin me", "sticky": true}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_assign_sticky",
    )
    .await;

    // A parent that would make a loop is dropped, as `wp_check_post_hierarchy_for_loops`.
    let top = site.published("editor", "page", "Top").await;
    let child = site
        .create(
            "editor",
            "page",
            NewPost {
                status: PostStatus::Publish,
                title: "Child".into(),
                parent_id: Some(top.id),
                published_at: Some(mission_control_now()),
                ..NewPost::new(0, "page")
            },
        )
        .await;
    let (status, json) = body(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/pages/{}", top.id),
            Some("editor"),
            Some(json!({"parent": child.id})),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["parent"], json!(0));
    assert_error(
        site.send(
            "PUT",
            &format!("/mc-json/mc/v2/pages/{}", top.id),
            Some("editor"),
            Some(json!({"parent": 999_999_999})),
            &[],
        )
        .await,
        Status::BadRequest,
        "rest_post_invalid_id",
    )
    .await;

    assert_error(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("editor"),
            json!({"title": "No such media", "featured_media": 999_999_999}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_featured_media",
    )
    .await;

    let json = assert_error(
        site.post(
            "/mc-json/mc/v2/pages",
            Some("editor"),
            json!({"title": "Templated", "template": "templates/missing.php"}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert!(json["data"]["params"]["template"].is_string(), "{json}");
    assert_error(
        site.post(
            "/mc-json/mc/v2/pages",
            Some("editor"),
            json!({"title": "Escaping", "template": "../../etc/passwd"}),
        )
        .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
}

#[rocket::async_test]
async fn a_publish_dated_ahead_is_scheduled() {
    let site = site().await;
    let date = (mission_control_now() + Duration::hours(3)).format("%Y-%m-%dT%H:%M:%S");
    let (status, json) = body(
        site.post(
            "/mc-json/mc/v2/posts",
            Some("author"),
            json!({"title": "Tomorrow", "status": "publish", "date_gmt": date.to_string()}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["status"], json!("future"));
    assert_eq!(json["date_gmt"], json!(date.to_string()));
    let id = json["id"].as_i64().unwrap();
    let scheduled: i64 = raw::fetch_scalar(
        site.db(),
        "SELECT COUNT(*) FROM scheduled_events WHERE hook = ? AND payload = ?",
        &["publish_future_post".into(), id.to_string().into()],
    )
    .await
    .unwrap();
    assert_eq!(scheduled, 1);
    // Trashing it removes the event.
    site.send(
        "DELETE",
        &format!("/mc-json/mc/v2/posts/{id}"),
        Some("author"),
        None,
        &[],
    )
    .await;
    let scheduled: i64 = raw::fetch_scalar(
        site.db(),
        "SELECT COUNT(*) FROM scheduled_events WHERE hook = ?",
        &["publish_future_post".into()],
    )
    .await
    .unwrap();
    assert_eq!(scheduled, 0);
}

#[rocket::async_test]
async fn a_trailing_slash_serves_the_same_route() {
    let site = site().await;
    let post = site.published("author", "post", "Slashed").await;
    let (status, json) = body(site.get("/mc-json/mc/v2/posts/", None).await).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(ids(&json), [post.id]);
    let (status, json) = body(
        site.get(&format!("/mc-json/mc/v2/posts/{}/", post.id), None)
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["id"], json!(post.id));
}

#[rocket::async_test]
async fn a_type_registered_on_the_app_is_served_only_when_shown() {
    let site = site().await;
    let types = site
        .client
        .rocket()
        .state::<Arc<PostTypes>>()
        .expect("the app manages its post types");
    let routes = site
        .client
        .rocket()
        .state::<Arc<RestRoutes>>()
        .expect("the app manages its routes");
    let book = PostType {
        name: "book".into(),
        rest_base: "books".into(),
        hierarchical: false,
        show_in_rest: false,
        supports: Supports::ALL,
        taxonomies: Vec::new(),
        caps: mission_control_core::users::PostTypeCaps::for_post_type("book"),
    };
    types.register(book.clone());
    types.register_routes(routes);
    let novel = site.published("author", "book", "A novel").await;
    assert_error(
        site.get("/mc-json/mc/v2/books", None).await,
        Status::NotFound,
        "rest_no_route",
    )
    .await;
    let (_, index) = body(site.get("/mc-json/mc/v2", None).await).await;
    assert!(!keys(&index["routes"]).iter().any(|r| r.contains("books")));

    // Shown, it is served and listed.
    types.register(PostType {
        show_in_rest: true,
        ..book
    });
    types.register_routes(routes);
    let (status, json) = body(site.get("/mc-json/mc/v2/books", None).await).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(ids(&json), [novel.id]);
    let (_, index) = body(site.get("/mc-json/mc/v2", None).await).await;
    assert!(keys(&index["routes"]).contains("/mc/v2/books"));
}

/// A post type from the type registry (Task 5.2) is served at its `rest_base`, with the
/// capability names of its definition: with `capability_type` `post`, an author edits and
/// publishes their own books, where the interim rule for an unknown type would ask for
/// `edit_others_posts`. Removed (its plugin deactivated), it is no longer served.
#[rocket::async_test]
async fn a_type_from_the_registry_is_served_with_its_capabilities() {
    let site = site().await;
    let rocket = site.client.rocket();
    let routes = rocket
        .state::<Arc<RestRoutes>>()
        .expect("the app manages its routes");
    let registry = Registry::new(
        Arc::clone(rocket.state::<Arc<PostTypes>>().expect("the post types")),
        Arc::clone(rocket.state::<Arc<Taxonomies>>().expect("the taxonomies")),
    );
    let book = PostTypeDef::from_json(
        r#"{"slug": "book", "public": true, "show_in_rest": true, "rest_base": "books",
            "labels": {"name": "Books", "singular_name": "Book"},
            "supports": ["title", "editor", "author"]}"#,
        Origin::Plugin("library".into()),
    )
    .unwrap();
    registry.register(book).unwrap();
    registry.post_types().register_routes(routes);

    let response = site
        .post(
            "/mc-json/mc/v2/books",
            Some("author"),
            json!({"title": "A novel", "status": "publish"}),
        )
        .await;
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["type"], json!("book"));
    let id = json["id"].as_i64().unwrap();
    let (status, json) = body(
        site.post(
            &format!("/mc-json/mc/v2/books/{id}"),
            Some("author"),
            json!({"title": "A better novel"}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    let (status, json) = body(site.get("/mc-json/mc/v2/books", None).await).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(ids(&json), [id]);

    assert_eq!(registry.remove_plugin("library"), 1);
    assert_error(
        site.get("/mc-json/mc/v2/books", None).await,
        Status::NotFound,
        "rest_no_route",
    )
    .await;
}

#[rocket::async_test]
async fn every_id_list_is_bounded() {
    let site = site().await;
    let many: Vec<String> = (1..=501).map(|n| n.to_string()).collect();
    for param in ["author_exclude", "categories", "tags_exclude"] {
        let json = assert_error(
            site.get(
                &format!("/mc-json/mc/v2/posts?{param}={}", many.join(",")),
                None,
            )
            .await,
            Status::BadRequest,
            "rest_invalid_param",
        )
        .await;
        assert!(json["data"]["params"][param].is_string(), "{param}: {json}");
    }
    let json = assert_error(
        site.get(
            &format!("/mc-json/mc/v2/pages?parent={}", many.join(",")),
            None,
        )
        .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert!(json["data"]["params"]["parent"].is_string(), "{json}");
}

#[rocket::async_test]
async fn an_attachment_like_inherit_post_reads_as_its_parent() {
    // Exercised through a page whose type is served: an `inherit` page under a draft parent is
    // not readable by a visitor, under a published one it is.
    let site = site().await;
    let draft = site
        .create(
            "editor",
            "page",
            NewPost {
                title: "Draft parent".into(),
                ..NewPost::new(0, "page")
            },
        )
        .await;
    let published = site.published("editor", "page", "Published parent").await;
    let under = |parent: i64, title: &'static str| NewPost {
        status: PostStatus::Inherit,
        title: title.into(),
        parent_id: Some(parent),
        ..NewPost::new(0, "page")
    };
    let hidden = site
        .create("editor", "page", under(draft.id, "Hidden"))
        .await;
    let shown = site
        .create("editor", "page", under(published.id, "Shown"))
        .await;
    assert_error(
        site.get(&format!("/mc-json/mc/v2/pages/{}", hidden.id), None)
            .await,
        Status::Unauthorized,
        "rest_forbidden",
    )
    .await;
    let (status, _) = body(
        site.get(&format!("/mc-json/mc/v2/pages/{}", shown.id), None)
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
}

#[rocket::async_test]
async fn a_stored_admin_post_type_is_served_after_a_restart() {
    let site = site().await;
    let registry = site
        .client
        .rocket()
        .state::<Arc<Registry>>()
        .expect("the app manages the type registry");
    let story = PostTypeDef::from_json(
        r#"{"slug": "story", "public": true, "show_in_rest": true, "rest_base": "stories",
            "capability_type": ["story", "stories"],
            "labels": {"name": "Stories", "singular_name": "Story"}}"#,
        Origin::Admin,
    )
    .unwrap();
    registry
        .save_admin_definition(site.db(), story)
        .await
        .expect("saves the admin's type");
    let published = site.published("author", "story", "Once upon a time").await;

    // A fresh app on the same database reads the stored type at ignition.
    let options = AppOptions {
        database: Some(site.test.shared()),
        user_roles: Some(Arc::clone(&site.roles) as Arc<dyn UserRoles>),
        ..AppOptions::default()
    };
    let client = Client::tracked(rocket_with_options(config(), options))
        .await
        .expect("the app ignites");
    let get = |uri: &'static str| client.get(uri).dispatch();
    let (status, json) = body(get("/mc-json/mc/v2/stories").await).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(ids(&json), [published.id]);
    let (_, discovery) = body(get("/mc-json/").await).await;
    assert!(
        discovery["routes"].get("/mc/v2/stories").is_some(),
        "discovery lists the stored type"
    );

    // `/mc/v2/types` names the meta capabilities by the irregular singular.
    let writer = site.subscriber_with("writer", "edit_stories").await;
    let request = client
        .get("/mc-json/mc/v2/types/story?context=edit")
        .header(writer);
    let (status, json) = body(request.dispatch().await).await;
    assert_eq!(status, Status::Ok, "{json}");
    let caps = &json["capabilities"];
    assert_eq!(caps["edit_post"], "edit_story", "{caps}");
    assert_eq!(caps["read_post"], "read_story");
    assert_eq!(caps["delete_post"], "delete_story");
    assert_eq!(caps["edit_posts"], "edit_stories");
}

#[rocket::async_test]
async fn a_private_custom_post_follows_its_types_read_private_capability() {
    let site = site().await;
    let rocket = site.client.rocket();
    let registry = rocket.state::<Arc<Registry>>().unwrap();
    let routes = rocket.state::<Arc<RestRoutes>>().unwrap();
    registry
        .register(
            PostTypeDef::from_json(
                r#"{"slug": "book", "public": true, "show_in_rest": true, "rest_base": "books",
                    "capability_type": "book",
                    "labels": {"name": "Books", "singular_name": "Book"}}"#,
                Origin::Code,
            )
            .unwrap(),
        )
        .unwrap();
    registry
        .register(
            TaxonomyDef::from_json(
                r#"{"slug": "genre", "object_type": ["book"], "show_in_rest": true,
                    "rest_base": "genres", "labels": {"name": "Genres", "singular_name": "Genre"}}"#,
                Origin::Code,
            )
            .unwrap(),
        )
        .unwrap();
    registry.post_types().register_routes(routes);
    registry.taxonomies().register_routes(routes);

    let librarian = site
        .subscriber_with("librarian", "read_private_books")
        .await;

    let book = site
        .create(
            "author",
            "book",
            NewPost {
                status: PostStatus::Private,
                title: "A private book".into(),
                ..NewPost::new(0, "book")
            },
        )
        .await;
    let scan = site
        .create(
            "author",
            "attachment",
            NewPost {
                status: PostStatus::Inherit,
                title: "A scan".into(),
                parent_id: Some(book.id),
                mime_type: "image/png".into(),
                ..NewPost::new(0, "attachment")
            },
        )
        .await;

    for uri in [
        format!("/mc-json/mc/v2/genres?post={}", book.id),
        format!("/mc-json/mc/v2/media/{}", scan.id),
    ] {
        // The librarian holds the type's capability, the editor only `read_private_posts`.
        let response = site
            .client
            .get(uri.clone())
            .header(librarian.clone())
            .dispatch()
            .await;
        let (status, json) = body(response).await;
        assert_eq!(status, Status::Ok, "{uri}: {json}");
        let (status, json) = body(site.get(&uri, Some("editor")).await).await;
        assert_eq!(status, Status::Forbidden, "{uri}: {json}");
    }
}

#[rocket::async_test]
async fn the_site_starts_when_the_type_tables_cannot_be_read() {
    let site = site().await;
    // A database installed before the field group tables.
    for table in ["field_group_locations", "field_groups"] {
        raw::execute(site.db(), &format!("DROP TABLE {table}"), &[])
            .await
            .expect("drops the table");
    }
    let options = AppOptions {
        database: Some(site.test.shared()),
        ..AppOptions::default()
    };
    let client = Client::tracked(rocket_with_options(config(), options))
        .await
        .expect("the app ignites without the stored types");
    let response = client.get("/mc-json/mc/v2/posts").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
}

/// Ruling R124: the REST API's `template` is one of the active theme's page templates (matched
/// by name, so `wide.php` is `wide`), `default` or empty; a template file without a `Template
/// Name:` header is refused, as WordPress's `check_template` refuses one `get_page_templates` lacks. An
/// unchanged stored value is not checked.
#[rocket::async_test]
async fn a_template_is_one_the_theme_declares_for_pages() {
    let themes = tempfile::tempdir().unwrap();
    let theme = themes.path().join("default");
    std::fs::create_dir_all(theme.join("templates")).unwrap();
    std::fs::write(theme.join("index.propel"), "index").unwrap();
    std::fs::write(
        theme.join("templates/wide.propel"),
        "{{-- Template Name: Full width --}}\nwide",
    )
    .unwrap();
    std::fs::write(theme.join("templates/plain.propel"), "plain").unwrap();
    std::fs::write(theme.join("theme.toml"), "name = \"Test\"\n").unwrap();
    let site = site_with(config_with_themes(themes.path().to_path_buf())).await;

    for template in ["templates/wide", "templates/wide.php", "default", ""] {
        let (status, json) = body(
            site.post(
                "/mc-json/mc/v2/pages",
                Some("editor"),
                json!({"title": "Templated", "template": template}),
            )
            .await,
        )
        .await;
        assert_eq!(status, Status::Created, "{template}: {json}");
    }
    for (uri, template) in [
        // A file the theme has, but does not declare.
        ("/mc-json/mc/v2/pages", "templates/plain"),
        // Page templates are for pages.
        ("/mc-json/mc/v2/posts", "templates/wide"),
    ] {
        assert_error(
            site.post(
                uri,
                Some("editor"),
                json!({"title": "x", "template": template}),
            )
            .await,
            Status::BadRequest,
            "rest_invalid_param",
        )
        .await;
    }
    // An imported value the theme does not declare stays when it is sent back unchanged.
    let page = site.published("editor", "page", "Imported").await;
    set_post_meta(site.db(), page.id, "_wp_page_template", &json!("old.php"))
        .await
        .unwrap();
    let (status, json) = body(
        site.post(
            &format!("/mc-json/mc/v2/pages/{}", page.id),
            Some("editor"),
            json!({"title": "Imported, edited", "template": "old.php"}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["template"], json!("old.php"));
}
