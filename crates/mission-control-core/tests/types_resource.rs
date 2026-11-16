//! Tests for the post type resources (Task 5.3): the admin screens of each post type, as an
//! Ignite runtime resource. They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh
//! SQLite file when it is unset.
//!
//! Ignite's runtime registry is one per process, so each test registers its own slug, and only
//! one test syncs the built-in `post` and `page`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::{Duration, Utc};
use ignite_core::{
    Direction, FieldKindDef, FormInput, ListQuery, Row, RuntimeResource, register_runtime,
    runtime_resource, unregister_runtime,
};
use mission_control_core::content::{
    NewTerm, Post, PostStatus, Taxonomy, Term, get_post_meta, get_post_revisions, set_option,
};
use mission_control_core::migrations::migrator;
use mission_control_core::rest::RestRoutes;
use mission_control_core::rest::meta_routes::MetaRoutesController;
use mission_control_core::theme::PageTemplate;
use mission_control_core::types::{
    FieldDef, FieldGroupDef, LocationOperator, LocationParam, LocationRule, MAX_OPTIONS, Origin,
    PostTypeDef, PostTypeResources, Registry, Support, TaxonomyDef, builtin_post_types,
    create_auto_draft, get_field, post_type_resource, post_type_resource_with_templates,
};
use mission_control_core::users::{ADMINISTRATOR, IgniteUserRoles, UserCaps, UserRoles};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw, schema};
use serde_json::{Value, json};

/// Runs the core migrations and returns the id of a new user.
async fn setup(db: &Database) -> i64 {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    create_role_tables(db).await;
    add_user(db, "ann").await
}

/// Creates Ignite's `roles` and `role_user` tables, as its migration does, unless they exist:
/// the author choices read them (Ruling R121).
async fn create_role_tables(db: &Database) {
    let roles = schema::create_if_not_exists("roles", |t| {
        t.id();
        t.string("name");
        t.unique(["name"]).name("roles_name_unique");
    });
    let role_user = schema::create_if_not_exists("role_user", |t| {
        t.big_integer("user_id");
        t.big_integer("role_id");
        t.primary(["user_id", "role_id"]);
    });
    for table in [roles, role_user] {
        for sql in table.to_sql(db.driver()) {
            raw::execute(db, &sql, &[])
                .await
                .expect("creates the table");
        }
    }
}

/// Gives user `user` the Ignite role `role`, creating the role when it is missing.
async fn grant(db: &Database, user: i64, role: &str) {
    let count: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM roles WHERE name = ?",
        &[role.into()],
    )
    .await
    .unwrap();
    if count == 0 {
        raw::execute(db, "INSERT INTO roles (name) VALUES (?)", &[role.into()])
            .await
            .unwrap();
    }
    let id: i64 = raw::fetch_scalar(db, "SELECT id FROM roles WHERE name = ?", &[role.into()])
        .await
        .unwrap();
    raw::execute(
        db,
        "INSERT INTO role_user (user_id, role_id) VALUES (?, ?)",
        &[user.into(), id.into()],
    )
    .await
    .unwrap();
}

/// Inserts a user and returns its id.
async fn add_user(db: &Database, name: &str) -> i64 {
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &[name.into(), unique_email(name).into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the user id")
}

/// A code-registered post type `slug` with `supports`.
fn post_type(slug: &str, supports: Vec<Support>) -> PostTypeDef {
    PostTypeDef {
        public: true,
        show_in_rest: true,
        supports,
        origin: Origin::Code,
        ..PostTypeDef::new(slug, "Things", "Thing")
    }
}

/// The names of the resource's fields.
fn field_names(resource: &RuntimeResource) -> Vec<&str> {
    resource.fields.iter().map(|f| f.name.as_str()).collect()
}

/// The field `name` of the resource.
fn field<'a>(resource: &'a RuntimeResource, name: &str) -> &'a ignite_core::FieldDef {
    resource
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("the resource has the field `{name}`"))
}

/// Held while a test registers on Ignite's runtime registry, which is one per process.
static IGNITE: Mutex<()> = Mutex::new(());

/// Locks [`IGNITE`]; a test that panicked while holding it left the registry usable.
fn ignite_lock() -> MutexGuard<'static, ()> {
    IGNITE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Registers `resource` on Ignite and returns the row Ignite's `validate_update` makes of
/// `form`, as the panel's update does; unregisters it again.
fn validated(resource: RuntimeResource, form: &[(&str, &str)]) -> Row {
    let _ignite = ignite_lock();
    let slug = resource.slug.clone();
    register_runtime(resource).expect("registers the resource");
    let registered = runtime_resource(&slug).expect("registered");
    let values: HashMap<String, String> = form
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let row = registered
        .validate_update(&FormInput::new(values))
        .expect("the form is valid");
    unregister_runtime(&slug).expect("unregisters the resource");
    row
}

/// The first page of the list, as the panel asks for it.
fn first_page(search: Option<&str>) -> ListQuery {
    ListQuery {
        page: 1,
        per_page: 20,
        search: search.map(str::to_owned),
        sort: None,
    }
}

/// The ids of the rows of a page.
fn ids(rows: &[Row]) -> Vec<i64> {
    rows.iter().map(|r| r["id"].as_i64().unwrap()).collect()
}

