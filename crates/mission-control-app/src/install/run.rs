//! The installer's migrations (Task 11.4, Ruling R78): Ignite's panel migrations, then the core
//! migrations, on the database at one `DATABASE_URL`, and the rollback of the last run.
//!
//! Ignite 0.6.0 and Mission Control are both built on Orbit 0.4. [`run_migrations`] opens a
//! connection from the URL, runs the panel's migrations on it and closes it, then opens another
//! and runs the core migrations. The panel goes first because the core tables reference its
//! `users` table.
//!
//! Both migrators record their runs in the same table, `__diesel_schema_migrations`, in the same
//! format, and match a migration by the 14-digit version at the start of its name. Before
//! anything runs, the versions of the two sets are compared, and the run is refused when one is
//! in both: the second migrator would take the first one's row for its own and skip its
//! migration. Running again applies nothing, since both migrators skip what they have recorded.
//!
//! One run is one batch, as one `php artisan migrate` is in Laravel (Ruling R92). Each migrator
//! numbers its own batch, so after the core migrations ran, the rows they wrote are moved into
//! the panel's batch with one `UPDATE` of the `batch` column. A run that applies only core
//! migrations (a new one after an upgrade) gets the next batch on its own.
//!
//! [`rollback_last_batch`] reverses the last batch, as `migrate:rollback` does: the core
//! migrations of the batch in reverse order, then the panel's in reverse order (the core tables
//! reference the panel's `users`). Each migrator only reverts its own last batch, and refuses one
//! that holds a version it does not know, so for a batch that holds both sets the core rows are
//! first moved one batch up: the core migrator reverts them as its last batch, and then Ignite's
//! `rollback` reverts what is left, its own migrations.
//!
//! Laravel reverts a batch by migration name, newest first. Core first, then panel, is the same
//! order as long as every core version is newer than every panel version in the batch, which
//! holds today (the core versions start at 20261114120000); a later panel migration in the same
//! run as older core ones would be reverted after them.
//!
//! Each migrator locks only its own `run` or `rollback`, not the steps between them, so run one
//! installer or rollback at a time: a second process migrating in between could have its rows
//! moved into, or reverted with, this one's batch.
//!
//! This lives in the app crate, not in `mission-control-core`, so the core crate does not depend
//! on `ignite-panel` (and on Rocket through it). The app is where the panel and the core meet.
//!
//! ```ignore
//! let summary = mission_control_app::install::run::run_migrations(&database_url).await?;
//! let reverted = mission_control_app::install::run::rollback_last_batch(&database_url).await?;
//! ```

use std::collections::BTreeSet;
use std::fmt;

use mission_control_core::migrations::migrator;
use orbit_orm::{Database, Value, raw};

/// The table both migrators record the applied migrations in.
const MIGRATIONS_TABLE: &str = "__diesel_schema_migrations";

/// The migrations one [`run_migrations`] applied, by name, in the order they ran; or the ones
/// one [`rollback_last_batch`] reverted, in the order it reverted them (last first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationSummary {
    /// The panel's migrations, such as `20261108000000_create_users`.
    pub panel: Vec<String>,
    /// The core migrations, such as `20261114120000_create_options_table`.
    pub core: Vec<String>,
}

impl MigrationSummary {
    /// Whether the run applied nothing (the database was migrated already), or the rollback
    /// reverted nothing (no migration was applied).
    pub fn is_empty(&self) -> bool {
        self.panel.is_empty() && self.core.is_empty()
    }
}

