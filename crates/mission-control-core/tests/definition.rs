//! The definition format of custom post types, taxonomies and field groups (Task 5.1).

use mission_control_core::rest::posts::RestTaxonomy;
use mission_control_core::types::{
    CapabilityType, DefinitionError, FieldDef, FieldGroupDef, FieldKindDef, HasArchive,
    HideOnScreen, LayoutDef, LocationOperator, LocationParam, LocationRule, MAX_CHOICES,
    MAX_FIELD_DEPTH, MAX_FIELDS, MAX_JSON_BYTES, MAX_LAYOUTS, MAX_LOCATION_RULES, MAX_TEXT_LEN,
    Origin, Position, PostTypeDef, Rewrite, RewriteRules, ShowInMenu, Support, TaxonomyDef,
};
use mission_control_core::users::{PostTypeCaps, TaxonomyCaps};
use serde_json::{Value, json};

/// A post type with every option set.
fn book() -> PostTypeDef {
    let mut def = PostTypeDef::new("book", "Books", "Book");
    def.labels.add_new_item = Some("Add New Book".to_owned());
    def.description = "Books we sell.".to_owned();
    def.public = true;
    def.hierarchical = true;
    def.show_ui = Some(true);
    def.show_in_menu = Some(ShowInMenu::Under("tools.php".to_owned()));
    def.menu_icon = Some("dashicons-book".to_owned());
    def.menu_position = Some(25);
    def.show_in_rest = true;
    def.rest_base = Some("books".to_owned());
    def.has_archive = HasArchive::Slug("library".to_owned());
    def.rewrite = Rewrite::Rules(RewriteRules {
        slug: Some("books".to_owned()),
        with_front: Some(false),
        ..RewriteRules::default()
    });
    def.supports = vec![
        Support::Title,
        Support::Editor,
        Support::Thumbnail,
        Support::CustomFields,
        Support::PageAttributes,
    ];
    def.taxonomies = vec!["genre".to_owned(), "category".to_owned()];
    def.field_groups = vec!["group_book_details".to_owned()];
    def.capability_type = CapabilityType {
        singular: "book".to_owned(),
        plural: "bookies".to_owned(),
    };
    def.origin = Origin::Plugin("shop".to_owned());
    def
}

/// A field group with a field of each structured kind.
fn details() -> FieldGroupDef {
    let mut group = FieldGroupDef::new("group_book_details", "Book details");
    let mut isbn = FieldDef::new("field_isbn", "isbn", "ISBN", FieldKindDef::Text);
    isbn.required = true;
    isbn.instructions = "Thirteen digits.".to_owned();
    isbn.settings
        .insert("placeholder".to_owned(), json!("978…"));
    let mut price = FieldDef::new(
        "field_price",
        "price",
        "Price",
        FieldKindDef::Decimal("0.01".to_owned()),
    );
    price.default_value = json!("9.99");
    let format = FieldDef::new(
        "field_format",
        "format",
        "Format",
        FieldKindDef::Select(vec![
            ("paper".to_owned(), "Paperback".to_owned()),
            ("hard".to_owned(), "Hardcover".to_owned()),
            ("audio".to_owned(), "Audiobook".to_owned()),
        ]),
    );
    let chapters = FieldDef::new(
        "field_chapters",
        "chapters",
        "Chapters",
        FieldKindDef::Repeater(Vec::new()),
    )
    .with_sub_fields(vec![
        FieldDef::new("field_chapter_title", "title", "Title", FieldKindDef::Text),
        FieldDef::new(
            "field_chapter_pages",
            "pages",
            "Pages",
            FieldKindDef::Number,
        ),
    ]);
    let sections = FieldDef::new(
        "field_sections",
        "sections",
        "Sections",
        FieldKindDef::FlexibleContent(Vec::new()),
    )
    .with_layouts(vec![LayoutDef {
        key: "layout_quote".to_owned(),
        name: "quote".to_owned(),
        label: "Quote".to_owned(),
        sub_fields: vec![FieldDef::new(
            "field_quote_text",
            "text",
            "Text",
            FieldKindDef::Textarea,
        )],
        settings: serde_json::Map::new(),
    }]);
    let mut reused = FieldDef::new("field_seo", "seo", "SEO", FieldKindDef::Clone(Vec::new()));
    reused.clone = vec!["group_seo".to_owned()];
    group.fields = vec![isbn, price, format, chapters, sections, reused];
    group.location = vec![
        vec![LocationRule {
            param: LocationParam::PostType,
            operator: LocationOperator::Is,
            value: "book".to_owned(),
        }],
        vec![
            LocationRule {
                param: LocationParam::PageTemplate,
                operator: LocationOperator::Is,
                value: "book.html".to_owned(),
            },
            LocationRule {
                param: LocationParam::CurrentUserRole,
                operator: LocationOperator::IsNot,
                value: "subscriber".to_owned(),
            },
        ],
    ];
    group.menu_order = 3;
    group.position = Position::Side;
    group.hide_on_screen = vec![HideOnScreen::Content, HideOnScreen::SendTrackbacks];
    group.origin = Origin::Code;
    group.settings.insert("style".to_owned(), json!("seamless"));
    group
}

