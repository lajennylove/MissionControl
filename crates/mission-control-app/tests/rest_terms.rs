//! The terms controller (Task 10.3): `/wp-json/wp/v2/categories` and `/tags`, as WordPress's
//! `WP_REST_Terms_Controller` answers them.
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
use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, set_option,
};
use mission_control_core::migrations::migrator;
use mission_control_core::rest::RestRoutes;
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

/// The keys of a category in `context=view`, as WordPress 6.7's `wp/v2/categories` gives them.
const CATEGORY_KEYS: &[&str] = &[
    "id",
    "count",
    "description",
    "link",
    "name",
    "slug",
    "taxonomy",
    "parent",
    "meta",
    "_links",
];

/// The keys of a tag in `context=view`, as WordPress 6.7's `wp/v2/tags` gives them: no `parent`.
const TAG_KEYS: &[&str] = &[
    "id",
    "count",
    "description",
    "link",
    "name",
    "slug",
    "taxonomy",
    "meta",
    "_links",
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

    async fn send(
        &self,
        method: &str,
        uri: &str,
        as_user: Option<&str>,
        body: Option<Value>,
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
        if let Some(body) = body {
            request = request.header(ContentType::JSON).body(body.to_string());
        }
        request.dispatch().await
    }

    async fn post(&self, uri: &str, as_user: Option<&str>, body: Value) -> LocalResponse<'_> {
        self.send("POST", uri, as_user, Some(body)).await
    }

    async fn category(&self, name: &str, parent: Option<&Term>) -> Term {
        Term::create(
            self.db(),
            &Taxonomy::CATEGORY,
            NewTerm {
                parent_id: parent.map(|p| p.term_id),
                ..NewTerm::new(name)
            },
        )
        .await
        .unwrap_or_else(|e| panic!("creates the category {name}: {e}"))
    }

    async fn tag(&self, name: &str) -> Term {
        Term::create(self.db(), &Taxonomy::POST_TAG, NewTerm::new(name))
            .await
            .unwrap_or_else(|e| panic!("creates the tag {name}: {e}"))
    }

    /// A post by the author, in `status`, with `terms`.
    async fn post_with(&self, status: PostStatus, terms: &[(&Taxonomy, &Term)]) -> Post {
        let post = Post::create(
            self.db(),
            NewPost {
                status,
                title: "A post".to_owned(),
                ..NewPost::new(self.users["author"].id, "post")
            },
        )
        .await
        .expect("creates the post");
        for (taxonomy, term) in terms {
            post.attach_terms(self.db(), taxonomy, &[term.term_taxonomy_id])
                .await
                .expect("gives the post the term");
        }
        post
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

/// A site with an administrator, an editor, an author and a contributor, pretty permalinks, the
/// address [`HOME`], and a default category, `Uncategorized`.
async fn site() -> (Site, Term) {
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
        ("gmt_offset", json!("0")),
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
    let user_roles: Arc<dyn UserRoles> = Arc::new(roles);
    let options = AppOptions {
        database: Some(test.shared()),
        user_roles: Some(user_roles),
        ..AppOptions::default()
    };
    let client = Client::tracked(rocket_with_options(config(), options))
        .await
        .expect("the app ignites");
    (
        Site {
            test,
            client,
            users,
        },
        uncategorized,
    )
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

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap_or_else(|| panic!("an array: {list}"))
        .iter()
        .map(|item| item["name"].as_str().expect("a name").to_owned())
        .collect()
}

#[rocket::async_test]
async fn a_category_and_a_tag_have_wordpresss_keys() {
    let (site, _) = site().await;
    let news = site.category("News", None).await;
    let local = site.category("Local", Some(&news)).await;
    let rust = site.tag("Rust").await;
    site.post_with(PostStatus::Publish, &[(&Taxonomy::CATEGORY, &local)])
        .await;

    let (status, json) = body(
        site.get(
            &format!("/wp-json/wp/v2/categories/{}", local.term_id),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(keys(&json), set(CATEGORY_KEYS));
    assert_eq!(json["id"], json!(local.term_id));
    assert_eq!(json["count"], json!(1));
    assert_eq!(json["name"], json!("Local"));
    assert_eq!(json["slug"], json!("local"));
    assert_eq!(json["taxonomy"], json!("category"));
    assert_eq!(json["parent"], json!(news.term_id));
    assert_eq!(json["meta"], json!({}));
    assert_eq!(json["link"], json!(format!("{HOME}/category/news/local/")));
    let links = &json["_links"];
    assert_eq!(
        keys(links),
        set(&[
            "self",
            "collection",
            "about",
            "up",
            "wp:post_type",
            "curies"
        ])
    );
    assert_eq!(
        links["self"][0]["href"],
        json!(format!("{HOME}/wp-json/wp/v2/categories/{}", local.term_id))
    );
    assert_eq!(links["self"][0]["targetHints"]["allow"], json!(["GET"]));
    assert_eq!(
        links["up"][0],
        json!({"embeddable": true, "href": format!("{HOME}/wp-json/wp/v2/categories/{}", news.term_id)})
    );
    assert_eq!(
        links["about"][0]["href"],
        json!(format!("{HOME}/wp-json/wp/v2/taxonomies/category"))
    );
    assert_eq!(
        links["wp:post_type"][0]["href"],
        json!(format!(
            "{HOME}/wp-json/wp/v2/posts?categories={}",
            local.term_id
        ))
    );

    let (status, json) = body(
        site.get(&format!("/wp-json/wp/v2/tags/{}", rust.term_id), None)
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(keys(&json), set(TAG_KEYS));
    assert_eq!(json["taxonomy"], json!("post_tag"));
    assert_eq!(json["link"], json!(format!("{HOME}/tag/rust/")));
    assert_eq!(
        json["_links"]["wp:post_type"][0]["href"],
        json!(format!("{HOME}/wp-json/wp/v2/posts?tags={}", rust.term_id))
    );

    // The embed context keeps WordPress's embed fields; the edit context adds nothing.
    let (_, json) = body(
        site.get(
            &format!("/wp-json/wp/v2/tags/{}?context=embed", rust.term_id),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        keys(&json),
        set(&["id", "link", "name", "slug", "taxonomy", "_links"])
    );
    let (status, json) = body(
        site.get(
            &format!("/wp-json/wp/v2/tags/{}?context=edit", rust.term_id),
            Some("editor"),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(keys(&json), set(TAG_KEYS));
    assert_eq!(
        json["_links"]["self"][0]["targetHints"]["allow"],
        json!(["GET", "POST", "PUT", "PATCH", "DELETE"])
    );
}

#[rocket::async_test]
async fn the_edit_context_and_missing_terms_are_refused() {
    let (site, _) = site().await;
    let rust = site.tag("Rust").await;
    let news = site.category("News", None).await;
    let uri = format!("/wp-json/wp/v2/tags/{}?context=edit", rust.term_id);
    assert_error(
        site.get(&uri, None).await,
        Status::Unauthorized,
        "rest_forbidden_context",
    )
    .await;
    assert_error(
        site.get(&uri, Some("author")).await,
        Status::Forbidden,
        "rest_forbidden_context",
    )
    .await;
    assert_error(
        site.get("/wp-json/wp/v2/categories?context=edit", Some("author"))
            .await,
        Status::Forbidden,
        "rest_forbidden_context",
    )
    .await;
    // A category is not a tag, and an id nobody has is no term.
    assert_error(
        site.get(&format!("/wp-json/wp/v2/tags/{}", news.term_id), None)
            .await,
        Status::NotFound,
        "rest_term_invalid",
    )
    .await;
    assert_error(
        site.get("/wp-json/wp/v2/categories/999999", None).await,
        Status::NotFound,
        "rest_term_invalid",
    )
    .await;
    // A base nobody serves is still no route.
    assert_error(
        site.get("/wp-json/wp/v2/genres", None).await,
        Status::NotFound,
        "rest_no_route",
    )
    .await;
}

#[rocket::async_test]
async fn lists_are_ordered_by_name_with_pagination_headers() {
    let (site, uncategorized) = site().await;
    for name in ["banana", "Apple", "cherry"] {
        site.tag(name).await;
    }
    let response = site.get("/wp-json/wp/v2/tags", None).await;
    assert_eq!(header(&response, "X-WP-Total"), Some("3"));
    assert_eq!(header(&response, "X-WP-TotalPages"), Some("1"));
    let (status, json) = body(response).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(names(&json), ["Apple", "banana", "cherry"]);
    for item in json.as_array().unwrap() {
        assert_eq!(keys(item), set(TAG_KEYS));
    }

    let response = site
        .get("/wp-json/wp/v2/tags?per_page=2&page=2&order=desc", None)
        .await;
    assert_eq!(header(&response, "X-WP-Total"), Some("3"));
    assert_eq!(header(&response, "X-WP-TotalPages"), Some("2"));
    let links: Vec<&str> = response.headers().get("Link").collect();
    assert_eq!(
        links,
        [format!(
            "<{HOME}/wp-json/wp/v2/tags?per_page=2&page=1&order=desc>; rel=\"prev\""
        )]
    );
    let (_, json) = body(response).await;
    assert_eq!(names(&json), ["Apple"]);

    // A page past the end is an empty list, not an error, for terms.
    let (status, json) = body(site.get("/wp-json/wp/v2/tags?page=9", None).await).await;
    assert_eq!((status, json), (Status::Ok, json!([])));

    // `search`, `slug`, `include` with its order, `exclude` and `offset` (flat taxonomies).
    let (_, json) = body(site.get("/wp-json/wp/v2/tags?search=AN", None).await).await;
    assert_eq!(names(&json), ["banana"]);
    let (_, json) = body(
        site.get("/wp-json/wp/v2/tags?slug=cherry,apple", None)
            .await,
    )
    .await;
    assert_eq!(names(&json), ["Apple", "cherry"]);
    let (_, json) = body(
        site.get(
            "/wp-json/wp/v2/tags?slug[]=cherry&slug[]=apple&orderby=include_slugs",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(names(&json), ["cherry", "Apple"]);
    let (_, json) = body(site.get("/wp-json/wp/v2/tags?offset=1", None).await).await;
    assert_eq!(names(&json), ["banana", "cherry"]);
    let (_, json) = body(
        site.get(
            &format!(
                "/wp-json/wp/v2/categories?exclude={}",
                uncategorized.term_id
            ),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(json, json!([]));

    assert_error(
        site.get("/wp-json/wp/v2/tags?per_page=101", None).await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
    assert_error(
        site.get("/wp-json/wp/v2/tags?orderby=date", None).await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
}

#[rocket::async_test]
async fn hide_empty_and_post_filter_the_list() {
    let (site, uncategorized) = site().await;
    let news = site.category("News", None).await;
    let local = site.category("Local", Some(&news)).await;
    let sport = site.category("Sport", None).await;
    let rust = site.tag("Rust").await;
    let go = site.tag("Go").await;
    let published = site
        .post_with(
            PostStatus::Publish,
            &[(&Taxonomy::CATEGORY, &local), (&Taxonomy::POST_TAG, &rust)],
        )
        .await;
    let draft = site
        .post_with(
            PostStatus::Draft,
            &[(&Taxonomy::CATEGORY, &sport), (&Taxonomy::POST_TAG, &go)],
        )
        .await;

    // A draft is not counted, so Sport and Go are empty. News is empty but keeps its place: a
    // category under it has posts, as `get_terms` keeps it.
    let response = site
        .get("/wp-json/wp/v2/categories?hide_empty=true", None)
        .await;
    assert_eq!(header(&response, "X-WP-Total"), Some("2"));
    let (_, json) = body(response).await;
    assert_eq!(names(&json), ["Local", "News"]);
    let (_, json) = body(site.get("/wp-json/wp/v2/tags?hide_empty=1", None).await).await;
    assert_eq!(names(&json), ["Rust"]);
    let (_, json) = body(site.get("/wp-json/wp/v2/categories?parent=0", None).await).await;
    assert_eq!(names(&json), ["News", "Sport", "Uncategorized"]);
    let (_, json) = body(
        site.get(
            &format!("/wp-json/wp/v2/categories?parent={}", news.term_id),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(names(&json), ["Local"]);
    // `parent` is a parameter of hierarchical taxonomies only: tags ignore it.
    let (_, json) = body(site.get("/wp-json/wp/v2/tags?parent=5", None).await).await;
    assert_eq!(names(&json), ["Go", "Rust"]);

    let response = site
        .get(&format!("/wp-json/wp/v2/tags?post={}", published.id), None)
        .await;
    assert_eq!(header(&response, "X-WP-Total"), Some("1"));
    let (_, json) = body(response).await;
    assert_eq!(names(&json), ["Rust"]);
    let (_, json) = body(
        site.get(
            &format!("/wp-json/wp/v2/categories?post={}", published.id),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(names(&json), ["Local"]);

    // A draft's terms are for those who may read it.
    let uri = format!("/wp-json/wp/v2/tags?post={}", draft.id);
    assert_error(
        site.get(&uri, None).await,
        Status::Unauthorized,
        "rest_forbidden_context",
    )
    .await;
    let (status, json) = body(site.get(&uri, Some("author")).await).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(names(&json), ["Go"]);
    // A post that does not exist is WordPress's 400, before any permission check.
    let json = assert_error(
        site.get("/wp-json/wp/v2/tags?post=999999", None).await,
        Status::BadRequest,
        "rest_post_invalid_id",
    )
    .await;
    assert_eq!(json["message"], json!("Invalid post ID."));
    let _ = uncategorized;
}

#[rocket::async_test]
async fn categories_and_tags_are_created() {
    let (site, _) = site().await;
    let response = site
        .post(
            "/wp-json/wp/v2/categories",
            Some("editor"),
            json!({"name": "News", "description": "What happened"}),
        )
        .await;
    assert_eq!(response.status(), Status::Created);
    let location = header(&response, "Location").map(str::to_owned);
    let (_, news) = body(response).await;
    assert_eq!(keys(&news), set(CATEGORY_KEYS));
    assert_eq!(news["name"], json!("News"));
    assert_eq!(news["slug"], json!("news"));
    assert_eq!(news["description"], json!("What happened"));
    assert_eq!(news["parent"], json!(0));
    assert_eq!(news["count"], json!(0));
    let id = news["id"].as_i64().unwrap();
    assert_eq!(
        location,
        Some(format!("{HOME}/wp-json/wp/v2/categories/{id}"))
    );

    // A child category, with its own slug.
    let (status, local) = body(
        site.post(
            "/wp-json/wp/v2/categories",
            Some("editor"),
            json!({"name": "Local", "slug": "Local News", "parent": id}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{local}");
    assert_eq!(local["parent"], json!(id));
    assert_eq!(local["slug"], json!("local-news"));
    assert_eq!(
        local["link"],
        json!(format!("{HOME}/category/news/local-news/"))
    );

    // A tag, as a form: an author may add tags.
    let response = site
        .client
        .post("/wp-json/wp/v2/tags")
        .header(site.auth("author"))
        .header(ContentType::Form)
        .body("name=Rust+lang")
        .dispatch()
        .await;
    let (status, tag) = body(response).await;
    assert_eq!(status, Status::Created, "{tag}");
    assert_eq!(keys(&tag), set(TAG_KEYS));
    assert_eq!(
        (tag["name"].clone(), tag["slug"].clone()),
        (json!("Rust lang"), json!("rust-lang"))
    );

    // Both are listed, and stored.
    let (_, json) = body(site.get("/wp-json/wp/v2/categories", None).await).await;
    assert!(names(&json).contains(&"Local".to_owned()), "{json}");
    assert!(
        Term::find_by_slug(site.db(), &Taxonomy::POST_TAG, "rust-lang")
            .await
            .unwrap()
            .is_some()
    );

    // A parent for a tag, or one that does not exist, is refused.
    assert_error(
        site.post(
            "/wp-json/wp/v2/tags",
            Some("editor"),
            json!({"name": "Go", "parent": id}),
        )
        .await,
        Status::BadRequest,
        "rest_taxonomy_not_hierarchical",
    )
    .await;
    assert_error(
        site.post(
            "/wp-json/wp/v2/categories",
            Some("editor"),
            json!({"name": "Orphan", "parent": 999999}),
        )
        .await,
        Status::BadRequest,
        "rest_term_invalid",
    )
    .await;
    assert_error(
        site.post("/wp-json/wp/v2/tags", Some("editor"), json!({"slug": "go"}))
            .await,
        Status::BadRequest,
        "rest_missing_callback_param",
    )
    .await;
    assert_error(
        site.post("/wp-json/wp/v2/tags", Some("editor"), json!({"name": "  "}))
            .await,
        Status::BadRequest,
        "empty_term_name",
    )
    .await;
    assert_error(
        site.post("/wp-json/wp/v2/tags", Some("editor"), json!({"name": 12}))
            .await,
        Status::BadRequest,
        "rest_invalid_param",
    )
    .await;
}

#[rocket::async_test]
async fn a_duplicate_is_term_exists_with_its_id() {
    let (site, _) = site().await;
    let news = site.category("News", None).await;
    let rust = site.tag("Rust").await;
    let json = assert_error(
        site.post(
            "/wp-json/wp/v2/categories",
            Some("admin"),
            json!({"name": "news"}),
        )
        .await,
        Status::BadRequest,
        "term_exists",
    )
    .await;
    assert_eq!(json["data"]["term_id"], json!(news.term_id));
    assert_eq!(
        json["message"],
        json!("A term with the name provided already exists with this parent.")
    );
    let json = assert_error(
        site.post(
            "/wp-json/wp/v2/tags",
            Some("admin"),
            json!({"name": "Rust"}),
        )
        .await,
        Status::BadRequest,
        "term_exists",
    )
    .await;
    assert_eq!(json["data"]["term_id"], json!(rust.term_id));
    // The same name under another parent is a new category.
    let (status, json) = body(
        site.post(
            "/wp-json/wp/v2/categories",
            Some("admin"),
            json!({"name": "News", "parent": news.term_id}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["slug"], json!("news-news"));
}

#[rocket::async_test]
async fn writes_need_the_taxonomys_capabilities() {
    let (site, _) = site().await;
    // An author may not create categories; an editor may.
    assert_error(
        site.post(
            "/wp-json/wp/v2/categories",
            Some("author"),
            json!({"name": "News"}),
        )
        .await,
        Status::Forbidden,
        "rest_cannot_create",
    )
    .await;
    assert_error(
        site.post("/wp-json/wp/v2/categories", None, json!({"name": "News"}))
            .await,
        Status::Unauthorized,
        "rest_cannot_create",
    )
    .await;
    let response = site
        .post(
            "/wp-json/wp/v2/categories",
            Some("editor"),
            json!({"name": "News"}),
        )
        .await;
    assert_eq!(response.status(), Status::Created);
    // A contributor may tag posts, so may add tags, as WordPress's `assign_terms`.
    let response = site
        .post(
            "/wp-json/wp/v2/tags",
            Some("contributor"),
            json!({"name": "Rust"}),
        )
        .await;
    assert_eq!(response.status(), Status::Created);

    let rust = Term::find_by_slug(site.db(), &Taxonomy::POST_TAG, "rust")
        .await
        .unwrap()
        .unwrap();
    let uri = format!("/wp-json/wp/v2/tags/{}", rust.term_id);
    assert_error(
        site.post(&uri, Some("author"), json!({"name": "Rust!"}))
            .await,
        Status::Forbidden,
        "rest_cannot_update",
    )
    .await;
    assert_error(
        site.send("DELETE", &format!("{uri}?force=true"), Some("author"), None)
            .await,
        Status::Forbidden,
        "rest_cannot_delete",
    )
    .await;
}

#[rocket::async_test]
async fn updates_change_the_term() {
    let (site, _) = site().await;
    let news = site.category("News", None).await;
    let sport = site.category("Sport", None).await;
    let uri = format!("/wp-json/wp/v2/categories/{}", sport.term_id);
    for method in ["POST", "PUT", "PATCH"] {
        let (status, json) = body(
            site.send(
                method,
                &uri,
                Some("editor"),
                Some(json!({"description": format!("By {method}")})),
            )
            .await,
        )
        .await;
        assert_eq!(status, Status::Ok, "{method}: {json}");
        assert_eq!(json["description"], json!(format!("By {method}")));
    }
    let (status, json) = body(
        site.post(
            &uri,
            Some("editor"),
            json!({"name": "Sports", "slug": "", "parent": news.term_id}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(
        (&json["name"], &json["slug"], &json["parent"]),
        (&json!("Sports"), &json!("sports"), &json!(news.term_id))
    );
    // Back to the top level with parent 0.
    let (_, json) = body(site.post(&uri, Some("editor"), json!({"parent": 0})).await).await;
    assert_eq!(json["parent"], json!(0));

    let json = assert_error(
        site.post(&uri, Some("editor"), json!({"slug": "news"}))
            .await,
        Status::BadRequest,
        "duplicate_term_slug",
    )
    .await;
    assert_eq!(
        json["message"],
        json!("The slug &#8220;news&#8221; is already in use by another term.")
    );
    assert_error(
        site.post(&uri, Some("editor"), json!({"parent": 999999}))
            .await,
        Status::BadRequest,
        "rest_term_invalid",
    )
    .await;
    assert_error(
        site.post(
            "/wp-json/wp/v2/categories/999999",
            Some("editor"),
            json!({"name": "x"}),
        )
        .await,
        Status::NotFound,
        "rest_term_invalid",
    )
    .await;
}

#[rocket::async_test]
async fn names_and_descriptions_are_sanitized_for_everyone() {
    let (site, _) = site().await;
    // As `pre_term_name` and `pre_term_description`, for an administrator too: the name is one
    // line of text, the description keeps only `$allowedtags`.
    let (status, json) = body(
        site.post(
            "/wp-json/wp/v2/categories",
            Some("admin"),
            json!({
                "name": " <b>Big</b>\n news<script>alert(1)</script> ",
                "description": "<strong>Ok</strong><script>alert(1)</script><p>para</p>",
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["name"], json!("Big news"));
    assert_eq!(json["slug"], json!("big-news"));
    assert_eq!(json["description"], json!("<strong>Ok</strong>para"));
    assert_error(
        site.post(
            "/wp-json/wp/v2/categories",
            Some("admin"),
            json!({"name": "<em></em>"}),
        )
        .await,
        Status::BadRequest,
        "invalid_term_name",
    )
    .await;
}

#[rocket::async_test]
async fn deletes_need_force_and_spare_the_default_category() {
    let (site, uncategorized) = site().await;
    let news = site.category("News", None).await;
    let post = site
        .post_with(PostStatus::Publish, &[(&Taxonomy::CATEGORY, &news)])
        .await;
    let uri = format!("/wp-json/wp/v2/categories/{}", news.term_id);
    assert_error(
        site.send("DELETE", &uri, Some("editor"), None).await,
        Status::NotImplemented,
        "rest_trash_not_supported",
    )
    .await;
    let (status, json) = body(
        site.send("DELETE", &format!("{uri}?force=true"), Some("editor"), None)
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(json["deleted"], json!(true));
    assert_eq!(json["previous"]["id"], json!(news.term_id));
    assert_eq!(keys(&json["previous"]), set(CATEGORY_KEYS));
    assert_error(
        site.get(&uri, None).await,
        Status::NotFound,
        "rest_term_invalid",
    )
    .await;
    // The post left with no category is in the default one.
    let terms = post.terms(site.db(), &Taxonomy::CATEGORY).await.unwrap();
    assert_eq!(terms[0].term_id, uncategorized.term_id);

    // Nobody may delete the default category, an administrator included.
    let default_uri = format!(
        "/wp-json/wp/v2/categories/{}?force=true",
        uncategorized.term_id
    );
    assert_error(
        site.send("DELETE", &default_uri, Some("admin"), None).await,
        Status::Forbidden,
        "rest_cannot_delete",
    )
    .await;
    let (_, json) = body(
        site.get(
            &format!(
                "/wp-json/wp/v2/categories/{}?context=edit",
                uncategorized.term_id
            ),
            Some("admin"),
        )
        .await,
    )
    .await;
    assert_eq!(
        json["_links"]["self"][0]["targetHints"]["allow"],
        json!(["GET", "POST", "PUT", "PATCH"])
    );

    // A term named by `default_term_{taxonomy}` is a default term too.
    let pinned = site.tag("Pinned").await;
    set_option(site.db(), "default_term_post_tag", &json!(pinned.term_id))
        .await
        .unwrap();
    assert_error(
        site.send(
            "DELETE",
            &format!("/wp-json/wp/v2/tags/{}?force=true", pinned.term_id),
            Some("admin"),
            None,
        )
        .await,
        Status::Forbidden,
        "rest_cannot_delete",
    )
    .await;

    // A tag goes with `force` given as a body parameter too.
    let rust = site.tag("Rust").await;
    let (status, _) = body(
        site.send(
            "DELETE",
            &format!("/wp-json/wp/v2/tags/{}", rust.term_id),
            Some("editor"),
            Some(json!({"force": true})),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Ok);
}

#[rocket::async_test]
async fn fields_and_embed_shape_the_response() {
    let (site, _) = site().await;
    let news = site.category("News", None).await;
    let local = site.category("Local", Some(&news)).await;
    let uri = format!("/wp-json/wp/v2/categories/{}", local.term_id);
    let (_, json) = body(site.get(&format!("{uri}?_fields=id,name"), None).await).await;
    assert_eq!(json, json!({"id": local.term_id, "name": "Local"}));
    let (_, json) = body(site.get(&format!("{uri}?_embed"), None).await).await;
    let up = &json["_embedded"]["up"][0];
    assert_eq!(up["id"], json!(news.term_id));
    assert_eq!(
        keys(up),
        set(&["id", "link", "name", "slug", "taxonomy", "_links"])
    );
    // No parent, nothing embedded.
    let (_, json) = body(
        site.get(
            &format!("/wp-json/wp/v2/categories/{}?_embed", news.term_id),
            None,
        )
        .await,
    )
    .await;
    assert!(json.get("_embedded").is_none(), "{json}");
}

#[rocket::async_test]
async fn discovery_lists_the_taxonomies_served() {
    let (site, _) = site().await;
    let (_, json) = body(site.get("/wp-json/wp/v2", None).await).await;
    let routes = json["routes"].as_object().expect("routes");
    for path in [
        "/wp/v2/categories",
        "/wp/v2/categories/(?P<id>[\\d]+)",
        "/wp/v2/tags",
        "/wp/v2/tags/(?P<id>[\\d]+)",
    ] {
        assert!(routes.contains_key(path), "{path}");
    }
    assert_eq!(
        routes["/wp/v2/tags"]["endpoints"][1]["args"]["name"]["required"],
        json!(true)
    );

    // A taxonomy registered with `show_in_rest` off is not served.
    let taxonomies = site
        .client
        .rocket()
        .state::<Arc<Taxonomies>>()
        .expect("the app manages its taxonomies");
    let registry = site
        .client
        .rocket()
        .state::<Arc<RestRoutes>>()
        .expect("the app manages its routes");
    taxonomies.register(TaxonomySpec {
        show_in_rest: false,
        ..TaxonomySpec::post_tag()
    });
    taxonomies.register_routes(registry);
    assert_error(
        site.get("/wp-json/wp/v2/tags", None).await,
        Status::NotFound,
        "rest_no_route",
    )
    .await;
}

#[rocket::async_test]
async fn a_description_over_64_kb_round_trips_on_every_database() {
    let (site, _) = site().await;
    // Over MySQL's 65,535-byte `TEXT`: the column is `LONGTEXT` there, as in WordPress.
    let description = "a".repeat(70_000);
    let (status, json) = body(
        site.post(
            "/wp-json/wp/v2/tags",
            Some("admin"),
            json!({"name": "Big", "description": description}),
        )
        .await,
    )
    .await;
    assert_eq!(status, Status::Created, "{json}");
    assert_eq!(json["description"], json!(description));
    let id = json["id"].as_i64().unwrap();
    let (status, json) = body(site.get(&format!("/wp-json/wp/v2/tags/{id}"), None).await).await;
    assert_eq!(status, Status::Ok);
    assert_eq!(json["description"], json!(description));
}