#[test]
fn title_and_content_in_supports_make_those_fields() {
    let resource = post_type_resource(
        &post_type("rs_note", vec![Support::Title, Support::Editor]),
        &Registry::default(),
    );
    assert_eq!(resource.slug, "rs_note");
    assert_eq!(resource.title, "Things");
    assert_eq!(resource.singular, "Thing");
    assert_eq!(
        field_names(&resource),
        ["title", "slug", "content", "status", "date", "password"]
    );
    assert_eq!(field(&resource, "title").kind, FieldKindDef::Text);
    assert_eq!(field(&resource, "content").kind, FieldKindDef::Wysiwyg);
    assert!(field(&resource, "status").required);
    let columns: Vec<&str> = resource.columns.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(columns, ["col_title", "col_status", "col_date"]);

    // A type with no supports has the slug, status, date and password only.
    let bare = post_type_resource(&post_type("rs_bare", Vec::new()), &Registry::default());
    assert_eq!(field_names(&bare), ["slug", "status", "date", "password"]);
}

#[test]
fn the_builtin_types_have_wordpress_s_boxes() {
    let [post, page] = builtin_post_types();
    let post = post_type_resource(&post, &Registry::default());
    assert_eq!(
        field_names(&post),
        [
            "title",
            "slug",
            "content",
            "excerpt",
            "author",
            "featured_media",
            "status",
            "date",
            "password",
            "sticky",
            "tax_category",
            "tax_post_tag"
        ]
    );
    assert_eq!(field(&post, "author").kind, FieldKindDef::User);
    assert_eq!(field(&post, "featured_media").kind, FieldKindDef::Image);
    assert_eq!(
        field(&post, "tax_category").kind,
        FieldKindDef::Relationship
    );
    assert_eq!(field(&post, "tax_category").label, "Categories");
    let columns: Vec<&str> = post.columns.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(
        columns,
        [
            "col_title",
            "col_author",
            "col_status",
            "col_tax_category",
            "col_tax_post_tag",
            "col_date"
        ]
    );

    let page = post_type_resource(&page, &Registry::default());
    assert_eq!(
        field_names(&page),
        [
            "title",
            "slug",
            "content",
            "author",
            "parent",
            "menu_order",
            "featured_media",
            "status",
            "date",
            "password"
        ]
    );
    assert_eq!(field(&page, "parent").kind, FieldKindDef::PostObject);
}

#[test]
fn a_type_shown_in_rest_is_listed_by_the_rest_api_and_gets_screens() {
    let _ignite = ignite_lock();
    let registry = Arc::new(Registry::default());
    registry
        .register(post_type("rs_movie", vec![Support::Title]))
        .unwrap();
    registry
        .register(PostTypeDef {
            show_ui: Some(false),
            ..post_type("rs_hidden", vec![Support::Title])
        })
        .unwrap();
    assert!(registry.post_types().by_rest_base("rs_movie").is_some());

    let resources = PostTypeResources::new(Arc::clone(&registry));
    let slugs: Vec<String> = resources.post_types().into_iter().map(|d| d.slug).collect();
    assert_eq!(slugs, ["post", "page", "rs_movie"]);
    assert!(resources.sync().is_empty());
    assert_eq!(resources.registered(), ["post", "page", "rs_movie"]);
    let movie = runtime_resource("rs_movie").expect("the type has screens");
    assert_eq!(movie.title(), "Things");

    // Syncing again is idempotent; a removed type loses its screens.
    assert!(resources.sync().is_empty());
    assert!(resources.remove("rs_movie"));
    assert!(runtime_resource("rs_movie").is_none());
    assert!(!resources.remove("rs_movie"));
    assert!(resources.sync().is_empty());
    assert!(runtime_resource("rs_movie").is_some());

    // A slug Ignite keeps for itself is refused with a value, not a panic.
    ignite_core::reserve_slugs(&["rs_taken"]).unwrap();
    registry
        .register(post_type("rs_taken", vec![Support::Title]))
        .unwrap();
    let refused = resources.sync();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].post_type, "rs_taken");
    assert!(runtime_resource("rs_movie").is_some());

    for slug in resources.registered() {
        assert!(resources.remove(&slug));
    }
    assert!(runtime_resource("post").is_none());
}

#[tokio::test]
async fn create_is_refused_and_an_auto_draft_is_hidden_from_the_list() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let resource = post_type_resource(
        &post_type("rs_draft", vec![Support::Title, Support::Editor]),
        &Registry::default(),
    );
    assert!(
        resource
            .data
            .create(db, Row::new())
            .await
            .is_err_and(|e| matches!(e, orbit_orm::Error::Unsupported { .. }))
    );

    let draft = create_auto_draft(db, "rs_draft", author).await.unwrap();
    assert_eq!(draft.status, PostStatus::AutoDraft);
    let page = resource.data.list(db, &first_page(None)).await.unwrap();
    assert_eq!(page.total, 0);
    assert!(page.rows.is_empty());

    // The New screen shows the auto-draft as a blank draft.
    let row = resource.data.find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(row["status"], "draft");
    assert_eq!(row["title"], "");
    for key in [
        "id",
        "content",
        "date",
        "password",
        "col_title",
        "col_status",
        "col_date",
    ] {
        assert!(row.contains_key(key), "the row has `{key}`");
    }

    // Its first save makes it a draft, listed.
    let input = validated(
        post_type_resource(
            &post_type("rs_draft", vec![Support::Title, Support::Editor]),
            &Registry::default(),
        ),
        &[
            ("title", "First"),
            ("content", "<p>Hi</p>"),
            ("status", "draft"),
            ("date", ""),
        ],
    );
    let saved = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(saved["status"], "draft");
    let page = resource.data.list(db, &first_page(None)).await.unwrap();
    assert_eq!(ids(&page.rows), [draft.id]);
    assert_eq!(page.rows[0]["col_title"], "First");
    assert_eq!(page.rows[0]["col_status"], "Draft");

    // A post of another type is not found.
    let other = create_auto_draft(db, "rs_other", author).await.unwrap();
    assert!(resource.data.find(db, other.id).await.unwrap().is_none());
    assert_eq!(resource.data.delete(db, other.id).await.unwrap(), 0);
}