#[test]
fn post_type_round_trips_through_json() {
    let def = book();
    def.validate().unwrap();
    let json = serde_json::to_string(&def).unwrap();
    let back = PostTypeDef::from_json(&json, def.origin.clone()).unwrap();
    assert_eq!(back, def);
}

#[test]
fn post_type_json_uses_wordpress_names() {
    let value = serde_json::to_value(book()).unwrap();
    assert_eq!(value["slug"], "book");
    assert_eq!(value["labels"]["singular_name"], "Book");
    assert_eq!(value["labels"]["add_new_item"], "Add New Book");
    assert_eq!(value["show_in_menu"], "tools.php");
    assert_eq!(value["has_archive"], "library");
    assert_eq!(
        value["rewrite"],
        json!({"slug": "books", "with_front": false})
    );
    assert_eq!(
        value["supports"],
        json!([
            "title",
            "editor",
            "thumbnail",
            "custom-fields",
            "page-attributes"
        ])
    );
    assert_eq!(value["capability_type"], json!(["book", "bookies"]));
    // The origin is never written: the loader sets it.
    assert!(value.get("origin").is_none());
}

#[test]
fn post_type_takes_wordpress_defaults_and_php_values() {
    // As ACF exports a post type: `post_type`, booleans as "1", empty strings for unset.
    let def = PostTypeDef::from_json(
        r#"{"post_type": "movie", "public": "1", "show_in_rest": 1,
            "menu_position": "", "rest_base": "", "menu_icon": "",
            "labels": {"name": "Movies", "singular_name": "Movie", "all_items": ""}}"#,
        Origin::Admin,
    )
    .unwrap();
    assert_eq!(def.slug, "movie");
    assert!(def.public && def.show_in_rest && !def.hierarchical);
    assert_eq!(def.menu_position, None);
    assert_eq!(def.labels.all_items, None);
    assert_eq!(def.supports, [Support::Title, Support::Editor]);
    assert_eq!(def.capability_type, CapabilityType::new("post"));
    assert_eq!(def.origin, Origin::Admin);
    assert_eq!(def.effective_rest_base(), "movie");
    assert_eq!(def.effective_rewrite_slug(), Some("movie"));
    assert!(def.effective_show_ui());
    assert_eq!(def.effective_show_in_menu(), ShowInMenu::Show(true));

    // `supports: false` is none, and is written back as `false`.
    let none =
        PostTypeDef::from_json(r#"{"slug": "log", "supports": false}"#, Origin::Admin).unwrap();
    assert!(none.supports.is_empty());
    assert_eq!(serde_json::to_value(&none).unwrap()["supports"], false);
    // An empty list gets the defaults, as `register_post_type` gives them.
    let empty =
        PostTypeDef::from_json(r#"{"slug": "log", "supports": []}"#, Origin::Admin).unwrap();
    assert_eq!(empty.supports, [Support::Title, Support::Editor]);
}

#[test]
fn unknown_support_is_refused_by_name() {
    let err = PostTypeDef::from_json(
        r#"{"slug": "book", "supports": ["title", "sparkles"]}"#,
        Origin::Admin,
    )
    .unwrap_err();
    let DefinitionError::Json(message) = &err else {
        panic!("expected a JSON error, got {err:?}");
    };
    assert!(
        message.contains("unknown post type support `sparkles`"),
        "{message}"
    );

    let err = "sparkles".parse::<Support>().unwrap_err();
    assert_eq!(
        err,
        DefinitionError::Unknown {
            what: "post type support",
            value: "sparkles".to_owned()
        }
    );
    assert_eq!(
        "custom-fields".parse::<Support>().unwrap(),
        Support::CustomFields
    );
    assert_eq!(Support::ALL.len(), 11);
}

#[test]
fn reserved_and_invalid_post_type_slugs_are_refused() {
    for slug in [
        "post",
        "page",
        "attachment",
        "wp_block",
        "action",
        "author",
        "order",
        "theme",
    ] {
        let err = PostTypeDef::new(slug, "X", "X").validate().unwrap_err();
        assert_eq!(
            err,
            DefinitionError::ReservedSlug {
                what: "post type",
                slug: slug.to_owned()
            }
        );
    }
    for slug in ["", "Book", "my book", "book!", "a_very_long_post_type"] {
        let err = PostTypeDef::new(slug, "X", "X").validate().unwrap_err();
        assert!(
            matches!(
                err,
                DefinitionError::InvalidSlug {
                    what: "post type",
                    ..
                }
            ),
            "{slug:?}: {err:?}"
        );
    }
    // Twenty characters is the limit.
    PostTypeDef::new("a_post_type_of_20_ch", "X", "X")
        .validate()
        .unwrap();
    PostTypeDef::new("book-2_x", "X", "X").validate().unwrap();

    let mut def = PostTypeDef::new("book", "Books", "Book");
    def.field_groups = vec!["details".to_owned()];
    assert!(matches!(
        def.validate(),
        Err(DefinitionError::InvalidKey {
            prefix: "group_",
            ..
        })
    ));
}

#[test]
fn origin_code_and_plugin_are_locked() {
    let mut def = PostTypeDef::new("book", "Books", "Book");
    assert!(!def.is_locked());
    def.ensure_editable().unwrap();

    def.origin = Origin::Code;
    assert!(def.is_locked());
    assert_eq!(
        def.ensure_editable(),
        Err(DefinitionError::Locked(Origin::Code))
    );

    // The origin is the loader's, never the JSON's: a definition that claims "admin" but is
    // loaded from code stays locked.
    let taxonomy =
        TaxonomyDef::from_json(r#"{"slug": "genre", "origin": "admin"}"#, Origin::Code).unwrap();
    assert!(taxonomy.is_locked());
    assert!(taxonomy.ensure_editable().is_err());
    assert!(!taxonomy.settings.contains_key("origin"));
    let def =
        PostTypeDef::from_json(r#"{"slug": "book", "origin": "admin"}"#, Origin::Code).unwrap();
    assert_eq!(def.origin, Origin::Code);
    // A plugin cannot claim another plugin's name.
    let shop = Origin::Plugin("shop".to_owned());
    let group = FieldGroupDef::from_json(
        r#"{"key": "group_a", "title": "A", "origin": {"plugin": "other"}}"#,
        shop.clone(),
    )
    .unwrap();
    assert_eq!(group.origin, shop);
    assert!(!group.settings.contains_key("origin"));
    // Read through serde without a loader, a definition is locked.
    let raw: PostTypeDef = serde_json::from_str(r#"{"slug": "book", "origin": "admin"}"#).unwrap();
    assert_eq!(raw.origin, Origin::Code);

    let group = details();
    assert!(group.is_locked());
    let plugin = Origin::Plugin("shop".to_owned());
    assert!(plugin.is_locked());
    assert!(
        plugin
            .ensure_editable()
            .unwrap_err()
            .to_string()
            .contains("shop")
    );

    let err =
        PostTypeDef::from_json(r#"{"slug": "book"}"#, Origin::Plugin(String::new())).unwrap_err();
    assert_eq!(err, DefinitionError::EmptyPluginName);
}

#[test]
fn post_type_converts_to_the_rest_descriptor_and_caps() {
    let def = book();
    let genre = RestTaxonomy {
        taxonomy: mission_control_core::content::Taxonomy::new(
            "genre",
            true,
            vec!["book".to_owned()],
        ),
        rest_base: "genres".into(),
    };
    let known = [RestTaxonomy::CATEGORIES, RestTaxonomy::TAGS, genre.clone()];
    let post_type = def.to_post_type(&known);
    assert_eq!(post_type.name, "book");
    assert_eq!(post_type.rest_base, "books");
    assert!(post_type.hierarchical && post_type.show_in_rest);
    assert!(post_type.supports.title && post_type.supports.editor);
    assert!(post_type.supports.thumbnail && post_type.supports.custom_fields);
    assert!(post_type.supports.page_attributes);
    assert!(!post_type.supports.author && !post_type.supports.comments);
    assert_eq!(post_type.taxonomies, [genre, RestTaxonomy::CATEGORIES]);
    assert_eq!(post_type.route(), "/mc/v2/books");

    let caps = def.caps();
    assert_eq!(caps.edit_posts, "edit_bookies");
    assert_eq!(caps.read_private_posts, "read_private_bookies");
    assert_eq!(caps.read, "read");

    // The default capability type gives WordPress's `post` names.
    let plain = PostTypeDef::new("movie", "Movies", "Movie");
    assert_eq!(plain.caps(), PostTypeCaps::for_capability_type("post"));
    assert_eq!(plain.to_post_type(&known).rest_base, "movie");
    assert!(plain.to_post_type(&known).taxonomies.is_empty());
}

#[test]
fn taxonomy_round_trips_and_converts() {
    let mut def = TaxonomyDef::new("genre", &["book"], "Genres", "Genre");
    def.labels.parent_item = Some("Parent Genre".to_owned());
    def.hierarchical = true;
    def.show_in_rest = true;
    def.show_admin_column = true;
    def.rest_base = Some("genres".to_owned());
    def.rewrite = Rewrite::Rules(RewriteRules {
        slug: Some("by-genre".to_owned()),
        hierarchical: Some(true),
        ..RewriteRules::default()
    });
    def.capabilities.assign_terms = Some("edit_books".to_owned());
    def.origin = Origin::Code;
    def.validate().unwrap();

    let json = serde_json::to_string(&def).unwrap();
    assert_eq!(
        TaxonomyDef::from_json(&json, def.origin.clone()).unwrap(),
        def
    );

    assert_eq!(def.effective_rest_base(), "genres");
    assert_eq!(def.effective_rewrite_slug(), Some("by-genre"));
    let caps = def.caps();
    let defaults = TaxonomyCaps::for_taxonomy("genre");
    assert_eq!(caps.assign_terms, "edit_books");
    assert_eq!(caps.manage_terms, defaults.manage_terms);

    // WordPress's defaults: public, flat, not in REST; ACF's `taxonomy` key.
    let plain = TaxonomyDef::from_json(
        r#"{"taxonomy": "mood", "object_type": ["post"]}"#,
        Origin::Admin,
    )
    .unwrap();
    assert_eq!(plain.slug, "mood");
    assert!(plain.public && !plain.hierarchical && !plain.show_in_rest);
    assert_eq!(plain.caps(), TaxonomyCaps::for_taxonomy("mood"));
}

#[test]
fn reserved_and_invalid_taxonomy_slugs_are_refused() {
    for slug in ["category", "post_tag", "nav_menu", "post_format"] {
        assert_eq!(
            TaxonomyDef::new(slug, &["post"], "X", "X").validate(),
            Err(DefinitionError::ReservedSlug {
                what: "taxonomy",
                slug: slug.to_owned()
            })
        );
    }
    let long = "a".repeat(33);
    for slug in ["", "Genre", "gen re", long.as_str()] {
        let err = TaxonomyDef::new(slug, &["post"], "X", "X")
            .validate()
            .unwrap_err();
        assert!(matches!(
            err,
            DefinitionError::InvalidSlug {
                what: "taxonomy",
                ..
            }
        ));
    }
    TaxonomyDef::new(&"a".repeat(32), &["post"], "X", "X")
        .validate()
        .unwrap();
    // An object type must be a post type slug; `post` itself is fine.
    assert!(matches!(
        TaxonomyDef::new("genre", &["Not A Type"], "X", "X").validate(),
        Err(DefinitionError::InvalidSlug {
            what: "post type",
            ..
        })
    ));
}

#[test]
fn field_group_round_trips_through_json() {
    let group = details();
    group.validate().unwrap();
    let json = serde_json::to_string(&group).unwrap();
    let back = FieldGroupDef::from_json(&json, group.origin.clone()).unwrap();
    assert_eq!(back, group);
}

#[test]
fn field_group_json_uses_acf_names() {
    let value = serde_json::to_value(details()).unwrap();
    let fields = value["fields"].as_array().unwrap();
    assert_eq!(fields[0]["type"], "text");
    assert_eq!(fields[0]["placeholder"], "978…");
    assert_eq!(fields[1]["type"], "number");
    assert_eq!(fields[1]["step"], "0.01");
    assert_eq!(fields[2]["type"], "select");
    // Choices are an object, in their order.
    assert_eq!(fields[2]["choices"]["hard"], "Hardcover");
    let text = serde_json::to_string(&details()).unwrap();
    let at = |s: &str| text.find(s).unwrap();
    assert!(at(r#""paper":"Paperback""#) < at(r#""hard":"Hardcover""#));
    assert!(at(r#""hard":"Hardcover""#) < at(r#""audio":"Audiobook""#));
    assert_eq!(fields[3]["type"], "repeater");
    assert_eq!(fields[3]["sub_fields"][1]["type"], "number");
    assert_eq!(fields[4]["type"], "flexible_content");
    assert_eq!(fields[4]["layouts"]["layout_quote"]["name"], "quote");
    assert_eq!(fields[5]["type"], "clone");
    assert_eq!(fields[5]["clone"], json!(["group_seo"]));
    assert_eq!(value["location"][1][1]["operator"], "!=");
    assert_eq!(value["location"][1][1]["param"], "current_user_role");
    assert_eq!(value["position"], "side");
    assert_eq!(
        value["hide_on_screen"],
        json!(["the_content", "send-trackbacks"])
    );
    assert_eq!(value["menu_order"], 3);
    assert_eq!(value["style"], "seamless");
    assert!(value.get("origin").is_none());
}

#[test]
fn field_group_reads_an_acf_export() {
    // As ACF exports a group: `0`/`1` booleans, `""` for empty values, choices as an object,
    // layouts keyed by layout key.
    let group = FieldGroupDef::from_json(
        r#"{
            "key": "group_6543", "title": "Event", "menu_order": 0, "position": "normal",
            "style": "default", "label_placement": "top", "hide_on_screen": "",
            "active": true, "description": "", "show_in_rest": 0,
            "location": [[{"param": "post_type", "operator": "==", "value": "event"}]],
            "fields": [
                {"key": "field_6543a", "label": "Starts", "name": "starts",
                 "type": "date_time_picker", "instructions": "", "required": 1,
                 "conditional_logic": 0, "wrapper": {"width": "", "class": "", "id": ""},
                 "display_format": "d/m/Y g:i a", "return_format": "Y-m-d H:i:s"},
                {"key": "field_6543b", "label": "Featured", "name": "featured",
                 "type": "true_false", "required": 0, "default_value": 0, "ui": 1},
                {"key": "field_6543c", "label": "Seats", "name": "seats",
                 "type": "number", "required": 0, "step": "", "min": "", "max": ""},
                {"key": "field_6543d", "label": "Body", "name": "body",
                 "type": "flexible_content", "button_label": "Add Row",
                 "layouts": {"layout_6543e": {"key": "layout_6543e", "name": "text",
                    "label": "Text", "display": "block", "sub_fields": [
                        {"key": "field_6543f", "label": "Text", "name": "text",
                         "type": "wysiwyg"}]}}}
            ]
        }"#,
        Origin::Admin,
    )
    .unwrap();
    assert!(group.hide_on_screen.is_empty());
    assert!(!group.show_in_rest);
    assert_eq!(group.position, Position::Normal);
    let starts = &group.fields[0];
    assert_eq!(starts.kind, FieldKindDef::DateTime);
    assert!(starts.required);
    assert_eq!(starts.settings["return_format"], "Y-m-d H:i:s");
    assert_eq!(group.fields[1].kind, FieldKindDef::Checkbox);
    assert_eq!(group.fields[1].default_value, json!(0));
    // A number without a step is a whole number; the empty step stays a setting.
    assert_eq!(group.fields[2].kind, FieldKindDef::Number);
    assert!(!group.fields[2].settings.contains_key("step"));
    let FieldKindDef::FlexibleContent(blocks) = &group.fields[3].kind else {
        panic!("expected flexible content");
    };
    assert_eq!(blocks[0].name, "text");
    assert_eq!(blocks[0].fields[0].kind, FieldKindDef::Wysiwyg);
    assert_eq!(group.fields[3].layouts[0].settings["display"], "block");
    assert_eq!(group.origin, Origin::Admin);
    assert_eq!(group.settings["label_placement"], "top");

    // The Ignite view of a field.
    let ignite = group.fields[0].to_ignite();
    assert_eq!(ignite.name, "starts");
    assert!(ignite.required);
}

#[test]
fn unknown_field_type_is_refused_by_name() {
    let err = FieldGroupDef::from_json(
        r#"{"key": "group_a", "title": "A", "fields": [
            {"key": "field_a", "name": "a", "label": "A", "type": "google_map"}]}"#,
        Origin::Admin,
    )
    .unwrap_err();
    let DefinitionError::Json(message) = &err else {
        panic!("expected a JSON error, got {err:?}");
    };
    assert!(
        message.contains("unknown field type `google_map`"),
        "{message}"
    );

    // A nested field's type is checked too.
    let err = FieldGroupDef::from_json(
        r#"{"key": "group_a", "title": "A", "fields": [
            {"key": "field_a", "name": "a", "label": "A", "type": "repeater",
             "sub_fields": [{"key": "field_b", "name": "b", "label": "B", "type": "range"}]}]}"#,
        Origin::Admin,
    )
    .unwrap_err();
    assert!(err.to_string().contains("`range`"), "{err}");
}

#[test]
fn unknown_location_param_and_hide_on_screen_are_refused_by_name() {
    let err = FieldGroupDef::from_json(
        r#"{"key": "group_a", "title": "A",
            "location": [[{"param": "moon_phase", "operator": "==", "value": "full"}]]}"#,
        Origin::Admin,
    )
    .unwrap_err();
    assert!(err.to_string().contains("`moon_phase`"), "{err}");
    let err = FieldGroupDef::from_json(
        r#"{"key": "group_a", "title": "A", "hide_on_screen": ["toolbar"]}"#,
        Origin::Admin,
    )
    .unwrap_err();
    assert!(err.to_string().contains("`toolbar`"), "{err}");
}

#[test]
fn field_group_keys_and_names_are_checked() {
    let field = |key: &str, name: &str| FieldDef::new(key, name, "Label", FieldKindDef::Text);

    let mut group = FieldGroupDef::new("details", "Details");
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::InvalidKey {
            prefix: "group_",
            ..
        })
    ));
    group.key = "group_details".to_owned();
    group.validate().unwrap();

    group.title = " ".to_owned();
    assert_eq!(
        group.validate(),
        Err(DefinitionError::MissingTitle("group_details".to_owned()))
    );
    group.title = "Details".to_owned();

    group.fields = vec![field("isbn", "isbn")];
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::InvalidKey {
            prefix: "field_",
            ..
        })
    ));
    group.fields = vec![field("field_", "isbn")];
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::InvalidKey { .. })
    ));

    group.fields = vec![field("field_a", "isbn"), field("field_b", "isbn")];
    assert_eq!(
        group.validate(),
        Err(DefinitionError::DuplicateFieldName("isbn".to_owned()))
    );
    group.fields = vec![field("field_a", "isbn"), field("field_a", "price")];
    assert_eq!(
        group.validate(),
        Err(DefinitionError::DuplicateKey("field_a".to_owned()))
    );
    group.fields = vec![field("field_a", "my isbn")];
    assert_eq!(
        group.validate(),
        Err(DefinitionError::InvalidFieldName("my isbn".to_owned()))
    );

    // A sub-field may share a name with a field of the group: names are unique per parent.
    group.fields = vec![
        field("field_a", "title"),
        FieldDef::new(
            "field_b",
            "rows",
            "Rows",
            FieldKindDef::Repeater(Vec::new()),
        )
        .with_sub_fields(vec![field("field_c", "title")]),
    ];
    group.validate().unwrap();
    // But keys are unique in the whole group.
    group.fields[1] = group.fields[1]
        .clone()
        .with_sub_fields(vec![field("field_a", "title")]);
    assert_eq!(
        group.validate(),
        Err(DefinitionError::DuplicateKey("field_a".to_owned()))
    );
}

