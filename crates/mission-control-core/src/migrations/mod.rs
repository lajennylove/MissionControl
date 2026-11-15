//! The core migrations: one Rust module per migration, named `m<timestamp>_<name>`, each
//! building its `up` and `down` with Orbit's database-neutral schema builder, as Laravel's
//! migration classes do. One migration runs on SQLite, MySQL, MariaDB and PostgreSQL.
//!
//! Ignite owns the `users` table and runs its own migrations; they are not listed here.
//!
//! Both sets record their runs in the same table, `__diesel_schema_migrations`, by the 14-digit
//! version at the start of the name only, so a core version must never equal one of Ignite's: the
//! migrator that ran second would take the other's row for its own and skip its migration. Ignite
//! 0.4.0 has one version a day at midnight (`20261114000000_account_lifecycle`,
//! `20261115000000_password_reset_tokens`), so the core versions are on the hour 12
//! (`20261114120000`, `20261115120000`, ...). The installer refuses to run when two versions match, and a test in
//! `mission-control-app` checks the two sets against each other.
//!
//! ```ignore
//! let applied = mission_control_core::migrations::migrator()?.run(&db).await?;
//! ```

use orbit_orm::{Error, Migrator};

mod m20261114120000_create_options_table;
mod m20261114120100_create_posts_table;
mod m20261114120200_create_scheduled_events_table;
mod m20261114120300_create_terms_table;
mod m20261114120400_create_term_taxonomy_table;
mod m20261114120500_create_term_relationships_table;
mod m20261114120600_create_post_meta_table;
mod m20261114120700_create_user_meta_table;
mod m20261114120800_create_term_meta_table;
mod m20261114120900_create_application_passwords_table;
mod m20261115120000_create_post_types_table;
mod m20261115120100_create_taxonomies_table;
mod m20261115120200_create_field_groups_table;
mod m20261115120300_create_field_group_locations_table;

/// Every core migration, in order. `run` applies the pending ones as one batch, and `rollback`
/// reverts the last batch.
///
/// # Errors
///
/// Fails when two migrations share a timestamp or a name lacks one, which a test catches.
pub fn migrator() -> Result<Migrator, Error> {
    Migrator::new(vec![
        m20261114120000_create_options_table::migration(),
        m20261114120100_create_posts_table::migration(),
        m20261114120200_create_scheduled_events_table::migration(),
        m20261114120300_create_terms_table::migration(),
        m20261114120400_create_term_taxonomy_table::migration(),
        m20261114120500_create_term_relationships_table::migration(),
        m20261114120600_create_post_meta_table::migration(),
        m20261114120700_create_user_meta_table::migration(),
        m20261114120800_create_term_meta_table::migration(),
        m20261114120900_create_application_passwords_table::migration(),
        m20261115120000_create_post_types_table::migration(),
        m20261115120100_create_taxonomies_table::migration(),
        m20261115120200_create_field_groups_table::migration(),
        m20261115120300_create_field_group_locations_table::migration(),
    ])
}