#[tokio::test]
async fn an_update_saves_the_fields_and_records_a_revision() {
    let test = database().await;
    let db = test.db();
    let ann = setup(db).await;
    let bob = add_user(db, "bob").await;
    grant(db, bob, "author").await;
    let def = &builtin_post_types()[0];
    let resource = post_type_resource(def, &Registry::default());
    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let draft = create_auto_draft(db, "post", ann).await.unwrap();

    let form = |title: &str, categories: &str| {
        let bob = bob.to_string();
        validated(
            post_type_resource(def, &Registry::default()),
            &[
                ("title", title),
                ("content", "<p onclick=\"x()\">Body</p>"),
                ("excerpt", ""),
                ("author", &bob),
                ("status", "publish"),
                ("date", ""),
                ("password", ""),
                ("sticky", "on"),
                ("tax_category", categories),
                ("tax_post_tag", "[]"),
            ],
        )
    };
    let input = form("Hello", &format!("[\"{}\"]", news.term_id));
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["title"], "Hello");
    // Written as sent: the caller is trusted (Ruling R115).
    assert_eq!(row["content"], "<p onclick=\"x()\">Body</p>");
    assert_eq!(row["author"], bob.to_string());
    assert_eq!(row["status"], "publish");
    assert!(row["date"].is_string(), "publishing set the date");
    assert_eq!(row["sticky"], true);
    assert_eq!(row["tax_category"], format!("[\"{}\"]", news.term_id));
    assert_eq!(row["col_author"], "bob");
    assert_eq!(row["col_tax_category"], "News");

    let post = Post::query().find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(post.status, PostStatus::Publish);
    assert_eq!(post.slug, "hello", "the slug comes from the title");
    assert_eq!(post.author_id, bob);
    assert_eq!(get_post_revisions(db, post.id).await.unwrap().len(), 1);
    let terms = post.terms(db, &Taxonomy::CATEGORY).await.unwrap();
    assert_eq!(terms[0].count, 1, "the term was recounted");

    // A save that changes no title, content or excerpt records no revision.
    let row = resource
        .data
        .update(db, draft.id, form("Hello", "[]"))
        .await
        .unwrap();
    assert_eq!(get_post_revisions(db, post.id).await.unwrap().len(), 1);
    // No category: the default category, as `wp_insert_post` gives (none is set here).
    assert_eq!(row["tax_category"], "[]");

    // A new title records a revision, and the published post keeps its old slug.
    resource
        .data
        .update(db, draft.id, form("Renamed", "[]"))
        .await
        .unwrap();
    assert_eq!(get_post_revisions(db, post.id).await.unwrap().len(), 2);
    let post = Post::query().find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(post.slug, "hello", "a published post's slug is kept");

    // The default category, when the site has one.
    set_option(db, "default_category", &json!(news.term_id))
        .await
        .unwrap();
    let row = resource
        .data
        .update(db, draft.id, form("Renamed", "[]"))
        .await
        .unwrap();
    assert_eq!(row["tax_category"], format!("[\"{}\"]", news.term_id));

    // A password-protected post is not sticky.
    let mut input = form("Renamed", "[]");
    input.insert("password".into(), json!("secret"));
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["password"], "secret");
    assert_eq!(row["sticky"], false);

    // The list searches the title, excerpt and content, and sorts by title.
    let page = resource
        .data
        .list(db, &first_page(Some("renamed")))
        .await
        .unwrap();
    assert_eq!(ids(&page.rows), [draft.id]);
    let page = resource
        .data
        .list(db, &first_page(Some("nothing-like-it")))
        .await
        .unwrap();
    assert_eq!(page.total, 0);
    let sorted = ListQuery {
        sort: Some(("col_title".into(), Direction::Desc)),
        ..first_page(None)
    };
    assert_eq!(resource.data.list(db, &sorted).await.unwrap().total, 1);

    // The author and category choices.
    // The author choices are the users who may edit posts (bounded: a shared test database
    // may hold thousands); ann has no role, bob is an author.
    let authors = resource.data.options(db, "author").await.unwrap();
    assert!(authors.len() <= MAX_OPTIONS);
    assert!(!authors.iter().any(|(id, _)| *id == ann.to_string()));
    if authors.len() < MAX_OPTIONS {
        assert!(authors.contains(&(bob.to_string(), "bob".to_owned())));
    }
    let categories = resource.data.options(db, "tax_category").await.unwrap();
    assert_eq!(categories, [(news.term_id.to_string(), "News".to_owned())]);
}

#[tokio::test]
async fn a_scheduled_post_dated_in_the_past_is_published() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let def = post_type("rs_event", vec![Support::Title]);
    let resource = post_type_resource(&def, &Registry::default());
    let draft = create_auto_draft(db, "rs_event", author).await.unwrap();

    let past = (Utc::now() - Duration::days(2))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let input = validated(
        post_type_resource(&def, &Registry::default()),
        &[("title", "Past"), ("status", "future"), ("date", &past)],
    );
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["status"], "publish");

    let ahead = (Utc::now() + Duration::days(2))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let input = validated(
        post_type_resource(&def, &Registry::default()),
        &[("title", "Ahead"), ("status", "publish"), ("date", &ahead)],
    );
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(
        row["status"], "future",
        "a publish dated ahead is scheduled"
    );
}

