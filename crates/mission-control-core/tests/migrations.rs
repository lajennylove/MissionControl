//! Tests for the core migrations. They run on the database in `MC_TEST_DATABASE_URL`, or on a
//! fresh SQLite file when it is unset.

use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, table_exists};
use orbit_orm::{State, Value, raw};

/// The core tables, in the order their migrations create them.
const CORE_TABLES: &[&str] = &["options"];

fn names() -> Vec<String> {
    migrator()
        .expect("the core migrations are valid")
        .migrations()
        .iter()
        .map(|m| m.name.clone())
        .collect()
}

#[test]
fn lists_the_options_migration_first() {
    assert_eq!(names()[0], "20261114000000_create_options_table");
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
    let status = migrator.status(db).await.unwrap();
    assert!(
        status
            .iter()
            .all(|s| s.state == State::Applied { batch: 1 })
    );

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
    assert!(status.iter().all(|s| s.state == State::Pending));
}

#[tokio::test]
async fn options_names_are_unique_and_autoload_by_default() {
    let test = database().await;
    let db = test.db();
    migrator().unwrap().run(db).await.unwrap();

    let insert = "INSERT INTO options (name, value) VALUES (?, ?)";
    raw::execute(db, insert, &["blogname".into(), "My Site".into()])
        .await
        .expect("inserts an option");
    let duplicate = raw::execute(db, insert, &["blogname".into(), "Other".into()]).await;
    assert!(duplicate.is_err(), "a second blogname was accepted");

    let autoloaded: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM options WHERE name = ? AND autoload = ?",
        &["blogname".into(), Value::Bool(true)],
    )
    .await
    .unwrap();
    assert_eq!(autoloaded, 1);
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

    let insert = "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)";
    let row = ["Ann".into(), "ann@example.com".into(), "hash".into()];
    raw::execute(db, insert, &row)
        .await
        .expect("inserts a user");
    assert!(raw::execute(db, insert, &row).await.is_err());
}
