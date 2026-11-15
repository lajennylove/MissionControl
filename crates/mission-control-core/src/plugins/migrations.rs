//! A plugin's own database tables (Task 12.6, design section 4.2): migrations the plugin
//! describes with the schema builder's terms, and the host checks, renders and runs.
//!
//! A plugin cannot run code in the host, so it hands its migrations over as data: its
//! `migrations` export (WIT interface `migrations`, permission `migrations`) returns them as
//! JSON text, in the format the WIT documents. [`parse_migrations`] reads that text, checks every
//! rule below, builds each statement with Orbit's schema builder (`schema::create`,
//! `schema::table`, `schema::drop`) and renders it for the site's database
//! ([`Statement::try_to_sql`]), so a list that breaks a rule, or that Orbit cannot render for
//! the database, is refused before any of it runs. The rules also refuse what MySQL and MariaDB
//! are known to refuse (keys on text columns, defaults on text columns, keys too long), but the
//! database may still refuse a statement when it runs: then that migration fails, and on MySQL
//! and MariaDB the DDL it ran before the failure stays (see "History"). [`run_pending_migrations`] runs the migrations the plugin has not run, in
//! name order, as one batch; [`reset_migrations`] reverts all it ran, newest first.
//!
//! # Rules
//!
//! - **Names.** Every table, index and foreign key name starts with the plugin's prefix
//!   ([`Manifest::table_prefix`], Ruling R101: `acme-forms` gives `acme_forms__`), has more after
//!   it, and like every column name is at most [`MAX_IDENTIFIER_BYTES`] bytes of lowercase ASCII
//!   letters, digits and `_`, not starting with a digit. No core table has `__` in its name and
//!   no slug has `_`, so a plugin can name neither a core table nor another plugin's, in `up`
//!   and in `down` alike: it can only create, change and drop its own. The names Orbit builds for
//!   an index or a foreign key without one (`{table}_{columns}_index`, `{table}_{column}_foreign`)
//!   start with the table, and must fit too.
//! - **Foreign keys** reference the plugin's own tables, or the `id` of the site's `posts`,
//!   `users` or `terms` ([`CORE_REFERENCES`]) with `on_delete` `cascade` or `set_null` and no
//!   `on_update` other than `cascade`, so a plugin's rows never stop the site from deleting its
//!   own. WordPress plugins keep post and user ids in their tables (often without a key).
//! - **Bounds.** At most [`MAX_MIGRATIONS_JSON_BYTES`] of JSON, [`MAX_PLUGIN_MIGRATIONS`]
//!   migrations, [`MAX_PLUGIN_TABLES`] table names, [`MAX_STATEMENTS`] statements in each `up`
//!   and `down`; per statement [`MAX_COLUMNS`] columns, [`MAX_INDEXES`] indexes,
//!   [`MAX_FOREIGN_KEYS`] foreign keys and [`MAX_CHANGES`] changes; [`MAX_INDEX_COLUMNS`] columns
//!   in an index or primary key; a migration name at most [`MAX_MIGRATION_NAME_BYTES`]; a text
//!   default at most [`MAX_DEFAULT_BYTES`].
//! - **Keys and defaults MySQL refuses.** A column in an index, a unique, a primary key or a
//!   foreign key is not `text`, `medium_text`, `long_text` or `binary` (MySQL cannot key them
//!   without a prefix length, which the builder has no way to say), and a `string` there is at
//!   most [`MAX_KEY_STRING_LENGTH`] characters (InnoDB's 3072-byte key limit in `utf8mb4`).
//!   Those columns take no `default` either (MySQL refuses one on `TEXT` and `BLOB`). The
//!   column types are followed through the plugin's `up` statements in name order (`create`,
//!   `add_column`, `rename_column`, `drop_column`, `drop`); a column the checks cannot see
//!   (one of a table created by another plugin version, say) is left to the database.
//! - **Text defaults** have no backslash: Orbit doubles a backslash in a literal on MySQL only, and on
//!   a PostgreSQL server with `standard_conforming_strings` off a backslash would end the
//!   literal.
//! - **What the builder can say, and no more.** Raw SQL (`schema::statement`) is not offered:
//!   it could name any table and is not portable. Nor is changing a column in place
//!   (`.change()`, which SQLite cannot do).
//!
//! # History
//!
//! The migrations a plugin ran are recorded in [`PLUGIN_MIGRATIONS_TABLE`] by its slug and the
//! migration's name, with a batch number: one activation that runs migrations is one batch, as
//! one `php artisan migrate` is (Ruling R92). Orbit's migrator and its `__diesel_schema_migrations`
//! are not used: they key a migration by its 14-digit version alone, shared with the core's and
//! the panel's, and a plugin's version could equal one of theirs. Each migration runs in its own
//! transaction with its history row, as Orbit's migrator runs one. SQLite and PostgreSQL roll a
//! failed migration back whole, DDL included; MySQL and MariaDB commit each DDL statement
//! implicitly, so there a migration that fails half-way leaves what it created before the
//! failure, without its history row, as in Laravel. The migrations before it stay applied.
//!
//! The history and the tables belong to the slug, not to one copy of the plugin: when a plugin
//! is deleted without its data, a plugin installed later under the same slug finds its tables
//! and its history, and runs only the migrations not recorded.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