#[tokio::test]
async fn delete_trashes_restore_untrashes_and_a_second_delete_is_permanent() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let def = post_type("rs_trash", vec![Support::Title, Support::Revisions]);
    let resource = post_type_resource(&def, &Registry::default());
    let draft = create_auto_draft(db, "rs_trash", author).await.unwrap();
    let input = validated(
        post_type_resource(&def, &Registry::default()),
        &[("title", "Keep me"), ("status", "publish"), ("date", "")],
    );
    resource.data.update(db, draft.id, input).await.unwrap();
    set_option(db, "unrelated", &json!(1)).await.unwrap();

    // Delete moves it to the trash; the list still shows it, as Trash.
    assert_eq!(resource.data.delete(db, draft.id).await.unwrap(), 1);
    let post = Post::query().find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(post.status, PostStatus::Trash);
    let page = resource.data.list(db, &first_page(None)).await.unwrap();
    assert_eq!(ids(&page.rows), [draft.id]);
    assert_eq!(page.rows[0]["col_status"], "Trash");
    let row = resource.data.find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(row["status"], "trash");

    // Saving it with another status restores it, then applies the form. The form sends back
    // the trashed slug it showed, which does not replace the slug the restore brings back.
    let trashed_slug = row["slug"].as_str().unwrap().to_owned();
    assert!(trashed_slug.ends_with("__trashed"));
    let input = validated(
        post_type_resource(&def, &Registry::default()),
        &[
            ("title", "Back"),
            ("slug", &trashed_slug),
            ("status", "pending"),
            ("date", ""),
        ],
    );
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["status"], "pending");
    assert_eq!(row["title"], "Back");
    let post = Post::query().find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(
        post.slug, "keep-me",
        "the slug from before the trash is back"
    );
    assert_eq!(
        get_post_meta(db, draft.id, "_wp_trash_meta_status")
            .await
            .unwrap(),
        None
    );

    // Choosing Trash on the form trashes it after the save.
    let input = validated(
        post_type_resource(&def, &Registry::default()),
        &[("title", "Back"), ("status", "trash"), ("date", "")],
    );
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["status"], "trash");

    // Deleting a trashed post deletes it for good, with its revisions and meta.
    assert!(!get_post_revisions(db, draft.id).await.unwrap().is_empty());
    assert_eq!(resource.data.delete(db, draft.id).await.unwrap(), 1);
    assert!(Post::query().find(db, draft.id).await.unwrap().is_none());
    let revisions: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM posts WHERE parent_id = ?",
        &[draft.id.into()],
    )
    .await
    .unwrap();
    assert_eq!(revisions, 0);
    assert_eq!(resource.data.delete(db, draft.id).await.unwrap(), 0);
}

#[tokio::test]
async fn a_field_group_field_is_stored_as_acf_meta_and_read_by_get_field() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let registry = Registry::default();
    let def = post_type("rs_venue", vec![Support::Title]);
    registry.register(def.clone()).unwrap();
    registry
        .register(FieldGroupDef {
            origin: Origin::Code,
            fields: vec![
                FieldDef::new("field_Venue-Name", "venue", "Venue", FieldKindDef::Text),
                FieldDef::new("field_seats", "seats", "Seats", FieldKindDef::Number),
                FieldDef::new("field_opens", "opens", "Opens", FieldKindDef::Date),
                FieldDef::new(
                    "field_ok",
                    "accessible",
                    "Accessible",
                    FieldKindDef::Checkbox,
                ),
            ],
            location: vec![vec![LocationRule {
                param: LocationParam::PostType,
                operator: LocationOperator::Is,
                value: "rs_venue".into(),
            }]],
            ..FieldGroupDef::new("group_venue", "Venue")
        })
        .unwrap();
    let resource = post_type_resource(&def, &registry);
    assert_eq!(
        field_names(&resource),
        [
            "title",
            "slug",
            "status",
            "date",
            "password",
            "field_venue_name",
            "field_seats",
            "field_opens",
            "field_ok"
        ]
    );
    assert_eq!(field(&resource, "field_venue_name").label, "Venue");

    let draft = create_auto_draft(db, "rs_venue", author).await.unwrap();
    let input = validated(
        post_type_resource(&def, &registry),
        &[
            ("title", "Hall"),
            ("status", "draft"),
            ("date", ""),
            ("field_venue_name", "Main hall"),
            ("field_seats", "120"),
            ("field_opens", "2026-11-15"),
            ("field_ok", "on"),
        ],
    );
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["field_venue_name"], "Main hall");
    assert_eq!(row["field_seats"], 120);
    assert_eq!(row["field_opens"], "2026-11-15");
    assert_eq!(row["field_ok"], true);

    let venue = get_field::<String>(db, &registry, draft.id, "venue")
        .await
        .unwrap();
    assert_eq!(venue.as_deref(), Some("Main hall"));
    let seats = get_field::<i64>(db, &registry, draft.id, "seats")
        .await
        .unwrap();
    assert_eq!(seats, Some(120));
    let opens = get_field::<String>(db, &registry, draft.id, "opens")
        .await
        .unwrap();
    assert_eq!(
        opens.as_deref(),
        Some("20261115"),
        "ACF's date_picker format"
    );
    assert_eq!(
        get_post_meta(db, draft.id, "_venue").await.unwrap(),
        Some(Value::from("field_Venue-Name"))
    );

    // A field the form clears is stored as null, which reads as none.
    let input = validated(
        post_type_resource(&def, &registry),
        &[
            ("title", "Hall"),
            ("status", "draft"),
            ("date", ""),
            ("field_venue_name", ""),
        ],
    );
    resource.data.update(db, draft.id, input).await.unwrap();
    let venue = get_field::<String>(db, &registry, draft.id, "venue")
        .await
        .unwrap();
    assert_eq!(venue, None);
    assert_eq!(
        get_field::<i64>(db, &registry, draft.id, "seats")
            .await
            .unwrap(),
        Some(120),
        "a field the form did not send keeps its value"
    );
}