#[test]
fn field_kind_must_match_its_sub_fields() {
    let mut group = FieldGroupDef::new("group_a", "A");
    // A repeater whose kind was set without `with_sub_fields`.
    let mut rows = FieldDef::new(
        "field_rows",
        "rows",
        "Rows",
        FieldKindDef::Repeater(Vec::new()),
    );
    rows.sub_fields = vec![FieldDef::new("field_x", "x", "X", FieldKindDef::Text)];
    group.fields = vec![rows];
    assert_eq!(
        group.validate(),
        Err(DefinitionError::SubFieldsMismatch("field_rows".to_owned()))
    );

    // A text field with sub-fields.
    let mut text = FieldDef::new("field_t", "t", "T", FieldKindDef::Text);
    text.sub_fields = vec![FieldDef::new("field_x", "x", "X", FieldKindDef::Text)];
    group.fields = vec![text];
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::SubFieldsMismatch(_))
    ));

    // A lookup has no ACF type, so it cannot be written in a definition.
    group.fields = vec![FieldDef::new("field_l", "l", "L", FieldKindDef::Lookup)];
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::UnsupportedFieldKind { .. })
    ));
    assert!(serde_json::to_string(&group).is_err());

    // A setting may not repeat a key of its own.
    let mut text = FieldDef::new("field_t", "t", "T", FieldKindDef::Text);
    text.settings
        .insert("type".to_owned(), Value::from("email"));
    group.fields = vec![text];
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::ReservedSetting { .. })
    ));
}

