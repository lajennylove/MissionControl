//! Task 12.6: a plugin's own database tables. A plugin exports its migrations as schema-builder
//! statements (JSON through the WIT `migrations` interface); the host checks them, runs the
//! pending ones on activation and records them in `plugin_migrations`, and rolls them back when
//! the owner deletes the plugin with its data.
//!
//! The sample plugin is `tests/plugins/migrator.wat`, a template whose JSON each test fills in
//! before it compiles the component (Ruling R99). Each test runs on a scratch database of its
//! own (a new database on the server in `MC_TEST_DATABASE_URL`, or a SQLite file), since the
//! plugins create tables the shared test database's helpers do not own.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use mission_control_core::migrations::migrator;
use mission_control_core::plugins::{
    Deleted, HostError, InstallError, LifecycleError, MANIFEST_FILE, MAX_MIGRATIONS_JSON_BYTES,
    MAX_PLUGIN_MIGRATIONS, MODULE_FILE, Manifest, MigrationError, PLUGIN_MIGRATIONS_TABLE,
    Permission, PluginLimits, Plugins, parse_migrations,
};
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::types::Registry;
use mission_control_core::users::{ADMINISTRATOR, UserCaps};
use mission_control_testing::{
    ScratchDatabase, create_users_table, scratch_database, table_exists,
};
use orbit_orm::{Database, Driver, Value, raw};
use serde_json::{Value as Json, json};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// The sample plugin with `migrations` (JSON) in its data segment, compiled.
fn migrator_wasm(migrations: &Json) -> Vec<u8> {
    let path = format!("{}/tests/plugins/migrator.wat", env!("CARGO_MANIFEST_DIR"));
    let template = std::fs::read_to_string(&path).expect("reads the template");
    let text = migrations.to_string();
    let escaped: String = text.bytes().map(|b| format!("\\{b:02x}")).collect();
    let wat = template
        .replace("@JSON@", &escaped)
        .replace("@LEN@", &text.len().to_string());
    wat::parse_str(&wat).unwrap_or_else(|e| panic!("compiles {path}: {e}"))
}

/// The manifest of plugin `slug` with `permissions`, a TOML list's items.
fn manifest_text(slug: &str, permissions: &str) -> String {
    format!(
        "slug = \"{slug}\"\nname = \"Sample\"\nversion = \"1.0.0\"\n\
         license = \"GPL-2.0-or-later\"\nrequires = \"0.1\"\npermissions = [{permissions}]\n"
    )
}

fn manifest(slug: &str) -> Manifest {
    Manifest::parse(&manifest_text(slug, "\"migrations\"")).expect("a valid manifest")
}