#[tokio::test]
async fn a_page_s_parent_and_order_are_saved_without_loops() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let def = PostTypeDef {
        hierarchical: true,
        ..post_type("rs_doc", vec![Support::Title, Support::PageAttributes])
    };
    let resource = post_type_resource(&def, &Registry::default());
    let parent = create_auto_draft(db, "rs_doc", author).await.unwrap();
    let child = create_auto_draft(db, "rs_doc", author).await.unwrap();
    for (id, title) in [(parent.id, "Parent"), (child.id, "Child")] {
        let input = validated(
            post_type_resource(&def, &Registry::default()),
            &[("title", title), ("status", "publish"), ("date", "")],
        );
        resource.data.update(db, id, input).await.unwrap();
    }
    let parent_id = parent.id.to_string();
    let input = validated(
        post_type_resource(&def, &Registry::default()),
        &[
            ("title", "Child"),
            ("status", "publish"),
            ("date", ""),
            ("parent", &parent_id),
            ("menu_order", "3"),
        ],
    );
    let row = resource.data.update(db, child.id, input).await.unwrap();
    assert_eq!(row["parent"], parent_id);
    assert_eq!(row["menu_order"], 3);
    let options = resource.data.options(db, "parent").await.unwrap();
    assert_eq!(options.len(), 2);

    // The parent made a child of its child: the loop is broken, as WordPress breaks it.
    let child_id = child.id.to_string();
    let input = validated(
        post_type_resource(&def, &Registry::default()),
        &[
            ("title", "Parent"),
            ("status", "publish"),
            ("date", ""),
            ("parent", &child_id),
        ],
    );
    let row = resource.data.update(db, parent.id, input).await.unwrap();
    assert_eq!(row["parent"], Value::Null);

    // A hierarchical type lists by order, then title.
    let page = resource.data.list(db, &first_page(None)).await.unwrap();
    assert_eq!(ids(&page.rows), [parent.id, child.id]);
}

/// What the edit form of `row` submits, as Ignite renders it (`record_form`) and a browser posts
/// it: a choice field's values are limited to the choices it shows, so a stored value that is not
/// among them comes back blank. `edits` are the user's changes. Every submitted choice is then
/// checked against the options, as Ignite's `lookup_errors` does, and the form is validated with
/// `validate_update`.
async fn resubmit(
    db: &Database,
    resource: &RuntimeResource,
    row: &Row,
    edits: &[(&str, &str)],
) -> Row {
    let text = |value: &Value| match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut form: HashMap<String, String> = HashMap::new();
    for f in &resource.fields {
        let value = match (&f.kind, row.get(&f.name)) {
            (FieldKindDef::Password, _) | (_, None | Some(Value::Null)) => continue,
            (FieldKindDef::Checkbox, Some(Value::Bool(true))) => "on".to_owned(),
            (FieldKindDef::Checkbox, _) => continue,
            (_, Some(value)) => text(value),
        };
        let value = if f.kind.loads_options() {
            let choices: Vec<String> = resource
                .data
                .options(db, &f.name)
                .await
                .unwrap()
                .into_iter()
                .map(|(v, _)| v)
                .collect();
            if f.kind == FieldKindDef::Relationship {
                let shown: Vec<String> = serde_json::from_str::<Vec<Value>>(&value)
                    .unwrap()
                    .iter()
                    .map(text)
                    .filter(|v| choices.contains(v))
                    .collect();
                json!(shown).to_string()
            } else if choices.contains(&value) {
                value
            } else {
                String::new()
            }
        } else {
            value
        };
        form.insert(f.name.clone(), value);
    }
    for (key, value) in edits {
        form.insert((*key).to_owned(), (*value).to_owned());
    }
    for f in resource.fields.iter().filter(|f| f.kind.loads_options()) {
        let Some(value) = form.get(&f.name).filter(|v| !v.is_empty()) else {
            continue;
        };
        let values: Vec<String> = if f.kind == FieldKindDef::Relationship {
            serde_json::from_str::<Vec<Value>>(value)
                .unwrap()
                .iter()
                .map(text)
                .collect()
        } else {
            vec![value.clone()]
        };
        let choices = resource.data.options(db, &f.name).await.unwrap();
        for value in values {
            assert!(
                choices.iter().any(|(choice, _)| *choice == value),
                "`{value}` of `{}` is one of the options",
                f.name
            );
        }
    }
    let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    validated(resource.clone(), &pairs)
}

