//! Tests for meta capabilities: `map_meta_cap` for posts and pages, as WordPress maps
//! `edit_post`, `delete_post`, `read_post` and `publish_post` to primitive capabilities, and the
//! table of what each default role may do with a post in each state, its own or someone else's.
//! Pure logic: no database.

use std::collections::BTreeMap;

use chrono::NaiveDateTime;
use mission_control_core::content::{Post, PostStatus};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, DO_NOT_ALLOW, EDITOR, Object, PostContext, SUBSCRIBER,
    UserCaps, can, map_meta_cap,
};

/// The id of the user every check is made as.
const ME: i64 = 10;
/// The id of another author.
const SOMEONE: i64 = 11;

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

/// User `ME` with one default role.
fn user(role: &str) -> UserCaps {
    UserCaps::with_default_roles(ME, &[role], &BTreeMap::new())
}

/// The primitive capabilities `cap` on `object` maps to, for user `ME` as an author.
fn mapped(cap: &str, object: Option<&Object>) -> Vec<String> {
    map_meta_cap(cap, &user(AUTHOR), object)
}

/// A post state in the table: a status, or a trashed post with the status it had before.
#[derive(Debug, Clone, Copy)]
enum State {
    /// A post in this status.
    Is(PostStatus),
    /// A trashed post whose `_wp_trash_meta_status` is this status.
    TrashedFrom(PostStatus),
}

use PostStatus::{Draft, Future, Pending, Private, Publish};
use State::{Is, TrashedFrom};

