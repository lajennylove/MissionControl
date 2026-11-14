//! Tests for taxonomies: the `terms`, `term_taxonomy` and `term_relationships` tables, terms of a
//! hierarchical and a flat taxonomy, and the terms of a post. They run on the database in
//! `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use mission_control_core::content::{
    NewPost, NewTerm, Post, PostStatus, Taxonomy, Term, TermError,
};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Value, raw};

/// Runs the core migrations and returns the id of a new user to author posts.
async fn setup(db: &Database) -> i64 {
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
    .expect("inserts the author");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the author id")
}

async fn post(db: &Database, author_id: i64, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: "Hello".into(),
            ..NewPost::new(author_id, "post")
        },
    )
    .await
    .expect("creates a post")
}

async fn category(db: &Database, name: &str, parent: Option<&Term>) -> Term {
    Term::create(
        db,
        &Taxonomy::CATEGORY,
        NewTerm {
            parent_id: parent.map(|p| p.term_taxonomy_id),
            ..NewTerm::new(name)
        },
    )
    .await
    .unwrap_or_else(|e| panic!("creates the category {name}: {e}"))
}

async fn tag(db: &Database, name: &str) -> Term {
    Term::create(db, &Taxonomy::POST_TAG, NewTerm::new(name))
        .await
        .unwrap_or_else(|e| panic!("creates the tag {name}: {e}"))
}

fn names(terms: &[Term]) -> Vec<&str> {
    terms.iter().map(|t| t.name.as_str()).collect()
}

async fn count_rows(db: &Database, table: &str) -> i64 {
    raw::fetch_scalar(db, &format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .unwrap()
}

#[test]
fn builtin_taxonomies_are_category_and_post_tag() {
    let category = Taxonomy::builtin("category").expect("category is built in");
    assert_eq!(category, Taxonomy::CATEGORY);
    assert_eq!(category.name, "category");
    assert!(category.hierarchical);
    assert_eq!(category.object_types, ["post"]);
    let tag = Taxonomy::builtin("post_tag").expect("post_tag is built in");
    assert_eq!(tag, Taxonomy::POST_TAG);
    assert_eq!(tag.name, "post_tag");
    assert!(!tag.hierarchical);
    assert_eq!(tag.object_types, ["post"]);
    assert_eq!(Taxonomy::builtin("genre"), None);
}

#[tokio::test]
async fn create_stores_the_term_and_its_taxonomy_row() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    let news = Term::create(
        db,
        &Taxonomy::CATEGORY,
        NewTerm {
            slug: "latest-news".into(),
            description: "What happened".into(),
            ..NewTerm::new("  News  ")
        },
    )
    .await
    .unwrap();
    assert_eq!(news.name, "News", "the name is trimmed");
    assert_eq!(news.slug, "latest-news", "the given slug is kept");
    assert_eq!(news.taxonomy, "category");
    assert_eq!(news.description, "What happened");
    assert_eq!(news.parent_id, None);
    assert_eq!(news.count, 0);
    assert_eq!(news.term_group, 0);

    assert_eq!(
        Term::find(db, news.term_taxonomy_id).await.unwrap(),
        Some(news.clone())
    );
    assert_eq!(
        Term::find_by_slug(db, &Taxonomy::CATEGORY, "latest-news")
            .await
            .unwrap(),
        Some(news.clone())
    );
    assert_eq!(
        Term::find_by_slug(db, &Taxonomy::POST_TAG, "latest-news")
            .await
            .unwrap(),
        None
    );
    assert_eq!(count_rows(db, "terms").await, 1);
    assert_eq!(count_rows(db, "term_taxonomy").await, 1);
}

#[tokio::test]
async fn an_empty_slug_comes_from_the_name() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    let term = tag(db, "Hello, World! 2026").await;
    assert_eq!(term.slug, "hello-world-2026");
}

#[tokio::test]
async fn an_empty_name_is_refused() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    let err = Term::create(db, &Taxonomy::POST_TAG, NewTerm::new("   "))
        .await
        .unwrap_err();
    assert!(matches!(err, TermError::EmptyName), "{err:?}");
    assert_eq!(count_rows(db, "terms").await, 0);
}

#[tokio::test]
async fn a_hierarchical_taxonomy_keeps_parent_and_child() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    let food = category(db, "Food", None).await;
    let fruit = category(db, "Fruit", Some(&food)).await;
    let apple = category(db, "Apple", Some(&fruit)).await;
    assert_eq!(fruit.parent_id, Some(food.term_taxonomy_id));
    assert_eq!(
        Term::find(db, apple.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .parent_id,
        Some(fruit.term_taxonomy_id)
    );
    assert_eq!(names(&food.children(db).await.unwrap()), ["Fruit"]);
    assert_eq!(names(&fruit.children(db).await.unwrap()), ["Apple"]);
    assert!(apple.children(db).await.unwrap().is_empty());
    assert_eq!(
        names(&Term::all(db, &Taxonomy::CATEGORY).await.unwrap()),
        ["Apple", "Food", "Fruit"],
        "ordered by name"
    );
}

