//! Tests for the type registry (Task 5.2): admin definitions stored and loaded again, code and
//! plugin definitions held in memory, ACF's local-over-admin rule, and the REST registries the
//! registry feeds. They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file
//! when it is unset.

use std::sync::Arc;

use mission_control_core::migrations::migrator;
use mission_control_core::rest::posts::{PostTypes, RestTaxonomy};
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::types::{
    DefinitionError, DefinitionKind, FieldGroupDef, LocationOperator, LocationParam, LocationRule,
    MAX_JSON_BYTES, Origin, PostTypeDef, Registry, RegistryError, TaxonomyDef,
};
use mission_control_testing::database;
use orbit_orm::{Database, raw};

/// Runs the core migrations.
async fn migrate(db: &Database) {
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
}

/// A registry feeding REST registries of its own.
fn registry() -> Registry {
    Registry::new(Arc::new(PostTypes::new()), Arc::new(Taxonomies::new()))
}

/// The post type `book`, shown in the REST API at `books`, from `origin`, named `name`.
fn book(origin: Origin, name: &str) -> PostTypeDef {
    let json = format!(
        r#"{{"slug": "book", "public": true, "show_in_rest": true, "rest_base": "books",
            "capability_type": "book", "labels": {{"name": "{name}", "singular_name": "Book"}},
            "taxonomies": ["genre"], "field_groups": ["group_book"]}}"#
    );
    PostTypeDef::from_json(&json, origin).unwrap()
}

/// The taxonomy `genre` for books and posts, shown in the REST API at `genres`.
fn genre(origin: Origin) -> TaxonomyDef {
    TaxonomyDef::from_json(
        r#"{"slug": "genre", "object_type": ["book", "post"], "hierarchical": true,
            "show_in_rest": true, "rest_base": "genres",
            "labels": {"name": "Genres", "singular_name": "Genre"}}"#,
        origin,
    )
    .unwrap()
}

/// The field group `group_book`, shown on pages with the template `book.html`.
fn group(origin: Origin) -> FieldGroupDef {
    FieldGroupDef::from_json(
        r#"{"key": "group_book", "title": "Book details",
            "fields": [{"key": "field_isbn", "name": "isbn", "label": "ISBN", "type": "text"}],
            "location": [[{"param": "page_template", "operator": "==", "value": "book.html"},
                          {"param": "post_status", "operator": "!=", "value": "draft"}]]}"#,
        origin,
    )
    .unwrap()
}

async fn count(db: &Database, table: &str) -> i64 {
    raw::fetch_scalar(db, &format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .unwrap()
}

/// The `field_group_locations` rows of `group_book`, in order.
async fn locations(db: &Database) -> Vec<(i32, i32, String, String, String)> {
    raw::fetch_all(
        db,
        "SELECT or_group, position, param, operator, value FROM field_group_locations \
         WHERE group_key = ? ORDER BY or_group, position",
        &["group_book".into()],
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn an_admin_post_type_survives_a_restart() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    let first = registry();
    first
        .save_admin_definition(db, book(Origin::Admin, "Books"))
        .await
        .expect("saves the admin's type");
    first
        .save_admin_definition(db, genre(Origin::Admin))
        .await
        .unwrap();
    first
        .save_admin_definition(db, group(Origin::Admin))
        .await
        .unwrap();
    assert_eq!(count(db, "post_types").await, 1);

    // A restart: a new registry reads what the first stored.
    let second = registry();
    assert_eq!(second.load_from_db(db).await.unwrap(), 3);
    assert_eq!(second.all_post_types(), [book(Origin::Admin, "Books")]);
    assert_eq!(second.all_taxonomies(), [genre(Origin::Admin)]);
    assert_eq!(second.post_type("book").unwrap().origin, Origin::Admin);
    assert!(second.field_group("group_book").is_some());
    let served = second
        .post_types()
        .by_rest_base("books")
        .expect("the stored type is served");
    assert_eq!(served.caps.edit_posts, "edit_books");

    // Saving again replaces the row.
    second
        .save_admin_definition(db, book(Origin::Admin, "Novels"))
        .await
        .unwrap();
    assert_eq!(count(db, "post_types").await, 1);
    let third = registry();
    third.load_from_db(db).await.unwrap();
    assert_eq!(third.all_post_types()[0].labels.name, "Novels");
}

#[tokio::test]
async fn a_code_or_plugin_definition_is_never_stored() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    let registry = registry();
    registry.register(book(Origin::Code, "Books")).unwrap();
    registry.register(genre(Origin::Code)).unwrap();
    registry
        .register(group(Origin::Plugin("shop".into())))
        .unwrap();
    for table in [
        "post_types",
        "taxonomies",
        "field_groups",
        "field_group_locations",
    ] {
        assert_eq!(count(db, table).await, 0, "{table} has rows");
    }
    assert_eq!(registry.all_post_types(), [book(Origin::Code, "Books")]);

    // The admin cannot store, change or delete them.
    let err = registry
        .save_admin_definition(db, book(Origin::Code, "Books"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RegistryError::Definition(DefinitionError::Locked(Origin::Code))
        ),
        "{err:?}"
    );
    let err = registry
        .save_admin_definition(db, group(Origin::Plugin("shop".into())))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RegistryError::Definition(DefinitionError::Locked(Origin::Plugin(p))) if p == "shop"),
        "{err:?}"
    );
    let err = registry
        .delete_admin_definition(db, DefinitionKind::PostType, "book")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RegistryError::Definition(DefinitionError::Locked(Origin::Code))
        ),
        "{err:?}"
    );
    assert_eq!(count(db, "post_types").await, 0);

    // An admin definition is saved, not registered.
    let err = registry.register(book(Origin::Admin, "Books")).unwrap_err();
    assert!(
        matches!(&err, RegistryError::AdminOrigin { kind: DefinitionKind::PostType, key } if key == "book"),
        "{err:?}"
    );

    // A restart forgets them: code registers them again at every start.
    let restarted = loaded_registry(db).await;
    assert!(restarted.all_post_types().is_empty());
}