/// Inserts the terms `names` of `taxonomy`, many to a statement, and returns their term ids by
/// name.
async fn bulk_terms(db: &Database, taxonomy: &str, names: &[String]) -> HashMap<String, i64> {
    for chunk in names.chunks(100) {
        let marks = vec!["(?, ?)"; chunk.len()].join(", ");
        let binds: Vec<orbit_orm::Value> = chunk
            .iter()
            .flat_map(|n| [n.as_str().into(), format!("{taxonomy}-{n}").into()])
            .collect();
        raw::execute(
            db,
            &format!("INSERT INTO terms (name, slug) VALUES {marks}"),
            &binds,
        )
        .await
        .unwrap();
    }
    raw::execute(
        db,
        "INSERT INTO term_taxonomy (term_id, taxonomy, description) \
         SELECT id, ?, ? FROM terms WHERE slug LIKE ?",
        &[taxonomy.into(), "".into(), format!("{taxonomy}-%").into()],
    )
    .await
    .unwrap();
    let rows: Vec<(i64, String)> = raw::fetch_all(
        db,
        "SELECT t.id, t.name FROM terms t INNER JOIN term_taxonomy tt ON tt.term_id = t.id \
         WHERE tt.taxonomy = ?",
        &[taxonomy.into()],
    )
    .await
    .unwrap();
    rows.into_iter().map(|(id, name)| (name, id)).collect()
}

#[tokio::test]
async fn a_reference_the_form_cannot_show_survives_a_save() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let registry = Registry::default();
    let def = PostTypeDef {
        hierarchical: true,
        ..post_type(
            "rs_cap",
            vec![Support::Title, Support::PageAttributes, Support::Author],
        )
    };
    registry.register(def.clone()).unwrap();
    registry
        .register(TaxonomyDef {
            public: true,
            origin: Origin::Code,
            ..TaxonomyDef::new("rs_cap_tag", &["rs_cap"], "Labels", "Label")
        })
        .unwrap();
    let resource = post_type_resource(&def, &registry);
    // More terms than the form shows: `0visible` is first by name, `zzbeyond` past the limit.
    let mut names: Vec<String> = (0..MAX_OPTIONS).map(|i| format!("a{i:04}")).collect();
    names.push("0visible".to_owned());
    names.push("zzbeyond".to_owned());
    let terms = bulk_terms(db, "rs_cap_tag", &names).await;
    let (visible, beyond) = (terms["0visible"], terms["zzbeyond"]);
    let shown = resource.data.options(db, "tax_rs_cap_tag").await.unwrap();
    assert_eq!(shown.len(), MAX_OPTIONS);
    assert!(shown.iter().any(|(id, _)| *id == visible.to_string()));
    assert!(!shown.iter().any(|(id, _)| *id == beyond.to_string()));

    // A parent that is trashed is not offered either.
    let parent = create_auto_draft(db, "rs_cap", author).await.unwrap();
    let other = create_auto_draft(db, "rs_cap", author).await.unwrap();
    let child = create_auto_draft(db, "rs_cap", author).await.unwrap();
    for id in [parent.id, other.id] {
        let input = validated(
            resource.clone(),
            &[("title", "Parent"), ("status", "publish"), ("date", "")],
        );
        resource.data.update(db, id, input).await.unwrap();
    }
    let tags = format!("[\"{visible}\",\"{beyond}\"]");
    let parent_id = parent.id.to_string();
    let input = validated(
        resource.clone(),
        &[
            ("title", "Child"),
            ("status", "publish"),
            ("date", ""),
            ("parent", &parent_id),
            ("tax_rs_cap_tag", &tags),
        ],
    );
    resource.data.update(db, child.id, input).await.unwrap();
    assert_eq!(resource.data.delete(db, parent.id).await.unwrap(), 1);

    // The form round trip: the trashed parent and the term past the limit show unselected and
    // come back blank, and the save keeps them.
    let row = resource.data.find(db, child.id).await.unwrap().unwrap();
    let input = resubmit(db, &resource, &row, &[("title", "Child, renamed")]).await;
    assert_eq!(input["parent"], Value::Null);
    let row = resource.data.update(db, child.id, input).await.unwrap();
    assert_eq!(row["title"], "Child, renamed");
    assert_eq!(row["parent"], parent_id);
    let mut kept = vec![visible.to_string(), beyond.to_string()];
    kept.sort();
    let mut stored: Vec<String> =
        serde_json::from_str(row["tax_rs_cap_tag"].as_str().unwrap()).unwrap();
    stored.sort();
    assert_eq!(stored, kept);

    // A value the form shows can still be cleared: the shown term is unticked, a shown parent
    // chosen and then cleared.
    let row = resource
        .data
        .update(
            db,
            child.id,
            resubmit(db, &resource, &row, &[("tax_rs_cap_tag", "[]")]).await,
        )
        .await
        .unwrap();
    assert_eq!(row["tax_rs_cap_tag"], format!("[\"{beyond}\"]"));
    let other_id = other.id.to_string();
    let row = resource
        .data
        .update(
            db,
            child.id,
            resubmit(db, &resource, &row, &[("parent", &other_id)]).await,
        )
        .await
        .unwrap();
    assert_eq!(row["parent"], other_id);
    let row = resource
        .data
        .update(
            db,
            child.id,
            resubmit(db, &resource, &row, &[("parent", "")]).await,
        )
        .await
        .unwrap();
    assert_eq!(row["parent"], Value::Null);

    // An author who may not edit posts is not offered; a save keeps them.
    let row = resource
        .data
        .update(db, child.id, resubmit(db, &resource, &row, &[]).await)
        .await
        .unwrap();
    assert_eq!(row["author"], author.to_string());
}

