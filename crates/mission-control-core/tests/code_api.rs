//! Tests for the code API (Task 5.5): the post type, taxonomy and field group builders, and
//! the typed `get_field`. The builder tests need no database; the `get_field` tests run on the
//! database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use mission_control_core::content::{NewPost, Post, PostStatus, set_post_meta, set_post_meta_raw};
use mission_control_core::migrations::migrator;
use mission_control_core::types::code::{Field, FieldGroup, Layout, PostType, Taxonomy};
use mission_control_core::types::{
    CapabilityType, DefinitionError, FieldError, FieldGroupDef, HideOnScreen, LocationOperator,
    LocationParam, Origin, Position, PostTypeCapabilities, PostTypeDef, Registry, Rewrite, Support,
    TaxonomyCapabilities, TaxonomyDef, get_field,
};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use serde::Deserialize;
use serde_json::json;

// ---------------------------------------------------------------------------------------------
// Builders

#[test]
fn post_type_builder_equals_the_json_definition() {
    let built = PostType::new("event")
        .labels("Events", "Event")
        .label("menu_name", "Calendar")
        .description("Things that happen")
        .public(true)
        .hierarchical(false)
        .show_in_menu(true)
        .menu_icon("dashicons-calendar")
        .menu_position(5)
        .show_in_rest(true)
        .rest_base("events")
        .archive_slug("calendar")
        .rewrite_slug("events")
        .supports([Support::Title, Support::Editor])
        .support(Support::Thumbnail)
        .support(Support::Thumbnail)
        .taxonomy("genre")
        .field_group("group_event")
        .setting("show_in_admin_bar", json!(false))
        .build()
        .expect("builds the post type");

    let json = PostTypeDef::from_json(
        r#"{
            "slug": "event",
            "labels": {"name": "Events", "singular_name": "Event", "menu_name": "Calendar"},
            "description": "Things that happen",
            "public": true,
            "hierarchical": false,
            "show_in_menu": true,
            "menu_icon": "dashicons-calendar",
            "menu_position": 5,
            "show_in_rest": true,
            "rest_base": "events",
            "has_archive": "calendar",
            "rewrite": {"slug": "events"},
            "supports": ["title", "editor", "thumbnail"],
            "taxonomies": ["genre"],
            "field_groups": ["group_event"],
            "show_in_admin_bar": false
        }"#,
        Origin::Code,
    )
    .expect("reads the post type");

    assert_eq!(built, json);
    assert_eq!(built.origin, Origin::Code);
    assert!(built.is_locked());
}

/// A post type read from `json`, as code would load it.
fn post_type_json(json: &str) -> PostTypeDef {
    PostTypeDef::from_json(json, Origin::Code).expect("reads the post type")
}

#[test]
fn post_type_builder_keeps_wordpress_defaults_and_derives_labels() {
    assert_eq!(
        PostType::new("event").build().unwrap(),
        post_type_json(
            r#"{"slug": "event", "labels": {"name": "Events", "singular_name": "Event"}}"#
        )
    );
    assert_eq!(
        PostType::new("case_study").build().unwrap(),
        post_type_json(
            r#"{"slug": "case_study",
                "labels": {"name": "Case Studies", "singular_name": "Case Study"}}"#
        )
    );
    assert_eq!(
        PostType::new("book")
            .labels("Books", "Book")
            .show_under_menu("tools.php")
            .has_archive(true)
            .rewrite(Rewrite::Enabled(false))
            .build()
            .unwrap(),
        post_type_json(
            r#"{"slug": "book", "labels": {"name": "Books", "singular_name": "Book"},
                "show_in_menu": "tools.php", "has_archive": true, "rewrite": false}"#
        )
    );
}