/// A group whose fields nest `depth` repeaters deep.
fn nested_repeaters(depth: usize) -> String {
    let mut field =
        r#"{"key": "field_leaf", "name": "leaf", "label": "Leaf", "type": "text"}"#.to_owned();
    for level in 0..depth {
        field = format!(
            r#"{{"key": "field_r{level}", "name": "r{level}", "label": "R", "type": "repeater",
                "sub_fields": [{field}]}}"#
        );
    }
    format!(r#"{{"key": "group_deep", "title": "Deep", "fields": [{field}]}}"#)
}

#[test]
fn deeply_nested_fields_are_refused_without_exhausting_the_stack() {
    // On a 2 MiB stack, as a server's worker thread may have.
    let result = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let deep = FieldGroupDef::from_json(&nested_repeaters(60), Origin::Admin);
            let fine =
                FieldGroupDef::from_json(&nested_repeaters(MAX_FIELD_DEPTH - 1), Origin::Admin);
            (deep, fine.map(|g| g.fields.len()))
        })
        .unwrap()
        .join()
        .unwrap();
    let err = result.0.unwrap_err();
    assert!(err.to_string().contains("field nesting depth"), "{err}");
    assert!(
        err.to_string().contains(&MAX_FIELD_DEPTH.to_string()),
        "{err}"
    );
    assert_eq!(result.1, Ok(1));

    // A group built in code is held to the same depth.
    let mut field = FieldDef::new("field_leaf", "leaf", "Leaf", FieldKindDef::Text);
    for level in 0..MAX_FIELD_DEPTH {
        field = FieldDef::new(
            &format!("field_r{level}"),
            &format!("r{level}"),
            "R",
            FieldKindDef::Repeater(Vec::new()),
        )
        .with_sub_fields(vec![field]);
    }
    let mut group = FieldGroupDef::new("group_deep", "Deep");
    group.fields = vec![field];
    assert_eq!(
        group.validate(),
        Err(DefinitionError::TooLarge {
            what: "field nesting depth",
            limit: MAX_FIELD_DEPTH
        })
    );
}

