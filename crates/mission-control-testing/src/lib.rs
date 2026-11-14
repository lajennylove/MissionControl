//! Shared test helpers: open the test database and give each test a clean slate.
//!
//! Tests read `MC_TEST_DATABASE_URL`, the variable CI sets. When it is unset, each test gets a
//! fresh SQLite file in its own temporary folder, so SQLite tests run in parallel. When it is set
//! (MySQL, MariaDB, PostgreSQL, or one shared SQLite file), every test in the process shares that
//! database: [`database`] holds a process-wide lock for the test's lifetime and drops the tables
//! Mission Control owns before handing the database over.
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

use orbit_orm::{Database, Driver, Value, raw, schema};
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard};

/// The variable that names the test database. Unset means a fresh SQLite file per test.
pub const DATABASE_URL_VAR: &str = "MC_TEST_DATABASE_URL";

/// Every table the Mission Control tests create, dropped before each test on a shared database.
/// Tables that reference others come first, so PostgreSQL drops them without `CASCADE`. Add each
/// new core table here, above the tables it references. `users` is the stand-in from
/// [`create_users_table`]; `__diesel_schema_migrations` is the migrator's history.
pub const OWNED_TABLES: &[&str] = &["options", "users", "__diesel_schema_migrations"];

/// Serializes the tests of one process that use a shared database.
static SHARED: Mutex<()> = Mutex::const_new(());

/// A test database. Call [`db`](Self::db) for the connection; keep this value alive for the test.
/// Dropping it releases the shared-database lock, or removes the temporary SQLite file.
pub struct TestDatabase {
    db: Database,
    _guard: Option<MutexGuard<'static, ()>>,
    _dir: Option<TempDir>,
}

impl TestDatabase {
    /// The database connection.
    pub fn db(&self) -> &Database {
        &self.db
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
        db,
        _guard: None,
        _dir: Some(dir),
    }
}

/// The database at `url`, locked for this test and cleared of the tables Mission Control owns,
/// including any left by an earlier run that crashed.
async fn shared(url: &str) -> TestDatabase {
    let guard = SHARED.lock().await;
    let db = Database::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connects to {DATABASE_URL_VAR}: {e}"));
    for table in OWNED_TABLES {
        drop_table(&db, table).await;
    }
    TestDatabase {
        db,
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

/// Creates the stand-in for Ignite's `users` table, which core tables reference by foreign key.
/// Ignite owns the real table, so the core migrations do not create it. The columns follow
/// Ignite's: a signed 64-bit auto-increment `id`, `name`, a unique `email`, `password_hash` and
/// `created_at`. `created_at` is nullable here, since the schema builder has no
/// `DEFAULT CURRENT_TIMESTAMP`.
///
/// # Panics
///
/// Panics when the table cannot be created.
pub async fn create_users_table(db: &Database) {
    let users = schema::create("users", |t| {
        t.id();
        t.string("name");
        t.string("email");
        t.string("password_hash");
        t.date_time("created_at").nullable();
        t.unique(["email"]);
    });
    for sql in users.to_sql(db.driver()) {
        raw::execute(db, &sql, &[])
            .await
            .unwrap_or_else(|e| panic!("creates the users table: {e}"));
    }
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