#[test]
fn post_type_supports_read_as_wordpress_reads_them() {
    let labels = r#""labels": {"name": "Events", "singular_name": "Event"}"#;
    // An empty list is WordPress's default, title and editor.
    let empty = PostType::new("event").supports([]).build().unwrap();
    assert_eq!(
        empty,
        post_type_json(&format!(r#"{{"slug": "event", {labels}, "supports": []}}"#))
    );
    assert_eq!(empty.supports, [Support::Title, Support::Editor]);
    // `supports => false` is none.
    let none = PostType::new("event").no_supports().build().unwrap();
    assert_eq!(
        none,
        post_type_json(&format!(
            r#"{{"slug": "event", {labels}, "supports": false}}"#
        ))
    );
    assert!(none.supports.is_empty());
}

#[test]
fn post_type_builder_sets_the_visibility_and_capability_options() {
    let built = PostType::new("story")
        .labels("Stories", "Story")
        .exclude_from_search(true)
        .publicly_queryable(false)
        .show_ui(true)
        .show_in_nav_menus(false)
        .capability_type(CapabilityType {
            singular: "story".into(),
            plural: "stories".into(),
        })
        .capabilities(PostTypeCapabilities {
            publish_posts: Some("publish_stories".into()),
            ..PostTypeCapabilities::default()
        })
        .build()
        .unwrap();
    assert_eq!(
        built,
        post_type_json(
            r#"{"slug": "story", "labels": {"name": "Stories", "singular_name": "Story"},
                "exclude_from_search": true, "publicly_queryable": false, "show_ui": true,
                "show_in_nav_menus": false, "capability_type": ["story", "stories"],
                "capabilities": {"publish_posts": "publish_stories"}}"#
        )
    );
}

#[test]
fn post_type_builder_refuses_what_validation_refuses() {
    assert_eq!(
        PostType::new("page").build(),
        Err(DefinitionError::ReservedSlug {
            what: "post type",
            slug: "page".into()
        })
    );
    assert!(matches!(
        PostType::new("Event").build(),
        Err(DefinitionError::InvalidSlug { what: "post type", slug, .. }) if slug == "Event"
    ));
    assert_eq!(
        PostType::new("event")
            .setting("public", json!(true))
            .build(),
        Err(DefinitionError::ReservedSetting {
            key: "event".into(),
            setting: "public".into()
        })
    );
}

#[test]
fn taxonomy_builder_equals_the_json_definition() {
    let built = Taxonomy::new("genre")
        .object_type("event")
        .labels("Genres", "Genre")
        .label("menu_name", "Kinds")
        .description("Event genres")
        .public(false)
        .publicly_queryable(true)
        .hierarchical(true)
        .show_ui(true)
        .show_in_menu(false)
        .show_in_nav_menus(false)
        .show_tagcloud(false)
        .show_in_quick_edit(true)
        .show_admin_column(true)
        .show_in_rest(true)
        .rest_base("genres")
        .rewrite_slug("genres")
        .capabilities(TaxonomyCapabilities {
            assign_terms: Some("edit_events".into()),
            ..TaxonomyCapabilities::default()
        })
        .setting("sort", json!(true))
        .build()
        .unwrap();
    let json = TaxonomyDef::from_json(
        r#"{
            "slug": "genre",
            "object_type": ["event"],
            "labels": {"name": "Genres", "singular_name": "Genre", "menu_name": "Kinds"},
            "description": "Event genres",
            "public": false,
            "publicly_queryable": true,
            "hierarchical": true,
            "show_ui": true,
            "show_in_menu": false,
            "show_in_nav_menus": false,
            "show_tagcloud": false,
            "show_in_quick_edit": true,
            "show_admin_column": true,
            "show_in_rest": true,
            "rest_base": "genres",
            "rewrite": {"slug": "genres"},
            "capabilities": {"assign_terms": "edit_events"},
            "sort": true
        }"#,
        Origin::Code,
    )
    .unwrap();
    assert_eq!(built, json);

    let derived = Taxonomy::new("event_category").build().unwrap();
    assert_eq!(derived.labels.name, "Event Categories");
    assert_eq!(derived.labels.singular_name, "Event Category");
}