/// One row: who, what kind of post, in which state, whose, and whether `edit_post`,
/// `delete_post`, `read_post` and `publish_post` are allowed (`y` or `n`, in that order).
type Row = (&'static str, &'static str, State, bool, &'static str);

/// `true` for "my own post", `false` for "someone else's".
const OWN: bool = true;
/// Someone else's post.
const OTHER: bool = false;

/// The table WordPress gives (6.7, `map_meta_cap` with the `populate_roles` roles), for `post`.
///
/// Editors and administrators hold every post capability, so they may do everything. Authors
/// hold `edit_posts`, `edit_published_posts`, `delete_posts`, `delete_published_posts` and
/// `publish_posts`, but none of the `_others_` or `_private_` ones. Contributors hold
/// `edit_posts` and `delete_posts`. Subscribers hold only `read`.
#[rustfmt::skip]
const POSTS: &[Row] = &[
    // role         type    state                 owner  edit/delete/read/publish
    (ADMINISTRATOR, "post", Is(Publish),          OTHER, "yyyy"),
    (ADMINISTRATOR, "post", Is(Private),          OTHER, "yyyy"),
    (ADMINISTRATOR, "post", Is(Draft),            OTHER, "yyyy"),
    (ADMINISTRATOR, "post", TrashedFrom(Publish), OTHER, "yyyy"),

    (EDITOR,        "post", Is(Publish),          OWN,   "yyyy"),
    (EDITOR,        "post", Is(Draft),            OWN,   "yyyy"),
    // WordPress lets an editor edit another author's published post: editors hold
    // `edit_others_posts` and `edit_published_posts`.
    (EDITOR,        "post", Is(Publish),          OTHER, "yyyy"),
    (EDITOR,        "post", Is(Future),           OTHER, "yyyy"),
    (EDITOR,        "post", Is(Draft),            OTHER, "yyyy"),
    (EDITOR,        "post", Is(Pending),          OTHER, "yyyy"),
    (EDITOR,        "post", Is(Private),          OTHER, "yyyy"),
    (EDITOR,        "post", TrashedFrom(Publish), OTHER, "yyyy"),
    (EDITOR,        "post", TrashedFrom(Draft),   OTHER, "yyyy"),

    (AUTHOR,        "post", Is(Publish),          OWN,   "yyyy"),
    (AUTHOR,        "post", Is(Future),           OWN,   "yyyy"),
    (AUTHOR,        "post", Is(Draft),            OWN,   "yyyy"),
    (AUTHOR,        "post", Is(Pending),          OWN,   "yyyy"),
    (AUTHOR,        "post", Is(Private),          OWN,   "yyyy"),
    (AUTHOR,        "post", TrashedFrom(Publish), OWN,   "yyyy"),
    (AUTHOR,        "post", TrashedFrom(Draft),   OWN,   "yyyy"),
    (AUTHOR,        "post", Is(Publish),          OTHER, "nnyy"),
    (AUTHOR,        "post", Is(Future),           OTHER, "nnny"),
    (AUTHOR,        "post", Is(Draft),            OTHER, "nnny"),
    (AUTHOR,        "post", Is(Pending),          OTHER, "nnny"),
    (AUTHOR,        "post", Is(Private),          OTHER, "nnny"),
    (AUTHOR,        "post", TrashedFrom(Publish), OTHER, "nnny"),
    (AUTHOR,        "post", TrashedFrom(Draft),   OTHER, "nnny"),

    // A contributor may not touch their post once it is published or scheduled.
    (CONTRIBUTOR,   "post", Is(Publish),          OWN,   "nnyn"),
    (CONTRIBUTOR,   "post", Is(Future),           OWN,   "nnyn"),
    (CONTRIBUTOR,   "post", Is(Draft),            OWN,   "yyyn"),
    (CONTRIBUTOR,   "post", Is(Pending),          OWN,   "yyyn"),
    (CONTRIBUTOR,   "post", Is(Private),          OWN,   "yyyn"),
    (CONTRIBUTOR,   "post", TrashedFrom(Publish), OWN,   "nnyn"),
    (CONTRIBUTOR,   "post", TrashedFrom(Draft),   OWN,   "yyyn"),
    (CONTRIBUTOR,   "post", Is(Publish),          OTHER, "nnyn"),
    (CONTRIBUTOR,   "post", Is(Future),           OTHER, "nnnn"),
    (CONTRIBUTOR,   "post", Is(Draft),            OTHER, "nnnn"),
    (CONTRIBUTOR,   "post", Is(Pending),          OTHER, "nnnn"),
    (CONTRIBUTOR,   "post", Is(Private),          OTHER, "nnnn"),
    (CONTRIBUTOR,   "post", TrashedFrom(Draft),   OTHER, "nnnn"),

    // A subscriber may read any post of their own (`read`), and others' published posts.
    (SUBSCRIBER,    "post", Is(Publish),          OWN,   "nnyn"),
    (SUBSCRIBER,    "post", Is(Draft),            OWN,   "nnyn"),
    (SUBSCRIBER,    "post", Is(Private),          OWN,   "nnyn"),
    (SUBSCRIBER,    "post", Is(Publish),          OTHER, "nnyn"),
    (SUBSCRIBER,    "post", Is(Future),           OTHER, "nnnn"),
    (SUBSCRIBER,    "post", Is(Draft),            OTHER, "nnnn"),
    (SUBSCRIBER,    "post", Is(Private),          OTHER, "nnnn"),
];

/// The table for `page`, whose capabilities are the `_pages` ones. Authors and contributors
/// hold none of them; editors and administrators hold them all.
#[rustfmt::skip]
const PAGES: &[Row] = &[
    (ADMINISTRATOR, "page", Is(Publish),          OTHER, "yyyy"),
    (EDITOR,        "page", Is(Publish),          OTHER, "yyyy"),
    (EDITOR,        "page", Is(Private),          OTHER, "yyyy"),
    (EDITOR,        "page", Is(Draft),            OWN,   "yyyy"),
    (AUTHOR,        "page", Is(Publish),          OWN,   "nnyn"),
    (AUTHOR,        "page", Is(Draft),            OWN,   "nnyn"),
    (AUTHOR,        "page", Is(Publish),          OTHER, "nnyn"),
    (AUTHOR,        "page", Is(Draft),            OTHER, "nnnn"),
    (CONTRIBUTOR,   "page", Is(Draft),            OWN,   "nnyn"),
    (CONTRIBUTOR,   "page", Is(Publish),          OTHER, "nnyn"),
    (SUBSCRIBER,    "page", Is(Publish),          OTHER, "nnyn"),
    (SUBSCRIBER,    "page", Is(Private),          OTHER, "nnnn"),
];

/// Checks every row of `rows`, and reports every row that differs.
fn check_table(rows: &[Row]) {
    let mut failures = Vec::new();
    for &(role, post_type, state, own, expected) in rows {
        let author = if own { ME } else { SOMEONE };
        let (status, before) = match state {
            Is(status) => (status, None),
            TrashedFrom(status) => (PostStatus::Trash, Some(status)),
        };
        let row = post(1, author, post_type, status);
        let context = PostContext {
            trash_meta_status: before,
            ..PostContext::new(&row)
        };
        let object = Object::PostWithContext(context);
        let user = user(role);
        let got: String = ["edit_post", "delete_post", "read_post", "publish_post"]
            .iter()
            .map(|cap| {
                if can(&user, cap, Some(&object)) {
                    'y'
                } else {
                    'n'
                }
            })
            .collect();
        if got != expected {
            failures.push(format!(
                "{role} on {} {post_type} {state:?}: expected {expected}, got {got}",
                if own { "own" } else { "other's" },
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_post_table() {
    check_table(POSTS);
}

#[test]
fn the_page_table() {
    check_table(PAGES);
}

#[test]
fn edit_post_maps_as_wordpress_does() {
    let own_draft = post(1, ME, "post", Draft);
    let own_published = post(2, ME, "post", Publish);
    let others_draft = post(3, SOMEONE, "post", Draft);
    let others_scheduled = post(4, SOMEONE, "post", Future);
    let others_private = post(5, SOMEONE, "post", Private);

    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&own_draft))),
        ["edit_posts"]
    );
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&own_published))),
        ["edit_published_posts"]
    );
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&others_draft))),
        ["edit_others_posts"]
    );
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&others_scheduled))),
        ["edit_others_posts", "edit_published_posts"]
    );
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&others_private))),
        ["edit_others_posts", "edit_private_posts"]
    );
    assert_eq!(
        mapped("delete_post", Some(&Object::Post(&others_private))),
        ["delete_others_posts", "delete_private_posts"]
    );
}

