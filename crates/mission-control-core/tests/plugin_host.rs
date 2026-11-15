//! Task 12.2: the plugin host. The sample plugins are components built from
//! `wit/mission-control.wit`, written in the component model's text format in `tests/plugins/`
//! and compiled when the test runs (Ruling R99). The tests that touch content run on the
//! database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};

use mission_control_core::content::{
    NewPost, Post, PostStatus, get_post_meta, get_post_revisions, set_post_meta,
};
use mission_control_core::hooks::{Action, Filter, Hooks, Owner};
use mission_control_core::migrations::migrator;
use mission_control_core::plugins::{
    DENIED_HOOK_PREFIXES, DENIED_HOOKS, HostError, Manifest, Permission, Plugin, PluginHost,
    is_denied_hook, with_current_user,
};
use mission_control_core::users::{ADMINISTRATOR, AUTHOR, SUBSCRIBER, UserCaps};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};
use serde_json::json;

/// The action `meta_writer.wat` subscribes to, with a post id.
const SAVED: Action<i64> = Action::new("mc_test_saved");
/// The action `editor.wat` subscribes to, with a post id.
const EDIT: Action<i64> = Action::new("mc_test_edit");
/// The filter `title_filter.wat` subscribes to.
const TITLE: Filter<String> = Filter::new("mc_test_title");
/// The action `trapper.wat` subscribes to; its handler traps.
const TRAP: Action<i64> = Action::new("mc_test_trap");
/// The action `late_subscriber.wat` subscribes to; its handler tries to subscribe again.
const LATE: Action<i64> = Action::new("mc_test_late");

/// The log lines of this test binary, captured so a test can see that a plugin's handler ran
/// and what it reported. Tests give their plugins distinct slugs, so the lines of one test do
/// not count for another.
static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target() == "mission_control::plugins"
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        LINES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(record.args().to_string());
    }

    fn flush(&self) {}
}

/// Starts capturing the plugin host's log lines, at `info` and above (once per test binary).
fn capture_logs() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&Capture).expect("installs the capturing logger");
        log::set_max_level(log::LevelFilter::Info);
    });
}

/// How many captured log lines are about plugin `slug` and contain `text`.
fn logged(slug: &str, text: &str) -> usize {
    let tag = format!("[{slug}]");
    LINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|line| line.contains(&tag) && line.contains(text))
        .count()
}

/// The sample plugin `name` in `tests/plugins/`, compiled to a binary component.
fn sample(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/plugins/{name}", env!("CARGO_MANIFEST_DIR"));
    wat::parse_file(&path).unwrap_or_else(|e| panic!("compiles {path}: {e}"))
}

/// A manifest for the plugin `slug` with `permissions`, a TOML list's items such as
/// `"hooks", "write-content"`.
fn manifest(slug: &str, permissions: &str) -> Manifest {
    Manifest::parse(&format!(
        "slug = \"{slug}\"\nname = \"Sample\"\nversion = \"1.0.0\"\n\
         license = \"GPL-2.0-or-later\"\nrequires = \"0.1\"\npermissions = [{permissions}]\n"
    ))
    .expect("a valid manifest")
}

fn user(id: i64, role: &str) -> UserCaps {
    UserCaps::with_default_roles(id, &[role], &BTreeMap::new())
}

/// A database with the core tables, a host on it, and the id of a new user.
async fn setup() -> (TestDatabase, Arc<Hooks>, PluginHost, i64) {
    let test = database().await;
    let db = test.db();
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
    let user_id = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the user id");
    capture_logs();
    let hooks = Arc::new(Hooks::new());
    let host = PluginHost::new(Arc::clone(&hooks), test.shared()).expect("starts the host");
    (test, hooks, host, user_id)
}