#[test]
fn field_group_builder_equals_the_json_definition() {
    let built = FieldGroup::new("event")
        .title("Event details")
        .field(
            Field::text("venue")
                .required(true)
                .instructions("Where it is"),
        )
        .field(Field::number("seats").default_value(json!(100)))
        .field(Field::number("price").step("0.01"))
        .field(
            Field::select("kind")
                .choice("talk", "Talk")
                .choice("workshop", "Workshop"),
        )
        .field(Field::true_false("free"))
        .field(Field::date_picker("starts_on").setting("return_format", json!("Y-m-d")))
        .field(
            Field::repeater("speakers")
                .sub_field(Field::text("name"))
                .sub_field(Field::url("website")),
        )
        .field(
            Field::flexible_content("sections")
                .layout(Layout::new("quote").field(Field::textarea("text"))),
        )
        .location(LocationParam::PostType, LocationOperator::Is, "event")
        .and_location(
            LocationParam::PostTemplate,
            LocationOperator::IsNot,
            "x.php",
        )
        .location(LocationParam::PostType, LocationOperator::Is, "page")
        .position(Position::Side)
        .menu_order(2)
        .hide_on_screen(HideOnScreen::Comments)
        .show_in_rest(true)
        .build()
        .expect("builds the group");

    let json = FieldGroupDef::from_json(
        r#"{
            "key": "group_event",
            "title": "Event details",
            "fields": [
                {"key": "field_event_venue", "name": "venue", "label": "Venue", "type": "text",
                 "required": 1, "instructions": "Where it is"},
                {"key": "field_event_seats", "name": "seats", "label": "Seats", "type": "number",
                 "default_value": 100},
                {"key": "field_event_price", "name": "price", "label": "Price", "type": "number",
                 "step": "0.01"},
                {"key": "field_event_kind", "name": "kind", "label": "Kind", "type": "select",
                 "choices": {"talk": "Talk", "workshop": "Workshop"}},
                {"key": "field_event_free", "name": "free", "label": "Free", "type": "true_false"},
                {"key": "field_event_starts_on", "name": "starts_on", "label": "Starts On",
                 "type": "date_picker", "return_format": "Y-m-d"},
                {"key": "field_event_speakers", "name": "speakers", "label": "Speakers",
                 "type": "repeater", "sub_fields": [
                    {"key": "field_event_speakers_name", "name": "name", "label": "Name",
                     "type": "text"},
                    {"key": "field_event_speakers_website", "name": "website",
                     "label": "Website", "type": "url"}
                 ]},
                {"key": "field_event_sections", "name": "sections", "label": "Sections",
                 "type": "flexible_content", "layouts": {
                    "layout_event_sections_quote": {"key": "layout_event_sections_quote",
                     "name": "quote", "label": "Quote", "sub_fields": [
                        {"key": "field_event_sections_quote_text", "name": "text",
                         "label": "Text", "type": "textarea"}
                     ]}
                 }}
            ],
            "location": [
                [{"param": "post_type", "operator": "==", "value": "event"},
                 {"param": "post_template", "operator": "!=", "value": "x.php"}],
                [{"param": "post_type", "operator": "==", "value": "page"}]
            ],
            "position": "side",
            "menu_order": 2,
            "hide_on_screen": ["comments"],
            "show_in_rest": 1
        }"#,
        Origin::Code,
    )
    .expect("reads the group");

    assert_eq!(built, json);
    assert!(built.is_locked());
}

#[test]
fn field_group_builder_takes_explicit_keys_and_derives_titles() {
    let built = FieldGroup::new("event")
        .key("group_5f1a")
        .field(Field::text("venue").key("field_5f1b").label("Where"))
        .field(Field::text("starts_on"))
        .build()
        .unwrap();
    assert_eq!(built.key, "group_5f1a");
    assert_eq!(built.title, "Event");
    assert_eq!(built.fields[0].key, "field_5f1b");
    assert_eq!(built.fields[0].label, "Where");
    // Made from the builder's name whatever the group key, as ACF Composer makes them, so
    // two `event` groups with different keys share field keys.
    assert_eq!(built.fields[1].key, "field_event_starts_on");
    assert_eq!(built.fields[1].label, "Starts On");

    let other = FieldGroup::new("event")
        .key("group_6a2b")
        .field(Field::text("starts_on"))
        .build()
        .unwrap();
    assert_eq!(other.fields[0].key, "field_event_starts_on");

    assert_eq!(
        FieldGroup::new("event_details").build().unwrap().title,
        "Event Details"
    );
}