#[tokio::test]
async fn a_flat_taxonomy_refuses_a_parent() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    let rust = tag(db, "Rust").await;
    let err = Term::create(
        db,
        &Taxonomy::POST_TAG,
        NewTerm {
            parent_id: Some(rust.term_taxonomy_id),
            ..NewTerm::new("Rocket")
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            TermError::NotHierarchical {
                taxonomy: "post_tag"
            }
        ),
        "{err:?}"
    );
    assert_eq!(count_rows(db, "terms").await, 1);
}

#[tokio::test]
async fn a_parent_must_exist_in_the_same_taxonomy() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    let rust = tag(db, "Rust").await;
    for parent in [rust.term_taxonomy_id, rust.term_taxonomy_id + 1000] {
        let err = Term::create(
            db,
            &Taxonomy::CATEGORY,
            NewTerm {
                parent_id: Some(parent),
                ..NewTerm::new("Child")
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TermError::MissingParent), "{err:?}");
    }
    assert_eq!(count_rows(db, "terms").await, 1);
}

#[tokio::test]
async fn a_duplicate_name_is_refused_as_wordpress_does() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    // Flat: the same name anywhere in the taxonomy, in any case.
    let rust = tag(db, "Rust").await;
    let err = Term::create(db, &Taxonomy::POST_TAG, NewTerm::new("rust"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, TermError::Exists { term_taxonomy_id } if term_taxonomy_id == rust.term_taxonomy_id),
        "{err:?}"
    );
    // A distinct slug of its own makes a second term with the same name.
    let other = Term::create(
        db,
        &Taxonomy::POST_TAG,
        NewTerm {
            slug: "rust-game".into(),
            ..NewTerm::new("Rust")
        },
    )
    .await
    .unwrap();
    assert_eq!(other.slug, "rust-game");

    // Hierarchical: the same name under the same parent.
    let food = category(db, "Food", None).await;
    let apple = category(db, "Apple", Some(&food)).await;
    let err = Term::create(
        db,
        &Taxonomy::CATEGORY,
        NewTerm {
            parent_id: Some(food.term_taxonomy_id),
            ..NewTerm::new("Apple")
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TermError::Exists { term_taxonomy_id } if term_taxonomy_id == apple.term_taxonomy_id),
        "{err:?}"
    );

    // Another taxonomy may reuse the name and the slug.
    let category_rust = category(db, "Rust", None).await;
    assert_eq!(category_rust.slug, "rust");
}

#[tokio::test]
async fn a_taken_slug_gets_a_unique_one() {
    let test = database().await;
    let db = test.db();
    setup(db).await;

    // A child under another parent takes its parent's slug as a suffix.
    let food = category(db, "Food", None).await;
    let tech = category(db, "Tech", None).await;
    let apple = category(db, "Apple", Some(&food)).await;
    let apple_tech = category(db, "Apple", Some(&tech)).await;
    assert_eq!(apple.slug, "apple");
    assert_eq!(apple_tech.slug, "apple-tech");

    // Without a parent, a number.
    tag(db, "Rust").await;
    let second = Term::create(
        db,
        &Taxonomy::POST_TAG,
        NewTerm {
            slug: "rust".into(),
            ..NewTerm::new("Rust language")
        },
    )
    .await
    .unwrap();
    assert_eq!(second.slug, "rust-2");
    let third = Term::create(
        db,
        &Taxonomy::POST_TAG,
        NewTerm {
            slug: "rust".into(),
            ..NewTerm::new("Rust belt")
        },
    )
    .await
    .unwrap();
    assert_eq!(third.slug, "rust-3");
}