use chrono::Utc;
use orbit_orm::schema::{self, Action, Blueprint, Statement};
use orbit_orm::{Database, Driver, Value, raw};
use serde::Deserialize;

use super::manifest::Manifest;

/// The table that records the migrations plugins ran: `plugin` (slug), `migration` (name),
/// `batch`, `applied_at`.
pub const PLUGIN_MIGRATIONS_TABLE: &str = "plugin_migrations";

/// The largest JSON text of a plugin's migrations, in bytes (1 MiB).
pub const MAX_MIGRATIONS_JSON_BYTES: usize = 1024 * 1024;

/// The most migrations a plugin may have.
pub const MAX_PLUGIN_MIGRATIONS: usize = 256;

/// The most table names a plugin's migrations may name, together.
pub const MAX_PLUGIN_TABLES: usize = 64;

/// The most statements in a migration's `up`, and in its `down`.
pub const MAX_STATEMENTS: usize = 64;

/// The most columns a statement may create or add.
pub const MAX_COLUMNS: usize = 128;

/// The most indexes (plain and unique) a statement may create.
pub const MAX_INDEXES: usize = 64;

/// The most foreign keys a statement may create.
pub const MAX_FOREIGN_KEYS: usize = 64;

/// The most changes in one `alter` statement.
pub const MAX_CHANGES: usize = 256;

/// The most columns in one index or primary key.
pub const MAX_INDEX_COLUMNS: usize = 16;

/// The longest table, column, index or constraint name, in bytes: PostgreSQL's limit
/// (MySQL's is 64).
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// The longest migration name, in bytes: the size of `plugin_migrations.migration`.
pub const MAX_MIGRATION_NAME_BYTES: usize = 128;

/// The longest text default of a column, in bytes.
pub const MAX_DEFAULT_BYTES: usize = 1024;

/// The longest `string` column, in characters: MySQL's `VARCHAR` limit in `utf8mb4`.
pub const MAX_STRING_LENGTH: u32 = 16_383;

/// The longest `string` column that may be in an index, a unique or a key, in characters:
/// InnoDB's 3072-byte key limit over 4 bytes a character in `utf8mb4`. (Laravel's
/// `defaultStringLength(191)` is for the 767-byte limit of MySQL before 5.7.7.)
pub const MAX_KEY_STRING_LENGTH: u32 = 768;

/// The core tables a plugin's foreign key may reference, by their `id` only.
pub const CORE_REFERENCES: [&str; 3] = ["posts", "users", "terms"];

/// Why a plugin's migrations were refused, or did not run or roll back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationError {
    /// The JSON text is over [`MAX_MIGRATIONS_JSON_BYTES`].
    TooLarge {
        /// Its size, in bytes.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The migrations break a rule (see the module documentation), are not the WIT's format,
    /// or a statement cannot be rendered for the site's database. Nothing ran.
    Invalid(String),
    /// The site's history holds migrations of the plugin that it no longer ships, so its data
    /// cannot be rolled back. Nothing was reverted.
    Unknown(Vec<String>),
    /// A migration failed in the database. The ones before it stay applied.
    Failed {
        /// The migration's name.
        migration: String,
        /// The database's complaint.
        message: String,
    },
    /// A migration's `down` failed in the database. The ones reverted before it stay reverted.
    RollbackFailed {
        /// The migration's name.
        migration: String,
        /// The database's complaint.
        message: String,
    },
    /// The history could not be read or written.
    Database(String),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len, max } => write!(
                f,
                "the plugin's migrations are {len} bytes of JSON; at most {max} are accepted"
            ),
            Self::Invalid(e) => write!(f, "the plugin's migrations were refused: {e}"),
            Self::Unknown(names) => write!(
                f,
                "the site ran migrations the plugin no longer has ({}), so its data cannot be \
                 removed",
                names.join(", ")
            ),
            Self::Failed { migration, message } => {
                write!(f, "the plugin's migration `{migration}` failed: {message}")
            }
            Self::RollbackFailed { migration, message } => write!(
                f,
                "the plugin's migration `{migration}` could not be rolled back: {message}"
            ),
            Self::Database(e) => write!(f, "the plugin migrations could not be recorded: {e}"),
        }
    }
}

impl std::error::Error for MigrationError {}