#[test]
fn field_group_builder_refuses_bad_definitions() {
    assert_eq!(
        FieldGroup::new("event")
            .field(Field::text("venue"))
            .field(Field::textarea("venue"))
            .build(),
        Err(DefinitionError::DuplicateFieldName("venue".into()))
    );
    assert_eq!(
        FieldGroup::new("event")
            .field(Field::text("bad name"))
            .build(),
        Err(DefinitionError::InvalidFieldName("bad name".into()))
    );
    assert_eq!(
        FieldGroup::new("event")
            .field(Field::text("venue").choice("a", "A"))
            .build(),
        Err(DefinitionError::MisplacedOption {
            key: "field_event_venue".into(),
            option: "choices"
        })
    );
}

#[test]
fn generated_keys_that_collide_or_are_too_long_are_named() {
    // `a_b`, and the sub-field `b` of `a`, both make `field_g_a_b`.
    assert_eq!(
        FieldGroup::new("g")
            .field(Field::text("a_b"))
            .field(Field::repeater("a").sub_field(Field::text("b")))
            .build(),
        Err(DefinitionError::GeneratedKey {
            key: "field_g_a_b".into(),
            reason: "is used twice"
        })
    );
    let long = "x".repeat(30);
    let err = FieldGroup::new("g")
        .field(Field::repeater(&long).sub_field(Field::text(&long)))
        .build()
        .unwrap_err();
    assert_eq!(
        err,
        DefinitionError::GeneratedKey {
            key: format!("field_g_{long}_{long}"),
            reason: "is longer than the 64-byte limit"
        }
    );
    assert!(err.to_string().contains(".key()"), "{err}");
    // An explicit key fixes it.
    assert!(
        FieldGroup::new("g")
            .field(Field::repeater(&long).sub_field(Field::text(&long).key("field_short")))
            .build()
            .is_ok()
    );

    // A group key made from a long name is blamed as made; a key set by hand avoids it.
    let name = "g".repeat(60);
    assert!(FieldGroup::new(&name).key("group_g").build().is_ok());
    assert_eq!(
        FieldGroup::new(&name).title("G").build(),
        Err(DefinitionError::GeneratedKey {
            key: format!("group_{name}"),
            reason: "is longer than the 64-byte limit"
        })
    );
    // A key set by hand that is too long is validation's error, not a made one.
    assert_eq!(
        FieldGroup::new("g")
            .field(Field::text("a").key(&format!("field_{}", "a".repeat(70))))
            .build(),
        Err(DefinitionError::TooLarge {
            what: "key length",
            limit: 64
        })
    );
}

// ---------------------------------------------------------------------------------------------
// get_field

/// Runs the core migrations and returns a new post of `post_type`.
async fn setup(db: &Database, post_type: &str) -> Post {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    let author: i64 = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the user id");
    Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "An event".into(),
            ..NewPost::new(author, post_type)
        },
    )
    .await
    .expect("creates the post")
}

/// A registry with an `event` group (`seats` defaults to 100, `venue` to "Main hall", `notes`
/// to ACF's empty default), a group for pages only (`color` defaults to "red"), and two groups
/// that both define `size`: `small` (menu order 1) and `large` (menu order 0).
fn registry() -> Registry {
    let registry = Registry::default();
    let groups = [
        FieldGroup::new("event")
            .field(Field::number("seats").default_value(json!(100)))
            .field(Field::text("venue").default_value(json!("Main hall")))
            .field(Field::text("notes").default_value(json!("")))
            .location(LocationParam::PostType, LocationOperator::Is, "event"),
        FieldGroup::new("page")
            .field(Field::text("color").default_value(json!("red")))
            .location(LocationParam::PostType, LocationOperator::Is, "page"),
        FieldGroup::new("small")
            .menu_order(1)
            .field(Field::text("size").default_value(json!("small"))),
        FieldGroup::new("large").field(Field::text("size").default_value(json!("large"))),
    ];
    for group in groups {
        registry.register(group.build().unwrap()).unwrap();
    }
    registry
}

/// Inserts a `post_meta` row with `text` as the stored value, as written.
async fn insert_meta(db: &Database, post_id: i64, key: &str, text: &str) {
    raw::execute(
        db,
        "INSERT INTO post_meta (post_id, meta_key, meta_value) VALUES (?, ?, ?)",
        &[post_id.into(), key.into(), text.into()],
    )
    .await
    .expect("inserts the meta row");
}