/// Why the installer's migrations did not run, or stopped. The messages name the database
/// server's complaint, never the password or the database URL.
///
/// Ignite's errors are kept as their message.
#[derive(Debug)]
pub enum InstallError {
    /// Ignite could not connect to the database.
    PanelConnect(String),
    /// Ignite could not read its migration history, or one of its migrations failed. The
    /// migrations before it stay applied.
    PanelMigrations(String),
    /// The core migrations are not valid (two share a version, or one has none).
    CoreInvalid(orbit_orm::Error),
    /// A version is in both sets, so one of the migrations would never run. Nothing was run.
    VersionCollision(Vec<String>),
    /// Mission Control could not connect to the database.
    CoreConnect(orbit_orm::Error),
    /// A core migration failed. The panel's migrations, and the core ones before it, stay
    /// applied, in one batch.
    CoreMigrations(orbit_orm::Error),
    /// Mission Control could not read or renumber the batches in the migrations table. After a
    /// run, the migrations it applied stay applied, but the core ones may be in a batch of their
    /// own, so a rollback reverts them apart from the panel's.
    History(orbit_orm::Error),
    /// The last batch holds versions that neither the panel nor the core migrations have, so
    /// nothing was reverted.
    UnknownMigrations(Vec<String>),
    /// A core migration's `down` failed. The ones reverted before it stay reverted; the rest of
    /// the batch, the panel's migrations with it, stays applied.
    CoreRollback(orbit_orm::Error),
    /// One of the panel's `down` migrations failed, through Ignite's `rollback`. The core
    /// migrations of the batch, and the panel ones reverted before it, stay reverted.
    PanelRollback(String),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PanelConnect(e) => write!(f, "Ignite could not connect to the database: {e}"),
            Self::PanelMigrations(e) => write!(f, "Ignite's migrations did not run: {e}"),
            Self::CoreInvalid(e) => write!(f, "The core migrations are not valid: {e}"),
            Self::VersionCollision(versions) => write!(
                f,
                "Ignite and the core migrations both have the version(s) {}, so nothing was run",
                versions.join(", ")
            ),
            Self::CoreConnect(e) => {
                write!(f, "Mission Control could not connect to the database: {e}")
            }
            Self::CoreMigrations(e) => write!(f, "The core migrations did not run: {e}"),
            Self::History(e) => write!(f, "The migration history could not be read: {e}"),
            Self::UnknownMigrations(versions) => write!(
                f,
                "The last batch holds the version(s) {}, which neither Ignite nor the core \
                 migrations have, so nothing was rolled back",
                versions.join(", ")
            ),
            Self::CoreRollback(e) => write!(f, "The core migrations did not roll back: {e}"),
            Self::PanelRollback(e) => write!(f, "Ignite's migrations did not roll back: {e}"),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CoreInvalid(e)
            | Self::CoreConnect(e)
            | Self::CoreMigrations(e)
            | Self::History(e)
            | Self::CoreRollback(e) => Some(e),
            _ => None,
        }
    }
}

/// Runs Ignite's panel migrations, then the core migrations, on the database at
/// `database_url`, as one batch, and returns what each applied. Running it again applies nothing
/// and records nothing.
///
/// Each set runs on its own connection, opened here and closed before this returns.
///
/// # Errors
///
/// Fails, before anything runs, when the core migrations are not valid, Ignite cannot connect,
/// or the two sets share a version ([`InstallError::VersionCollision`]). Fails when a migration
/// fails, or when Mission Control cannot connect after the panel's migrations ran; what ran
/// before stays applied, in the run's batch, as Orbit's migrator leaves it, and a second run
/// picks up from there.
pub async fn run_migrations(database_url: &str) -> Result<MigrationSummary, InstallError> {
    let core = migrator().map_err(InstallError::CoreInvalid)?;

    let ignite = Database::connect(database_url)
        .await
        .map_err(|e| InstallError::PanelConnect(e.to_string()))?;
    let panel = run_panel(&ignite, &core).await;
    ignite.close().await;
    let panel = panel?;

    let db = Database::connect(database_url)
        .await
        .map_err(InstallError::CoreConnect)?;
    let result = run_core(&db, &core, &panel).await;
    db.close().await;
    Ok(MigrationSummary {
        panel,
        core: result?,
    })
}

/// Runs the core migrations, then moves the rows they wrote into the batch of the panel's
/// migrations this run applied, if it applied any. The move is made when a core migration
/// fails too, so what the run applied stays one batch.
async fn run_core(
    db: &Database,
    core: &orbit_orm::Migrator,
    panel: &[String],
) -> Result<Vec<String>, InstallError> {
    // The panel's batch, read before the core migrator adds its own.
    let shared = match panel.first() {
        Some(name) => batch_of(db, version(name)).await?,
        None => None,
    };
    let applied = core.run(db).await;
    if let Some(batch) = shared {
        let versions: Vec<&str> = core.migrations().iter().map(|m| version(&m.name)).collect();
        // Only this run's rows are above the panel's batch: the core migrations recorded
        // before it are in a lower one.
        let (filter, mut binds) = version_filter(&versions);
        binds.insert(0, Value::Int(batch));
        binds.insert(1, Value::Int(batch));
        let moved = raw::execute(
            db,
            &format!("UPDATE {MIGRATIONS_TABLE} SET batch = ? WHERE batch > ? AND {filter}"),
            &binds,
        )
        .await;
        match (&applied, moved) {
            (_, Ok(_)) => {}
            // The migration's own error is the one to report; the split batch is logged.
            (Err(_), Err(e)) => {
                log::warn!("The core migrations' rows could not join the run's batch: {e}");
            }
            (Ok(_), Err(e)) => return Err(InstallError::History(e)),
        }
    }
    applied.map_err(InstallError::CoreMigrations)
}