/// A new registry with the stored definitions loaded.
async fn loaded_registry(db: &Database) -> Registry {
    let registry = registry();
    registry.load_from_db(db).await.unwrap();
    registry
}

#[tokio::test]
async fn deactivating_a_plugin_removes_its_definitions() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    let registry = registry();
    let shop = || Origin::Plugin("shop".into());
    registry.register(book(shop(), "Books")).unwrap();
    registry.register(genre(shop())).unwrap();
    registry.register(group(shop())).unwrap();
    let mut movie = PostTypeDef::new("movie", "Movies", "Movie");
    movie.origin = Origin::Plugin("cinema".into());
    registry.register(movie).unwrap();
    assert!(registry.post_types().by_rest_base("books").is_some());
    assert!(registry.taxonomies().by_rest_base("genres").is_some());

    assert_eq!(registry.remove_plugin("shop"), 3);
    let left: Vec<String> = registry
        .all_post_types()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    assert_eq!(left, ["movie"]);
    assert!(registry.all_taxonomies().is_empty());
    assert!(registry.all_field_groups().is_empty());
    assert!(registry.post_types().registered("book").is_none());
    assert!(registry.taxonomies().registered("genre").is_none());
    // `post` no longer lists the genres.
    let post = registry.post_types().by_name("post").unwrap();
    assert_eq!(
        post.taxonomies,
        [RestTaxonomy::CATEGORIES, RestTaxonomy::TAGS]
    );
    assert_eq!(registry.remove_plugin("shop"), 0);
}

#[tokio::test]
async fn a_local_definition_overrides_an_admin_one_with_the_same_slug() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    let registry = registry();
    registry
        .save_admin_definition(db, book(Origin::Admin, "Admin books"))
        .await
        .unwrap();
    assert!(!registry.is_overridden(DefinitionKind::PostType, "book"));

    // ACF's local JSON rule: the local definition is used, the admin copy stays stored.
    let library = || Origin::Plugin("library".into());
    registry.register(book(library(), "Plugin books")).unwrap();
    assert_eq!(registry.all_post_types(), [book(library(), "Plugin books")]);
    assert!(registry.is_overridden(DefinitionKind::PostType, "book"));
    assert_eq!(count(db, "post_types").await, 1);
    let err = registry
        .save_admin_definition(db, book(Origin::Admin, "Edited"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RegistryError::Definition(DefinitionError::Locked(Origin::Plugin(p))) if p == "library"),
        "{err:?}"
    );

    // The same after a restart, whichever comes first.
    let restarted = registry_with_code_first(db).await;
    assert_eq!(
        restarted.all_post_types(),
        [book(Origin::Code, "Code books")]
    );
    assert!(restarted.is_overridden(DefinitionKind::PostType, "book"));

    // The plugin goes: the stored definition is used again.
    assert_eq!(registry.remove_plugin("library"), 1);
    assert_eq!(
        registry.all_post_types(),
        [book(Origin::Admin, "Admin books")]
    );
    assert!(!registry.is_overridden(DefinitionKind::PostType, "book"));

    // An overridden admin copy may be deleted.
    assert!(
        restarted
            .delete_admin_definition(db, DefinitionKind::PostType, "book")
            .await
            .unwrap()
    );
    assert!(!restarted.is_overridden(DefinitionKind::PostType, "book"));
    assert_eq!(
        restarted.all_post_types(),
        [book(Origin::Code, "Code books")]
    );
    assert_eq!(count(db, "post_types").await, 0);
}