#[tokio::test]
async fn get_field_reads_a_text_field() {
    let test = database().await;
    let db = test.db();
    let registry = Registry::default();
    let post = setup(db, "event").await;
    set_post_meta(db, post.id, "venue", &json!("Main hall"))
        .await
        .unwrap();

    let venue = get_field::<String>(db, &registry, post.id, "venue")
        .await
        .unwrap();
    assert_eq!(venue.as_deref(), Some("Main hall"));
    // A post works as well as its id.
    let venue = get_field::<String>(db, &registry, &post, "venue")
        .await
        .unwrap();
    assert_eq!(venue.as_deref(), Some("Main hall"));
}

#[tokio::test]
async fn get_field_on_the_wrong_type_is_a_type_error() {
    let test = database().await;
    let db = test.db();
    let registry = Registry::default();
    let post = setup(db, "event").await;
    set_post_meta(db, post.id, "venue", &json!("Main hall"))
        .await
        .unwrap();

    let err = get_field::<i64>(db, &registry, post.id, "venue")
        .await
        .unwrap_err();
    match &err {
        FieldError::Type {
            field, expected, ..
        } => {
            assert_eq!(field, "venue");
            assert_eq!(*expected, "i64");
        }
        other => panic!("expected a type error, got {other:?}"),
    }
    let message = err.to_string();
    assert!(
        message.contains("venue") && message.contains("i64"),
        "{message}"
    );
}

