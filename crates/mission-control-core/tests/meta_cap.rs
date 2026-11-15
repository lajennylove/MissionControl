//! Tests for meta capabilities: `map_meta_cap` for posts and pages, as WordPress maps
//! `edit_post`, `delete_post`, `read_post` and `publish_post` to primitive capabilities, and the
//! table of what each default role may do with a post in each state, its own or someone else's.
//! Pure logic: no database.

use std::collections::BTreeMap;

use chrono::NaiveDateTime;
use mission_control_core::content::{Post, PostStatus};
use mission_control_core::users::{
    ADMINISTRATOR, AUTHOR, CONTRIBUTOR, DO_NOT_ALLOW, EDITOR, Object, PostContext, SUBSCRIBER,
    SitePages, TaxonomyCaps, TermContext, UserCaps, can, map_meta_cap,
};

/// `post` on a site with none of the special pages set.
fn known(post: &Post) -> Object<'_> {
    Object::PostWithContext(PostContext {
        site_pages: Some(SitePages::NONE),
        ..PostContext::new(post)
    })
}

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
    (EDITOR,        "page", TrashedFrom(Publish), OTHER, "yyyy"),
    (EDITOR,        "page", TrashedFrom(Draft),   OWN,   "yyyy"),
    (ADMINISTRATOR, "page", TrashedFrom(Publish), OTHER, "yyyy"),
    (ADMINISTRATOR, "page", TrashedFrom(Draft),   OWN,   "yyyy"),
    (AUTHOR,        "page", Is(Publish),          OWN,   "nnyn"),
    (AUTHOR,        "page", Is(Draft),            OWN,   "nnyn"),
    (AUTHOR,        "page", Is(Publish),          OTHER, "nnyn"),
    (AUTHOR,        "page", Is(Future),           OTHER, "nnnn"),
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
            site_pages: Some(SitePages::NONE),
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
            mapped(cap, Some(&known(&others_page))),
            ["edit_others_pages", "edit_published_pages"]
        );
    }
    assert_eq!(
        mapped("delete_page", Some(&known(&others_page))),
        ["delete_others_pages", "delete_published_pages"]
    );
    assert_eq!(
        mapped("publish_post", Some(&known(&others_page))),
        ["publish_pages"]
    );
    let draft_page = post(2, SOMEONE, "page", Draft);
    assert_eq!(
        mapped("read_page", Some(&known(&draft_page))),
        ["edit_others_pages"]
    );
    let private_page = post(3, SOMEONE, "page", Private);
    assert_eq!(
        mapped("read_page", Some(&known(&private_page))),
        ["read_private_pages"]
    );
}

/// Which special page a row's page is.
#[derive(Debug, Clone, Copy)]
enum Special {
    /// None of them, on a site whose special pages are known.
    Plain,
    /// The static front page.
    Front,
    /// The posts page.
    PostsPage,
    /// The privacy policy page.
    Privacy,
    /// Unknown: the site's special pages were not given.
    Unknown,
}

/// The special pages: who, which page (someone else's published page, id 1), and whether
/// `edit_post` and `delete_post` are allowed. WordPress requires `manage_options` to delete the
/// front page or the posts page, and to edit or delete the privacy policy page; when the pages
/// are not known, a page is checked as if it might be any of them.
#[rustfmt::skip]
const SPECIAL_PAGES: &[(&str, Special, &str)] = &[
    (EDITOR,        Special::Plain,     "yy"),
    (EDITOR,        Special::Front,     "yn"),
    (EDITOR,        Special::PostsPage, "yn"),
    (EDITOR,        Special::Privacy,   "nn"),
    (EDITOR,        Special::Unknown,   "nn"),
    (ADMINISTRATOR, Special::Plain,     "yy"),
    (ADMINISTRATOR, Special::Front,     "yy"),
    (ADMINISTRATOR, Special::PostsPage, "yy"),
    (ADMINISTRATOR, Special::Privacy,   "yy"),
    (ADMINISTRATOR, Special::Unknown,   "yy"),
    (AUTHOR,        Special::Front,     "nn"),
];

