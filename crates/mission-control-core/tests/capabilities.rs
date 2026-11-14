//! Tests for WordPress's default roles and a user's capabilities: the role lists of
//! `populate_roles` (single site), the merge of `WP_User::get_role_caps`, and the primitive check
//! of `WP_User::has_cap`. Pure logic: no database.

use std::collections::BTreeMap;

use chrono::NaiveDateTime;
use mission_control_core::content::{Post, PostStatus};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, DO_NOT_ALLOW, EDITOR, Object, SUBSCRIBER, UserCaps, can,
    default_role, default_roles, has_cap,
};

/// A post of `post_type` by `author_id` in `status`.
fn post(id: i64, author_id: i64, post_type: &str, status: PostStatus) -> Post {
    Post {
        id,
        author_id,
        post_type: post_type.to_owned(),
        status,
        title: String::new(),
        slug: String::new(),
        content: String::new(),
        excerpt: String::new(),
        parent_id: None,
        menu_order: 0,
        published_at: None,
        modified_at: NaiveDateTime::default(),
        password: None,
        mime_type: String::new(),
    }
}

/// A user with one default role and no capabilities of their own.
fn user(id: i64, role: &str) -> UserCaps {
    UserCaps::with_default_roles(id, &[role], &BTreeMap::new())
}

/// The capabilities `role` grants, sorted.
fn caps_of(role: &str) -> Vec<String> {
    let role = default_role(role).expect("a default role");
    let mut caps: Vec<String> = role
        .capabilities
        .iter()
        .filter(|(_, granted)| **granted)
        .map(|(cap, _)| cap.clone())
        .collect();
    caps.sort();
    caps
}

#[test]
fn the_five_default_roles_in_wordpress_order() {
    let names: Vec<(String, String)> = default_roles()
        .into_iter()
        .map(|role| (role.name, role.display_name))
        .collect();
    assert_eq!(
        names,
        [
            ("administrator", "Administrator"),
            ("editor", "Editor"),
            ("author", "Author"),
            ("contributor", "Contributor"),
            ("subscriber", "Subscriber"),
        ]
        .map(|(name, display)| (name.to_owned(), display.to_owned()))
    );
    assert!(default_role("super_admin").is_none());
}

#[test]
fn the_role_lists_match_populate_roles() {
    // The counts of a fresh single-site WordPress (`populate_roles`, 1.6 to 3.0), with the
    // `level_N` capabilities.
    assert_eq!(caps_of(ADMINISTRATOR).len(), 61);
    assert_eq!(caps_of(EDITOR).len(), 34);
    assert_eq!(caps_of(AUTHOR).len(), 10);
    assert_eq!(caps_of(CONTRIBUTOR).len(), 5);
    assert_eq!(caps_of(SUBSCRIBER).len(), 2);

    assert_eq!(
        caps_of(AUTHOR),
        [
            "delete_posts",
            "delete_published_posts",
            "edit_posts",
            "edit_published_posts",
            "level_0",
            "level_1",
            "level_2",
            "publish_posts",
            "read",
            "upload_files",
        ]
    );
    assert_eq!(
        caps_of(CONTRIBUTOR),
        ["delete_posts", "edit_posts", "level_0", "level_1", "read"]
    );
    assert_eq!(caps_of(SUBSCRIBER), ["level_0", "read"]);

    let admin = caps_of(ADMINISTRATOR);
    for cap in [
        "manage_options",
        "update_core",
        "list_users",
        "promote_users",
        "edit_theme_options",
        "export",
        "unfiltered_upload",
        "edit_dashboard",
        "install_themes",
        "level_10",
    ] {
        assert!(admin.contains(&cap.to_owned()), "administrator has {cap}");
    }
    // Every editor capability is an administrator one too.
    for cap in caps_of(EDITOR) {
        assert!(admin.contains(&cap), "administrator has the editor's {cap}");
    }
    let editor = caps_of(EDITOR);
    for cap in [
        "manage_options",
        "list_users",
        "edit_theme_options",
        "level_8",
    ] {
        assert!(!editor.contains(&cap.to_owned()), "an editor lacks {cap}");
    }
}

#[test]
fn has_cap_reads_the_merged_capabilities() {
    let author = user(7, AUTHOR);
    assert_eq!(author.id(), 7);
    assert_eq!(author.roles(), ["author"]);
    assert!(has_cap(&author, "edit_posts"));
    assert!(has_cap(&author, "publish_posts"));
    assert!(!has_cap(&author, "edit_others_posts"));
    assert!(!has_cap(&author, "manage_options"));
    // A role name is a capability too, as WordPress's `$user->caps` puts it in `allcaps`.
    assert!(has_cap(&author, "author"));
    assert!(!has_cap(&author, "editor"));
}