#[tokio::test]
async fn get_field_on_a_missing_value_is_none() {
    let test = database().await;
    let db = test.db();
    let registry = Registry::default();
    let post = setup(db, "event").await;

    assert_eq!(
        get_field::<String>(db, &registry, post.id, "venue")
            .await
            .unwrap(),
        None
    );
    // ACF returns null for a stored null as well.
    set_post_meta(db, post.id, "venue", &json!(null))
        .await
        .unwrap();
    assert_eq!(
        get_field::<String>(db, &registry, post.id, "venue")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn get_field_gives_the_default_of_the_field_by_name() {
    let test = database().await;
    let db = test.db();
    let registry = registry();
    let post = setup(db, "event").await;

    // Nothing stored: the default, by post or by id.
    assert_eq!(
        get_field::<i64>(db, &registry, &post, "seats")
            .await
            .unwrap(),
        Some(100)
    );
    assert_eq!(
        get_field::<i64>(db, &registry, post.id, "seats")
            .await
            .unwrap(),
        Some(100)
    );
    // ACF's empty default is none.
    assert_eq!(
        get_field::<String>(db, &registry, &post, "notes")
            .await
            .unwrap(),
        None
    );
    // The default is decoded as a stored value is.
    assert!(matches!(
        get_field::<i64>(db, &registry, &post, "venue").await,
        Err(FieldError::Type { .. })
    ));
    // Found by name in any group, as `acf_get_field($name)`: the page group's field gives its
    // default on an event too.
    assert_eq!(
        get_field::<String>(db, &registry, &post, "color")
            .await
            .unwrap()
            .as_deref(),
        Some("red")
    );
    // Two groups define `size`: the first in ACF's order (menu order, then title) wins.
    assert_eq!(
        get_field::<String>(db, &registry, &post, "size")
            .await
            .unwrap()
            .as_deref(),
        Some("large")
    );
    // A name no group defines has no default.
    assert_eq!(
        get_field::<String>(db, &registry, &post, "nothing")
            .await
            .unwrap(),
        None
    );

    // A stored value wins over the default, and so does a stored null.
    set_post_meta(db, post.id, "seats", &json!(5))
        .await
        .unwrap();
    assert_eq!(
        get_field::<i64>(db, &registry, &post, "seats")
            .await
            .unwrap(),
        Some(5)
    );
    set_post_meta(db, post.id, "seats", &json!(null))
        .await
        .unwrap();
    assert_eq!(
        get_field::<i64>(db, &registry, &post, "seats")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn get_field_gives_a_default_as_its_kind_stores_it() {
    let test = database().await;
    let db = test.db();
    let registry = Registry::default();
    let group = FieldGroupDef::from_json(
        r#"{"key": "group_acf", "title": "From ACF", "fields": [
            {"key": "field_free", "name": "free", "label": "Free", "type": "true_false",
             "default_value": 0},
            {"key": "field_open", "name": "open", "label": "Open", "type": "true_false",
             "default_value": "1"},
            {"key": "field_seats", "name": "seats", "label": "Seats", "type": "number",
             "default_value": "100"},
            {"key": "field_price", "name": "price", "label": "Price", "type": "number",
             "step": "0.01", "default_value": "9.5"},
            {"key": "field_kind", "name": "kind", "label": "Kind", "type": "select",
             "choices": {"talk": "Talk"}, "default_value": false},
            {"key": "field_track", "name": "track", "label": "Track", "type": "select",
             "choices": {"main": "Main"}, "default_value": [], "multiple": 0},
            {"key": "field_room", "name": "room", "label": "Room", "type": "select",
             "choices": {"a": "A"}, "default_value": ["a"], "multiple": 0},
            {"key": "field_rooms", "name": "rooms", "label": "Rooms", "type": "select",
             "choices": {"a": "A"}, "default_value": ["a"], "multiple": 1}
        ]}"#,
        Origin::Code,
    )
    .unwrap();
    registry.register(group).unwrap();
    let post = setup(db, "event").await;

    assert_eq!(
        get_field::<bool>(db, &registry, &post, "free")
            .await
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        get_field::<bool>(db, &registry, &post, "open")
            .await
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        get_field::<i64>(db, &registry, &post, "seats")
            .await
            .unwrap(),
        Some(100)
    );
    assert_eq!(
        get_field::<f64>(db, &registry, &post, "price")
            .await
            .unwrap(),
        Some(9.5)
    );
    assert_eq!(
        get_field::<String>(db, &registry, &post, "kind")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        get_field::<String>(db, &registry, &post, "track")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        get_field::<String>(db, &registry, &post, "room")
            .await
            .unwrap(),
        Some("a".to_owned())
    );
    assert_eq!(
        get_field::<Vec<String>>(db, &registry, &post, "rooms")
            .await
            .unwrap(),
        Some(vec!["a".to_owned()])
    );
}

#[tokio::test]
async fn get_field_reads_the_first_of_repeated_values() {
    let test = database().await;
    let db = test.db();
    let registry = Registry::default();
    let post = setup(db, "event").await;
    insert_meta(db, post.id, "venue", r#""First hall""#).await;
    insert_meta(db, post.id, "venue", r#""Second hall""#).await;

    assert_eq!(
        get_field::<String>(db, &registry, &post, "venue")
            .await
            .unwrap()
            .as_deref(),
        Some("First hall")
    );
}

#[tokio::test]
async fn get_field_on_stored_text_that_is_not_json_is_invalid_json() {
    let test = database().await;
    let db = test.db();
    let registry = Registry::default();
    let post = setup(db, "event").await;
    insert_meta(db, post.id, "venue", "Main hall, not JSON").await;

    assert!(matches!(
        get_field::<String>(db, &registry, &post, "venue").await,
        Err(FieldError::InvalidJson(_))
    ));
}

#[tokio::test]
async fn get_field_decodes_structured_values() {
    #[derive(Debug, PartialEq, Deserialize)]
    struct Speaker {
        name: String,
        website: Option<String>,
    }

    let test = database().await;
    let db = test.db();
    let registry = Registry::default();
    let post = setup(db, "event").await;
    set_post_meta_raw(
        db,
        post.id,
        "speakers",
        r#"[{"name": "Ada", "website": "https://ada.dev"}, {"name": "Grace"}]"#,
    )
    .await
    .unwrap();
    set_post_meta(db, post.id, "free", &json!(true))
        .await
        .unwrap();

    let speakers = get_field::<Vec<Speaker>>(db, &registry, &post, "speakers")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        speakers,
        [
            Speaker {
                name: "Ada".into(),
                website: Some("https://ada.dev".into())
            },
            Speaker {
                name: "Grace".into(),
                website: None
            },
        ]
    );
    assert_eq!(
        get_field::<bool>(db, &registry, &post, "free")
            .await
            .unwrap(),
        Some(true)
    );
}
