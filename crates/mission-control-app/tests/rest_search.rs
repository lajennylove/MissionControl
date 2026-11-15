//! The search route (Task 10.8): `/wp-json/wp/v2/search`, as WordPress's
//! `WP_REST_Search_Controller` answers it.
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
use mission_control_core::users::app_password::{self, PlainPassword};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, InMemoryUserRoles, RoleAssignment, UserRoles,
};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use rocket::http::{Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use serde_json::{Value, json};

/// A result's keys, as WordPress 6.7's `wp/v2/search` gives them.
const RESULT_KEYS: &[&str] = &["id", "title", "url", "type", "subtype", "_links"];

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

/// The content the searches find.
struct Content {
    launch: Post,
    fuel: Post,
    page: Post,
    rocketry: Term,
    rockets: Term,
}

impl Site {
    fn db(&self) -> &Database {
        self.test.db()
    }

    async fn get(&self, uri: &str, as_user: Option<&str>) -> LocalResponse<'_> {
        let mut request = self.client.get(uri.to_owned());
        if let Some(name) = as_user {
            let user = &self.users[name];
            request = request.header(Header::new(
                "Authorization",
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{}:{}", user.email, user.password.as_str()))
                ),
            ));
        }
        request.dispatch().await
    }

    async fn post(
        &self,
        post_type: &str,
        status: PostStatus,
        title: &str,
        password: Option<&str>,
    ) -> Post {
        Post::create(
            self.db(),
            NewPost {
                status,
                title: title.to_owned(),
                content: format!("All about {title}."),
                password: password.map(str::to_owned),
                ..NewPost::new(self.users["author"].id, post_type)
            },
        )
        .await
        .unwrap_or_else(|e| panic!("creates {title}: {e}"))
    }

    /// Published posts, a protected post, a page, a draft, a category and a tag.
    async fn content(&self) -> Content {
        let launch = self
            .post("post", PostStatus::Publish, "Rocket launch", None)
            .await;
        let fuel = self
            .post("post", PostStatus::Publish, "Rocket fuel", Some("secret"))
            .await;
        self.post("post", PostStatus::Publish, "Garden notes", None)
            .await;
        self.post("post", PostStatus::Draft, "Rocket draft", None)
            .await;
        self.post("post", PostStatus::Private, "Rocket private", None)
            .await;
        let page = self
            .post("page", PostStatus::Publish, "Rocket page", None)
            .await;
        let rocketry = Term::create(self.db(), &Taxonomy::CATEGORY, NewTerm::new("Rocketry"))
            .await
            .expect("creates the category");
        let rockets = Term::create(self.db(), &Taxonomy::POST_TAG, NewTerm::new("rockets"))
            .await
            .expect("creates the tag");
        Term::create(self.db(), &Taxonomy::POST_TAG, NewTerm::new("gardening"))
            .await
            .expect("creates the tag");
        Content {
            launch,
            fuel,
            page,
            rocketry,
            rockets,
        }
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

/// A site with an administrator and an author, pretty permalinks and the address [`HOME`].
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
        ("gmt_offset", json!("0")),
    ] {
        set_option(db, name, &value).await.expect("sets the option");
    }
    let roles = InMemoryUserRoles::new();
    let mut users = BTreeMap::new();
    for (name, role) in [("admin", ADMINISTRATOR), ("author", AUTHOR)] {
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
    Site {
        test,
        client,
        users,
    }
}

/// A response's status, total headers and body.
async fn answer(response: LocalResponse<'_>) -> (Status, Option<String>, Option<String>, Value) {
    let status = response.status();
    let total = response.headers().get_one("X-WP-Total").map(str::to_owned);
    let pages = response
        .headers()
        .get_one("X-WP-TotalPages")
        .map(str::to_owned);
    let text = response.into_string().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{status}: {e}: {text}"));
    (status, total, pages, json)
}

/// A successful search's results.
async fn results(response: LocalResponse<'_>) -> Vec<Value> {
    let (status, _, _, json) = answer(response).await;
    assert_eq!(status, Status::Ok, "{json}");
    json.as_array()
        .unwrap_or_else(|| panic!("a list: {json}"))
        .clone()
}

