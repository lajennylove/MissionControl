//! The template hierarchy: the candidates for a resolved request, in WordPress's order.

use chrono::NaiveDate;
use mission_control_core::content::{Post, PostStatus, Term};
use mission_control_core::front::{Archive, Resolved};
use mission_control_core::theme::templates_for;

fn post(id: i64, post_type: &str, slug: &str) -> Post {
    Post {
        id,
        author_id: 1,
        post_type: post_type.to_owned(),
        status: PostStatus::Publish,
        title: slug.to_owned(),
        slug: slug.to_owned(),
        content: String::new(),
        excerpt: String::new(),
        parent_id: None,
        menu_order: 0,
        published_at: NaiveDate::from_ymd_opt(2026, 3, 1).map(|d| d.and_hms_opt(0, 0, 0).unwrap()),
        modified_at: NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
        password: None,
        mime_type: String::new(),
    }
}

fn term(id: i64, taxonomy: &str, slug: &str) -> Term {
    Term {
        term_id: id,
        term_taxonomy_id: id,
        name: slug.to_owned(),
        slug: slug.to_owned(),
        term_group: 0,
        taxonomy: taxonomy.to_owned(),
        description: String::new(),
        parent_id: None,
        count: 0,
    }
}

fn attachment(id: i64, slug: &str, mime: &str) -> Post {
    Post {
        mime_type: mime.to_owned(),
        ..post(id, "attachment", slug)
    }
}

fn archive(archive: Archive) -> Resolved {
    Resolved::Archive { archive, paged: 1 }
}

#[test]
fn the_hierarchy_for_each_kind_of_request() {
    let cases: Vec<(&str, Resolved, Option<&str>, Vec<&str>)> = vec![
        (
            "a post",
            Resolved::Single(post(7, "post", "hello-world")),
            None,
            vec![
                "single-post-hello-world",
                "single-post",
                "single",
                "singular",
                "index",
            ],
        ),
        (
            "a custom type's entry",
            Resolved::Single(post(8, "book", "dune")),
            None,
            vec![
                "single-book-dune",
                "single-book",
                "single",
                "singular",
                "index",
            ],
        ),
        (
            "a post with a custom template",
            Resolved::Single(post(7, "post", "hello-world")),
            Some("templates/wide.php"),
            vec![
                "templates/wide.php",
                "single-post-hello-world",
                "single-post",
                "single",
                "singular",
                "index",
            ],
        ),
        (
            "a page",
            Resolved::Page(post(12, "page", "about")),
            None,
            vec!["page-about", "page-12", "page", "singular", "index"],
        ),
        (
            "a page with a custom template",
            Resolved::Page(post(12, "page", "about")),
            Some("templates/wide.php"),
            vec![
                "templates/wide.php",
                "page-about",
                "page-12",
                "page",
                "singular",
                "index",
            ],
        ),
        (
            "a page whose template is default",
            Resolved::Page(post(12, "page", "about")),
            Some("default"),
            vec!["page-about", "page-12", "page", "singular", "index"],
        ),
        (
            "a page whose template tries to leave the theme",
            Resolved::Page(post(12, "page", "about")),
            Some("../../etc/passwd"),
            vec!["page-about", "page-12", "page", "singular", "index"],
        ),
        (
            "a page with a non-ASCII slug, stored percent-encoded",
            Resolved::Page(post(13, "page", "caf%c3%a9")),
            None,
            vec![
                "page-café",
                "page-caf%c3%a9",
                "page-13",
                "page",
                "singular",
                "index",
            ],
        ),
        (
            "a page with a non-ASCII slug, stored as letters",
            Resolved::Page(post(13, "page", "café")),
            None,
            vec![
                "page-café",
                "page-caf%c3%a9",
                "page-13",
                "page",
                "singular",
                "index",
            ],
        ),
        (
            "a post type archive",
            archive(Archive::PostType("book".to_owned())),
            None,
            vec!["archive-book", "archive", "index"],
        ),
        (
            "a category",
            archive(Archive::Category(term(4, "category", "news"))),
            None,
            vec![
                "category-news",
                "category-4",
                "category",
                "archive",
                "index",
            ],
        ),
        (
            "a tag",
            archive(Archive::Tag(term(5, "post_tag", "rust"))),
            None,
            vec!["tag-rust", "tag-5", "tag", "archive", "index"],
        ),
        (
            "a date archive",
            archive(Archive::Date {
                year: 2026,
                month: Some(3),
                day: None,
            }),
            None,
            vec!["date", "archive", "index"],
        ),
        (
            "an author",
            archive(Archive::Author(3)),
            None,
            vec!["author-3", "author", "archive", "index"],
        ),
        (
            "a search",
            Resolved::Search {
                terms: "rust".to_owned(),
                paged: 1,
            },
            None,
            vec!["search", "index"],
        ),
        ("a 404", Resolved::NotFound, None, vec!["404", "index"]),
        (
            "the latest posts as the front page",
            Resolved::FrontPage {
                page: None,
                paged: 1,
            },
            None,
            vec!["front-page", "home", "index"],
        ),
        (
            "a static front page",
            Resolved::FrontPage {
                page: Some(post(20, "page", "welcome")),
                paged: 1,
            },
            None,
            vec![
                "front-page",
                "page-welcome",
                "page-20",
                "page",
                "singular",
                "index",
            ],
        ),
        (
            "a static front page with a custom template",
            Resolved::FrontPage {
                page: Some(post(20, "page", "welcome")),
                paged: 1,
            },
            Some("templates/landing.php"),
            vec![
                "front-page",
                "templates/landing.php",
                "page-welcome",
                "page-20",
                "page",
                "singular",
                "index",
            ],
        ),
        (
            "the posts page",
            Resolved::Home {
                page: post(21, "page", "blog"),
                paged: 1,
            },
            Some("templates/ignored.php"),
            vec!["home", "index"],
        ),
        (
            "an image attachment",
            Resolved::Attachment(attachment(30, "photo", "image/jpeg")),
            None,
            vec![
                "image-jpeg",
                "jpeg",
                "image",
                "attachment",
                "single-attachment-photo",
                "single-attachment",
                "single",
                "singular",
                "index",
            ],
        ),
        (
            "an attachment without a MIME type",
            Resolved::Attachment(attachment(31, "blob", "")),
            None,
            vec![
                "attachment",
                "single-attachment-blob",
                "single-attachment",
                "single",
                "singular",
                "index",
            ],
        ),
        (
            "a redirect",
            Resolved::Redirect {
                location: "/new/".to_owned(),
                status: 301,
            },
            None,
            vec![],
        ),
    ];
    for (name, resolved, page_template, expected) in cases {
        assert_eq!(templates_for(&resolved, page_template), expected, "{name}");
    }
}

#[test]
fn a_paged_list_has_the_same_templates_as_its_first_page() {
    let first = Resolved::Archive {
        archive: Archive::Author(3),
        paged: 1,
    };
    let third = Resolved::Archive {
        archive: Archive::Author(3),
        paged: 3,
    };
    assert_eq!(templates_for(&first, None), templates_for(&third, None));
}

#[test]
fn a_term_slug_with_encoded_letters_lists_both_forms() {
    let resolved = archive(Archive::Category(term(9, "category", "caf%c3%a9")));
    assert_eq!(
        templates_for(&resolved, None),
        [
            "category-café",
            "category-caf%c3%a9",
            "category-9",
            "category",
            "archive",
            "index"
        ]
    );
}
