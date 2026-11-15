//! The installer's migrations (Task 11.4a): Ignite's panel migrations, then the core ones, on
//! one database, and a second run that applies nothing.
//!
//! Each test gets a database of its own ([`scratch_database`]): a SQLite file, or a new database
//! on the server `MC_TEST_DATABASE_URL` names. The panel's migrations create and alter tables the
//! shared test database's helpers do not own (`users` among them), so they never run there.

use mission_control_app::install::run::run_migrations;
use mission_control_core::migrations::migrator;
use mission_control_testing::{CORE_TABLES, scratch_database, table_exists};
use orbit_orm::{Database, raw};

/// The tables Ignite 0.4.0's panel migrations create.
const PANEL_TABLES: &[&str] = &[
    "users",
    "sessions",
    "login_throttles",
    "user_avatars",
    "roles",
    "permissions",
    "role_permission",
    "role_user",
    "password_reset_tokens",
];

#[rocket::async_test]
async fn a_new_database_gets_the_panel_and_core_tables_once() {
    let scratch = scratch_database("install_run").await;

    let summary = run_migrations(scratch.url()).await.expect("migrates");
    assert_eq!(summary.panel.len(), 9, "{:?}", summary.panel);
    assert_eq!(summary.panel[0], "20261108000000_create_users");
    let core: Vec<String> = migrator()
        .expect("the core migrations are valid")
        .migrations()
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert_eq!(summary.core, core, "every core migration, in order");

    // The two sets record different versions in the shared history table.
    let mut versions: Vec<&str> = summary
        .panel
        .iter()
        .chain(&summary.core)
        .map(|name| &name[..14])
        .collect();
    versions.sort_unstable();
    versions.dedup();
    assert_eq!(versions.len(), summary.panel.len() + summary.core.len());

    let db = Database::connect(scratch.url()).await.expect("connects");
    for table in PANEL_TABLES.iter().chain(CORE_TABLES) {
        assert!(table_exists(&db, table).await, "{table}");
    }
    let recorded: i64 =
        raw::fetch_scalar(&db, "SELECT COUNT(*) FROM __diesel_schema_migrations", &[])
            .await
            .expect("reads the history");
    assert_eq!(
        recorded as usize,
        summary.panel.len() + summary.core.len(),
        "one row per migration"
    );
    // Each set is its own batch, the panel's first.
    let batches: Vec<(i32,)> = raw::fetch_all(
        &db,
        "SELECT DISTINCT batch FROM __diesel_schema_migrations ORDER BY batch",
        &[],
    )
    .await
    .expect("reads the batches");
    assert_eq!(batches, vec![(1,), (2,)]);

    // Running again applies nothing, and both sides see every migration as applied.
    let again = run_migrations(scratch.url()).await.expect("migrates again");
    assert!(again.is_empty(), "{again:?}");
    let ignite = orbit_orm_ignite::Database::connect(scratch.url())
        .await
        .expect("Ignite connects");
    assert!(
        ignite_panel::pending_migrations(&ignite)
            .await
            .expect("reads the panel's status")
            .is_empty()
    );
    ignite.close().await;
    let status = migrator()
        .unwrap()
        .status(&db)
        .await
        .expect("reads the core status");
    assert!(
        status
            .iter()
            .all(|s| matches!(s.state, orbit_orm::State::Applied { batch: 2 })),
        "{status:?}"
    );
    db.close().await;
}

#[rocket::async_test]
async fn the_core_tables_reference_the_panels_users() {
    let scratch = scratch_database("install_run_fk").await;
    run_migrations(scratch.url()).await.expect("migrates");

    // A user as the panel's table holds it, and a meta row the core table keys to it.
    let db = Database::connect(scratch.url()).await.expect("connects");
    raw::execute(
        &db,
        "INSERT INTO users (name, email, password_hash) VALUES ('Ann', 'ann@example.test', 'x')",
        &[],
    )
    .await
    .expect("inserts a user");
    let id: i64 = raw::fetch_scalar(
        &db,
        "SELECT id FROM users WHERE email = 'ann@example.test'",
        &[],
    )
    .await
    .expect("reads the id");
    raw::execute(
        &db,
        &format!(
            "INSERT INTO user_meta (user_id, meta_key, meta_value) VALUES ({id}, 'nickname', 'ann')"
        ),
        &[],
    )
    .await
    .expect("a user_meta row references the panel's user");
    if scratch.is_server() {
        // The key is there: a row for a user who does not exist is refused. (SQLite checks keys
        // only with `PRAGMA foreign_keys`.)
        let missing = id + 1_000_000;
        let refused = raw::execute(
            &db,
            &format!(
                "INSERT INTO user_meta (user_id, meta_key, meta_value) \
                 VALUES ({missing}, 'nickname', 'nobody')"
            ),
            &[],
        )
        .await;
        assert!(refused.is_err(), "the foreign key to users refuses it");
    }
    db.close().await;
}