/// The zip of plugin `slug`, in one folder named by its slug.
fn plugin_zip(slug: &str, permissions: &str, wasm: &[u8]) -> Vec<u8> {
    let toml = manifest_text(slug, permissions);
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in [
        (format!("{slug}/{MANIFEST_FILE}"), toml.as_bytes()),
        (format!("{slug}/{MODULE_FILE}"), wasm),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn admin() -> UserCaps {
    UserCaps::with_default_roles(1, &[ADMINISTRATOR], &BTreeMap::new())
}

/// A site on a scratch database with the core tables, and a plugins folder.
struct Site {
    db: Arc<Database>,
    dir: tempfile::TempDir,
    // Dropped last: the scratch database goes once the connection is closed.
    _scratch: ScratchDatabase,
}

impl Site {
    async fn new() -> Site {
        let scratch = scratch_database("plugin_mig").await;
        let db = Database::connect(scratch.url())
            .await
            .expect("connects to the scratch database");
        create_users_table(&db).await;
        migrator()
            .unwrap()
            .run(&db)
            .await
            .expect("runs the core migrations");
        Site {
            db: Arc::new(db),
            dir: tempfile::tempdir().unwrap(),
            _scratch: scratch,
        }
    }

    fn plugins(&self) -> Plugins {
        Plugins::new(
            Arc::new(mission_control_core::hooks::Hooks::new()),
            Arc::clone(&self.db),
            Arc::new(Registry::new(
                Arc::new(PostTypes::new()),
                Arc::new(Taxonomies::new()),
            )),
            self.dir.path().join("plugins"),
            PluginLimits::default(),
        )
        .expect("starts the plugin host")
    }

    async fn has_table(&self, table: &str) -> bool {
        table_exists(&self.db, table).await
    }

    /// The plugin's rows in `plugin_migrations`: (migration, batch), in name order.
    async fn history(&self, slug: &str) -> Vec<(String, i64)> {
        let rows: Vec<(String, i32)> = raw::fetch_all(
            &*self.db,
            &format!(
                "SELECT migration, batch FROM {PLUGIN_MIGRATIONS_TABLE} WHERE plugin = ? \
                 ORDER BY migration"
            ),
            &[Value::from(slug)],
        )
        .await
        .expect("reads the plugin migrations");
        rows.into_iter().map(|(m, b)| (m, i64::from(b))).collect()
    }
}

/// A migration that creates `table` with an id, a unique title and a post id that references
/// the site's posts, and drops it on the way down.
fn create_books(name: &str, table: &str) -> Json {
    json!({
        "name": name,
        "up": [{
            "op": "create",
            "table": table,
            "columns": [
                {"name": "id", "type": "id"},
                {"name": "title", "type": "string", "length": 200},
                {"name": "pages", "type": "integer", "default": 0},
                {"name": "post_id", "type": "big_integer", "nullable": true},
                {"name": "added_at", "type": "date_time", "nullable": true}
            ],
            "indexes": [{"columns": ["title"], "unique": true}],
            "foreign_keys": [{
                "column": "post_id",
                "references": {"table": "posts", "column": "id"},
                "on_delete": "cascade"
            }]
        }],
        "down": [{"op": "drop", "table": table, "if_exists": true}]
    })
}

/// The migrations of the `shelf` plugin: its books, then its loans (which reference the books),
/// and a column added to the books.
fn shelf_migrations() -> Json {
    json!([
        create_books("20261115000000_create_books", "shelf__books"),
        {
            "name": "20261115000100_create_loans",
            "up": [
                {
                    "op": "create",
                    "table": "shelf__loans",
                    "columns": [
                        {"name": "id", "type": "id"},
                        {"name": "book_id", "type": "big_integer"},
                        {"name": "user_id", "type": "big_integer", "nullable": true},
                        {"name": "returned", "type": "boolean", "default": false}
                    ],
                    "indexes": [{"columns": ["book_id"]}],
                    "foreign_keys": [
                        {
                            "column": "book_id",
                            "references": {"table": "shelf__books", "column": "id"},
                            "on_delete": "cascade"
                        },
                        {
                            "column": "user_id",
                            "references": {"table": "users", "column": "id"},
                            "on_delete": "set_null"
                        }
                    ]
                },
                {
                    "op": "alter",
                    "table": "shelf__books",
                    "changes": [{"add_column": {"name": "isbn", "type": "string", "length": 20, "nullable": true}}]
                }
            ],
            "down": [
                {"op": "alter", "table": "shelf__books", "changes": [{"drop_column": "isbn"}]},
                {"op": "drop", "table": "shelf__loans"}
            ]
        }
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_migration_runs_on_activation_and_creates_its_prefixed_table() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&shelf_migrations()),
        ))
        .expect("installs the plugin");
    // Installing runs nothing.
    assert!(!site.has_table("shelf__books").await);

    plugins
        .activate("shelf", admin())
        .await
        .expect("activates the plugin");
    assert!(site.has_table("shelf__books").await);
    assert!(site.has_table("shelf__loans").await);
    // One activation is one batch, as one `php artisan migrate`.
    assert_eq!(
        site.history("shelf").await,
        [
            ("20261115000000_create_books".to_owned(), 1),
            ("20261115000100_create_loans".to_owned(), 1),
        ]
    );
    // The table works: the added column is there, and the defaults are applied.
    raw::execute(
        &*site.db,
        "INSERT INTO shelf__books (title, isbn) VALUES (?, ?)",
        &[Value::from("Dune"), Value::from("978-0441013593")],
    )
    .await
    .expect("inserts a book");
    let pages: i32 = raw::fetch_scalar(
        &*site.db,
        "SELECT pages FROM shelf__books WHERE title = ?",
        &[Value::from("Dune")],
    )
    .await
    .unwrap();
    assert_eq!(pages, 0);

    // Deactivating keeps the tables (WordPress), and activating again runs nothing again.
    assert!(plugins.deactivate("shelf", admin()).await.unwrap());
    assert!(site.has_table("shelf__books").await);
    plugins.activate("shelf", admin()).await.unwrap();
    assert_eq!(site.history("shelf").await.len(), 2);
    assert!(site.history("shelf").await.iter().all(|(_, b)| *b == 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn each_activation_that_runs_migrations_is_its_own_batch() {
    let site = Site::new().await;
    let plugins = site.plugins();
    for slug in ["shelf", "atlas"] {
        let migrations = json!([create_books(
            "20261115000000_create_books",
            &format!("{slug}__books")
        )]);
        plugins
            .install(&plugin_zip(
                slug,
                "\"migrations\"",
                &migrator_wasm(&migrations),
            ))
            .unwrap();
        plugins.activate(slug, admin()).await.unwrap();
    }
    assert_eq!(
        site.history("shelf").await,
        [("20261115000000_create_books".to_owned(), 1)]
    );
    assert_eq!(
        site.history("atlas").await,
        [("20261115000000_create_books".to_owned(), 2)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_with_its_data_rolls_the_migrations_back() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&shelf_migrations()),
        ))
        .unwrap();
    plugins.activate("shelf", admin()).await.unwrap();

    // An active plugin is not deleted, as in WordPress.
    let err = plugins.delete("shelf", true).await.unwrap_err();
    assert!(matches!(err, LifecycleError::Active { .. }), "{err:?}");
    assert!(site.has_table("shelf__books").await);

    plugins.deactivate("shelf", admin()).await.unwrap();
    let deleted = plugins
        .delete("shelf", true)
        .await
        .expect("deletes the plugin");
    // Newest first, as `migrate:reset`.
    assert_eq!(
        deleted.reverted,
        ["20261115000100_create_loans", "20261115000000_create_books"]
    );
    assert!(deleted.leftover_tables.is_empty());
    assert!(!site.has_table("shelf__loans").await);
    assert!(!site.has_table("shelf__books").await);
    assert!(site.history("shelf").await.is_empty());
    assert!(!site.dir.path().join("plugins/shelf").exists());
    // The core tables are untouched.
    assert!(site.has_table("posts").await);
    assert!(site.has_table(PLUGIN_MIGRATIONS_TABLE).await);

    // It is gone.
    let err = plugins.delete("shelf", true).await.unwrap_err();
    assert!(
        matches!(err, LifecycleError::NotInstalled { .. }),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_without_its_data_keeps_the_tables_and_the_history() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let zip = plugin_zip(
        "shelf",
        "\"migrations\"",
        &migrator_wasm(&shelf_migrations()),
    );
    plugins.install(&zip).unwrap();
    plugins.activate("shelf", admin()).await.unwrap();
    plugins.deactivate("shelf", admin()).await.unwrap();

    let deleted = plugins.delete("shelf", false).await.unwrap();
    assert_eq!(deleted, Deleted::default());
    assert!(!site.dir.path().join("plugins/shelf").exists());
    assert!(site.has_table("shelf__books").await);
    assert!(site.has_table("shelf__loans").await);
    assert_eq!(site.history("shelf").await.len(), 2);

    // Installed and activated again, it finds its tables and runs nothing.
    plugins.install(&zip).unwrap();
    plugins.activate("shelf", admin()).await.unwrap();
    assert_eq!(site.history("shelf").await.len(), 2);
    assert!(site.history("shelf").await.iter().all(|(_, b)| *b == 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_without_migrations_is_deleted_with_or_without_its_data() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "plain",
            "\"migrations\"",
            &migrator_wasm(&json!([])),
        ))
        .unwrap();
    plugins.activate("plain", admin()).await.unwrap();
    assert!(site.history("plain").await.is_empty());
    plugins.deactivate("plain", admin()).await.unwrap();
    assert_eq!(
        plugins.delete("plain", true).await.unwrap(),
        Deleted::default()
    );
    assert!(!site.dir.path().join("plugins/plain").exists());
}

/// The `shelf` plugin's folder exists.
fn installed(site: &Site, slug: &str) -> bool {
    site.dir.path().join("plugins").join(slug).exists()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_migration_that_fails_in_the_database_refuses_the_activation() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let migrations = json!([
        create_books("20261115000000_create_books", "shelf__books"),
        create_books("20261115000100_create_more", "shelf__more"),
        {
            // It creates a table, then creates the books again: the database refuses.
            "name": "20261115000200_twice",
            "up": [
                create("shelf__extra", json!([{"name": "id", "type": "id"}])),
                create("shelf__books", json!([{"name": "id", "type": "id"}]))
            ],
            "down": [{"op": "drop", "table": "shelf__extra"}]
        }
    ]);
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&migrations),
        ))
        .unwrap();
    let err = plugins.activate("shelf", admin()).await.unwrap_err();
    assert!(
        matches!(
            &err,
            LifecycleError::Migrations(MigrationError::Failed { migration, .. })
                if migration == "20261115000200_twice"
        ),
        "{err:?}"
    );
    assert!(!plugins.is_active("shelf"));
    // The migrations before it stay applied, in the batch; the failed one is not recorded.
    assert_eq!(
        site.history("shelf").await,
        [
            ("20261115000000_create_books".to_owned(), 1),
            ("20261115000100_create_more".to_owned(), 1),
        ]
    );
    assert!(site.has_table("shelf__books").await);
    assert!(site.has_table("shelf__more").await);
    // SQLite and PostgreSQL roll the failed migration back whole; MySQL and MariaDB commit
    // each DDL statement, so the table it created first stays there.
    let extra = site.has_table("shelf__extra").await;
    match site.db.driver() {
        Driver::MySql => assert!(extra),
        _ => assert!(!extra),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn data_is_not_removed_when_the_history_holds_a_migration_the_plugin_no_longer_has() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&shelf_migrations()),
        ))
        .unwrap();
    plugins.activate("shelf", admin()).await.unwrap();
    plugins.deactivate("shelf", admin()).await.unwrap();
    plugins.delete("shelf", false).await.unwrap();
    // A copy that ships only the first migration.
    let older = json!([create_books("20261115000000_create_books", "shelf__books")]);
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&older),
        ))
        .unwrap();

    let err = plugins.delete("shelf", true).await.unwrap_err();
    assert!(
        matches!(
            &err,
            LifecycleError::Migrations(MigrationError::Unknown(names))
                if names == &["20261115000100_create_loans".to_owned()]
        ),
        "{err:?}"
    );
    // Nothing was reverted, and the plugin is still installed.
    assert_eq!(site.history("shelf").await.len(), 2);
    assert!(site.has_table("shelf__books").await);
    assert!(site.has_table("shelf__loans").await);
    assert!(installed(&site, "shelf"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_down_that_fails_keeps_the_plugin_and_what_was_not_reverted() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let migrations = json!([
        create_books("20261115000000_create_books", "shelf__books"),
        {
            "name": "20261115000100_ghost",
            "up": [],
            // There is no such table, and no `if_exists`.
            "down": [{"op": "drop", "table": "shelf__ghost"}]
        }
    ]);
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&migrations),
        ))
        .unwrap();
    plugins.activate("shelf", admin()).await.unwrap();
    plugins.deactivate("shelf", admin()).await.unwrap();

    let err = plugins.delete("shelf", true).await.unwrap_err();
    assert!(
        matches!(
            &err,
            LifecycleError::Migrations(MigrationError::RollbackFailed { migration, .. })
                if migration == "20261115000100_ghost"
        ),
        "{err:?}"
    );
    assert!(installed(&site, "shelf"));
    assert_eq!(site.history("shelf").await.len(), 2);
    assert!(site.has_table("shelf__books").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn tables_a_down_does_not_drop_are_reported_as_left_over() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let migrations = json!([{
        "name": "20261115000000_create_books",
        "up": [create("shelf__books", json!([{"name": "id", "type": "id"}]))],
        "down": []
    }]);
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&migrations),
        ))
        .unwrap();
    plugins.activate("shelf", admin()).await.unwrap();
    plugins.deactivate("shelf", admin()).await.unwrap();

    let deleted = plugins.delete("shelf", true).await.unwrap();
    assert_eq!(deleted.reverted, ["20261115000000_create_books"]);
    assert_eq!(deleted.leftover_tables, ["shelf__books"]);
    assert!(site.history("shelf").await.is_empty());
    assert!(!installed(&site, "shelf"));
}