/// Reverses the last batch of migrations on the database at `database_url`, as Laravel's
/// `migrate:rollback` does, and returns what it reverted, last first: the batch's core
/// migrations in reverse order (their `down`), then its panel migrations in reverse order,
/// through Ignite's own `rollback`. Their rows are removed from the migrations table, so a
/// later [`run_migrations`] applies them again. Returns nothing when no migration is applied.
///
/// # Errors
///
/// Fails, before anything is reverted, when the core migrations are not valid, a connection
/// cannot be opened, or the last batch holds a version neither set has
/// ([`InstallError::UnknownMigrations`]). Fails when a `down` fails: what was reverted before it
/// stays reverted, and the rest stays applied, in the same batch, so a second call picks up
/// from there.
pub async fn rollback_last_batch(database_url: &str) -> Result<MigrationSummary, InstallError> {
    let core = migrator().map_err(InstallError::CoreInvalid)?;

    let ignite = Database::connect(database_url)
        .await
        .map_err(|e| InstallError::PanelConnect(e.to_string()))?;
    let panel = ignite_panel::migration_status(&ignite).await;
    ignite.close().await;
    let panel: Vec<String> = panel
        .map_err(|e| InstallError::PanelMigrations(e.to_string()))?
        .into_iter()
        .map(|(name, _)| version(&name).to_owned())
        .collect();

    let db = Database::connect(database_url)
        .await
        .map_err(InstallError::CoreConnect)?;
    let reverted = rollback_core(&db, &core, &panel).await;
    db.close().await;
    let (core_reverted, panel_left) = reverted?;
    if !panel_left {
        return Ok(MigrationSummary {
            panel: Vec::new(),
            core: core_reverted,
        });
    }

    // The core migrations of the batch are reverted, so the last batch now holds only the
    // panel's: Ignite's own rollback reverts it.
    let ignite = Database::connect(database_url)
        .await
        .map_err(|e| InstallError::PanelConnect(e.to_string()))?;
    let panel_reverted = ignite_panel::rollback(&ignite).await;
    ignite.close().await;
    Ok(MigrationSummary {
        panel: panel_reverted.map_err(|e| InstallError::PanelRollback(e.to_string()))?,
        core: core_reverted,
    })
}

/// Reverts the core migrations of the last batch and returns their names, and whether the
/// batch also holds panel migrations, left for Ignite to revert. `panel` is the panel's
/// versions.
async fn rollback_core(
    db: &Database,
    core: &orbit_orm::Migrator,
    panel: &[String],
) -> Result<(Vec<String>, bool), InstallError> {
    let Some(last) = last_batch(db, core).await? else {
        return Ok((Vec::new(), false));
    };
    let versions: Vec<(String,)> = raw::fetch_all(
        db,
        &format!("SELECT version FROM {MIGRATIONS_TABLE} WHERE batch = ?"),
        &[Value::Int(last)],
    )
    .await
    .map_err(InstallError::History)?;
    let core_versions: BTreeSet<&str> =
        core.migrations().iter().map(|m| version(&m.name)).collect();
    let (mut in_core, mut in_panel, mut unknown) = (Vec::new(), false, Vec::new());
    for (v,) in &versions {
        if core_versions.contains(v.as_str()) {
            in_core.push(v.as_str());
        } else if panel.iter().any(|p| p == v) {
            in_panel = true;
        } else {
            unknown.push(v.clone());
        }
    }
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(InstallError::UnknownMigrations(unknown));
    }
    if in_core.is_empty() {
        return Ok((Vec::new(), in_panel));
    }
    if !in_panel {
        let reverted = core
            .rollback(db)
            .await
            .map_err(InstallError::CoreRollback)?;
        return Ok((reverted, false));
    }

    // A batch of both sets: the core rows go one batch up, so the core migrator's rollback,
    // which reverts the last batch, reverts them alone.
    let (filter, mut binds) = version_filter(&in_core);
    binds.insert(0, Value::Int(last + 1));
    binds.insert(1, Value::Int(last));
    raw::execute(
        db,
        &format!("UPDATE {MIGRATIONS_TABLE} SET batch = ? WHERE batch = ? AND {filter}"),
        &binds,
    )
    .await
    .map_err(InstallError::History)?;
    match core.rollback(db).await {
        Ok(reverted) => Ok((reverted, true)),
        Err(e) => {
            // The core rows not yet reverted go back into the batch, so it stays one. When this
            // fails too, they stay one batch up and the next rollback reverts them first.
            if let Err(restore) = raw::execute(
                db,
                &format!("UPDATE {MIGRATIONS_TABLE} SET batch = ? WHERE batch = ?"),
                &[Value::Int(last), Value::Int(last + 1)],
            )
            .await
            {
                log::warn!("The core migrations' batch could not be restored: {restore}");
            }
            Err(InstallError::CoreRollback(e))
        }
    }
}