#[test]
fn size_limits_are_refused_by_name() {
    let too_large = |err: DefinitionError, what: &str, limit: usize| {
        assert_eq!(
            err,
            DefinitionError::TooLarge {
                what: match what {
                    "fields" => "number of fields",
                    "choices" => "number of choices",
                    "layouts" => "number of layouts",
                    "label" => "label length",
                    "rules" => "number of location rules",
                    _ => "definition size in bytes",
                },
                limit
            }
        );
    };

    let mut group = FieldGroupDef::new("group_big", "Big");
    group.fields = (0..=MAX_FIELDS)
        .map(|i| {
            FieldDef::new(
                &format!("field_{i}"),
                &format!("f{i}"),
                "F",
                FieldKindDef::Text,
            )
        })
        .collect();
    too_large(group.validate().unwrap_err(), "fields", MAX_FIELDS);

    let choices = (0..=MAX_CHOICES)
        .map(|i| (i.to_string(), i.to_string()))
        .collect();
    group.fields = vec![FieldDef::new(
        "field_s",
        "s",
        "S",
        FieldKindDef::Select(choices),
    )];
    too_large(group.validate().unwrap_err(), "choices", MAX_CHOICES);

    let layouts = (0..=MAX_LAYOUTS)
        .map(|i| LayoutDef {
            key: format!("layout_{i}"),
            name: format!("l{i}"),
            label: "L".to_owned(),
            sub_fields: Vec::new(),
            settings: serde_json::Map::new(),
        })
        .collect();
    group.fields = vec![
        FieldDef::new(
            "field_fc",
            "fc",
            "FC",
            FieldKindDef::FlexibleContent(Vec::new()),
        )
        .with_layouts(layouts),
    ];
    too_large(group.validate().unwrap_err(), "layouts", MAX_LAYOUTS);

    group.fields = vec![FieldDef::new(
        "field_t",
        "t",
        &"x".repeat(MAX_TEXT_LEN + 1),
        FieldKindDef::Text,
    )];
    too_large(group.validate().unwrap_err(), "label", MAX_TEXT_LEN);

    group.fields.clear();
    let rule = LocationRule {
        param: LocationParam::PostType,
        operator: LocationOperator::Is,
        value: "book".to_owned(),
    };
    group.location = vec![vec![rule; MAX_LOCATION_RULES + 1]];
    too_large(group.validate().unwrap_err(), "rules", MAX_LOCATION_RULES);

    let mut def = PostTypeDef::new("book", "Books", "Book");
    def.labels.add_new = Some("x".repeat(MAX_TEXT_LEN + 1));
    too_large(def.validate().unwrap_err(), "label", MAX_TEXT_LEN);

    let huge = format!(
        r#"{{"slug": "book", "description": "{}"}}"#,
        "x".repeat(MAX_JSON_BYTES)
    );
    too_large(
        PostTypeDef::from_json(&huge, Origin::Admin).unwrap_err(),
        "bytes",
        MAX_JSON_BYTES,
    );
}

