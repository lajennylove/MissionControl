//! Who may use the admin screens of a post type (Ruling R115, Task 6.1): the type's whole
//! capability set and `unfiltered_html`, then `edit_post` or `delete_post` on the post, with the
//! site's special pages. They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite
//! file when it is unset.

use std::collections::BTreeMap;

use mission_control_core::admin::{PostScreen, Refusal, authorize_post_screen};
use mission_control_core::content::{NewPost, Post, PostStatus, set_option};
use mission_control_core::migrations::migrator;
use mission_control_core::types::builtin_post_types;
use mission_control_core::users::{AUTHOR, EDITOR, SUBSCRIBER, UserCaps};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, raw};
use serde_json::json;

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

async fn published(db: &Database, author: i64, post_type: &str) -> Post {
    Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: format!("A published {post_type}"),
            ..NewPost::new(author, post_type)
        },
    )
    .await
    .expect("creates the post")
}

fn caps(id: i64, role: &'static str) -> UserCaps {
    UserCaps::with_default_roles(id, &[role], &BTreeMap::new())
}

#[tokio::test]
async fn editors_use_the_screens_and_the_post_checks_apply() {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator().unwrap().run(db).await.expect("migrates");
    let (owner, editor_id, author_id) = (
        add_user(db, "owner").await,
        add_user(db, "edna").await,
        add_user(db, "arthur").await,
    );
    let [post_def, page_def] = builtin_post_types();
    let editor = caps(editor_id, EDITOR);
    let author = caps(author_id, AUTHOR);
    let someones_post = published(db, owner, "post").await;
    let privacy = published(db, owner, "page").await;
    set_option(db, "wp_page_for_privacy_policy", &json!(privacy.id))
        .await
        .unwrap();

    let check = async |user: &UserCaps, def, screen| {
        authorize_post_screen(db, user, def, screen)
            .await
            .expect("checks")
    };

    // An editor: every screen, and another user's published post.
    for screen in [
        PostScreen::List,
        PostScreen::New,
        PostScreen::Edit(someones_post.id),
        PostScreen::Update(someones_post.id),
        PostScreen::Delete(someones_post.id),
        PostScreen::Other,
    ] {
        assert_eq!(check(&editor, &post_def, screen).await, None, "{screen:?}");
    }
    // Ignite's create is never used (Ruling R116).
    assert_eq!(
        check(&editor, &post_def, PostScreen::Create).await,
        Some(Refusal::Create)
    );
    // A post of another type, or none, is Ignite's 404, not a refusal.
    assert_eq!(
        check(&editor, &page_def, PostScreen::Edit(someones_post.id)).await,
        None
    );
    assert_eq!(
        check(&editor, &post_def, PostScreen::Edit(i64::MAX)).await,
        None
    );
    // The privacy policy page needs `manage_options`, which an editor lacks.
    assert_eq!(
        check(&editor, &page_def, PostScreen::Edit(privacy.id)).await,
        Some(Refusal::EditPost)
    );
    assert_eq!(
        check(&editor, &page_def, PostScreen::Delete(privacy.id)).await,
        Some(Refusal::TrashPost)
    );
    let mut trashed = privacy.clone();
    trashed.trash(db).await.unwrap();
    assert_eq!(
        check(&editor, &page_def, PostScreen::Delete(privacy.id)).await,
        Some(Refusal::DeletePost)
    );

    // The front page: an editor may edit it, not trash it (`manage_options`); a save of it once
    // it is in the trash is a restore, which needs `delete_post` too (`wp_untrash_post`).
    let front = published(db, owner, "page").await;
    set_option(db, "page_on_front", &json!(front.id))
        .await
        .unwrap();
    assert_eq!(
        check(&editor, &page_def, PostScreen::Update(front.id)).await,
        None
    );
    assert_eq!(
        check(&editor, &page_def, PostScreen::Delete(front.id)).await,
        Some(Refusal::TrashPost)
    );
    let mut trashed_front = front.clone();
    trashed_front.trash(db).await.unwrap();
    assert_eq!(
        check(&editor, &page_def, PostScreen::Edit(front.id)).await,
        None
    );
    assert_eq!(
        check(&editor, &page_def, PostScreen::Update(front.id)).await,
        Some(Refusal::RestorePost)
    );
    // An editor restores another user's trashed post.
    let mut trashed_post = someones_post.clone();
    trashed_post.trash(db).await.unwrap();
    assert_eq!(
        check(&editor, &post_def, PostScreen::Update(someones_post.id)).await,
        None
    );

    // Authors and subscribers do not hold the whole set, even on their own post.
    let own = published(db, author_id, "post").await;
    assert_eq!(
        check(&author, &post_def, PostScreen::Edit(own.id)).await,
        Some(Refusal::Screen)
    );
    assert_eq!(
        check(&author, &post_def, PostScreen::New).await,
        Some(Refusal::NewScreen)
    );
    assert_eq!(
        check(&caps(author_id, SUBSCRIBER), &page_def, PostScreen::List).await,
        Some(Refusal::Screen)
    );
    assert_eq!(
        Refusal::Screen.message(),
        "Sorry, you are not allowed to edit posts in this post type."
    );
    assert_eq!(Refusal::Screen.status(), 403);
}