#[tokio::test]
async fn a_type_shown_in_rest_is_listed_by_the_rest_types_route() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let registry = Registry::default();
    registry
        .register(post_type("rs_film", vec![Support::Title]))
        .unwrap();
    registry
        .register(PostTypeDef {
            show_in_rest: false,
            ..post_type("rs_secret", vec![Support::Title])
        })
        .unwrap();
    let visitor = UserCaps::with_default_roles(0, &[], &BTreeMap::new());
    let controller = MetaRoutesController {
        db,
        post_types: registry.post_types(),
        taxonomies: registry.taxonomies(),
        user: &visitor,
    };
    let types = controller.types("").await.unwrap().body;
    assert!(types.get("rs_film").is_some(), "{types}");
    assert!(types.get("rs_secret").is_none());
    assert!(types.get("post").is_some());
    let routes = RestRoutes::new();
    registry.post_types().register_routes(&routes);
    let paths: Vec<String> = routes.routes().into_iter().map(|r| r.path).collect();
    assert!(paths.contains(&"/mc/v2/rs_film".to_owned()));
    assert!(!paths.iter().any(|p| p.contains("rs_secret")));

    // Both have admin screens: the admin shows a type by `show_ui`, not `show_in_rest`.
    let resources = PostTypeResources::new(Arc::new(registry));
    let slugs: Vec<String> = resources.post_types().into_iter().map(|d| d.slug).collect();
    assert!(slugs.contains(&"rs_film".to_owned()));
    assert!(slugs.contains(&"rs_secret".to_owned()));
}

#[tokio::test]
async fn the_list_clamps_its_page_excludes_terms_and_sorts_by_date() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let resource = post_type_resource(
        &post_type("rs_many", vec![Support::Title]),
        &Registry::default(),
    );
    let start = Utc::now().naive_utc() - Duration::days(1);
    let mut created = Vec::new();
    for i in 0..101_i64 {
        let title = match i {
            0 => "apple pie".to_owned(),
            1 => "apple tart".to_owned(),
            n => format!("filler {n}"),
        };
        let post = Post::create(
            db,
            mission_control_core::content::NewPost {
                status: PostStatus::Publish,
                title,
                published_at: Some(start + Duration::minutes(i)),
                ..mission_control_core::content::NewPost::new(author, "rs_many")
            },
        )
        .await
        .unwrap();
        created.push(post.id);
    }
    let huge = ListQuery {
        per_page: 500,
        ..first_page(None)
    };
    let page = resource.data.list(db, &huge).await.unwrap();
    assert_eq!(page.total, 101);
    assert_eq!(
        page.rows.len(),
        100,
        "a page holds at most MAX_PER_PAGE rows"
    );

    let page = resource
        .data
        .list(db, &first_page(Some("apple -tart")))
        .await
        .unwrap();
    assert_eq!(ids(&page.rows), [created[0]]);

    let by_date = |direction| ListQuery {
        sort: Some(("col_date".to_owned(), direction)),
        ..first_page(None)
    };
    let page = resource
        .data
        .list(db, &by_date(Direction::Asc))
        .await
        .unwrap();
    assert_eq!(ids(&page.rows)[..2], [created[0], created[1]]);
    let page = resource
        .data
        .list(db, &by_date(Direction::Desc))
        .await
        .unwrap();
    assert_eq!(ids(&page.rows)[..2], [created[100], created[99]]);
}

#[tokio::test]
async fn the_slug_box_and_the_private_visibility_save_as_wordpress() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let def = post_type("rs_slug", vec![Support::Title]);
    let resource = post_type_resource(&def, &Registry::default());
    let draft = create_auto_draft(db, "rs_slug", author).await.unwrap();
    let input = validated(
        resource.clone(),
        &[
            ("title", "Title"),
            ("slug", "My Custom Slug!"),
            ("status", "publish"),
            ("date", ""),
            ("password", "secret"),
        ],
    );
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["slug"], "my-custom-slug");
    assert_eq!(row["password"], "secret");

    // Private clears the password, as `edit_post` does.
    let input = validated(
        resource.clone(),
        &[
            ("title", "Title"),
            ("slug", "my-custom-slug"),
            ("status", "private"),
            ("date", ""),
            ("password", "secret"),
        ],
    );
    let row = resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(row["status"], "private");
    assert_eq!(row["password"], Value::Null);
}

#[tokio::test]
async fn ignite_roles_are_read_as_wordpress_roles() {
    let test = database().await;
    let db = test.db();
    let ann = setup(db).await;
    grant(db, ann, "super-admin").await;
    grant(db, ann, "not-a-wordpress-role").await;
    let assignment = IgniteUserRoles.roles(db, ann).await.unwrap();
    assert_eq!(assignment.roles, [ADMINISTRATOR]);
    let nobody = add_user(db, "nobody").await;
    assert!(
        IgniteUserRoles
            .roles(db, nobody)
            .await
            .unwrap()
            .roles
            .is_empty()
    );
}