#[test]
fn page_checks_use_page_capabilities_whichever_name_is_asked() {
    let others_page = post(1, SOMEONE, "page", Publish);
    for cap in ["edit_post", "edit_page"] {
        assert_eq!(
            mapped(cap, Some(&Object::Post(&others_page))),
            ["edit_others_pages", "edit_published_pages"]
        );
    }
    assert_eq!(
        mapped("delete_page", Some(&Object::Post(&others_page))),
        ["delete_others_pages", "delete_published_pages"]
    );
    assert_eq!(
        mapped("publish_post", Some(&Object::Post(&others_page))),
        ["publish_pages"]
    );
    let draft_page = post(2, SOMEONE, "page", Draft);
    assert_eq!(
        mapped("read_page", Some(&Object::Post(&draft_page))),
        ["edit_others_pages"]
    );
    let private_page = post(3, SOMEONE, "page", Private);
    assert_eq!(
        mapped("read_page", Some(&Object::Post(&private_page))),
        ["read_private_pages"]
    );
}

#[test]
fn read_post_maps_as_wordpress_does() {
    let published = post(1, SOMEONE, "post", Publish);
    let own_draft = post(2, ME, "post", Draft);
    let others_private = post(3, SOMEONE, "post", Private);
    let others_pending = post(4, SOMEONE, "post", Pending);
    assert_eq!(
        mapped("read_post", Some(&Object::Post(&published))),
        ["read"]
    );
    assert_eq!(
        mapped("read_post", Some(&Object::Post(&own_draft))),
        ["read"]
    );
    assert_eq!(
        mapped("read_post", Some(&Object::Post(&others_private))),
        ["read_private_posts"]
    );
    assert_eq!(
        mapped("read_post", Some(&Object::Post(&others_pending))),
        ["edit_others_posts"]
    );
}

#[test]
fn a_trashed_post_without_its_previous_status_counts_as_a_draft() {
    // WordPress reads `_wp_trash_meta_status`; when it is missing, the post is not published
    // or scheduled, so `edit_posts` is enough for its author.
    let trashed = post(1, ME, "post", PostStatus::Trash);
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&trashed))),
        ["edit_posts"]
    );
    let context = PostContext {
        trash_meta_status: Some(Future),
        ..PostContext::new(&trashed)
    };
    assert_eq!(
        mapped("delete_post", Some(&Object::PostWithContext(context))),
        ["delete_published_posts"]
    );
}

#[test]
fn a_missing_object_is_never_allowed() {
    for cap in [
        "edit_post",
        "edit_page",
        "delete_post",
        "delete_page",
        "read_post",
        "read_page",
        "publish_post",
        "edit_post_meta",
    ] {
        assert_eq!(mapped(cap, None), [DO_NOT_ALLOW], "{cap} with no object");
        assert!(
            !can(&user(ADMINISTRATOR), cap, None),
            "{cap} with no object"
        );
    }
}