#[tokio::test]
async fn a_post_has_several_terms_of_several_taxonomies() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = post(db, author, PostStatus::Publish).await;
    let news = category(db, "News", None).await;
    let sport = category(db, "Sport", None).await;
    let rust = tag(db, "Rust").await;
    let rocket = tag(db, "Rocket").await;

    post.sync_terms(
        db,
        &Taxonomy::CATEGORY,
        &[sport.term_taxonomy_id, news.term_taxonomy_id],
    )
    .await
    .unwrap();
    post.sync_terms(
        db,
        &Taxonomy::POST_TAG,
        &[rust.term_taxonomy_id, rocket.term_taxonomy_id],
    )
    .await
    .unwrap();
    assert_eq!(
        names(&post.terms(db, &Taxonomy::CATEGORY).await.unwrap()),
        ["News", "Sport"]
    );
    assert_eq!(
        names(&post.terms(db, &Taxonomy::POST_TAG).await.unwrap()),
        ["Rocket", "Rust"]
    );
    for term in post.terms(db, &Taxonomy::CATEGORY).await.unwrap() {
        assert_eq!(term.count, 1, "{} counts the post", term.name);
    }

    // Sync replaces the post's terms of that taxonomy only, and keeps the counts in step.
    post.sync_terms(db, &Taxonomy::CATEGORY, &[news.term_taxonomy_id])
        .await
        .unwrap();
    assert_eq!(
        names(&post.terms(db, &Taxonomy::CATEGORY).await.unwrap()),
        ["News"]
    );
    assert_eq!(post.terms(db, &Taxonomy::POST_TAG).await.unwrap().len(), 2);
    assert_eq!(
        Term::find(db, sport.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        0
    );
    assert_eq!(
        Term::find(db, news.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        1
    );
}

#[tokio::test]
async fn attach_and_detach_change_only_the_given_terms() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = post(db, author, PostStatus::Publish).await;
    let rust = tag(db, "Rust").await;
    let rocket = tag(db, "Rocket").await;
    let news = category(db, "News", None).await;

    post.attach_terms(db, &Taxonomy::POST_TAG, &[rust.term_taxonomy_id])
        .await
        .unwrap();
    // Attaching again, a missing id and a term of another taxonomy change nothing.
    post.attach_terms(
        db,
        &Taxonomy::POST_TAG,
        &[
            rocket.term_taxonomy_id,
            rust.term_taxonomy_id,
            news.term_taxonomy_id,
            rocket.term_taxonomy_id + 1000,
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        names(&post.terms(db, &Taxonomy::POST_TAG).await.unwrap()),
        ["Rocket", "Rust"]
    );
    assert!(
        post.terms(db, &Taxonomy::CATEGORY)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(count_rows(db, "term_relationships").await, 2);
    assert_eq!(
        Term::find(db, rust.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        1
    );

    let removed = post
        .detach_terms(db, &Taxonomy::POST_TAG, &[rust.term_taxonomy_id])
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        names(&post.terms(db, &Taxonomy::POST_TAG).await.unwrap()),
        ["Rocket"]
    );
    assert_eq!(
        Term::find(db, rust.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        0
    );
}

#[tokio::test]
async fn count_is_the_number_of_published_posts() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let rust = tag(db, "Rust").await;
    for status in [PostStatus::Publish, PostStatus::Publish, PostStatus::Draft] {
        post(db, author, status)
            .await
            .attach_terms(db, &Taxonomy::POST_TAG, &[rust.term_taxonomy_id])
            .await
            .unwrap();
    }
    assert_eq!(
        Term::find(db, rust.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .count,
        2
    );
}

#[tokio::test]
async fn deleting_a_term_removes_its_relationships_and_lifts_its_children() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = post(db, author, PostStatus::Publish).await;
    let food = category(db, "Food", None).await;
    let fruit = category(db, "Fruit", Some(&food)).await;
    let apple = category(db, "Apple", Some(&fruit)).await;
    let pear = category(db, "Pear", Some(&fruit)).await;
    post.sync_terms(
        db,
        &Taxonomy::CATEGORY,
        &[fruit.term_taxonomy_id, apple.term_taxonomy_id],
    )
    .await
    .unwrap();

    assert!(fruit.clone().delete(db).await.unwrap());

    assert_eq!(Term::find(db, fruit.term_taxonomy_id).await.unwrap(), None);
    assert_eq!(
        names(&post.terms(db, &Taxonomy::CATEGORY).await.unwrap()),
        ["Apple"]
    );
    assert_eq!(count_rows(db, "term_relationships").await, 1);
    assert_eq!(count_rows(db, "terms").await, 3);
    // WordPress gives the children of a deleted term its parent.
    for child in [&apple, &pear] {
        let child = Term::find(db, child.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            child.parent_id,
            Some(food.term_taxonomy_id),
            "{}",
            child.name
        );
    }
    assert_eq!(names(&food.children(db).await.unwrap()), ["Apple", "Pear"]);

    // A top-level term's children become top-level; a second delete finds nothing.
    assert!(food.clone().delete(db).await.unwrap());
    assert_eq!(
        Term::find(db, apple.term_taxonomy_id)
            .await
            .unwrap()
            .unwrap()
            .parent_id,
        None
    );
    assert!(!food.delete(db).await.unwrap());
}

#[tokio::test]
async fn deleting_a_post_removes_its_relationships() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = post(db, author, PostStatus::Publish).await;
    let rust = tag(db, "Rust").await;
    post.attach_terms(db, &Taxonomy::POST_TAG, &[rust.term_taxonomy_id])
        .await
        .unwrap();

    raw::execute(
        db,
        "DELETE FROM posts WHERE id = ?",
        &[Value::from(post.id)],
    )
    .await
    .unwrap();
    assert_eq!(count_rows(db, "term_relationships").await, 0);
    assert!(
        Term::find(db, rust.term_taxonomy_id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_relationship_is_unique_per_post_and_term() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = post(db, author, PostStatus::Publish).await;
    let rust = tag(db, "Rust").await;
    let insert = "INSERT INTO term_relationships (object_id, term_taxonomy_id) VALUES (?, ?)";
    let row = [Value::from(post.id), Value::from(rust.term_taxonomy_id)];
    raw::execute(db, insert, &row).await.unwrap();
    assert!(matches!(
        raw::execute(db, insert, &row).await,
        Err(orbit_orm::Error::UniqueViolation { .. })
    ));
}