/// One of a plugin's migrations, checked and rendered for one database: what applies it (`up`)
/// and reverts it (`down`), as SQL statements to run in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMigration {
    /// Its name, such as `20261115000000_create_books`.
    pub name: String,
    /// The SQL that applies it, in order.
    pub up: Vec<String>,
    /// The SQL that reverts it, in order.
    pub down: Vec<String>,
}

// ----- The format (the WIT `migrations` interface's documentation) -----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationDef {
    name: String,
    #[serde(default)]
    up: Vec<StatementDef>,
    #[serde(default)]
    down: Vec<StatementDef>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum StatementDef {
    Create {
        table: String,
        #[serde(default)]
        if_not_exists: bool,
        columns: Vec<ColumnDef>,
        #[serde(default)]
        indexes: Vec<IndexDef>,
        #[serde(default)]
        foreign_keys: Vec<ForeignKeyDef>,
        #[serde(default)]
        primary: Option<Vec<String>>,
    },
    Alter {
        table: String,
        changes: Vec<ChangeDef>,
    },
    Drop {
        table: String,
        #[serde(default)]
        if_exists: bool,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ChangeDef {
    AddColumn(ColumnDef),
    DropColumn(String),
    RenameColumn { from: String, to: String },
    AddIndex(IndexDef),
    DropIndex(String),
    AddForeignKey(ForeignKeyDef),
    DropForeignKey(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ColumnDef {
    name: String,
    #[serde(rename = "type")]
    ty: ColumnType,
    #[serde(default)]
    nullable: bool,
    #[serde(default)]
    default: Option<serde_json::Value>,
    #[serde(default)]
    length: Option<u32>,
    #[serde(default)]
    precision: Option<u32>,
    #[serde(default)]
    use_current: bool,
    #[serde(default)]
    primary: bool,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ColumnType {
    Id,
    String,
    Text,
    MediumText,
    LongText,
    Integer,
    BigInteger,
    Boolean,
    DateTime,
    Timestamp,
    Binary,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexDef {
    columns: Vec<String>,
    #[serde(default)]
    unique: bool,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForeignKeyDef {
    column: String,
    references: TargetDef,
    #[serde(default)]
    on_delete: Option<ActionDef>,
    #[serde(default)]
    on_update: Option<ActionDef>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetDef {
    table: String,
    column: String,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ActionDef {
    Cascade,
    Restrict,
    SetNull,
    SetDefault,
    NoAction,
}

impl ActionDef {
    fn orbit(self) -> Action {
        match self {
            Self::Cascade => Action::Cascade,
            Self::Restrict => Action::Restrict,
            Self::SetNull => Action::SetNull,
            Self::SetDefault => Action::SetDefault,
            Self::NoAction => Action::NoAction,
        }
    }
}

// ----- Checking -----

/// Reads the migrations `json` of the plugin `manifest` describes (its `migrations` export),
/// checks them against the rules of the module documentation, and renders each statement for
/// `driver`. Returns them in name order, as Laravel and Orbit run migrations.
///
/// # Errors
///
/// [`MigrationError::TooLarge`] over [`MAX_MIGRATIONS_JSON_BYTES`];
/// [`MigrationError::Invalid`] for text that is not the format, a broken rule (the message
/// names the migration and the name or bound at fault), or a statement Orbit cannot render for
/// `driver`.
pub fn parse_migrations(
    manifest: &Manifest,
    json: &str,
    driver: Driver,
) -> Result<Vec<PluginMigration>, MigrationError> {
    if json.len() > MAX_MIGRATIONS_JSON_BYTES {
        return Err(MigrationError::TooLarge {
            len: json.len(),
            max: MAX_MIGRATIONS_JSON_BYTES,
        });
    }
    let defs: Vec<MigrationDef> = serde_json::from_str(json).map_err(|e| {
        MigrationError::Invalid(format!(
            "they are not a list of migrations in the expected format: {}",
            shown(&e.to_string())
        ))
    })?;
    if defs.len() > MAX_PLUGIN_MIGRATIONS {
        return Err(invalid(format!(
            "there are {} migrations; a plugin may have at most {MAX_PLUGIN_MIGRATIONS}",
            defs.len()
        )));
    }
    let mut checker = Checker {
        prefix: manifest.table_prefix(),
        tables: BTreeSet::new(),
        columns: HashMap::new(),
    };
    let mut names = HashSet::new();
    let mut migrations = Vec::with_capacity(defs.len());
    // In name order, the order they run in, so the column types are followed as they change.
    let mut defs = defs;
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    for def in defs {
        check_migration_name(&def.name)?;
        if !names.insert(def.name.clone()) {
            return Err(invalid(format!(
                "the migration `{}` is listed twice",
                def.name
            )));
        }
        let at = |e: String| invalid(format!("in the migration `{}`: {e}", def.name));
        let up = checker.render_all(&def.up, "up", driver).map_err(at)?;
        // A `down` is checked against the columns after the `up`, and changes nothing.
        let after_up = checker.columns.clone();
        let down = checker.render_all(&def.down, "down", driver).map_err(at)?;
        checker.columns = after_up;
        migrations.push(PluginMigration {
            name: def.name,
            up,
            down,
        });
    }
    Ok(migrations)
}

fn invalid(message: String) -> MigrationError {
    MigrationError::Invalid(message)
}

/// `text` as a message may show it: escaped, at most 256 characters.
fn shown(text: &str) -> String {
    super::host::clean(text, 256)
}

/// Refuses a migration name that is not 14 digits, `_`, and lowercase letters, digits and `_`,
/// or that is over [`MAX_MIGRATION_NAME_BYTES`].
fn check_migration_name(name: &str) -> Result<(), MigrationError> {
    let bad = || {
        invalid(format!(
            "the migration name `{}` is not 14 digits, `_` and a name of lowercase letters, \
             digits and `_` (`20261115000000_create_books`), at most \
             {MAX_MIGRATION_NAME_BYTES} bytes",
            shown(name)
        ))
    };
    if name.len() > MAX_MIGRATION_NAME_BYTES || name.len() < 16 {
        return Err(bad());
    }
    let (stamp, rest) = name.split_at(14);
    let Some(rest) = rest.strip_prefix('_') else {
        return Err(bad());
    };
    if !stamp.bytes().all(|b| b.is_ascii_digit())
        || rest.is_empty()
        || !rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(bad());
    }
    Ok(())
}

/// Checks a plugin's statements against its prefix, counting the tables they name and following
/// the type of each column they create.
struct Checker {
    prefix: String,
    tables: BTreeSet<String>,
    /// The columns of each table, by name, as the statements so far leave them.
    columns: HashMap<String, HashMap<String, (ColumnType, Option<u32>)>>,
}

impl Checker {
    /// Checks, builds and renders `statements` (a migration's `up` or `down`, `which`).
    fn render_all(
        &mut self,
        statements: &[StatementDef],
        which: &str,
        driver: Driver,
    ) -> Result<Vec<String>, String> {
        if statements.len() > MAX_STATEMENTS {
            return Err(format!(
                "its `{which}` has {} statements; at most {MAX_STATEMENTS} are allowed",
                statements.len()
            ));
        }
        let mut sql = Vec::new();
        for def in statements {
            let statement = self.build(def)?;
            let rendered = statement.try_to_sql(driver).map_err(|e| {
                format!(
                    "a statement of its `{which}` cannot run on {}: {}",
                    driver.name(),
                    shown(&e.to_string())
                )
            })?;
            sql.extend(rendered);
        }
        Ok(sql)
    }

    /// Checks `def` and builds it with the schema builder.
    fn build(&mut self, def: &StatementDef) -> Result<Statement, String> {
        match def {
            StatementDef::Create {
                table,
                if_not_exists,
                columns,
                indexes,
                foreign_keys,
                primary,
            } => {
                self.table(table)?;
                if columns.is_empty() {
                    return Err(format!("the table `{table}` is created with no column"));
                }
                count(columns.len(), MAX_COLUMNS, "columns", table)?;
                count(indexes.len(), MAX_INDEXES, "indexes", table)?;
                count(foreign_keys.len(), MAX_FOREIGN_KEYS, "foreign keys", table)?;
                for column in columns {
                    check_column(column)?;
                }
                let known = self.columns.entry(table.clone()).or_default();
                known.clear();
                for column in columns {
                    known.insert(column.name.clone(), (column.ty, column.length));
                }
                for column in columns.iter().filter(|c| c.primary) {
                    self.key_column(table, &column.name)?;
                }
                for index in indexes {
                    self.index(table, index)?;
                }
                for key in foreign_keys {
                    self.foreign_key(table, key)?;
                }
                if let Some(primary) = primary {
                    index_columns(primary, table)?;
                    for column in primary {
                        self.key_column(table, column)?;
                    }
                }
                let describe = |t: &mut Blueprint| {
                    for column in columns {
                        add_column(t, column);
                    }
                    for index in indexes {
                        add_index(t, index);
                    }
                    for key in foreign_keys {
                        add_foreign_key(t, key);
                    }
                    if let Some(primary) = primary {
                        t.primary(primary);
                    }
                };
                Ok(if *if_not_exists {
                    schema::create_if_not_exists(table, describe)
                } else {
                    schema::create(table, describe)
                })
            }
            StatementDef::Alter { table, changes } => {
                self.table(table)?;
                count(changes.len(), MAX_CHANGES, "changes", table)?;
                let columns = changes
                    .iter()
                    .filter(|c| matches!(c, ChangeDef::AddColumn(_)))
                    .count();
                count(columns, MAX_COLUMNS, "columns", table)?;
                for change in changes {
                    match change {
                        ChangeDef::AddColumn(column) => {
                            check_column(column)?;
                            self.columns
                                .entry(table.clone())
                                .or_default()
                                .insert(column.name.clone(), (column.ty, column.length));
                            if column.primary {
                                self.key_column(table, &column.name)?;
                            }
                        }
                        ChangeDef::DropColumn(name) => {
                            identifier(name, "column")?;
                            if let Some(known) = self.columns.get_mut(table) {
                                known.remove(name);
                            }
                        }
                        ChangeDef::RenameColumn { from, to } => {
                            identifier(from, "column")?;
                            identifier(to, "column")?;
                            if let Some(known) = self.columns.get_mut(table)
                                && let Some(info) = known.remove(from)
                            {
                                known.insert(to.clone(), info);
                            }
                        }
                        ChangeDef::AddIndex(index) => self.index(table, index)?,
                        ChangeDef::DropIndex(name) => self.owned(name, "index")?,
                        ChangeDef::AddForeignKey(key) => self.foreign_key(table, key)?,
                        ChangeDef::DropForeignKey(name) => self.owned(name, "foreign key")?,
                    }
                }
                Ok(schema::table(table, |t| {
                    for change in changes {
                        match change {
                            ChangeDef::AddColumn(column) => add_column(t, column),
                            ChangeDef::DropColumn(name) => t.drop_column(name),
                            ChangeDef::RenameColumn { from, to } => t.rename_column(from, to),
                            ChangeDef::AddIndex(index) => add_index(t, index),
                            ChangeDef::DropIndex(name) => t.drop_index(name),
                            ChangeDef::AddForeignKey(key) => add_foreign_key(t, key),
                            ChangeDef::DropForeignKey(name) => t.drop_foreign(name),
                        }
                    }
                }))
            }
            StatementDef::Drop { table, if_exists } => {
                self.table(table)?;
                self.columns.remove(table);
                Ok(if *if_exists {
                    schema::drop_if_exists(table)
                } else {
                    schema::drop(table)
                })
            }
        }
    }

    /// Refuses a column of `table` in a key (an index, a unique, a primary or a foreign key)
    /// that MySQL cannot key: a text or binary column, or a `string` longer than
    /// [`MAX_KEY_STRING_LENGTH`]. A column the checker has not seen is left to the database.
    fn key_column(&self, table: &str, column: &str) -> Result<(), String> {
        let Some(&(ty, length)) = self.columns.get(table).and_then(|c| c.get(column)) else {
            return Ok(());
        };
        if matches!(
            ty,
            ColumnType::Text | ColumnType::MediumText | ColumnType::LongText | ColumnType::Binary
        ) {
            return Err(format!(
                "the column `{column}` of `{table}` is in a key, and a text or binary column \
                 cannot be (MySQL needs a prefix length): use a `string`"
            ));
        }
        if ty == ColumnType::String && length.unwrap_or(255) > MAX_KEY_STRING_LENGTH {
            return Err(format!(
                "the column `{column}` of `{table}` is in a key, and a `string` in a key is at \
                 most {MAX_KEY_STRING_LENGTH} characters long"
            ));
        }
        Ok(())
    }

    /// Refuses a table name that is not the plugin's, and counts it.
    fn table(&mut self, table: &str) -> Result<(), String> {
        self.owned(table, "table")?;
        if !self.tables.contains(table) {
            if self.tables.len() >= MAX_PLUGIN_TABLES {
                return Err(format!(
                    "the migrations name more than {MAX_PLUGIN_TABLES} tables, the most allowed"
                ));
            }
            self.tables.insert(table.to_owned());
        }
        Ok(())
    }

    /// Refuses a table, index or constraint name (`what`) that does not start with the
    /// plugin's prefix and have more after it, or is not a valid name.
    fn owned(&self, name: &str, what: &str) -> Result<(), String> {
        identifier(name, what)?;
        if !name.starts_with(&self.prefix) || name.len() == self.prefix.len() {
            return Err(format!(
                "the {what} `{}` is not the plugin's: its name must start with `{}` (the \
                 plugin's slug, `-` as `_`, then `__`) and go on after it; a plugin cannot \
                 create, change or drop the site's tables or another plugin's",
                shown(name),
                self.prefix
            ));
        }
        Ok(())
    }

    /// Checks an index of `table`: its columns, and its name (the plugin's own, or the one
    /// Orbit builds, which must fit).
    fn index(&self, table: &str, index: &IndexDef) -> Result<(), String> {
        index_columns(&index.columns, table)?;
        for column in &index.columns {
            self.key_column(table, column)?;
        }
        match &index.name {
            Some(name) => self.owned(name, "index"),
            None => {
                let suffix = if index.unique { "unique" } else { "index" };
                built_name(&format!("{table}_{}_{suffix}", index.columns.join("_")))
            }
        }
    }

    /// Checks a foreign key of `table`: its column, its name, and what it references (the
    /// plugin's own tables, or a core table's `id` with an action that never blocks the site).
    fn foreign_key(&self, table: &str, key: &ForeignKeyDef) -> Result<(), String> {
        identifier(&key.column, "column")?;
        self.key_column(table, &key.column)?;
        match &key.name {
            Some(name) => self.owned(name, "foreign key")?,
            None => built_name(&format!("{table}_{}_foreign", key.column))?,
        }
        let target = &key.references;
        identifier(&target.column, "column")?;
        if target.table.starts_with(&self.prefix) {
            return self.owned(&target.table, "table");
        }
        let refused = |why: &str| {
            Err(format!(
                "the foreign key on `{}` references `{}.{}`: {why}",
                key.column,
                shown(&target.table),
                shown(&target.column)
            ))
        };
        if !CORE_REFERENCES.contains(&target.table.as_str()) {
            return refused(
                "a plugin's foreign key references its own tables, or the `id` of `posts`, \
                 `users` or `terms`",
            );
        }
        if target.column != "id" {
            return refused("a foreign key to the site's tables references their `id`");
        }
        if !matches!(key.on_delete, Some(ActionDef::Cascade | ActionDef::SetNull)) {
            return refused(
                "a foreign key to the site's tables needs `on_delete` `cascade` or `set_null`, \
                 so the site can always delete its rows",
            );
        }
        if !matches!(key.on_update, None | Some(ActionDef::Cascade)) {
            return refused("a foreign key to the site's tables has no `on_update`, or `cascade`");
        }
        Ok(())
    }
}

/// Refuses more than `max` `what` in one statement on `table`.
fn count(n: usize, max: usize, what: &str, table: &str) -> Result<(), String> {
    if n > max {
        return Err(format!(
            "the statement on `{table}` has {n} {what}; at most {max} are allowed"
        ));
    }
    Ok(())
}

/// Refuses a name (`what`: a table, column, index, ...) that is empty, over
/// [`MAX_IDENTIFIER_BYTES`], or not lowercase ASCII letters, digits and `_` starting with a
/// letter or `_`.
fn identifier(name: &str, what: &str) -> Result<(), String> {
    let plain = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    let starts = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_');
    if !plain || !starts {
        return Err(format!(
            "the {what} name `{}` is not lowercase ASCII letters, digits and `_`, starting with \
             a letter or `_`",
            shown(name)
        ));
    }
    if name.len() > MAX_IDENTIFIER_BYTES {
        return Err(format!(
            "the {what} name `{name}` is {} bytes; at most {MAX_IDENTIFIER_BYTES} are allowed",
            name.len()
        ));
    }
    Ok(())
}

/// Refuses an index or constraint name Orbit would build that is over
/// [`MAX_IDENTIFIER_BYTES`] (PostgreSQL would cut it, MySQL refuse it).
fn built_name(name: &str) -> Result<(), String> {
    if name.len() > MAX_IDENTIFIER_BYTES {
        return Err(format!(
            "the name `{name}` built for an index or foreign key is {} bytes, over \
             {MAX_IDENTIFIER_BYTES}: give it a shorter `name`",
            name.len()
        ));
    }
    Ok(())
}

/// Checks the columns of an index or primary key on `table`.
fn index_columns(columns: &[String], table: &str) -> Result<(), String> {
    if columns.is_empty() {
        return Err(format!("an index or key on `{table}` has no column"));
    }
    count(columns.len(), MAX_INDEX_COLUMNS, "columns in a key", table)?;
    for column in columns {
        identifier(column, "column")?;
    }
    Ok(())
}

/// Checks a column: its name, and the options its type takes.
fn check_column(column: &ColumnDef) -> Result<(), String> {
    let name = &column.name;
    identifier(name, "column")?;
    let ty = column.ty;
    let dated = matches!(ty, ColumnType::DateTime | ColumnType::Timestamp);
    let not_for = |option: &str| {
        Err(format!(
            "the column `{name}`: `{option}` is not for its type"
        ))
    };
    if ty == ColumnType::Id {
        if name != "id" {
            return Err(format!(
                "the column `{name}` has the type `id`, which is the column named `id`"
            ));
        }
        if column.nullable
            || column.default.is_some()
            || column.length.is_some()
            || column.precision.is_some()
            || column.use_current
            || column.primary
        {
            return Err("the `id` column takes no options: it is the primary key".to_owned());
        }
        return Ok(());
    }
    match column.length {
        Some(_) if ty != ColumnType::String => return not_for("length"),
        Some(length) if length == 0 || length > MAX_STRING_LENGTH => {
            return Err(format!(
                "the column `{name}`: `length` is 1 to {MAX_STRING_LENGTH}"
            ));
        }
        _ => {}
    }
    match column.precision {
        Some(_) if !dated => return not_for("precision"),
        Some(precision) if precision > 6 => {
            return Err(format!("the column `{name}`: `precision` is 0 to 6"));
        }
        _ => {}
    }
    if column.use_current && !dated {
        return not_for("use_current");
    }
    if column.use_current && column.default.is_some() {
        return Err(format!(
            "the column `{name}` has both `default` and `use_current`"
        ));
    }
    if let Some(default) = &column.default {
        if matches!(
            ty,
            ColumnType::Text | ColumnType::MediumText | ColumnType::LongText | ColumnType::Binary
        ) {
            return Err(format!(
                "the column `{name}`: a text or binary column takes no `default` (MySQL \
                 refuses one)"
            ));
        }
        default_value(name, default)?;
    }
    Ok(())
}

/// The value of a column's `default`: `null`, a boolean, a number or a string (at most
/// [`MAX_DEFAULT_BYTES`], without control characters).
fn default_value(name: &str, default: &serde_json::Value) -> Result<Value, String> {
    use serde_json::Value as Json;
    Ok(match default {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::Int(i),
            (None, Some(f)) if f.is_finite() => Value::Float(f),
            _ => return Err(format!("the column `{name}`: its default is out of range")),
        },
        Json::String(s) if s.len() > MAX_DEFAULT_BYTES => {
            return Err(format!(
                "the column `{name}`: its default is over {MAX_DEFAULT_BYTES} bytes"
            ));
        }
        Json::String(s) if s.chars().any(char::is_control) => {
            return Err(format!(
                "the column `{name}`: its default has a control character"
            ));
        }
        Json::String(s) if s.contains('\\') => {
            return Err(format!(
                "the column `{name}`: its default has a `\\`, which no default may have"
            ));
        }
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(_) | Json::Object(_) => {
            return Err(format!(
                "the column `{name}`: a default is null, a boolean, a number or a string"
            ));
        }
    })
}

// ----- Building, from checked definitions -----

fn add_column(t: &mut Blueprint, def: &ColumnDef) {
    let name = def.name.as_str();
    let column = match def.ty {
        ColumnType::Id => {
            t.id();
            return;
        }
        ColumnType::String => t.string(name),
        ColumnType::Text => t.text(name),
        ColumnType::MediumText => t.medium_text(name),
        ColumnType::LongText => t.long_text(name),
        ColumnType::Integer => t.integer(name),
        ColumnType::BigInteger => t.big_integer(name),
        ColumnType::Boolean => t.boolean(name),
        ColumnType::DateTime => t.date_time(name),
        ColumnType::Timestamp => t.timestamp(name),
        ColumnType::Binary => t.binary(name),
    };
    if def.nullable {
        column.nullable();
    }
    if let Some(length) = def.length {
        column.length(length);
    }
    if let Some(precision) = def.precision {
        column.precision(precision);
    }
    if def.use_current {
        column.use_current();
    }
    // Checked by `check_column`.
    if let Some(Ok(value)) = def.default.as_ref().map(|d| default_value(name, d)) {
        column.default(value);
    }
    if def.primary {
        column.primary();
    }
}

fn add_index(t: &mut Blueprint, def: &IndexDef) {
    let index = if def.unique {
        t.unique(&def.columns)
    } else {
        t.index(&def.columns)
    };
    if let Some(name) = &def.name {
        index.name(name);
    }
}

fn add_foreign_key(t: &mut Blueprint, def: &ForeignKeyDef) {
    let key = t.foreign(&def.column);
    key.references(&def.references.table, &def.references.column);
    if let Some(action) = def.on_delete {
        key.on_delete(action.orbit());
    }
    if let Some(action) = def.on_update {
        key.on_update(action.orbit());
    }
    if let Some(name) = &def.name {
        key.name(name);
    }
}

// ----- Running -----

/// The migrations the plugin `slug` ran, by name, with their batch: newest batch first, and in
/// a batch the last name first (the order [`reset_migrations`] reverts them in).
///
/// # Errors
///
/// [`MigrationError::Database`] when the history cannot be read.
pub async fn applied_migrations(
    db: &Database,
    slug: &str,
) -> Result<Vec<(String, i64)>, MigrationError> {
    let rows: Vec<(String, i32)> = raw::fetch_all(
        db,
        &format!(
            "SELECT migration, batch FROM {PLUGIN_MIGRATIONS_TABLE} WHERE plugin = ? \
             ORDER BY batch DESC, migration DESC"
        ),
        &[Value::from(slug)],
    )
    .await
    .map_err(|e| MigrationError::Database(e.to_string()))?;
    Ok(rows
        .into_iter()
        .map(|(name, batch)| (name, i64::from(batch)))
        .collect())
}

/// The tables of the database whose names start with `prefix` (a plugin's
/// [`Manifest::table_prefix`]), sorted: what is left of a plugin's data. It reads the catalog
/// of the connected database (the current schema on PostgreSQL), the one place where the query
/// differs by database, as Orbit has no call that lists tables.
///
/// # Errors
///
/// [`MigrationError::Database`] when the catalog cannot be read.
pub async fn plugin_tables(db: &Database, prefix: &str) -> Result<Vec<String>, MigrationError> {
    let sql = match db.driver() {
        Driver::MySql => {
            "SELECT CAST(table_name AS CHAR) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE'"
        }
        Driver::Postgres => {
            "SELECT CAST(table_name AS TEXT) FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'"
        }
        Driver::Sqlite => "SELECT name FROM sqlite_master WHERE type = 'table'",
    };
    let rows: Vec<(String,)> = raw::fetch_all(db, sql, &[])
        .await
        .map_err(|e| MigrationError::Database(e.to_string()))?;
    let mut tables: Vec<String> = rows
        .into_iter()
        .map(|(name,)| name)
        .filter(|name| name.starts_with(prefix))
        .collect();
    tables.sort();
    Ok(tables)
}

/// Runs the migrations of the plugin `slug` that it has not run, in order, as one new batch,
/// each in its own transaction with its row in [`PLUGIN_MIGRATIONS_TABLE`]. Returns the names
/// it ran; none when every one has run.
///
/// # Errors
///
/// [`MigrationError::Database`] when the history cannot be read; [`MigrationError::Failed`]
/// when a migration fails: the ones before it stay applied, in this batch.
pub async fn run_pending_migrations(
    db: &Database,
    slug: &str,
    migrations: &[PluginMigration],
) -> Result<Vec<String>, MigrationError> {
    let applied: HashSet<String> = applied_migrations(db, slug)
        .await?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let pending: Vec<&PluginMigration> = migrations
        .iter()
        .filter(|m| !applied.contains(&m.name))
        .collect();
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let last: Option<i32> = raw::fetch_scalar(
        db,
        &format!("SELECT MAX(batch) FROM {PLUGIN_MIGRATIONS_TABLE}"),
        &[],
    )
    .await
    .map_err(|e| MigrationError::Database(e.to_string()))?;
    let batch = i64::from(last.unwrap_or(0)) + 1;
    let mut ran = Vec::with_capacity(pending.len());
    for migration in pending {
        let applied_at = Utc::now().naive_utc();
        db.transaction(async |tx| {
            for sql in &migration.up {
                raw::execute(&mut *tx, sql, &[]).await?;
            }
            raw::execute(
                &mut *tx,
                &format!(
                    "INSERT INTO {PLUGIN_MIGRATIONS_TABLE} (plugin, migration, batch, applied_at) \
                     VALUES (?, ?, ?, ?)"
                ),
                &[
                    Value::from(slug),
                    Value::from(migration.name.as_str()),
                    Value::Int(batch),
                    Value::DateTime(applied_at),
                ],
            )
            .await?;
            Ok::<(), orbit_orm::Error>(())
        })
        .await
        .map_err(|e| MigrationError::Failed {
            migration: migration.name.clone(),
            message: e.to_string(),
        })?;
        ran.push(migration.name.clone());
    }
    Ok(ran)
}

/// Reverts every migration the plugin `slug` ran, newest batch first and in a batch the last
/// name first (Laravel's `migrate:reset`), each `down` in its own transaction with the removal
/// of its row. Returns the names it reverted, in that order.
///
/// # Errors
///
/// [`MigrationError::Unknown`], before anything is reverted, when the history holds a
/// migration `migrations` does not have; [`MigrationError::Database`] when the history cannot
/// be read; [`MigrationError::RollbackFailed`] when a `down` fails: the ones before it stay
/// reverted, the rest applied.
pub async fn reset_migrations(
    db: &Database,
    slug: &str,
    migrations: &[PluginMigration],
) -> Result<Vec<String>, MigrationError> {
    let applied = applied_migrations(db, slug).await?;
    let unknown: Vec<String> = applied
        .iter()
        .filter(|(name, _)| !migrations.iter().any(|m| &m.name == name))
        .map(|(name, _)| name.clone())
        .collect();
    if !unknown.is_empty() {
        return Err(MigrationError::Unknown(unknown));
    }
    let mut reverted = Vec::with_capacity(applied.len());
    for (name, _) in applied {
        let Some(migration) = migrations.iter().find(|m| m.name == name) else {
            continue;
        };
        db.transaction(async |tx| {
            for sql in &migration.down {
                raw::execute(&mut *tx, sql, &[]).await?;
            }
            raw::execute(
                &mut *tx,
                &format!(
                    "DELETE FROM {PLUGIN_MIGRATIONS_TABLE} WHERE plugin = ? AND migration = ?"
                ),
                &[Value::from(slug), Value::from(name.as_str())],
            )
            .await?;
            Ok::<(), orbit_orm::Error>(())
        })
        .await
        .map_err(|e| MigrationError::RollbackFailed {
            migration: name.clone(),
            message: e.to_string(),
        })?;
        reverted.push(name);
    }
    Ok(reverted)
}