#[test]
fn the_special_pages_table() {
    let page = post(1, SOMEONE, "page", Publish);
    let mut failures = Vec::new();
    for &(role, special, expected) in SPECIAL_PAGES {
        let site_pages = match special {
            Special::Plain => Some(SitePages::NONE),
            Special::Front => Some(SitePages {
                page_on_front: Some(1),
                ..SitePages::NONE
            }),
            Special::PostsPage => Some(SitePages {
                page_for_posts: Some(1),
                ..SitePages::NONE
            }),
            Special::Privacy => Some(SitePages {
                privacy_policy: Some(1),
                ..SitePages::NONE
            }),
            Special::Unknown => None,
        };
        let object = Object::PostWithContext(PostContext {
            site_pages,
            ..PostContext::new(&page)
        });
        let user = user(role);
        let got: String = ["edit_post", "delete_post"]
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
                "{role} on {special:?}: expected {expected}, got {got}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn special_pages_map_as_wordpress_does() {
    let page = post(1, SOMEONE, "page", Publish);
    let front = Object::PostWithContext(PostContext {
        site_pages: Some(SitePages {
            page_on_front: Some(1),
            ..SitePages::NONE
        }),
        ..PostContext::new(&page)
    });
    assert_eq!(mapped("delete_post", Some(&front)), ["manage_options"]);
    assert_eq!(
        mapped("edit_post", Some(&front)),
        ["edit_others_pages", "edit_published_pages"]
    );
    let privacy = Object::PostWithContext(PostContext {
        site_pages: Some(SitePages {
            privacy_policy: Some(1),
            ..SitePages::NONE
        }),
        ..PostContext::new(&page)
    });
    assert_eq!(
        mapped("edit_post", Some(&privacy)),
        [
            "edit_others_pages",
            "edit_published_pages",
            "manage_options"
        ]
    );
    assert_eq!(mapped("read_post", Some(&privacy)), ["read"]);
    // Not given: the strictest outcome, for editing and deleting a page.
    assert_eq!(
        mapped("delete_post", Some(&Object::Post(&page))),
        [
            "delete_others_pages",
            "delete_published_pages",
            "manage_options"
        ]
    );
    // A post is never one of the special pages when they are not given.
    let others_post = post(2, SOMEONE, "post", Publish);
    assert_eq!(
        mapped("delete_post", Some(&Object::Post(&others_post))),
        ["delete_others_posts", "delete_published_posts"]
    );
    // A revision of the privacy page counts as the privacy page.
    let mut revision = post(3, SOMEONE, "revision", PostStatus::Inherit);
    revision.parent_id = Some(1);
    let parent = PostContext::new(&page);
    let object = Object::PostWithContext(PostContext {
        parent: Some(&parent),
        site_pages: Some(SitePages {
            privacy_policy: Some(1),
            ..SitePages::NONE
        }),
        ..PostContext::new(&revision)
    });
    assert_eq!(
        mapped("edit_post", Some(&object)),
        [
            "edit_others_pages",
            "edit_published_pages",
            "manage_options"
        ]
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

    // A parent whose id is not the revision's `parent_id` is not its parent.
    let stranger = post(99, ME, "post", Publish);
    let stranger_context = PostContext::new(&stranger);
    let object = Object::PostWithContext(PostContext {
        parent: Some(&stranger_context),
        ..PostContext::new(&revision)
    });
    assert_eq!(mapped("edit_post", Some(&object)), [DO_NOT_ALLOW]);
    assert_eq!(mapped("read_post", Some(&object)), [DO_NOT_ALLOW]);
}

#[test]
fn other_post_types_follow_the_interim_rule_until_the_registry() {
    // Ruling R24. An unregistered type needs `edit_others_posts`, as WordPress maps it.
    let book = post(1, ME, "book", Draft);
    for cap in ["edit_post", "delete_post", "read_post", "publish_post"] {
        assert_eq!(
            mapped(cap, Some(&Object::Post(&book))),
            ["edit_others_posts"],
            "{cap}"
        );
    }
    assert!(!can(&user(AUTHOR), "edit_post", Some(&Object::Post(&book))));
    assert!(can(&user(EDITOR), "edit_post", Some(&Object::Post(&book))));

    // Attachments use the post capabilities.
    let image = post(2, SOMEONE, "attachment", PostStatus::Inherit);
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&image))),
        ["edit_others_posts"]
    );

    // WordPress's theme types need `edit_theme_options`.
    for post_type in [
        "nav_menu_item",
        "wp_template",
        "wp_template_part",
        "wp_global_styles",
        "wp_navigation",
    ] {
        let item = post(3, SOMEONE, post_type, Publish);
        assert_eq!(
            mapped("edit_post", Some(&Object::Post(&item))),
            ["edit_theme_options"],
            "{post_type}"
        );
        assert!(!can(&user(EDITOR), "edit_post", Some(&Object::Post(&item))));
        assert!(can(
            &user(ADMINISTRATOR),
            "delete_post",
            Some(&Object::Post(&item))
        ));
    }

    // Changesets need `customize`, and a published one is never edited.
    let draft = post(4, ME, "customize_changeset", Draft);
    let published = post(5, ME, "customize_changeset", Publish);
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&draft))),
        ["customize"]
    );
    assert_eq!(
        mapped("edit_post", Some(&Object::Post(&published))),
        [DO_NOT_ALLOW]
    );
    assert_eq!(
        mapped("publish_post", Some(&Object::Post(&published))),
        ["customize"]
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
fn post_meta_capabilities_map_to_edit_post_and_refuse_protected_keys() {
    let own = post(1, ME, "post", Draft);
    let meta = |key| Object::PostMeta {
        post: PostContext::new(&own),
        key,
    };
    let author = user(AUTHOR);
    for cap in ["edit_post_meta", "add_post_meta", "delete_post_meta"] {
        assert_eq!(mapped(cap, Some(&meta("subtitle"))), ["edit_posts"]);
        assert!(can(&author, cap, Some(&meta("subtitle"))));
        // A protected key also needs the meta capability itself, which nobody holds.
        assert_eq!(
            mapped(cap, Some(&meta("_thumbnail_id"))),
            ["edit_posts", cap]
        );
        assert!(!can(&author, cap, Some(&meta("_thumbnail_id"))));
        // With no key given, the key is taken to be protected.
        assert_eq!(mapped(cap, Some(&Object::Post(&own))), ["edit_posts", cap]);
        assert!(!can(&author, cap, Some(&Object::Post(&own))));
    }
    // The leading-character test skips every character outside printable ASCII, so a key is
    // protected whenever WordPress's byte-wise `is_protected_meta` protects it: a zero-width
    // space or a Hebrew letter before `_` does not unprotect the key.
    assert!(!can(
        &author,
        "edit_post_meta",
        Some(&meta("\u{200B}_secret"))
    ));
    assert!(!can(&author, "edit_post_meta", Some(&meta("\u{05D0}_x"))));
    assert!(can(&author, "edit_post_meta", Some(&meta(" _spaced"))));
    assert!(can(&author, "edit_post_meta", Some(&meta(""))));

    // Granting the meta capability by name allows a protected key, as a filter would.
    let mut extra = BTreeMap::new();
    extra.insert("edit_post_meta".to_owned(), true);
    let trusted = UserCaps::with_default_roles(ME, &[AUTHOR], &extra);
    assert!(can(
        &trusted,
        "edit_post_meta",
        Some(&meta("_thumbnail_id"))
    ));
    // Someone else's post still needs `edit_others_posts`.
    let others = post(2, SOMEONE, "post", Draft);
    let others_meta = Object::PostMeta {
        post: PostContext::new(&others),
        key: "subtitle",
    };
    assert!(!can(&author, "edit_post_meta", Some(&others_meta)));
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

/// A term of `taxonomy`, with term 1 as the taxonomy's default.
fn term(term_id: i64, taxonomy: &str) -> Object<'_> {
    Object::Term(TermContext {
        term_id,
        taxonomy,
        default_terms: &[1],
    })
}

#[test]
fn term_capabilities_map_as_wordpress_maps_them() {
    // The tag capabilities map to the category ones, as `map_meta_cap` maps them.
    for cap in [
        "manage_post_tags",
        "edit_post_tags",
        "delete_post_tags",
        "edit_categories",
        "delete_categories",
    ] {
        assert_eq!(mapped(cap, None), ["manage_categories"], "{cap}");
    }
    for cap in ["assign_post_tags", "assign_categories"] {
        assert_eq!(mapped(cap, None), ["edit_posts"], "{cap}");
    }
    assert_eq!(
        mapped("edit_term", Some(&term(2, "post_tag"))),
        ["manage_categories"]
    );
    assert_eq!(
        mapped("assign_term", Some(&term(2, "post_tag"))),
        ["edit_posts"]
    );
    assert_eq!(
        mapped("delete_term", Some(&term(2, "category"))),
        ["manage_categories"]
    );
    // The default term cannot be deleted, and a term check needs a term.
    assert_eq!(
        mapped("delete_term", Some(&term(1, "category"))),
        [DO_NOT_ALLOW]
    );
    assert_eq!(mapped("edit_term", None), [DO_NOT_ALLOW]);
    // WordPress 4.7's category names, which map back to the primitive capabilities.
    let category = TaxonomyCaps::for_taxonomy("category");
    assert_eq!(
        [
            category.manage_terms.as_str(),
            category.edit_terms.as_str(),
            category.delete_terms.as_str(),
            category.assign_terms.as_str(),
        ],
        [
            "manage_categories",
            "edit_categories",
            "delete_categories",
            "assign_categories"
        ]
    );
    assert_eq!(mapped(&category.assign_terms, None), ["edit_posts"]);
    assert_eq!(mapped(&category.edit_terms, None), ["manage_categories"]);
    // A term that is the default by either option cannot be deleted.
    let by_term_option = Object::Term(TermContext {
        term_id: 7,
        taxonomy: "genre",
        default_terms: &[3, 7],
    });
    assert_eq!(mapped("delete_term", Some(&by_term_option)), [DO_NOT_ALLOW]);
}

#[test]
fn editors_manage_terms_and_authors_assign_them() {
    let tag = term(2, "post_tag");
    let category = term(3, "category");
    for (role, manage) in [
        (ADMINISTRATOR, true),
        (EDITOR, true),
        (AUTHOR, false),
        (CONTRIBUTOR, false),
        (SUBSCRIBER, false),
    ] {
        let user = user(role);
        for object in [&tag, &category] {
            assert_eq!(can(&user, "edit_term", Some(object)), manage, "{role}");
            assert_eq!(can(&user, "delete_term", Some(object)), manage, "{role}");
            assert_eq!(
                can(&user, "assign_term", Some(object)),
                role != SUBSCRIBER,
                "{role}"
            );
        }
        assert!(
            !can(&user, "delete_term", Some(&term(1, "category"))),
            "{role}"
        );
    }
}
