//! The core migrations: one Rust module per migration, named `m<timestamp>_<name>`, each
//! building its `up` and `down` with Orbit's database-neutral schema builder, as Laravel's
//! migration classes do. One migration runs on SQLite, MySQL, MariaDB and PostgreSQL.
//!
//! Ignite owns the `users` table and runs its own migrations; they are not listed here.
//!
//! ```ignore
//! let applied = mission_control_core::migrations::migrator()?.run(&db).await?;
//! ```

use orbit_orm::{Error, Migrator};

mod m20261114000000_create_options_table;
mod m20261114000100_create_posts_table;
mod m20261114000200_create_scheduled_events_table;
mod m20261114000300_create_terms_table;
mod m20261114000400_create_term_taxonomy_table;
mod m20261114000500_create_term_relationships_table;

/// Every core migration, in order. `run` applies the pending ones as one batch, and `rollback`
/// reverts the last batch.
///
/// # Errors
///
/// Fails when two migrations share a timestamp or a name lacks one, which a test catches.
pub fn migrator() -> Result<Migrator, Error> {
    Migrator::new(vec![
        m20261114000000_create_options_table::migration(),
        m20261114000100_create_posts_table::migration(),
        m20261114000200_create_scheduled_events_table::migration(),
        m20261114000300_create_terms_table::migration(),
        m20261114000400_create_term_taxonomy_table::migration(),
        m20261114000500_create_term_relationships_table::migration(),
    ])
}