/// The highest batch in the migrations table, or `None` when no migration is recorded. The
/// table is created first when it does not exist, as Orbit's own `rollback` does.
async fn last_batch(
    db: &Database,
    core: &orbit_orm::Migrator,
) -> Result<Option<i64>, InstallError> {
    core.status(db).await.map_err(InstallError::History)?;
    let last: Option<(Option<i32>,)> = raw::fetch_optional(
        db,
        &format!("SELECT MAX(batch) FROM {MIGRATIONS_TABLE}"),
        &[],
    )
    .await
    .map_err(InstallError::History)?;
    Ok(last.and_then(|(batch,)| batch).map(i64::from))
}

/// The batch `version` was recorded in, or `None` when it is not recorded.
async fn batch_of(db: &Database, version: &str) -> Result<Option<i64>, InstallError> {
    let batch: Option<(i32,)> = raw::fetch_optional(
        db,
        &format!("SELECT batch FROM {MIGRATIONS_TABLE} WHERE version = ?"),
        &[Value::Text(version.to_owned())],
    )
    .await
    .map_err(InstallError::History)?;
    Ok(batch.map(|(b,)| i64::from(b)))
}

/// `version IN (?, ...)` for `versions`, and its binds. `versions` is never empty here.
fn version_filter(versions: &[&str]) -> (String, Vec<Value>) {
    let marks = vec!["?"; versions.len()].join(", ");
    let binds = versions
        .iter()
        .map(|v| Value::Text((*v).to_owned()))
        .collect();
    (format!("version IN ({marks})"), binds)
}

/// The 14-digit version at the start of a migration's name, which is what Orbit records.
fn version(name: &str) -> &str {
    name.get(..14).unwrap_or(name)
}

/// Checks the two sets' versions against each other, then runs the panel's migrations on
/// Ignite's connection.
async fn run_panel(
    ignite: &Database,
    core: &orbit_orm::Migrator,
) -> Result<Vec<String>, InstallError> {
    // Reads only: on a new database it lists every panel migration as pending.
    let panel: Vec<String> = ignite_panel::migration_status(ignite)
        .await
        .map_err(|e| InstallError::PanelMigrations(e.to_string()))?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let collisions = shared_versions(
        panel.iter().map(String::as_str),
        core.migrations().iter().map(|m| m.name.as_str()),
    );
    if !collisions.is_empty() {
        return Err(InstallError::VersionCollision(collisions));
    }
    ignite_panel::migrate(ignite)
        .await
        .map_err(|e| InstallError::PanelMigrations(e.to_string()))
}

/// The 14-digit versions that are at the start of a name in both `a` and `b`, sorted. Orbit
/// records only that version, so two migrations that share one are the same to it.
fn shared_versions<'a>(
    a: impl IntoIterator<Item = &'a str>,
    b: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let a: BTreeSet<&str> = a.into_iter().map(version).collect();
    b.into_iter()
        .map(version)
        .filter(|v| a.contains(v))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_versions_compare_the_timestamps_only() {
        assert_eq!(
            shared_versions(
                [
                    "20261114000000_account_lifecycle",
                    "20261108000000_create_users"
                ],
                [
                    "20261114000000_create_options_table",
                    "20261114120000_create_posts_table"
                ],
            ),
            vec!["20261114000000".to_owned()]
        );
        assert!(
            shared_versions(
                ["20261108000000_create_users"],
                ["20261114120000_create_options_table"]
            )
            .is_empty()
        );
    }

    #[test]
    fn a_collision_names_the_versions() {
        let e = InstallError::VersionCollision(vec!["20261114000000".to_owned()]);
        assert_eq!(
            e.to_string(),
            "Ignite and the core migrations both have the version(s) 20261114000000, so \
             nothing was run"
        );
    }
}
