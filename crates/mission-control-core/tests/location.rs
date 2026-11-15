//! Tests for field group locations (Task 5.4): ACF's or-of-and rules matched against what an
//! edit screen knows. They need no database.

use std::sync::Arc;

use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::types::{
    FieldGroupDef, LocationOperator, LocationParam, LocationRule, Origin, PostTypeDef, Registry,
    Rule, ScreenContext, matches,
};

fn rule(param: LocationParam, operator: LocationOperator, value: &str) -> LocationRule {
    LocationRule {
        param,
        operator,
        value: value.to_owned(),
    }
}

fn is(param: LocationParam, value: &str) -> LocationRule {
    rule(param, LocationOperator::Is, value)
}

fn is_not(param: LocationParam, value: &str) -> LocationRule {
    rule(param, LocationOperator::IsNot, value)
}

fn group(key: &str, title: &str, location: Vec<Vec<LocationRule>>) -> FieldGroupDef {
    let mut g = FieldGroupDef::new(key, title);
    g.location = location;
    g
}

#[test]
fn and_of_post_type_and_template_shows_only_on_the_combination() {
    // The brief's "Event post type and page template x": ACF's `post_template` is the rule
    // for a template on a custom post type (`page_template` is for pages only).
    let g = group(
        "group_a",
        "A",
        vec![vec![
            is(LocationParam::PostType, "event"),
            is(LocationParam::PostTemplate, "x.php"),
        ]],
    );
    assert!(matches(
        &g,
        &ScreenContext::post("event").with_page_template("x.php")
    ));
    assert!(!matches(
        &g,
        &ScreenContext::post("event").with_page_template("y.php")
    ));
    assert!(!matches(
        &g,
        &ScreenContext::post("page").with_page_template("x.php")
    ));
    assert!(!matches(&g, &ScreenContext::post("event")));
}

#[test]
fn and_of_page_and_page_template_shows_only_on_the_combination() {
    let g = group(
        "group_a",
        "A",
        vec![vec![
            is(LocationParam::PostType, "page"),
            is(LocationParam::PageTemplate, "x.php"),
        ]],
    );
    assert!(matches(
        &g,
        &ScreenContext::post("page").with_page_template("x.php")
    ));
    assert!(!matches(&g, &ScreenContext::post("page")));
    assert!(!matches(
        &g,
        &ScreenContext::post("event").with_page_template("x.php")
    ));
}

#[test]
fn or_of_two_post_types_shows_on_either() {
    let g = group(
        "group_a",
        "A",
        vec![
            vec![is(LocationParam::PostType, "event")],
            vec![is(LocationParam::PostType, "book")],
        ],
    );
    assert!(matches(&g, &ScreenContext::post("event")));
    assert!(matches(&g, &ScreenContext::post("book")));
    assert!(!matches(&g, &ScreenContext::post("page")));
}

#[test]
fn not_equal_inverts_when_the_screen_has_the_param() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is_not(LocationParam::PostType, "page")]],
    );
    assert!(matches(&g, &ScreenContext::post("post")));
    assert!(!matches(&g, &ScreenContext::post("page")));
    // ACF: a screen without the param matches neither operator.
    assert!(!matches(&g, &ScreenContext::term("genre")));
}

#[test]
fn empty_location_and_empty_and_list_show_nowhere() {
    let none = group("group_a", "A", vec![]);
    assert!(!matches(&none, &ScreenContext::post("post")));
    let empty_and = group("group_b", "B", vec![vec![]]);
    assert!(!matches(&empty_and, &ScreenContext::post("post")));
}

#[test]
fn inactive_group_shows_nowhere() {
    let mut g = group(
        "group_a",
        "A",
        vec![vec![is(LocationParam::PostType, "post")]],
    );
    g.active = false;
    assert!(!matches(&g, &ScreenContext::post("post")));
}

#[test]
fn unknown_param_fails_closed_with_either_operator() {
    for op in [LocationOperator::Is, LocationOperator::IsNot] {
        let g = group(
            "group_a",
            "A",
            vec![vec![rule(LocationParam::PostStatus, op, "draft")]],
        );
        assert!(!matches(&g, &ScreenContext::post("post")));
    }
    // An unknown rule does not poison another or-list.
    let g = group(
        "group_a",
        "A",
        vec![
            vec![is(LocationParam::PostStatus, "draft")],
            vec![is(LocationParam::PostType, "post")],
        ],
    );
    assert!(matches(&g, &ScreenContext::post("post")));
    assert_eq!(
        Rule::from_location(&is(LocationParam::PostFormat, "aside")),
        None
    );
}

#[test]
fn default_template_matches_a_page_without_one() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is(LocationParam::PageTemplate, "default")]],
    );
    assert!(matches(&g, &ScreenContext::post("page")));
    assert!(matches(
        &g,
        &ScreenContext::post("page").with_page_template("")
    ));
    assert!(!matches(
        &g,
        &ScreenContext::post("page").with_page_template("x.php")
    ));
}

#[test]
fn user_role_matches_the_edited_user() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is(LocationParam::UserRole, "editor")]],
    );
    assert!(matches(&g, &ScreenContext::user(&["author", "editor"])));
    assert!(!matches(&g, &ScreenContext::user(&["author"])));
    assert!(!matches(&g, &ScreenContext::post("post")));
    let not = group(
        "group_b",
        "B",
        vec![vec![is_not(LocationParam::UserRole, "editor")]],
    );
    assert!(matches(&not, &ScreenContext::user(&["author"])));
}