/// Task 6.2: the Template select of `page`, from the active theme's page templates, stored in
/// `_wp_page_template` as the REST API stores it.
#[tokio::test]
async fn the_page_template_select_saves_as_wordpress_does() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let [post, page] = builtin_post_types();
    let templates = vec![PageTemplate {
        name: "templates/wide".to_owned(),
        title: "Full width".to_owned(),
    }];
    let resource = post_type_resource_with_templates(&page, &Registry::default(), &templates);
    let names = field_names(&resource);
    let parent_at = names.iter().position(|n| *n == "parent").unwrap();
    assert_eq!(names[parent_at + 1], "template");
    assert_eq!(
        field(&resource, "template").kind,
        FieldKindDef::Select(vec![
            ("default".to_owned(), "Default template".to_owned()),
            ("templates/wide".to_owned(), "Full width".to_owned()),
        ])
    );
    // Only pages get it, and only when the theme has templates (WordPress's Page Attributes box).
    let posts = post_type_resource_with_templates(&post, &Registry::default(), &templates);
    assert!(!field_names(&posts).contains(&"template"));
    let bare = post_type_resource(&page, &Registry::default());
    assert!(!field_names(&bare).contains(&"template"));

    let draft = create_auto_draft(db, "page", author).await.unwrap();
    let row = resource.data.find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(row["template"], "default");
    let input = |template: &str| {
        let mut row = Row::new();
        row.insert("status".into(), json!("draft"));
        row.insert("template".into(), json!(template));
        row
    };
    let row = resource
        .data
        .update(db, draft.id, input("templates/wide"))
        .await
        .unwrap();
    assert_eq!(row["template"], "templates/wide");
    assert_eq!(
        get_post_meta(db, draft.id, "_wp_page_template")
            .await
            .unwrap(),
        Some(json!("templates/wide"))
    );
    // A name the theme does not offer is not stored (the form's select refuses it first).
    resource
        .data
        .update(db, draft.id, input("templates/other"))
        .await
        .unwrap();
    assert_eq!(
        get_post_meta(db, draft.id, "_wp_page_template")
            .await
            .unwrap(),
        Some(json!("templates/wide"))
    );
    // The default stores none.
    let row = resource
        .data
        .update(db, draft.id, input("default"))
        .await
        .unwrap();
    assert_eq!(row["template"], "default");
    assert_eq!(
        get_post_meta(db, draft.id, "_wp_page_template")
            .await
            .unwrap(),
        None
    );
    // A stored template the theme no longer has reads as the default.
    mission_control_core::content::set_post_meta(
        db,
        draft.id,
        "_wp_page_template",
        &json!("templates/gone"),
    )
    .await
    .unwrap();
    let row = resource.data.find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(row["template"], "default");
}

/// A template stored with WordPress's file name (`templates/wide.php`, as an import brings it)
/// is the theme's `templates/wide`: the form shows it, and a save that keeps it leaves the stored
/// value as it is.
#[tokio::test]
async fn an_imported_template_name_is_matched_and_kept() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let [_, page] = builtin_post_types();
    let templates = vec![PageTemplate {
        name: "templates/wide".to_owned(),
        title: "Full width".to_owned(),
    }];
    let resource = post_type_resource_with_templates(&page, &Registry::default(), &templates);
    let draft = create_auto_draft(db, "page", author).await.unwrap();
    mission_control_core::content::set_post_meta(
        db,
        draft.id,
        "_wp_page_template",
        &json!("templates/wide.php"),
    )
    .await
    .unwrap();
    let row = resource.data.find(db, draft.id).await.unwrap().unwrap();
    assert_eq!(row["template"], "templates/wide");
    let mut input = Row::new();
    input.insert("status".into(), json!("draft"));
    input.insert("template".into(), json!("templates/wide"));
    resource.data.update(db, draft.id, input).await.unwrap();
    assert_eq!(
        get_post_meta(db, draft.id, "_wp_page_template")
            .await
            .unwrap(),
        Some(json!("templates/wide.php"))
    );
}

/// `delete_post` on the front page or the posts page needs `manage_options`; the data layer does
/// not know the user, so the form's Trash status is refused on them (the list's Delete, which the
/// admin checks, still trashes them).
#[tokio::test]
async fn the_form_cannot_trash_the_front_page() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let [_, page] = builtin_post_types();
    let resource = post_type_resource(&page, &Registry::default());
    let front = create_auto_draft(db, "page", author).await.unwrap();
    set_option(db, "page_on_front", &json!(front.id))
        .await
        .unwrap();
    let mut input = Row::new();
    input.insert("status".into(), json!("trash"));
    assert!(resource.data.update(db, front.id, input).await.is_err());
    let stored = Post::query().find(db, front.id).await.unwrap().unwrap();
    assert_ne!(stored.status, PostStatus::Trash);
    // Delete still trashes it.
    assert_eq!(resource.data.delete(db, front.id).await.unwrap(), 1);
}

/// Fail closed: a post type whose Ignite resource something else registered is not this value's.
#[test]
fn a_slug_another_registration_holds_is_unowned() {
    let _ignite = ignite_lock();
    let registry = Arc::new(Registry::default());
    registry
        .register(post_type("rs_foreign", vec![Support::Title]))
        .unwrap();
    // Someone else takes the slug first.
    register_runtime(post_type_resource(
        &post_type("rs_foreign", vec![Support::Title]),
        &Registry::default(),
    ))
    .unwrap();
    let resources = PostTypeResources::new(Arc::clone(&registry));
    assert!(resources.is_unowned("rs_foreign"));
    assert!(resources.post_type("rs_foreign").is_none());
    assert!(!resources.is_unowned("rs_nothing"));
    unregister_runtime("rs_foreign").unwrap();
    assert!(
        !resources.is_unowned("rs_foreign"),
        "no resource at all: Ignite's 404"
    );
}