#[test]
fn php_values_are_booleans_never_slugs() {
    let parse = |json: &str| PostTypeDef::from_json(json, Origin::Admin).unwrap();

    for (raw, show) in [
        ("\"1\"", true),
        ("\"0\"", false),
        ("\"\"", false),
        ("1", true),
        ("0", false),
        ("\"true\"", true),
    ] {
        let def = parse(&format!(r#"{{"slug": "book", "show_in_menu": {raw}}}"#));
        assert_eq!(def.show_in_menu, Some(ShowInMenu::Show(show)), "{raw}");
        let def = parse(&format!(r#"{{"slug": "book", "has_archive": {raw}}}"#));
        assert_eq!(def.has_archive, HasArchive::Enabled(show), "{raw}");
        let def = parse(&format!(r#"{{"slug": "book", "rewrite": {raw}}}"#));
        assert_eq!(def.rewrite, Rewrite::Enabled(show), "{raw}");
    }
    assert_eq!(
        parse(r#"{"slug": "book", "show_in_menu": "tools.php"}"#).show_in_menu,
        Some(ShowInMenu::Under("tools.php".to_owned()))
    );
    assert_eq!(
        parse(r#"{"slug": "book", "show_in_menu": null}"#).show_in_menu,
        None
    );
    assert_eq!(
        parse(r#"{"slug": "book", "has_archive": "library"}"#).has_archive,
        HasArchive::Slug("library".to_owned())
    );
    assert_eq!(
        parse(r#"{"slug": "book", "has_archive": null}"#).has_archive,
        HasArchive::Enabled(false)
    );
    assert_eq!(
        parse(r#"{"slug": "book", "rewrite": null}"#).rewrite,
        Rewrite::Enabled(true)
    );
    assert_eq!(
        parse(r#"{"slug": "book", "rewrite": []}"#).rewrite,
        Rewrite::Rules(RewriteRules::default())
    );
    assert_eq!(
        parse(r#"{"slug": "book", "rewrite": {"slug": "books", "with_front": "0"}}"#).rewrite,
        Rewrite::Rules(RewriteRules {
            slug: Some("books".to_owned()),
            with_front: Some(false),
            ..RewriteRules::default()
        })
    );
    assert!(
        PostTypeDef::from_json(r#"{"slug": "book", "rewrite": "books"}"#, Origin::Admin).is_err()
    );

    // `supports`: only `false` is none; "", null, "0" and true get the defaults.
    for raw in ["\"\"", "null", "\"0\"", "true", "0"] {
        let def = parse(&format!(r#"{{"slug": "book", "supports": {raw}}}"#));
        assert_eq!(def.supports, [Support::Title, Support::Editor], "{raw}");
    }
    // A PHP array exported as an object.
    let def = parse(r#"{"slug": "book", "supports": {"0": "title", "editor": {"notes": true}}}"#);
    assert_eq!(def.supports, [Support::Title, Support::Editor]);

    // A location rule's value may be a number, such as a post's id.
    let group = FieldGroupDef::from_json(
        r#"{"key": "group_a", "title": "A",
            "location": [[{"param": "post", "operator": "==", "value": 42}]]}"#,
        Origin::Admin,
    )
    .unwrap();
    assert_eq!(group.location[0][0].value, "42");
}

#[test]
fn options_not_modelled_are_kept() {
    let json = r#"{"slug": "book", "show_in_admin_bar": false, "query_var": "book",
        "can_export": true, "template": [["core/paragraph", {}]], "rest_namespace": "shop/v1",
        "capabilities": {"edit_posts": "edit_books_please"}}"#;
    let def = PostTypeDef::from_json(json, Origin::Admin).unwrap();
    assert_eq!(def.settings["query_var"], "book");
    assert_eq!(def.settings["rest_namespace"], "shop/v1");
    assert!(!def.settings.contains_key("capabilities"));
    assert_eq!(def.caps().edit_posts, "edit_books_please");
    assert_eq!(def.caps().publish_posts, "publish_posts");
    // Saving it again keeps them.
    let again = serde_json::to_string(&def).unwrap();
    assert_eq!(PostTypeDef::from_json(&again, Origin::Admin).unwrap(), def);

    let taxonomy = TaxonomyDef::from_json(
        r#"{"slug": "genre", "query_var": "genre", "sort": true,
            "default_term": {"name": "Fiction"}}"#,
        Origin::Admin,
    )
    .unwrap();
    assert_eq!(taxonomy.settings["default_term"]["name"], "Fiction");
    let again = serde_json::to_string(&taxonomy).unwrap();
    assert_eq!(
        TaxonomyDef::from_json(&again, Origin::Admin).unwrap(),
        taxonomy
    );

    // A setting built in code may not repeat a modelled option.
    let mut def = PostTypeDef::new("book", "Books", "Book");
    def.settings.insert("public".to_owned(), Value::Bool(true));
    assert_eq!(
        def.validate(),
        Err(DefinitionError::ReservedSetting {
            key: "book".to_owned(),
            setting: "public".to_owned()
        })
    );
}

#[test]
fn number_step_round_trips() {
    let parse = |step: &str| {
        let group = FieldGroupDef::from_json(
            &format!(
                r#"{{"key": "group_a", "title": "A", "fields": [
                    {{"key": "field_n", "name": "n", "label": "N", "type": "number", "step": {step}}}]}}"#
            ),
            Origin::Admin,
        )
        .unwrap();
        let again = serde_json::to_string(&group).unwrap();
        assert_eq!(
            FieldGroupDef::from_json(&again, Origin::Admin).unwrap(),
            group
        );
        group.fields[0].kind.clone()
    };
    assert_eq!(parse("0.5"), FieldKindDef::Decimal("0.5".to_owned()));
    assert_eq!(parse("\"0.01\""), FieldKindDef::Decimal("0.01".to_owned()));
    assert_eq!(parse("\"any\""), FieldKindDef::Decimal("any".to_owned()));
    assert_eq!(parse("1"), FieldKindDef::Number);
    assert_eq!(parse("\"\""), FieldKindDef::Number);

    // `step` is the kind's, so a setting may not hold it.
    let mut group = FieldGroupDef::new("group_a", "A");
    let mut n = FieldDef::new("field_n", "n", "N", FieldKindDef::Number);
    n.settings.insert("step".to_owned(), Value::from("2"));
    group.fields = vec![n];
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::ReservedSetting { .. })
    ));
}

#[test]
fn layouts_are_checked() {
    let layout = |key: &str, name: &str| LayoutDef {
        key: key.to_owned(),
        name: name.to_owned(),
        label: "L".to_owned(),
        sub_fields: Vec::new(),
        settings: serde_json::Map::new(),
    };
    let field = |layouts| {
        FieldDef::new(
            "field_fc",
            "fc",
            "FC",
            FieldKindDef::FlexibleContent(Vec::new()),
        )
        .with_layouts(layouts)
    };
    let mut group = FieldGroupDef::new("group_a", "A");

    group.fields = vec![field(vec![layout("layout_a", "")])];
    assert_eq!(
        group.validate(),
        Err(DefinitionError::InvalidLayoutName(String::new()))
    );
    group.fields = vec![field(vec![layout("layout_a", "a b")])];
    assert!(matches!(
        group.validate(),
        Err(DefinitionError::InvalidLayoutName(_))
    ));
    group.fields = vec![field(vec![
        layout("layout_a", "hero"),
        layout("layout_b", "hero"),
    ])];
    assert_eq!(
        group.validate(),
        Err(DefinitionError::DuplicateLayoutName("hero".to_owned()))
    );
    group.fields = vec![field(vec![
        layout("layout_a", "hero"),
        layout("layout_b", "quote"),
    ])];
    group.validate().unwrap();

    // In ACF's object form, each layout's key must match the key it is under.
    let err = FieldGroupDef::from_json(
        r#"{"key": "group_a", "title": "A", "fields": [
            {"key": "field_fc", "name": "fc", "label": "FC", "type": "flexible_content",
             "layouts": {"layout_a": {"key": "layout_b", "name": "hero", "label": "Hero"}}}]}"#,
        Origin::Admin,
    )
    .unwrap_err();
    assert!(err.to_string().contains("`layout_a`"), "{err}");
}

/// ACF's `checkbox` (several choices) is Ignite's checkbox list, and round-trips with its
/// choices in order (design gap M1).
#[test]
fn a_checkbox_field_is_a_checkbox_list() {
    let json = r#"{"key": "group_c", "title": "C", "fields": [{"key": "field_c",
        "name": "colours", "label": "Colours", "type": "checkbox",
        "choices": {"red": "Red", "blue": "Blue"}}]}"#;
    let group = FieldGroupDef::from_json(json, Origin::Admin).unwrap();
    assert_eq!(
        group.fields[0].kind,
        FieldKindDef::CheckboxList(vec![
            ("red".to_owned(), "Red".to_owned()),
            ("blue".to_owned(), "Blue".to_owned()),
        ])
    );
    let written = serde_json::to_string(&group).unwrap();
    assert!(written.contains(r#""type":"checkbox""#), "{written}");
    assert!(
        written.contains(r#""choices":{"red":"Red","blue":"Blue"}"#),
        "{written}"
    );
}
