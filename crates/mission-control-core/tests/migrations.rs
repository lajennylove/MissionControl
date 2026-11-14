//! Tests for the core migrations. They run on the database in `MC_TEST_DATABASE_URL`, or on a
//! fresh SQLite file when it is unset.

use mission_control_core::migrations::migrator;
use mission_control_testing::{
    CORE_TABLES, create_users_table, database, table_exists, tables, unique_email,
};
use orbit_orm::{State, Value, raw};

fn names() -> Vec<String> {
    migrator()
        .expect("the core migrations are valid")
        .migrations()
        .iter()
        .map(|m| m.name.clone())
        .collect()
}

/// Every `m<timestamp>_<name>.rs` module in `src/migrations` is listed in `migrator()`.
#[test]
fn migrator_lists_every_core_migration() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/migrations");
    let mut modules: Vec<String> = std::fs::read_dir(dir)
        .expect("reads the migrations folder")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter_map(|file| {
            let name = file.strip_prefix('m')?.strip_suffix(".rs")?;
            // `mod.rs` and other helpers have no timestamp.
            let stamped = name.len() > 15 && name.as_bytes()[..14].iter().all(u8::is_ascii_digit);
            stamped.then(|| name.to_owned())
        })
        .collect();
    modules.sort();
    assert!(!modules.is_empty());
    assert_eq!(names(), modules);
}

#[tokio::test]
async fn run_applies_every_migration_then_nothing() {
    let test = database().await;
    let db = test.db();
    let migrator = migrator().unwrap();

    let applied = migrator.run(db).await.expect("runs the migrations");
    assert_eq!(applied, names());
    for table in CORE_TABLES {
        assert!(table_exists(db, table).await, "{table} was created");
    }
    // One new batch. Its number depends on other apps' rows in a shared migrations table.
    let status = migrator.status(db).await.unwrap();
    assert_eq!(status.len(), names().len());
    let State::Applied { batch } = status[0].state else {
        panic!("{} was not applied", status[0].name);
    };
    assert!(status.iter().all(|s| s.state == State::Applied { batch }));

    let again = migrator.run(db).await.expect("runs again");
    assert!(again.is_empty(), "a second run applied {again:?}");
}

#[tokio::test]
async fn rollback_reverts_the_last_batch() {
    let test = database().await;
    let db = test.db();
    let migrator = migrator().unwrap();
    migrator.run(db).await.unwrap();

    let reverted = migrator.rollback(db).await.expect("rolls back");
    let mut expected = names();
    expected.reverse();
    assert_eq!(reverted, expected);
    for table in CORE_TABLES {
        assert!(!table_exists(db, table).await, "{table} was dropped");
    }
    let status = migrator.status(db).await.unwrap();
    assert_eq!(status.len(), names().len());
    assert!(status.iter().all(|s| s.state == State::Pending));
}

/// `CORE_TABLES` in the test helpers names exactly the tables the core migrations create, so the
/// clean slate drops all of them.
#[tokio::test]
async fn core_tables_match_what_the_migrations_create() {
    let test = database().await;
    let db = test.db();
    let before = tables(db).await;
    migrator().unwrap().run(db).await.unwrap();

    let mut created: Vec<String> = tables(db)
        .await
        .into_iter()
        .filter(|t| !before.contains(t) && t != "__diesel_schema_migrations")
        .collect();
    created.sort();
    let mut listed: Vec<String> = CORE_TABLES.iter().map(|t| t.to_string()).collect();
    listed.sort();
    assert_eq!(created, listed);
}

#[tokio::test]
async fn options_names_are_unique_and_autoload_defaults_to_auto() {
    let test = database().await;
    let db = test.db();
    migrator().unwrap().run(db).await.unwrap();

    let insert = "INSERT INTO options (name, value) VALUES (?, ?)";
    raw::execute(db, insert, &["blogname".into(), "My Site".into()])
        .await
        .expect("inserts an option");
    let duplicate = raw::execute(db, insert, &["blogname".into(), "Other".into()]).await;
    assert!(duplicate.is_err(), "a second blogname was accepted");

    let autoload: String = raw::fetch_scalar(
        db,
        "SELECT autoload FROM options WHERE name = ?",
        &[Value::from("blogname")],
    )
    .await
    .unwrap();
    assert_eq!(autoload, "auto");

    raw::execute(
        db,
        "INSERT INTO options (name, value, autoload) VALUES (?, ?, ?)",
        &[
            "siteurl".into(),
            "https://example.test".into(),
            "auto-off".into(),
        ],
    )
    .await
    .expect("stores a WordPress autoload value");
}

#[tokio::test]
async fn options_value_holds_sixty_thousand_bytes() {
    let test = database().await;
    let db = test.db();
    migrator().unwrap().run(db).await.unwrap();

    let long = "x".repeat(60_000);
    raw::execute(
        db,
        "INSERT INTO options (name, value) VALUES (?, ?)",
        &["long".into(), long.clone().into()],
    )
    .await
    .expect("stores a long value");
    let stored: String = raw::fetch_scalar(
        db,
        "SELECT value FROM options WHERE name = ?",
        &["long".into()],
    )
    .await
    .unwrap();
    assert_eq!(stored.len(), long.len());
}

#[tokio::test]
async fn users_stand_in_has_a_unique_email() {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    // A second call keeps the existing table.
    create_users_table(db).await;

    let insert = "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)";
    let row = ["Ann".into(), unique_email("ann").into(), "hash".into()];
    raw::execute(db, insert, &row)
        .await
        .expect("inserts a user");
    assert!(raw::execute(db, insert, &row).await.is_err());
}