async fn new_post(db: &Database, author: i64, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: "A post".into(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("creates the post")
}

/// Loads `wasm` as `manifest` and runs its `init`, where it subscribes.
fn activated(host: &PluginHost, manifest: Manifest, wasm: &[u8]) -> Arc<Plugin> {
    let plugin = host.load(manifest, wasm).expect("loads the plugin");
    plugin.init().expect("starts the plugin");
    plugin
}

/// Fires `action` with `id` on a blocking thread, as the scheduler runs hooks, acting as `user`
/// when one is given.
async fn fire(hooks: &Arc<Hooks>, action: Action<i64>, id: i64, user: Option<UserCaps>) {
    let hooks = Arc::clone(hooks);
    tokio::task::spawn_blocking(move || match user {
        Some(user) => with_current_user(user, || hooks.do_action(action, &id)),
        None => hooks.do_action(action, &id),
    })
    .await
    .expect("the action runs");
}

#[tokio::test]
async fn a_filter_subscribed_in_init_changes_the_value() {
    let (_test, hooks, host, _) = setup().await;
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Hello");

    let _plugin = activated(
        &host,
        manifest("title-filter", "\"hooks\""),
        &sample("title_filter.wat"),
    );

    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Filtered");
}

#[tokio::test]
async fn an_action_handler_writes_post_meta_as_the_acting_user() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let post = new_post(db, user_id, PostStatus::Draft).await;
    let _plugin = activated(
        &host,
        manifest("meta-writer", "\"hooks\", \"write-content\""),
        &sample("meta_writer.wat"),
    );

    fire(&hooks, SAVED, post.id, Some(user(user_id, ADMINISTRATOR))).await;

    assert_eq!(
        get_post_meta(db, post.id, "plugin_saw").await.unwrap(),
        Some(json!("seen"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_fired_on_a_runtime_worker_reaches_the_database() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let post = new_post(db, user_id, PostStatus::Draft).await;
    let _plugin = activated(
        &host,
        manifest("meta-writer", "\"hooks\", \"write-content\""),
        &sample("meta_writer.wat"),
    );

    with_current_user(user(user_id, ADMINISTRATOR), || {
        hooks.do_action(SAVED, &post.id)
    });

    assert_eq!(
        get_post_meta(db, post.id, "plugin_saw").await.unwrap(),
        Some(json!("seen"))
    );
}

#[tokio::test]
async fn content_calls_follow_the_acting_users_capabilities() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let post = new_post(db, user_id, PostStatus::Publish).await;
    let _plugin = activated(
        &host,
        manifest("refused-writer", "\"hooks\", \"write-content\""),
        &sample("meta_writer.wat"),
    );

    // A subscriber may not edit the post, nor a visitor (nobody acting).
    fire(
        &hooks,
        SAVED,
        post.id,
        Some(user(user_id + 1000, SUBSCRIBER)),
    )
    .await;
    fire(&hooks, SAVED, post.id, None).await;

    assert_eq!(
        get_post_meta(db, post.id, "plugin_saw").await.unwrap(),
        None
    );
    // The handler ran both times, and both writes were refused.
    assert_eq!(logged("refused-writer", "set-post-meta failed"), 2);
}

#[tokio::test]
async fn an_editor_plugin_reads_and_updates_a_post_through_kses() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let post = new_post(db, user_id, PostStatus::Draft).await;
    set_post_meta(db, post.id, "source", &json!({"n": 1}))
        .await
        .unwrap();
    let _plugin = activated(
        &host,
        manifest("editor", "\"hooks\", \"read-content\", \"write-content\""),
        &sample("editor.wat"),
    );

    // An author may edit their own draft, but not write HTML into a title.
    fire(&hooks, EDIT, post.id, Some(user(user_id, AUTHOR))).await;

    let saved = Post::query().find(db, post.id).await.unwrap().unwrap();
    assert_eq!(saved.title, "Changed");
    assert_eq!(
        get_post_meta(db, post.id, "copy").await.unwrap(),
        Some(json!({"n": 1}))
    );
    assert_eq!(get_post_revisions(db, post.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_editor_plugin_cannot_edit_someone_elses_post_for_an_author() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let post = new_post(db, user_id, PostStatus::Publish).await;
    set_post_meta(db, post.id, "source", &json!("x"))
        .await
        .unwrap();
    let _plugin = activated(
        &host,
        manifest(
            "other-editor",
            "\"hooks\", \"read-content\", \"write-content\"",
        ),
        &sample("editor.wat"),
    );

    fire(&hooks, EDIT, post.id, Some(user(user_id + 1000, AUTHOR))).await;

    let saved = Post::query().find(db, post.id).await.unwrap().unwrap();
    assert_eq!(saved.title, "A post");
    assert_eq!(get_post_meta(db, post.id, "copy").await.unwrap(), None);
    // It read the published post, and the update was refused.
    assert_eq!(logged("other-editor", "update-post failed"), 1);
}

#[tokio::test]
async fn a_revision_needs_edit_post_on_its_parent_even_when_the_parent_is_published() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let mut post = new_post(db, user_id, PostStatus::Publish).await;
    post.password = Some("secret".into());
    post.save(db).await.unwrap();
    mission_control_core::content::save_revision(db, &post, post.modified_at)
        .await
        .unwrap();
    let revision_id: i64 = raw::fetch_scalar(
        db,
        "SELECT MAX(id) FROM posts WHERE post_type = 'revision' AND parent_id = ?",
        &[post.id.into()],
    )
    .await
    .unwrap();
    let _plugin = activated(
        &host,
        manifest(
            "revision-reader",
            "\"hooks\", \"read-content\", \"write-content\"",
        ),
        &sample("editor.wat"),
    );

    // A visitor may not read it; an administrator may (and then may not update a revision).
    fire(&hooks, EDIT, revision_id, None).await;
    assert_eq!(logged("revision-reader", "get-post failed"), 1);
    fire(
        &hooks,
        EDIT,
        revision_id,
        Some(user(user_id, ADMINISTRATOR)),
    )
    .await;
    assert_eq!(logged("revision-reader", "get-post failed"), 1);
    assert_eq!(logged("revision-reader", "update-post failed"), 1);
}

#[tokio::test]
async fn a_published_post_of_an_internal_type_is_not_public() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let item = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Menu item".into(),
            ..NewPost::new(user_id, "nav_menu_item")
        },
    )
    .await
    .unwrap();
    let _plugin = activated(
        &host,
        manifest(
            "menu-reader",
            "\"hooks\", \"read-content\", \"write-content\"",
        ),
        &sample("editor.wat"),
    );

    fire(&hooks, EDIT, item.id, None).await;

    assert_eq!(logged("menu-reader", "get-post failed"), 1);
}