/// A new registry that registers a code `book` before it loads the stored definitions.
async fn registry_with_code_first(db: &Database) -> Registry {
    let registry = registry();
    registry.register(book(Origin::Code, "Code books")).unwrap();
    registry.load_from_db(db).await.unwrap();
    registry
}

#[test]
fn registered_types_and_taxonomies_feed_the_rest_registries() {
    let registry = registry();
    registry.register(genre(Origin::Code)).unwrap();
    registry.register(book(Origin::Code, "Books")).unwrap();

    let genres = registry
        .taxonomies()
        .by_rest_base("genres")
        .expect("the taxonomy is served");
    assert!(genres.taxonomy.hierarchical);
    assert!(genres.taxonomy.is_for("book"));
    assert_eq!(genres.caps.assign_terms, "edit_posts");

    let books = registry
        .post_types()
        .by_rest_base("books")
        .expect("the type is served");
    assert_eq!(books.name, "book");
    assert_eq!(books.caps.edit_posts, "edit_books");
    assert_eq!(books.caps.publish_posts, "publish_books");
    let bases: Vec<&str> = books.taxonomies.iter().map(|t| &*t.rest_base).collect();
    assert_eq!(bases, ["genres"]);
    // `object_type` attaches the taxonomy to `post` too, after its own.
    let post = registry.post_types().by_name("post").unwrap();
    let bases: Vec<&str> = post.taxonomies.iter().map(|t| &*t.rest_base).collect();
    assert_eq!(bases, ["categories", "tags", "genres"]);
    assert!(
        registry
            .post_types()
            .by_name("page")
            .unwrap()
            .taxonomies
            .is_empty()
    );

    // A type not shown in the REST API is registered, not served.
    let mut hidden = PostTypeDef::new("ledger", "Ledgers", "Ledger");
    hidden.origin = Origin::Code;
    registry.register(hidden).unwrap();
    assert!(registry.post_types().registered("ledger").is_some());
    assert!(registry.post_types().by_name("ledger").is_none());

    // Registering again replaces it.
    let mut renamed = book(Origin::Code, "Books");
    renamed.rest_base = Some("library".into());
    registry.register(renamed).unwrap();
    assert!(registry.post_types().by_rest_base("books").is_none());
    assert!(registry.post_types().by_rest_base("library").is_some());
}

#[test]
fn a_post_types_field_groups_become_location_rules() {
    let registry = registry();
    registry.register(group(Origin::Code)).unwrap();
    registry.register(book(Origin::Code, "Books")).unwrap();
    let rules = registry.field_group("group_book").unwrap().location;
    assert_eq!(rules.len(), 2);
    assert_eq!(
        rules[1],
        [LocationRule {
            param: LocationParam::PostType,
            operator: LocationOperator::Is,
            value: "book".into(),
        }]
    );
    // Registered again, the rule is not repeated.
    registry.register(book(Origin::Code, "Books")).unwrap();
    assert_eq!(
        registry.field_group("group_book").unwrap().location.len(),
        2
    );
}

#[tokio::test]
async fn a_stored_groups_location_rows_follow_its_json() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    let registry = registry();
    registry
        .save_admin_definition(db, group(Origin::Admin))
        .await
        .unwrap();
    assert_eq!(
        locations(db).await,
        [
            (
                0,
                0,
                "page_template".into(),
                "==".into(),
                "book.html".into()
            ),
            (0, 1, "post_status".into(), "!=".into(), "draft".into()),
        ]
    );

    let mut changed = group(Origin::Admin);
    changed.location = vec![
        vec![LocationRule {
            param: LocationParam::PostType,
            operator: LocationOperator::Is,
            value: "book".into(),
        }],
        vec![LocationRule {
            param: LocationParam::UserRole,
            operator: LocationOperator::IsNot,
            value: "subscriber".into(),
        }],
    ];
    registry.save_admin_definition(db, changed).await.unwrap();
    assert_eq!(
        locations(db).await,
        [
            (0, 0, "post_type".into(), "==".into(), "book".into()),
            (1, 0, "user_role".into(), "!=".into(), "subscriber".into()),
        ]
    );

    assert!(
        registry
            .delete_admin_definition(db, DefinitionKind::FieldGroup, "group_book")
            .await
            .unwrap()
    );
    assert!(locations(db).await.is_empty());
    assert_eq!(count(db, "field_groups").await, 0);
    assert!(registry.all_field_groups().is_empty());
    assert!(
        !registry
            .delete_admin_definition(db, DefinitionKind::FieldGroup, "group_book")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_stored_definition_that_does_not_read_is_left_out() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    registry()
        .save_admin_definition(db, book(Origin::Admin, "Books"))
        .await
        .unwrap();
    raw::execute(
        db,
        "INSERT INTO taxonomies (slug, definition, created_at, updated_at) VALUES (?, ?, ?, ?)",
        &[
            "category".into(),
            r#"{"slug": "category"}"#.into(),
            chrono::Utc::now().naive_utc().into(),
            chrono::Utc::now().naive_utc().into(),
        ],
    )
    .await
    .unwrap();
    let registry = registry();
    assert_eq!(registry.load_from_db(db).await.unwrap(), 1);
    assert!(registry.all_taxonomies().is_empty());
    assert_eq!(registry.all_post_types().len(), 1);
}