fn ids(results: &[Value]) -> BTreeSet<i64> {
    results.iter().map(|r| r["id"].as_i64().unwrap()).collect()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

fn set(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[rocket::async_test]
async fn posts_are_found_with_wordpresss_keys() {
    let site = site().await;
    let content = site.content().await;
    let (status, total, pages, json) =
        answer(site.get("/wp-json/wp/v2/search?search=rocket", None).await).await;
    assert_eq!(status, Status::Ok, "{json}");
    let found = json.as_array().unwrap();
    // The draft and the private post are not published; a visitor's search never matches the
    // protected post.
    assert_eq!(ids(found), [content.launch.id, content.page.id].into());
    assert_eq!((total.as_deref(), pages.as_deref()), (Some("2"), Some("1")));
    for result in found {
        assert_eq!(keys(result), set(RESULT_KEYS));
        assert_eq!(result["type"], "post");
    }
    let launch = found.iter().find(|r| r["id"] == content.launch.id).unwrap();
    assert_eq!(launch["title"], "Rocket launch");
    assert_eq!(launch["subtype"], "post");
    assert_eq!(launch["url"], format!("{HOME}/rocket-launch/"));
    assert_eq!(
        launch["_links"]["self"][0],
        json!({"embeddable": true, "href": format!("{HOME}/wp-json/wp/v2/posts/{}", content.launch.id)})
    );
    assert_eq!(
        launch["_links"]["about"][0]["href"],
        format!("{HOME}/wp-json/wp/v2/types/post")
    );
    assert_eq!(
        launch["_links"]["collection"][0]["href"],
        format!("{HOME}/wp-json/wp/v2/search")
    );
    let page = found.iter().find(|r| r["id"] == content.page.id).unwrap();
    assert_eq!(page["subtype"], "page");

    // Signed in, the protected post matches, with its title marked.
    let signed_in = results(
        site.get("/wp-json/wp/v2/search?search=rocket", Some("admin"))
            .await,
    )
    .await;
    assert_eq!(
        ids(&signed_in),
        [content.launch.id, content.page.id, content.fuel.id].into()
    );
    let fuel = signed_in
        .iter()
        .find(|r| r["id"] == content.fuel.id)
        .unwrap();
    assert_eq!(fuel["title"], "Protected: Rocket fuel");

    // With no search, a visitor sees every published post, the protected one too.
    let all = results(site.get("/wp-json/wp/v2/search", None).await).await;
    assert_eq!(all.len(), 4);
}

#[rocket::async_test]
async fn subtypes_include_and_exclude_narrow_the_search() {
    let site = site().await;
    let content = site.content().await;
    let pages = results(
        site.get("/wp-json/wp/v2/search?search=rocket&subtype=page", None)
            .await,
    )
    .await;
    assert_eq!(ids(&pages), [content.page.id].into());
    let any = results(
        site.get("/wp-json/wp/v2/search?search=rocket&subtype[]=any", None)
            .await,
    )
    .await;
    assert_eq!(any.len(), 2);
    let excluded = results(
        site.get(
            &format!(
                "/wp-json/wp/v2/search?search=rocket&exclude={}",
                content.page.id
            ),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(ids(&excluded), [content.launch.id].into());
    let included = results(
        site.get(
            &format!("/wp-json/wp/v2/search?include={}", content.page.id),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(ids(&included), [content.page.id].into());
    let fields = results(
        site.get("/wp-json/wp/v2/search?search=launch&_fields=id,title", None)
            .await,
    )
    .await;
    assert_eq!(
        fields,
        [json!({"id": content.launch.id, "title": "Rocket launch"})]
    );
}

#[rocket::async_test]
async fn terms_are_found_by_name_or_slug() {
    let site = site().await;
    let content = site.content().await;
    let (status, total, _, json) = answer(
        site.get("/wp-json/wp/v2/search?type=term&search=rocket", None)
            .await,
    )
    .await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!(total.as_deref(), Some("2"));
    let found = json.as_array().unwrap();
    assert_eq!(
        ids(found),
        [content.rocketry.term_id, content.rockets.term_id].into()
    );
    // Ordered by name.
    assert_eq!(found[0]["title"], "Rocketry");
    assert_eq!(found[1]["title"], "rockets");
    let category = &found[0];
    assert_eq!(keys(category), set(RESULT_KEYS));
    assert_eq!(category["type"], "term");
    assert_eq!(category["subtype"], "category");
    assert_eq!(category["url"], format!("{HOME}/category/rocketry/"));
    assert_eq!(
        category["_links"]["self"][0]["href"],
        format!(
            "{HOME}/wp-json/wp/v2/categories/{}",
            content.rocketry.term_id
        )
    );
    assert_eq!(
        category["_links"]["about"][0]["href"],
        format!("{HOME}/wp-json/wp/v2/taxonomies/category")
    );
    let tags = results(
        site.get("/wp-json/wp/v2/search?type=term&subtype=post_tag", None)
            .await,
    )
    .await;
    assert_eq!(tags.len(), 2);
    assert!(tags.iter().all(|t| t["subtype"] == "post_tag"));

    let formats = results(
        site.get("/wp-json/wp/v2/search?type=post-format", None)
            .await,
    )
    .await;
    assert!(formats.is_empty());
}

#[rocket::async_test]
async fn search_pages_through_the_results() {
    let site = site().await;
    let content = site.content().await;
    let response = site
        .get(
            "/wp-json/wp/v2/search?search=rocket&per_page=1&page=2",
            None,
        )
        .await;
    let link = response.headers().get_one("Link").map(str::to_owned);
    let (status, total, pages, json) = answer(response).await;
    assert_eq!(status, Status::Ok, "{json}");
    assert_eq!((total.as_deref(), pages.as_deref()), (Some("2"), Some("2")));
    // The two results, one on each page.
    let first = results(
        site.get("/wp-json/wp/v2/search?search=rocket&per_page=1", None)
            .await,
    )
    .await;
    let second = json.as_array().unwrap();
    assert_eq!(
        ids(&[first[0].clone(), second[0].clone()]),
        [content.launch.id, content.page.id].into()
    );
    assert!(
        link.as_deref()
            .is_some_and(|l| l.contains("rel=\"prev\"") && l.contains("page=1")),
        "{link:?}"
    );
    let (status, _, _, error) = answer(
        site.get(
            "/wp-json/wp/v2/search?search=rocket&per_page=1&page=3",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (Status::BadRequest, Some("rest_search_invalid_page_number"))
    );
}

#[rocket::async_test]
async fn bad_parameters_are_refused() {
    let site = site().await;
    for uri in [
        "/wp-json/wp/v2/search?type=user",
        "/wp-json/wp/v2/search?subtype=book",
        "/wp-json/wp/v2/search?context=edit",
        "/wp-json/wp/v2/search?per_page=101",
    ] {
        let (status, _, _, error) = answer(site.get(uri, None).await).await;
        assert_eq!(
            (status, error["code"].as_str()),
            (Status::BadRequest, Some("rest_invalid_param")),
            "{uri}: {error}"
        );
    }
}

#[rocket::async_test]
async fn embed_brings_the_post_or_term() {
    let site = site().await;
    let content = site.content().await;
    let found = results(
        site.get("/wp-json/wp/v2/search?search=launch&_embed", None)
            .await,
    )
    .await;
    let embedded = &found[0]["_embedded"]["self"][0];
    assert_eq!(embedded["id"], content.launch.id);
    assert_eq!(embedded["slug"], "rocket-launch");
    let found = results(
        site.get(
            "/wp-json/wp/v2/search?type=term&search=rocketry&_embed=self",
            None,
        )
        .await,
    )
    .await;
    let embedded = &found[0]["_embedded"]["self"][0];
    assert_eq!(embedded["id"], content.rocketry.term_id);
    assert_eq!(embedded["taxonomy"], "category");
}

#[rocket::async_test]
async fn a_visitor_never_finds_an_unpublished_post_by_id() {
    let site = site().await;
    site.content().await;
    let draft = site
        .post("post", PostStatus::Draft, "Hidden draft", None)
        .await;
    let found = results(
        site.get(&format!("/wp-json/wp/v2/search?include={}", draft.id), None)
            .await,
    )
    .await;
    assert!(found.is_empty());
}

#[rocket::async_test]
async fn no_search_and_no_post_subtype_lists_posts_only() {
    let site = site().await;
    let content = site.content().await;
    // `category` is no post type: with no search, `WP_Query` lists `post` only.
    let found = results(
        site.get("/wp-json/wp/v2/search?subtype=category", None)
            .await,
    )
    .await;
    assert!(!found.is_empty());
    assert!(found.iter().all(|r| r["subtype"] == "post"), "{found:?}");
    assert!(!ids(&found).contains(&content.page.id));
    // With a search, every searchable type.
    let found = results(
        site.get("/wp-json/wp/v2/search?subtype=category&search=rocket", None)
            .await,
    )
    .await;
    assert!(ids(&found).contains(&content.page.id));
    // `search` is sanitized: tags and surrounding space go.
    let found = results(
        site.get(
            "/wp-json/wp/v2/search?search=%20%3Cb%3Elaunch%3C%2Fb%3E%20",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(ids(&found), [content.launch.id].into());
}