#[test]
fn everyone_exists_and_nobody_is_allowed_do_not_allow() {
    let nobody = UserCaps::from_roles(0, &[], &BTreeMap::new());
    assert!(has_cap(&nobody, "exist"));
    assert!(!has_cap(&nobody, "read"));

    let mut extra = BTreeMap::new();
    extra.insert(DO_NOT_ALLOW.to_owned(), true);
    let admin = UserCaps::with_default_roles(1, &[ADMINISTRATOR], &extra);
    assert!(!has_cap(&admin, DO_NOT_ALLOW));
}

#[test]
fn per_user_capabilities_overlay_the_roles() {
    let mut extra = BTreeMap::new();
    extra.insert("edit_others_posts".to_owned(), true);
    extra.insert("publish_posts".to_owned(), false);
    let author = UserCaps::with_default_roles(3, &[AUTHOR], &extra);
    assert!(
        has_cap(&author, "edit_others_posts"),
        "a granted capability"
    );
    assert!(!has_cap(&author, "publish_posts"), "a denied capability");
    assert!(
        has_cap(&author, "edit_posts"),
        "the role's other capabilities stay"
    );
}

#[test]
fn several_roles_merge_in_order() {
    let user = UserCaps::with_default_roles(4, &[SUBSCRIBER, EDITOR], &BTreeMap::new());
    assert_eq!(user.roles(), ["subscriber", "editor"]);
    assert!(has_cap(&user, "edit_others_posts"));
    assert!(has_cap(&user, "read"));

    // A later role's `false` overrides an earlier role's grant, as `array_merge` does.
    let mut no_read = default_role(SUBSCRIBER).unwrap();
    no_read.name = "no_read".into();
    no_read.capabilities.insert("read".into(), false);
    let editor = default_role(EDITOR).unwrap();
    let user = UserCaps::from_roles(5, &[&editor, &no_read], &BTreeMap::new());
    assert!(!has_cap(&user, "read"));
    assert!(has_cap(&user, "edit_posts"));
}

#[test]
fn unknown_role_names_are_not_roles() {
    let user = UserCaps::with_default_roles(6, &["ghost", AUTHOR], &BTreeMap::new());
    assert_eq!(user.roles(), ["author"]);
    assert!(!has_cap(&user, "ghost"));
}

#[test]
fn an_author_edits_their_own_post_and_cannot_publish_or_edit_someone_elses() {
    let author = user(10, AUTHOR);
    let own = post(1, 10, "post", PostStatus::Draft);
    let others = post(2, 11, "post", PostStatus::Draft);
    let others_published = post(3, 11, "post", PostStatus::Publish);

    assert!(can(&author, "edit_post", Some(&Object::Post(&own))));
    assert!(can(&author, "publish_post", Some(&Object::Post(&own))));
    assert!(!can(&author, "edit_post", Some(&Object::Post(&others))));
    assert!(!can(
        &author,
        "edit_post",
        Some(&Object::Post(&others_published))
    ));
    assert!(!can(
        &author,
        "delete_post",
        Some(&Object::Post(&others_published))
    ));
    // WordPress has no "publish someone else's post" capability: `publish_post` maps to
    // `publish_posts` whoever wrote it, and publishing goes through the edit screen, which
    // needs `edit_post` on that post. An author lacks it, so cannot publish another's post.
    assert!(
        !(can(&author, "edit_post", Some(&Object::Post(&others)))
            && can(&author, "publish_post", Some(&Object::Post(&others))))
    );
}

#[test]
fn a_subscriber_has_none_of_the_content_admin_capabilities() {
    // Ruling R23: "a subscriber cannot open the admin" means a subscriber has none of the
    // capabilities the content screens need. WordPress still lets a subscriber reach their
    // profile screen with `read`.
    let subscriber = user(20, SUBSCRIBER);
    assert!(has_cap(&subscriber, "read"));
    for cap in [
        "edit_posts",
        "edit_pages",
        "upload_files",
        "manage_categories",
        "moderate_comments",
        "edit_theme_options",
        "manage_options",
        "list_users",
    ] {
        assert!(!has_cap(&subscriber, cap), "a subscriber lacks {cap}");
    }
    let own = post(1, 20, "post", PostStatus::Draft);
    assert!(!can(&subscriber, "edit_post", Some(&Object::Post(&own))));
}
