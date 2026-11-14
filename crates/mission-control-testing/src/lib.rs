//! Shared test helpers: open the test database and give each test a clean slate.
//!
//! Tests read `MC_TEST_DATABASE_URL`, the variable CI sets. When it is unset, each test gets a
//! fresh SQLite file in its own temporary folder, so SQLite tests run in parallel. When it is set
//! (MySQL, MariaDB, PostgreSQL, or one shared SQLite file), every test in the process shares that
//! database: [`database`] holds a process-wide lock for the test's lifetime and removes what the
//! core migrations left there, and nothing else. Other suites may share the database, so the
//! helpers never drop `users` or the migrations table.
//!
//! This crate depends on `mission-control-core` for the migration names, and the core crate uses
//! it as a dev-dependency. Use it from integration tests (`tests/`), not from unit tests inside
//! `mission-control-core`, which would see a second copy of the core crate.
//!
//! ```ignore
//! #[tokio::test]
//! async fn creates_a_post() {
//!     let test = mission_control_testing::database().await;
//!     let db = test.db();
//!     mission_control_testing::create_users_table(db).await;
//!     // ...
//! }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mission_control_core::migrations::migrator;
use orbit_orm::{Database, Driver, Value, raw, schema};
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard};

/// The variable that names the test database. Unset means a fresh SQLite file per test.
pub const DATABASE_URL_VAR: &str = "MC_TEST_DATABASE_URL";

/// The tables the core migrations create, dropped before each test on a shared database. Tables
/// that reference others come first, so PostgreSQL drops them without `CASCADE`. Add each new
/// core table here, above the tables it references; a test in `mission-control-core` checks that
/// this list matches what the migrations create.
pub const CORE_TABLES: &[&str] = &[
    "term_meta",
    "user_meta",
    "post_meta",
    "term_relationships",
    "term_taxonomy",
    "terms",
    "scheduled_events",
    "posts",
    "options",
];

/// The table the migrator records applied migrations in. Shared with Ignite and any other Orbit
/// app on the database, so only the core migrations' rows are removed from it.
const MIGRATIONS_TABLE: &str = "__diesel_schema_migrations";

/// Serializes the tests of one process that use a shared database.
static SHARED: Mutex<()> = Mutex::const_new(());

/// A test database. Call [`db`](Self::db) for the connection; keep this value alive for the test.
/// Dropping it releases the shared-database lock, or removes the temporary SQLite file.
pub struct TestDatabase {
    db: Arc<Database>,
    _guard: Option<MutexGuard<'static, ()>>,
    _dir: Option<TempDir>,
}

impl TestDatabase {
    /// The database connection.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// The connection, shared: for code that keeps it, such as the scheduler.
    pub fn shared(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }
}

/// Opens the test database with no Mission Control tables in it.
///
/// # Panics
///
/// Panics when the database cannot be reached or a table cannot be dropped: a test cannot run
/// without its database.
pub async fn database() -> TestDatabase {
    match std::env::var(DATABASE_URL_VAR) {
        Ok(url) if !url.is_empty() => shared(&url).await,
        _ => fresh_sqlite().await,
    }
}

/// A new SQLite file in its own temporary folder, removed when the test ends.
async fn fresh_sqlite() -> TestDatabase {
    let dir = tempfile::tempdir().expect("creates a temporary folder");
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("test.db").display());
    let db = Database::connect(&url)
        .await
        .expect("opens the SQLite test database");
    TestDatabase {
        db: Arc::new(db),
        _guard: None,
        _dir: Some(dir),
    }
}

/// The database at `url`, locked for this test and cleared of what the core migrations left,
/// including after an earlier run that crashed.
async fn shared(url: &str) -> TestDatabase {
    let guard = SHARED.lock().await;
    let db = Database::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connects to {DATABASE_URL_VAR}: {e}"));
    for table in CORE_TABLES {
        drop_table(&db, table).await;
    }
    forget_core_migrations(&db).await;
    TestDatabase {
        db: Arc::new(db),
        _guard: Some(guard),
        _dir: None,
    }
}