#[tokio::test]
async fn importing_an_interface_the_manifest_does_not_grant_fails_to_instantiate() {
    let (_test, _hooks, host, _) = setup().await;

    let error = host
        .load(
            manifest("meta-writer", "\"hooks\""),
            &sample("meta_writer.wat"),
        )
        .expect_err("the plugin is refused");

    match &error {
        HostError::MissingPermission { import, permission } => {
            assert_eq!(import, "mission-control:plugin/content-write@0.1.0");
            assert_eq!(*permission, Permission::WriteContent);
        }
        other => panic!("unexpected error: {other}"),
    }
    let message = error.to_string();
    assert!(
        message.contains("mission-control:plugin/content-write@0.1.0")
            && message.contains("write-content"),
        "{message}"
    );
}

#[tokio::test]
async fn hooks_need_their_permission_too() {
    let (_test, _hooks, host, _) = setup().await;

    let error = host
        .load(manifest("title-filter", ""), &sample("title_filter.wat"))
        .expect_err("the plugin is refused");

    assert!(
        matches!(
            &error,
            HostError::MissingPermission {
                permission: Permission::Hooks,
                ..
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn an_import_the_host_does_not_know_is_refused_by_name() {
    let (_test, _hooks, host, _) = setup().await;
    let wasm = wat::parse_str(
        r#"(component
             (import "acme:secrets/vault@1.0.0" (instance))
           )"#,
    )
    .unwrap();

    let error = host
        .load(manifest("acme", "\"hooks\""), &wasm)
        .expect_err("the plugin is refused");

    assert!(
        matches!(&error, HostError::UnknownImport { import } if import == "acme:secrets/vault@1.0.0"),
        "{error}"
    );
}

#[tokio::test]
async fn bytes_that_are_not_a_component_are_refused() {
    let (_test, _hooks, host, _) = setup().await;

    for wasm in [b"not wasm".to_vec(), wat::parse_str("(module)").unwrap()] {
        let error = host
            .load(manifest("broken", "\"hooks\""), &wasm)
            .expect_err("the bytes are refused");
        assert!(matches!(error, HostError::Compile(_)), "{error}");
    }
}

#[tokio::test]
async fn a_component_without_the_handler_export_fails_to_instantiate() {
    let (_test, _hooks, host, _) = setup().await;

    let error = host
        .load(
            manifest("empty", "\"hooks\""),
            &wat::parse_str("(component)").unwrap(),
        )
        .expect_err("the plugin is refused");

    assert!(matches!(error, HostError::Instantiate(_)), "{error}");
}

#[tokio::test]
async fn removing_the_plugins_hooks_stops_its_handlers() {
    let (_test, hooks, host, _) = setup().await;
    let plugin = activated(
        &host,
        manifest("title-filter", "\"hooks\""),
        &sample("title_filter.wat"),
    );

    plugin.remove_hooks();

    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Hello");
}

#[tokio::test]
async fn on_the_thread_of_a_current_thread_runtime_a_content_call_fails_without_a_panic() {
    let (test, hooks, host, user_id) = setup().await;
    let db = test.db();
    let post = new_post(db, user_id, PostStatus::Draft).await;
    let plugin = activated(
        &host,
        manifest("current-thread-writer", "\"hooks\", \"write-content\""),
        &sample("meta_writer.wat"),
    );

    // The host cannot block this thread on its own runtime: the call fails, the plugin's
    // handler gets the error, and the action returns.
    with_current_user(user(user_id, ADMINISTRATOR), || {
        hooks.do_action(SAVED, &post.id)
    });

    assert_eq!(
        get_post_meta(db, post.id, "plugin_saw").await.unwrap(),
        None
    );
    assert!(!plugin.trapped());
    assert_eq!(logged("current-thread-writer", "cannot block"), 1);
    assert_eq!(logged("current-thread-writer", "set-post-meta failed"), 1);
}

#[tokio::test]
async fn a_plugin_that_traps_is_stopped_and_the_site_goes_on() {
    let (_test, hooks, host, _) = setup().await;
    let plugin = activated(
        &host,
        manifest("trapper", "\"hooks\""),
        &sample("trapper.wat"),
    );
    let before = logged("trapper", "failed and is stopped");
    let ran = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ran);
    hooks.add_action(TRAP, 20, Owner::Core, move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    hooks.do_action(TRAP, &1);
    hooks.do_action(TRAP, &2);

    // The plugin ran once: the trap stopped it and removed its hooks; core's callback ran both
    // times.
    assert!(plugin.trapped());
    assert_eq!(logged("trapper", "failed and is stopped") - before, 1);
    assert_eq!(ran.load(Ordering::SeqCst), 2);
    assert!(matches!(plugin.activate(), Err(HostError::Trap(_))));
    assert!(matches!(plugin.init(), Err(HostError::Trap(_))));
}

#[tokio::test]
async fn a_failed_init_removes_what_the_plugin_subscribed() {
    let (_test, hooks, host, _) = setup().await;
    let plugin = host
        .load(
            manifest("half-activator", "\"hooks\""),
            &sample("half_activator.wat"),
        )
        .expect("loads the plugin");

    let error = plugin.init().expect_err("the plugin refuses");

    assert_eq!(error, HostError::Init("not today".into()));
    assert!(plugin.deactivated());
    // It subscribed before refusing; the subscription is gone.
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Hello");
}

#[tokio::test]
async fn a_plugin_is_started_and_activated_once_per_load() {
    let (_test, hooks, host, _) = setup().await;
    let plugin = host
        .load(
            manifest("activated-twice", "\"hooks\""),
            &sample("title_filter.wat"),
        )
        .expect("loads the plugin");
    assert_eq!(plugin.activate(), Err(HostError::NotInitialized));
    assert!(!plugin.deactivated());

    plugin.init().expect("starts the plugin");
    assert_eq!(plugin.init(), Err(HostError::AlreadyInitialized));
    plugin.activate().expect("activates the plugin");
    assert_eq!(plugin.activate(), Err(HostError::AlreadyActivated));
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Filtered");
}

#[tokio::test]
async fn a_plugin_may_not_subscribe_in_activate() {
    let (_test, hooks, host, _) = setup().await;
    let plugin = activated(
        &host,
        manifest("activate-subscriber", "\"hooks\""),
        &sample("activate_subscriber.wat"),
    );

    let error = plugin.activate().expect_err("the subscription is refused");

    assert!(
        matches!(&error, HostError::Activation(m) if m.contains("subscribe in init")),
        "{error:?}"
    );
    assert!(plugin.deactivated());
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Hello");
}

#[tokio::test]
async fn a_failing_deactivate_is_reported_and_the_plugin_is_deactivated_anyway() {
    let (_test, hooks, host, _) = setup().await;
    let plugin = activated(
        &host,
        manifest("bad-deactivator", "\"hooks\""),
        &sample("bad_deactivator.wat"),
    );
    plugin.activate().expect("activates the plugin");
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Filtered");

    let error = plugin.deactivate().expect_err("its deactivate traps");

    assert!(matches!(error, HostError::Trap(_)), "{error:?}");
    assert!(plugin.deactivated());
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Hello");
    // A plugin already deactivated is not called again.
    assert_eq!(plugin.deactivate(), Ok(()));
}

#[test]
fn every_denied_hook_is_refused() {
    for name in DENIED_HOOKS {
        assert!(is_denied_hook(name), "{name}");
    }
    for prefix in DENIED_HOOK_PREFIXES {
        assert!(is_denied_hook(&format!("{prefix}x")), "{prefix}");
    }
    for name in [
        "save_post",
        "the_content",
        "the_title",
        "init",
        "publish_future_post",
    ] {
        assert!(!is_denied_hook(name), "{name}");
    }
}

#[tokio::test]
async fn a_deactivated_plugin_is_never_called_again_and_cannot_resubscribe() {
    let (_test, hooks, host, _) = setup().await;
    let plugin = activated(
        &host,
        manifest("deactivated-filter", "\"hooks\""),
        &sample("title_filter.wat"),
    );

    plugin.remove_hooks();

    assert!(plugin.deactivated());
    assert_eq!(plugin.init(), Err(HostError::Deactivated));
    assert_eq!(plugin.activate(), Err(HostError::Deactivated));
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Hello");
}

#[tokio::test]
async fn subscribing_outside_init_is_refused() {
    let (_test, hooks, host, _) = setup().await;
    let _plugin = activated(
        &host,
        manifest("late-subscriber", "\"hooks\""),
        &sample("late_subscriber.wat"),
    );

    hooks.do_action(LATE, &1);
    hooks.do_action(LATE, &2);

    // Each run tried to subscribe once more, and was refused: the hook still has one callback.
    assert_eq!(logged("late-subscriber", "subscribe in init"), 2);
}

#[tokio::test]
async fn authorization_hooks_are_refused_to_every_plugin() {
    let (_test, _hooks, host, _) = setup().await;
    let plugin = host
        .load(
            manifest("cap-snooper", "\"hooks\""),
            &sample("cap_snooper.wat"),
        )
        .expect("loads the plugin");

    let error = plugin.init().expect_err("the subscription is refused");

    match &error {
        HostError::Init(message) => {
            assert!(
                message.contains("may not subscribe to `user_has_cap`"),
                "{message}"
            )
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn one_slug_is_loaded_once_until_it_is_deactivated() {
    let (_test, hooks, host, _) = setup().await;
    let first = activated(
        &host,
        manifest("twice", "\"hooks\""),
        &sample("title_filter.wat"),
    );

    let error = host
        .load(manifest("twice", "\"hooks\""), &sample("title_filter.wat"))
        .expect_err("the second load is refused");
    assert_eq!(
        error,
        HostError::AlreadyLoaded {
            slug: "twice".into()
        }
    );

    first.remove_hooks();
    let _second = activated(
        &host,
        manifest("twice", "\"hooks\""),
        &sample("title_filter.wat"),
    );
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Filtered");

    // The first load's callbacks are its own: removing them again leaves the second's alone.
    first.remove_hooks();
    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Filtered");
}