/// Activates `shelf` with a good first migration followed by `bad` and checks that it is
/// refused before anything ran: no table, no history, not active.
async fn refused_before_anything_runs(bad: Json, expect: &str) {
    let site = Site::new().await;
    let plugins = site.plugins();
    let migrations = json!([
        create_books("20261115000000_create_books", "shelf__books"),
        bad
    ]);
    plugins
        .install(&plugin_zip(
            "shelf",
            "\"migrations\"",
            &migrator_wasm(&migrations),
        ))
        .unwrap();
    let err = plugins.activate("shelf", admin()).await.unwrap_err();
    let LifecycleError::Migrations(MigrationError::Invalid(message)) = &err else {
        panic!("refused as invalid migrations: {err:?}");
    };
    assert!(message.contains(expect), "{message}");
    assert!(!plugins.is_active("shelf"));
    assert!(!site.has_table("shelf__books").await);
    assert!(site.history("shelf").await.is_empty());
    // The core tables are all there.
    for table in ["posts", "options", "users", PLUGIN_MIGRATIONS_TABLE] {
        assert!(site.has_table(table).await, "{table}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_table_without_the_prefix_is_refused_before_anything_runs() {
    refused_before_anything_runs(
        create_books("20261115000100_create_more", "books"),
        "`books`",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_core_table_is_refused_before_anything_runs() {
    // Created, dropped in a `down`, or altered.
    refused_before_anything_runs(
        create_books("20261115000100_create_posts", "posts"),
        "`posts`",
    )
    .await;
    refused_before_anything_runs(
        json!({
            "name": "20261115000100_innocent",
            "up": [],
            "down": [{"op": "drop", "table": "__diesel_schema_migrations"}]
        }),
        "`__diesel_schema_migrations`",
    )
    .await;
    refused_before_anything_runs(
        json!({
            "name": "20261115000100_options",
            "up": [{"op": "alter", "table": "options", "changes": [{"drop_column": "autoload"}]}],
            "down": []
        }),
        "`options`",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_migrations_permission_is_required() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let wasm = migrator_wasm(&shelf_migrations());

    // Refused at install, as an import the manifest does not grant.
    let err = plugins
        .install(&plugin_zip("shelf", "\"hooks\"", &wasm))
        .unwrap_err();
    assert!(
        matches!(
            &err,
            InstallError::Module(HostError::UngrantedExport { export, permission: Permission::Migrations })
                if export == "mission-control:plugin/migrations@0.1.0"
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("`migrations` permission"), "{err}");

    // And at activation, for files put in the folder by hand.
    let folder = site.dir.path().join("plugins/shelf");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join(MANIFEST_FILE),
        manifest_text("shelf", "\"hooks\""),
    )
    .unwrap();
    std::fs::write(folder.join(MODULE_FILE), &wasm).unwrap();
    let err = plugins.activate("shelf", admin()).await.unwrap_err();
    assert!(
        matches!(
            err,
            LifecycleError::Host(HostError::UngrantedExport {
                permission: Permission::Migrations,
                ..
            })
        ),
        "{err:?}"
    );
    assert!(!site.has_table("shelf__books").await);
    assert!(site.history("shelf").await.is_empty());
}

// ----- The rules, checked without a database -----

/// The message `parse_migrations` refuses `migrations` (JSON) of plugin `shelf` with.
fn refusal(migrations: Json) -> String {
    match parse_migrations(&manifest("shelf"), &migrations.to_string(), Driver::Sqlite) {
        Ok(_) => panic!("refuses {migrations}"),
        Err(MigrationError::Invalid(message)) => message,
        Err(e) => panic!("refuses as invalid: {e:?}"),
    }
}

/// One migration named `20261115000000_test` with `up` (and an empty `down`).
fn one(up: Json) -> Json {
    json!([{"name": "20261115000000_test", "up": [up], "down": []}])
}

fn create(table: &str, columns: Json) -> Json {
    json!({"op": "create", "table": table, "columns": columns})
}

#[test]
fn table_names_must_carry_the_prefix() {
    let id = json!([{"name": "id", "type": "id"}]);
    // Another plugin's prefix, the prefix alone, and a near miss.
    for table in [
        "acme__books",
        "shelf_books",
        "shelf__",
        "shelfbooks",
        "Shelf__books",
    ] {
        let message = refusal(one(create(table, id.clone())));
        assert!(message.contains(table), "{table}: {message}");
    }
    // A plugin whose slug has a `-`: its prefix has `_` there.
    let ok = parse_migrations(
        &manifest("my-shelf"),
        &one(create("my_shelf__books", id.clone())).to_string(),
        Driver::Sqlite,
    );
    assert!(ok.is_ok(), "{ok:?}");
    let err = parse_migrations(
        &manifest("my-shelf"),
        &one(create("my-shelf__books", id)).to_string(),
        Driver::Sqlite,
    );
    assert!(matches!(err, Err(MigrationError::Invalid(_))), "{err:?}");
}

#[test]
fn names_are_bounded_and_plain() {
    let long = format!("shelf__{}", "b".repeat(57));
    assert_eq!(long.len(), 64);
    let message = refusal(one(create(&long, json!([{"name": "id", "type": "id"}]))));
    assert!(message.contains("63"), "{message}");
    // A column with a quote, a space, upper case, a leading digit or nothing.
    for column in ["ti\"tle", "ti tle", "Title", "1st", "", "x`y"] {
        let message = refusal(one(create(
            "shelf__books",
            json!([{"name": column, "type": "text"}]),
        )));
        assert!(message.contains("column"), "{column}: {message}");
    }
    // An index name built from the table and its columns must fit too.
    // `shelf__books_<50 c>_index` is 69 bytes.
    let column = "c".repeat(50);
    let message = refusal(one(json!({
        "op": "create",
        "table": "shelf__books",
        "columns": [{"name": column, "type": "integer"}],
        "indexes": [{"columns": [column]}]
    })));
    assert!(message.contains("name"), "{message}");
    // A named index is the plugin's too.
    let message = refusal(one(json!({
        "op": "create",
        "table": "shelf__books",
        "columns": [{"name": "title", "type": "string"}],
        "indexes": [{"columns": ["title"], "name": "posts_title_index"}]
    })));
    assert!(message.contains("posts_title_index"), "{message}");
    let message = refusal(one(json!({
        "op": "alter",
        "table": "shelf__books",
        "changes": [{"drop_index": "posts_slug_unique"}]
    })));
    assert!(message.contains("posts_slug_unique"), "{message}");
}

#[test]
fn foreign_keys_reach_only_the_plugins_tables_and_core_ids() {
    let key = |table: &str, column: &str, on_delete: Json| {
        one(json!({
            "op": "create",
            "table": "shelf__books",
            "columns": [{"name": "id", "type": "id"}, {"name": "ref_id", "type": "big_integer", "nullable": true}],
            "foreign_keys": [{
                "column": "ref_id",
                "references": {"table": table, "column": column},
                "on_delete": on_delete
            }]
        }))
    };
    for (table, column, on_delete) in [
        ("options", "id", json!("cascade")),
        ("acme__things", "id", json!("cascade")),
        ("posts", "author_id", json!("cascade")),
        // The site could no longer delete its posts or users.
        ("posts", "id", json!("restrict")),
        ("users", "id", json!(null)),
        ("terms", "id", json!("no_action")),
    ] {
        let message = refusal(key(table, column, on_delete.clone()));
        assert!(
            message.contains("foreign key"),
            "{table}.{column} {on_delete}: {message}"
        );
    }
    for (table, on_delete) in [
        ("posts", "cascade"),
        ("users", "set_null"),
        ("terms", "cascade"),
        ("shelf__other", "restrict"),
    ] {
        let parsed = parse_migrations(
            &manifest("shelf"),
            &key(table, "id", json!(on_delete)).to_string(),
            Driver::Sqlite,
        );
        assert!(parsed.is_ok(), "{table} {on_delete}: {parsed:?}");
    }
}

#[test]
fn the_list_is_bounded_and_well_formed() {
    // Too large, before it is parsed.
    let big = " ".repeat(MAX_MIGRATIONS_JSON_BYTES + 1);
    let err = parse_migrations(&manifest("shelf"), &big, Driver::Sqlite).unwrap_err();
    assert!(matches!(err, MigrationError::TooLarge { .. }), "{err:?}");

    // Too many migrations.
    let many: Vec<Json> = (0..=MAX_PLUGIN_MIGRATIONS)
        .map(|i| json!({"name": format!("2026111500{i:04}_m"), "up": [], "down": []}))
        .collect();
    assert!(refusal(Json::from(many)).contains(&MAX_PLUGIN_MIGRATIONS.to_string()));

    // A name that is not `<14 digits>_<name>`, and the same name twice.
    for name in [
        "create_books",
        "2026111500000_x",
        "20261115000000_",
        "20261115000000_Books",
    ] {
        let message = refusal(json!([{"name": name, "up": [], "down": []}]));
        assert!(message.contains("name"), "{name}: {message}");
    }
    let message = refusal(json!([
        {"name": "20261115000000_a", "up": [], "down": []},
        {"name": "20261115000000_a", "up": [], "down": []}
    ]));
    assert!(message.contains("twice"), "{message}");

    // A key the format does not have, a type Orbit does not have, and not JSON at all.
    let message = refusal(one(
        json!({"op": "create", "table": "shelf__b", "columns": [], "engine": "MyISAM"}),
    ));
    assert!(message.contains("engine"), "{message}");
    let message = refusal(one(create(
        "shelf__b",
        json!([{"name": "v", "type": "json"}]),
    )));
    assert!(message.contains("json"), "{message}");
    let err = parse_migrations(&manifest("shelf"), "not json", Driver::Sqlite).unwrap_err();
    assert!(matches!(err, MigrationError::Invalid(_)), "{err:?}");

    // A table needs columns, and `id` is the `id` column.
    assert!(refusal(one(create("shelf__b", json!([])))).contains("column"));
    assert!(
        refusal(one(create(
            "shelf__b",
            json!([{"name": "key", "type": "id"}])
        )))
        .contains("id")
    );
}

#[test]
fn a_backslash_in_a_default_is_refused() {
    for default in ["C:\\temp", "\\", "a\\'b"] {
        let message = refusal(one(create(
            "shelf__books",
            json!([{"name": "path", "type": "string", "default": default}]),
        )));
        assert!(message.contains("path"), "{default}: {message}");
    }
    let ok = parse_migrations(
        &manifest("shelf"),
        &one(create(
            "shelf__books",
            json!([{"name": "path", "type": "string", "default": "it's /tmp?"}]),
        ))
        .to_string(),
        Driver::Postgres,
    );
    assert!(ok.is_ok(), "{ok:?}");
}

#[test]
fn keys_and_defaults_mysql_refuses_are_refused() {
    let books = |extra: Json| {
        let mut statement = json!({
            "op": "create",
            "table": "shelf__books",
            "columns": [
                {"name": "id", "type": "id"},
                {"name": "body", "type": "long_text"},
                {"name": "cover", "type": "binary"},
                {"name": "slug", "type": "string", "length": 1000},
                {"name": "code", "type": "string", "length": 768}
            ]
        });
        for (key, value) in extra.as_object().unwrap() {
            statement[key] = value.clone();
        }
        statement
    };
    // A text, binary or too long string column in an index, a unique, a primary or a key.
    for (extra, column) in [
        (json!({"indexes": [{"columns": ["body"]}]}), "body"),
        (
            json!({"indexes": [{"columns": ["cover"], "unique": true}]}),
            "cover",
        ),
        (
            json!({"indexes": [{"columns": ["slug"], "unique": true}]}),
            "slug",
        ),
        (
            json!({"foreign_keys": [{"column": "slug", "references": {"table": "shelf__other", "column": "slug"}}]}),
            "slug",
        ),
    ] {
        let message = refusal(one(books(extra.clone())));
        assert!(
            message.contains(column) && message.contains("key"),
            "{extra}: {message}"
        );
    }
    // 768 characters is the longest a key takes.
    let ok = parse_migrations(
        &manifest("shelf"),
        &one(books(
            json!({"indexes": [{"columns": ["code"], "unique": true}]}),
        ))
        .to_string(),
        Driver::MySql,
    );
    assert!(ok.is_ok(), "{ok:?}");
    // Followed across migrations: a later index on a text column, after a rename.
    let message = refusal(json!([
        {"name": "20261115000000_a", "up": [books(json!({}))], "down": []},
        {"name": "20261115000100_b", "up": [{"op": "alter", "table": "shelf__books", "changes": [
            {"rename_column": {"from": "body", "to": "content"}},
            {"add_index": {"columns": ["content"]}}
        ]}], "down": []}
    ]));
    assert!(message.contains("content"), "{message}");
    // A primary key on a text column.
    let message = refusal(one(create(
        "shelf__notes",
        json!([{"name": "note", "type": "text", "primary": true}]),
    )));
    assert!(message.contains("note"), "{message}");
    // No default on a text or binary column.
    for ty in ["text", "medium_text", "long_text", "binary"] {
        let message = refusal(one(create(
            "shelf__notes",
            json!([{"name": "note", "type": ty, "default": ""}]),
        )));
        assert!(message.contains("default"), "{ty}: {message}");
    }
}

#[test]
fn a_statement_the_database_cannot_run_is_refused_before_anything_runs() {
    // SQLite cannot add a primary key to an existing table: Orbit refuses to render it.
    let migrations = one(json!({
        "op": "alter",
        "table": "shelf__books",
        "changes": [{"add_column": {"name": "id", "type": "id"}}]
    }));
    let err =
        parse_migrations(&manifest("shelf"), &migrations.to_string(), Driver::Sqlite).unwrap_err();
    assert!(matches!(err, MigrationError::Invalid(_)), "{err:?}");
}

#[test]
fn a_valid_list_is_rendered_in_name_order() {
    let migrations = json!([
        {"name": "20261115000100_second", "up": [create("shelf__b", json!([{"name": "id", "type": "id"}]))], "down": [{"op": "drop", "table": "shelf__b"}]},
        create_books("20261115000000_first", "shelf__books"),
    ]);
    let parsed =
        parse_migrations(&manifest("shelf"), &migrations.to_string(), Driver::Sqlite).unwrap();
    let names: Vec<&str> = parsed.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["20261115000000_first", "20261115000100_second"]);
    assert!(
        parsed[0].up[0].starts_with("CREATE TABLE \"shelf__books\""),
        "{:?}",
        parsed[0].up
    );
    assert_eq!(parsed[1].down, ["DROP TABLE \"shelf__b\""]);
}