#[test]
fn current_user_role_matches_the_signed_in_user() {
    let g = group(
        "group_a",
        "A",
        vec![vec![
            is(LocationParam::PostType, "post"),
            is(LocationParam::CurrentUserRole, "administrator"),
        ]],
    );
    let ctx = ScreenContext::post("post").with_current_user_roles(&["administrator"]);
    assert!(matches(&g, &ctx));
    let ctx = ScreenContext::post("post").with_current_user_roles(&["editor"]);
    assert!(!matches(&g, &ctx));
}

#[test]
fn taxonomy_matches_the_term_form_and_all() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is(LocationParam::Taxonomy, "genre")]],
    );
    assert!(matches(&g, &ScreenContext::term("genre")));
    assert!(!matches(&g, &ScreenContext::term("category")));
    assert!(!matches(&g, &ScreenContext::post("post")));
    let all = group(
        "group_b",
        "B",
        vec![vec![is(LocationParam::Taxonomy, "all")]],
    );
    assert!(matches(&all, &ScreenContext::term("category")));
}

#[test]
fn options_page_matches_its_slug() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is(LocationParam::OptionsPage, "site-settings")]],
    );
    assert!(matches(&g, &ScreenContext::options_page("site-settings")));
    assert!(!matches(&g, &ScreenContext::options_page("other")));
    assert!(!matches(&g, &ScreenContext::post("post")));
}

#[test]
fn registry_groups_for_orders_and_includes_generated_post_type_rules() {
    let registry = Registry::new(Arc::new(PostTypes::new()), Arc::new(Taxonomies::new()));
    let mut second = group(
        "group_b",
        "Zebra",
        vec![vec![is(LocationParam::PostType, "book")]],
    );
    second.menu_order = 1;
    let mut first_by_title = group(
        "group_a",
        "Alpha",
        vec![vec![is(LocationParam::PostType, "book")]],
    );
    first_by_title.menu_order = 1;
    let mut first = group(
        "group_c",
        "Last title",
        vec![vec![is(LocationParam::PostType, "book")]],
    );
    first.menu_order = 0;
    // No location of its own: shown on books only through the post type's field_groups (R81).
    let generated = group("group_gen", "Generated", vec![]);
    let elsewhere = group(
        "group_d",
        "Elsewhere",
        vec![vec![is(LocationParam::PostType, "page")]],
    );
    for g in [second, first_by_title, first, generated, elsewhere] {
        registry
            .register(FieldGroupDef {
                origin: Origin::Code,
                ..g
            })
            .unwrap();
    }
    let book = PostTypeDef::from_json(
        r#"{"slug": "book", "public": true, "field_groups": ["group_gen"]}"#,
        Origin::Code,
    )
    .unwrap();
    registry.register(book).unwrap();

    let keys: Vec<_> = registry
        .groups_for(&ScreenContext::post("book"))
        .into_iter()
        .map(|g| g.key)
        .collect();
    assert_eq!(keys, ["group_gen", "group_c", "group_a", "group_b"]);
    let page: Vec<_> = registry
        .groups_for(&ScreenContext::post("page"))
        .into_iter()
        .map(|g| g.key)
        .collect();
    assert_eq!(page, ["group_d"]);
}

#[test]
fn post_template_default_matches_any_post_type_without_a_template() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is(LocationParam::PostTemplate, "default")]],
    );
    assert!(matches(&g, &ScreenContext::post("post")));
    assert!(matches(&g, &ScreenContext::post("book")));
    assert!(matches(
        &g,
        &ScreenContext::post("book").with_page_template("")
    ));
    assert!(!matches(
        &g,
        &ScreenContext::post("book").with_page_template("x.php")
    ));
    // No post on the screen: nothing to test.
    assert!(!matches(&g, &ScreenContext::term("genre")));
}

#[test]
fn post_template_matches_a_named_template_on_a_custom_post_type() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is(LocationParam::PostTemplate, "x.php")]],
    );
    assert!(matches(
        &g,
        &ScreenContext::post("book").with_page_template("x.php")
    ));
    assert!(!matches(&g, &ScreenContext::post("book")));
}

#[test]
fn taxonomy_not_all_never_matches() {
    let g = group(
        "group_a",
        "A",
        vec![vec![is_not(LocationParam::Taxonomy, "all")]],
    );
    assert!(!matches(&g, &ScreenContext::term("genre")));
    assert!(!matches(&g, &ScreenContext::post("post")));
}

#[test]
fn page_template_not_equal_fails_closed_off_pages() {
    // ACF's page_template rule is for pages only: on another post type it has nothing to test,
    // so it matches with neither operator.
    let g = group(
        "group_a",
        "A",
        vec![vec![is_not(LocationParam::PageTemplate, "x.php")]],
    );
    assert!(matches(&g, &ScreenContext::post("page")));
    assert!(!matches(
        &g,
        &ScreenContext::post("page").with_page_template("x.php")
    ));
    assert!(!matches(&g, &ScreenContext::post("book")));
    assert!(!matches(
        &g,
        &ScreenContext::post("book").with_page_template("y.php")
    ));
}

#[test]
fn groups_for_breaks_ties_by_key() {
    let registry = Registry::new(Arc::new(PostTypes::new()), Arc::new(Taxonomies::new()));
    for key in ["group_z", "group_m", "group_a"] {
        let g = group(key, "Same", vec![vec![is(LocationParam::PostType, "book")]]);
        registry
            .register(FieldGroupDef {
                origin: Origin::Code,
                ..g
            })
            .unwrap();
    }
    let keys: Vec<_> = registry
        .groups_for(&ScreenContext::post("book"))
        .into_iter()
        .map(|g| g.key)
        .collect();
    assert_eq!(keys, ["group_a", "group_m", "group_z"]);
}