#[test]
fn a_post_without_an_author_is_someone_elses() {
    let orphan = post(1, 0, "post", Draft);
    let nobody = UserCaps::with_default_roles(0, &[AUTHOR], &BTreeMap::new());
    assert_eq!(
        map_meta_cap("edit_post", &nobody, Some(&Object::Post(&orphan))),
        ["edit_others_posts"]
    );
}

#[test]
fn revisions_follow_their_parent() {
    let parent = post(1, ME, "post", Publish);
    let mut revision = post(2, SOMEONE, "revision", PostStatus::Inherit);
    revision.parent_id = Some(1);

    // Without its parent, a revision cannot be checked.
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&revision))),
        [DO_NOT_ALLOW]
    );
    assert_eq!(
        mapped("read_post", Some(&Object::Post(&revision))),
        [DO_NOT_ALLOW]
    );

    let parent_context = PostContext::new(&parent);
    let context = PostContext {
        parent: Some(&parent_context),
        ..PostContext::new(&revision)
    };
    let object = Object::PostWithContext(context);
    assert_eq!(mapped("edit_post", Some(&object)), ["edit_published_posts"]);
    assert_eq!(mapped("read_post", Some(&object)), ["read"]);
    // A revision is never deleted through `delete_post`.
    assert_eq!(mapped("delete_post", Some(&object)), [DO_NOT_ALLOW]);
}

#[test]
fn other_post_types_use_post_capabilities_until_the_registry() {
    let book = post(1, SOMEONE, "book", Publish);
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&book))),
        ["edit_others_posts", "edit_published_posts"]
    );
}

#[test]
fn attachments_read_their_status_as_get_post_status_does() {
    let mut unattached = post(1, SOMEONE, "attachment", PostStatus::Inherit);
    assert_eq!(
        mapped("read_post", Some(&Object::Post(&unattached))),
        ["read"]
    );

    // Attached to a draft: the draft's status decides.
    unattached.parent_id = Some(2);
    let draft = post(2, SOMEONE, "post", Draft);
    let parent = PostContext::new(&draft);
    let context = PostContext {
        parent: Some(&parent),
        ..PostContext::new(&unattached)
    };
    assert_eq!(
        mapped("read_post", Some(&Object::PostWithContext(context))),
        ["edit_others_posts"]
    );

    // Attached to a parent that was not given: unknown, so not allowed.
    assert_eq!(
        mapped("read_post", Some(&Object::Post(&unattached))),
        [DO_NOT_ALLOW]
    );

    // Attached to a trashed post that was published: published.
    let trashed = post(2, SOMEONE, "post", PostStatus::Trash);
    let parent = PostContext {
        trash_meta_status: Some(Publish),
        ..PostContext::new(&trashed)
    };
    let context = PostContext {
        parent: Some(&parent),
        ..PostContext::new(&unattached)
    };
    assert_eq!(
        mapped("read_post", Some(&Object::PostWithContext(context))),
        ["read"]
    );
}

#[test]
fn post_meta_capabilities_map_to_edit_post() {
    let own = post(1, ME, "post", Draft);
    for cap in ["edit_post_meta", "add_post_meta", "delete_post_meta"] {
        assert_eq!(mapped(cap, Some(&Object::Post(&own))), ["edit_posts"]);
    }
}

#[test]
fn primitive_capabilities_map_to_themselves() {
    assert_eq!(mapped("edit_posts", None), ["edit_posts"]);
    assert_eq!(mapped("read_private_posts", None), ["read_private_posts"]);
    assert_eq!(mapped("manage_options", None), ["manage_options"]);
    assert!(can(&user(EDITOR), "read_private_posts", None));
    assert!(!can(&user(AUTHOR), "read_private_posts", None));
}

#[test]
fn per_user_grants_count_in_meta_checks() {
    let mut extra = BTreeMap::new();
    extra.insert("edit_others_posts".to_owned(), true);
    let author = UserCaps::with_default_roles(ME, &[AUTHOR], &extra);
    let others_draft = post(1, SOMEONE, "post", Draft);
    let others_private = post(2, SOMEONE, "post", Private);
    assert!(can(
        &author,
        "edit_post",
        Some(&Object::Post(&others_draft))
    ));
    assert!(!can(
        &author,
        "edit_post",
        Some(&Object::Post(&others_private))
    ));
}