/// Drops `table` if it exists.
async fn drop_table(db: &Database, table: &str) {
    for sql in schema::drop_if_exists(table).to_sql(db.driver()) {
        raw::execute(db, &sql, &[])
            .await
            .unwrap_or_else(|e| panic!("drops the {table} table: {e}"));
    }
}

/// Deletes the core migrations' rows from the migrations table, if it exists. Other apps' rows
/// stay. Raw SQL, since the query builder needs a model and this is test-only.
async fn forget_core_migrations(db: &Database) {
    if !table_exists(db, MIGRATIONS_TABLE).await {
        return;
    }
    let versions: Vec<Value> = migrator()
        .expect("the core migrations are valid")
        .migrations()
        .iter()
        .map(|m| Value::from(&m.name[..14]))
        .collect();
    let marks = vec!["?"; versions.len()].join(", ");
    raw::execute(
        db,
        &format!("DELETE FROM {MIGRATIONS_TABLE} WHERE version IN ({marks})"),
        &versions,
    )
    .await
    .expect("deletes the core migration history");
}

/// Creates the stand-in for Ignite's `users` table, which core tables reference by foreign key,
/// unless a `users` table exists already (Ignite's, or one from an earlier test). It is never
/// dropped or emptied, so its rows outlive the test: insert users with [`unique_email`].
///
/// Ignite owns the real table, so the core migrations do not create it. The columns follow
/// Ignite's: a signed 64-bit auto-increment `id`, `name`, a unique `email`, `password_hash` and
/// `created_at`. `created_at` is nullable here, since the schema builder has no
/// `DEFAULT CURRENT_TIMESTAMP`.
///
/// # Panics
///
/// Panics when the table cannot be created.
pub async fn create_users_table(db: &Database) {
    if table_exists(db, "users").await {
        return;
    }
    let users = schema::create("users", |t| {
        t.id();
        t.string("name");
        t.string("email");
        t.string("password_hash");
        t.date_time("created_at").nullable();
        t.unique(["email"]);
    });
    for sql in users.to_sql(db.driver()) {
        if let Err(e) = raw::execute(db, &sql, &[]).await {
            // Another process may have created it since the check.
            if !table_exists(db, "users").await {
                panic!("creates the users table: {e}");
            }
            return;
        }
    }
}

/// An email address no other test run has used, such as `ann.4242.1731600000000000000.0@example.test`,
/// for rows in the shared `users` table.
pub fn unique_email(local: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{local}.{}.{nanos}.{n}@example.test", std::process::id())
}

/// Whether the database has a table named `table`.
///
/// # Panics
///
/// Panics when the catalog cannot be read.
pub async fn table_exists(db: &Database, table: &str) -> bool {
    let sql = match db.driver() {
        Driver::MySql => {
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = ?"
        }
        Driver::Postgres => {
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_name = ?"
        }
        Driver::Sqlite => "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
    };
    let count: i64 = raw::fetch_scalar(db, sql, &[Value::from(table)])
        .await
        .expect("reads the table catalog");
    count > 0
}

/// The names of every table in the database (the current schema on PostgreSQL), sorted. SQLite's
/// own tables, such as `sqlite_sequence`, are left out.
///
/// # Panics
///
/// Panics when the catalog cannot be read.
pub async fn tables(db: &Database) -> Vec<String> {
    let sql = match db.driver() {
        Driver::MySql => {
            "SELECT CAST(table_name AS CHAR) FROM information_schema.tables \
             WHERE table_schema = DATABASE() ORDER BY 1"
        }
        Driver::Postgres => {
            "SELECT CAST(table_name AS TEXT) FROM information_schema.tables \
             WHERE table_schema = current_schema() ORDER BY 1"
        }
        Driver::Sqlite => {
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY 1"
        }
    };
    let rows: Vec<(String,)> = raw::fetch_all(db, sql, &[])
        .await
        .expect("reads the table catalog");
    rows.into_iter().map(|(name,)| name).collect()
}
