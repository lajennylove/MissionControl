//! The installer's migrations (Task 11.4a): Ignite's panel migrations, then the core ones, on
//! one database, and a second run that applies nothing. One run is one batch, and one rollback
//! reverses it (Task OB1, Ruling R92).
//!
//! Each test gets a database of its own ([`scratch_database`]): a SQLite file, or a new database
//! on the server `MC_TEST_DATABASE_URL` names. The panel's migrations create and alter tables the
//! shared test database's helpers do not own (`users` among them), so they never run there.

use mission_control_app::install::run::{rollback_last_batch, run_migrations};
use mission_control_core::migrations::migrator;
use mission_control_testing::{CORE_TABLES, scratch_database, table_exists};
use orbit_orm::{Database, Migrator, raw};

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
    // The run is one batch, the panel's and the core's rows alike.
    assert_eq!(batches(&db).await, vec![1]);

    // Running again applies nothing and records nothing, and both sides see every migration
    // as applied.
    let again = run_migrations(scratch.url()).await.expect("migrates again");
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(history_rows(&db).await, recorded);
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
            .all(|s| matches!(s.state, orbit_orm::State::Applied { batch: 1 })),
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

#[rocket::async_test]
async fn one_rollback_reverses_the_whole_run_core_first() {
    let scratch = scratch_database("install_rollback").await;
    let applied = run_migrations(scratch.url()).await.expect("migrates");

    let reverted = rollback_last_batch(scratch.url())
        .await
        .expect("rolls back");
    // The core migrations in reverse order, then the panel's in reverse order.
    let mut core = applied.core.clone();
    core.reverse();
    let mut panel = applied.panel.clone();
    panel.reverse();
    assert_eq!(reverted.core, core);
    assert_eq!(reverted.panel, panel);

    let db = Database::connect(scratch.url()).await.expect("connects");
    for table in PANEL_TABLES.iter().chain(CORE_TABLES) {
        assert!(!table_exists(&db, table).await, "{table} is dropped");
    }
    assert_eq!(
        history_rows(&db).await,
        0,
        "every row of the run is removed"
    );

    // Nothing is left to roll back.
    let nothing = rollback_last_batch(scratch.url())
        .await
        .expect("rolls back nothing");
    assert!(nothing.is_empty(), "{nothing:?}");

    // A new run applies everything again, as one batch.
    let again = run_migrations(scratch.url()).await.expect("migrates again");
    assert_eq!(again, applied);
    for table in PANEL_TABLES.iter().chain(CORE_TABLES) {
        assert!(table_exists(&db, table).await, "{table}");
    }
    assert_eq!(batches(&db).await, vec![1]);
    db.close().await;
}

#[rocket::async_test]
async fn a_run_of_new_core_migrations_rolls_back_on_its_own() {
    let scratch = scratch_database("install_upgrade").await;
    let core = migrator().expect("the core migrations are valid");
    let (newest, older) = core.migrations().split_last().expect("has migrations");

    // A site installed before the newest core migration existed.
    let ignite = orbit_orm_ignite::Database::connect(scratch.url())
        .await
        .expect("Ignite connects");
    let panel = ignite_panel::migrate(&ignite)
        .await
        .expect("migrates the panel");
    ignite.close().await;
    let db = Database::connect(scratch.url()).await.expect("connects");
    Migrator::new(older.to_vec())
        .expect("valid")
        .run(&db)
        .await
        .expect("migrates the older core");
    assert_eq!(batches(&db).await, vec![1, 2]);

    // The upgrade's run applies only the new core migration, in a batch of its own.
    let upgrade = run_migrations(scratch.url()).await.expect("migrates");
    assert!(upgrade.panel.is_empty(), "{upgrade:?}");
    assert_eq!(upgrade.core, vec![newest.name.clone()]);
    assert_eq!(batches(&db).await, vec![1, 2, 3]);

    // Its rollback reverts it alone.
    let reverted = rollback_last_batch(scratch.url())
        .await
        .expect("rolls back");
    assert!(reverted.panel.is_empty(), "{reverted:?}");
    assert_eq!(reverted.core, vec![newest.name.clone()]);
    let core_status = core.status(&db).await.expect("reads the core history");
    let (newest_status, older_status) = core_status.split_last().expect("has migrations");
    assert_eq!(
        newest_status.state,
        orbit_orm::State::Pending,
        "{newest_status:?}"
    );
    for status in older_status {
        assert_eq!(
            status.state,
            orbit_orm::State::Applied { batch: 2 },
            "{status:?}"
        );
    }
    assert!(table_exists(&db, "users").await);
    assert_eq!(batches(&db).await, vec![1, 2]);

    // The older batches roll back one at a time: the core one, then the panel's alone.
    let reverted = rollback_last_batch(scratch.url())
        .await
        .expect("rolls back the older core");
    assert!(reverted.panel.is_empty(), "{reverted:?}");
    assert_eq!(reverted.core.len(), older.len());
    let reverted = rollback_last_batch(scratch.url())
        .await
        .expect("rolls back the panel");
    assert!(reverted.core.is_empty(), "{reverted:?}");
    assert_eq!(reverted.panel.len(), panel.len());
    for table in PANEL_TABLES.iter().chain(CORE_TABLES) {
        assert!(!table_exists(&db, table).await, "{table} is dropped");
    }
    assert_eq!(history_rows(&db).await, 0);
    db.close().await;
}

#[rocket::async_test]
async fn a_last_batch_with_an_unknown_version_is_left_alone() {
    let scratch = scratch_database("install_unknown").await;
    run_migrations(scratch.url()).await.expect("migrates");
    let db = Database::connect(scratch.url()).await.expect("connects");
    raw::execute(
        &db,
        "INSERT INTO __diesel_schema_migrations (version, batch) VALUES ('29991231000000', 1)",
        &[],
    )
    .await
    .expect("records a migration neither set has");
    let before = history_rows(&db).await;

    let refused = rollback_last_batch(scratch.url()).await;
    assert!(
        matches!(
            &refused,
            Err(mission_control_app::install::run::InstallError::UnknownMigrations(v))
                if v == &["29991231000000".to_owned()]
        ),
        "{refused:?}"
    );
    assert_eq!(history_rows(&db).await, before, "nothing was reverted");
    for table in PANEL_TABLES.iter().chain(CORE_TABLES) {
        assert!(table_exists(&db, table).await, "{table}");
    }
    db.close().await;
}

/// The distinct batches in the migrations table, in order.
async fn batches(db: &Database) -> Vec<i32> {
    let rows: Vec<(i32,)> = raw::fetch_all(
        db,
        "SELECT DISTINCT batch FROM __diesel_schema_migrations ORDER BY batch",
        &[],
    )
    .await
    .expect("reads the batches");
    rows.into_iter().map(|(b,)| b).collect()
}

/// The number of rows in the migrations table.
async fn history_rows(db: &Database) -> i64 {
    raw::fetch_scalar(db, "SELECT COUNT(*) FROM __diesel_schema_migrations", &[])
        .await
        .expect("reads the history")
}
