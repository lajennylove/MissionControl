//! The installer's migrations (Task 11.4, Ruling R78): Ignite's panel migrations, then the core
//! migrations, on the database at one `DATABASE_URL`.
//!
//! Ignite 0.4.0 is built on Orbit 0.3 and Mission Control on Orbit 0.4, so no Orbit type passes
//! between them. [`run_migrations`] opens Ignite's own connection (Orbit 0.3) from the URL, runs
//! the panel's migrations on it and closes it, then opens Mission Control's connection (Orbit
//! 0.4) and runs the core migrations. The panel goes first because the core tables reference its
//! `users` table.
//!
//! Both migrators record their runs in the same table, `__diesel_schema_migrations`, in the same
//! format, and match a migration by the 14-digit version at the start of its name. Before
//! anything runs, the versions of the two sets are compared, and the run is refused when one is
//! in both: the second migrator would take the first one's row for its own and skip its
//! migration. Running again applies nothing, since both migrators skip what they have recorded.
//!
//! Each set is one batch, the panel's first, as two `migrate` runs would record them. Laravel
//! runs a package's migrations in the same batch as the app's, because they are published into
//! the app; here they are two migrators. To roll back, the core batch goes first: Ignite's
//! `rollback` refuses a last batch that holds versions it does not know.
//!
//! This lives in the app crate, not in `mission-control-core`, so the core crate does not depend
//! on Ignite (and on Rocket through it). The app is where Ignite and the core meet.
//!
//! ```ignore
//! let summary = mission_control_app::install::run::run_migrations(&database_url).await?;
//! ```

use std::collections::BTreeSet;
use std::fmt;

use mission_control_core::migrations::migrator;
use orbit_orm::Database;

/// The migrations one [`run_migrations`] applied, by name, in the order they ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationSummary {
    /// The panel's migrations, such as `20261108000000_create_users`.
    pub panel: Vec<String>,
    /// The core migrations, such as `20261114120000_create_options_table`.
    pub core: Vec<String>,
}

impl MigrationSummary {
    /// Whether the run applied nothing: the database was migrated already.
    pub fn is_empty(&self) -> bool {
        self.panel.is_empty() && self.core.is_empty()
    }
}

/// Why the installer's migrations did not run, or stopped. The messages name the database
/// server's complaint, never the password or the database URL.
///
/// Ignite's errors are Orbit 0.3 types, so they are kept as their message.
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
    /// applied.
    CoreMigrations(orbit_orm::Error),
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
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CoreInvalid(e) | Self::CoreConnect(e) | Self::CoreMigrations(e) => Some(e),
            _ => None,
        }
    }
}

/// Runs Ignite's panel migrations, then the core migrations, on the database at
/// `database_url`, and returns what each applied. Running it again applies nothing.
///
/// Each set runs on its own connection, opened here and closed before this returns.
///
/// # Errors
///
/// Fails, before anything runs, when the core migrations are not valid, Ignite cannot connect,
/// or the two sets share a version ([`InstallError::VersionCollision`]). Fails when a migration
/// fails, or when Mission Control cannot connect after the panel's migrations ran; what ran
/// before stays applied, as Orbit's migrator leaves it, and a second run picks up from there.
pub async fn run_migrations(database_url: &str) -> Result<MigrationSummary, InstallError> {
    let core = migrator().map_err(InstallError::CoreInvalid)?;

    let ignite = orbit_orm_ignite::Database::connect(database_url)
        .await
        .map_err(|e| InstallError::PanelConnect(e.to_string()))?;
    let panel = run_panel(&ignite, &core).await;
    ignite.close().await;
    let panel = panel?;

    let db = Database::connect(database_url)
        .await
        .map_err(InstallError::CoreConnect)?;
    let applied = core.run(&db).await;
    db.close().await;
    Ok(MigrationSummary {
        panel,
        core: applied.map_err(InstallError::CoreMigrations)?,
    })
}

/// Checks the two sets' versions against each other, then runs the panel's migrations on
/// Ignite's connection.
async fn run_panel(
    ignite: &orbit_orm_ignite::Database,
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
    let version = |name: &'a str| name.get(..14).unwrap_or(name);
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