/// Stores `json` as the row of `table` with `key` in `column`, as a hand edit or another version
/// would.
async fn insert_row(db: &Database, table: &str, column: &str, key: &str, json: &str) {
    let now = chrono::Utc::now().naive_utc();
    raw::execute(
        db,
        &format!(
            "INSERT INTO {table} ({column}, definition, created_at, updated_at) \
             VALUES (?, ?, ?, ?)"
        ),
        &[key.into(), json.into(), now.into(), now.into()],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn malformed_over_size_and_mismatched_rows_are_left_out() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    registry()
        .save_admin_definition(db, book(Origin::Admin, "Books"))
        .await
        .unwrap();
    // Not JSON.
    insert_row(db, "post_types", "slug", "broken", "{\"slug\": \"broken\",").await;
    // Over `MAX_JSON_BYTES`: valid JSON padded with spaces.
    let padded = format!(
        r#"{{"slug": "huge", "labels": {{"name": "Huge", "singular_name": "Huge"}}}}{}"#,
        " ".repeat(MAX_JSON_BYTES)
    );
    insert_row(db, "post_types", "slug", "huge", &padded).await;
    // A JSON slug that is not the row's.
    insert_row(
        db,
        "taxonomies",
        "slug",
        "genre",
        r#"{"slug": "mood", "object_type": ["book"],
            "labels": {"name": "Moods", "singular_name": "Mood"}}"#,
    )
    .await;
    insert_row(
        db,
        "field_groups",
        "group_key",
        "group_other",
        &serde_json::to_string(&group(Origin::Admin)).unwrap(),
    )
    .await;

    let registry = registry();
    assert_eq!(registry.load_from_db(db).await.unwrap(), 1);
    assert_eq!(registry.all_post_types(), [book(Origin::Admin, "Books")]);
    assert!(registry.all_taxonomies().is_empty());
    assert!(registry.taxonomies().registered("mood").is_none());
    assert!(registry.all_field_groups().is_empty());
}

#[tokio::test]
async fn a_stored_row_overridden_by_code_can_be_deleted_before_the_load() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    registry()
        .save_admin_definition(db, book(Origin::Admin, "Admin books"))
        .await
        .unwrap();
    // A start where the code registers `book` and the admin deletes its copy before the
    // stored definitions are read.
    let registry = registry();
    registry.register(book(Origin::Code, "Code books")).unwrap();
    assert!(
        registry
            .delete_admin_definition(db, DefinitionKind::PostType, "book")
            .await
            .unwrap()
    );
    assert_eq!(count(db, "post_types").await, 0);
    assert_eq!(
        registry.all_post_types(),
        [book(Origin::Code, "Code books")]
    );
    // With nothing stored, the code's definition stays locked.
    let err = registry
        .delete_admin_definition(db, DefinitionKind::PostType, "book")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RegistryError::Definition(DefinitionError::Locked(Origin::Code))
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn concurrent_admin_writes_leave_memory_as_stored() {
    let test = database().await;
    let db = test.db();
    migrate(db).await;
    let registry = Arc::new(registry());
    let mut tasks = Vec::new();
    for round in 0..8 {
        let registry = Arc::clone(&registry);
        let db = test.shared();
        tasks.push(tokio::spawn(async move {
            let name = format!("Books {round}");
            registry
                .save_admin_definition(&db, book(Origin::Admin, &name))
                .await
                .unwrap();
            if round % 3 == 0 {
                registry
                    .delete_admin_definition(&db, DefinitionKind::PostType, "book")
                    .await
                    .unwrap();
            }
            if round % 4 == 0 {
                registry.load_from_db(&db).await.unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let stored = loaded_registry(db).await;
    assert_eq!(registry.all_post_types(), stored.all_post_types());
    assert_eq!(
        registry.post_types().registered("book"),
        stored.post_types().registered("book")
    );
}
