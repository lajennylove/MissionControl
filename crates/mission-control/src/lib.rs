//! The Mission Control library entry point.
//!
//! Builds the Rocket application. The prebuilt binary and a site's own binary
//! both call it, so they run the same app.
//!
//! ```ignore
//! #[rocket::launch]
//! fn launch() -> _ {
//!     let config = mission_control_core::config::load().expect("reads the configuration");
//!     mission_control::rocket(config)
//! }
//! ```
//!
//! At ignition the app opens the database (`DATABASE_URL`), loads the active theme from the
//! themes folder (`MC_THEMES_DIR`, default `themes`; the theme is the `stylesheet` option, or
//! `default`), mounts the public site and serves each theme's built assets at
//! `/themes/{slug}/build/`. The database must already be installed: the app does not run
//! migrations. Switching the theme takes effect at the next start.
//!
//! The app manages, for code that extends it: the database (`Arc<Database>`), the hook registry
//! (`Arc<Hooks>`), the shortcode registry (`Arc<Shortcodes>`, run on `the_content`), the
//! scheduler (`Arc<Scheduler>`), the REST route registry (`Arc<RestRoutes>`) and the front end
//! (`Arc<Frontend>`). The scheduler's loop starts
//! at liftoff, once the server listens, and runs due events every minute.
//!
//! The REST API serves discovery at `/wp-json/` and `/?rest_route=/`. REST clients sign in with
//! application passwords; routes take the [`RestAuth`] or [`RestUser`] guard for the current
//! user, whose roles come from the [`UserRoles`] provider given in [`AppOptions`] (none: no
//! role). Not mounted yet: the admin (Ignite).

mod front;
mod rest;

pub use rest::auth::{RestAuth, RestUser};

use std::sync::Arc;
use std::time::Duration;

use mission_control_core::config::Config;
use mission_control_core::content::{Shortcodes, register_publish_future_post};
use mission_control_core::front::{Frontend, load_active_theme};
use mission_control_core::hooks::Hooks;
use mission_control_core::rest::RestRoutes;
use mission_control_core::scheduler::Scheduler;
use mission_control_core::users::{NoUserRoles, UserRoles};
use orbit_orm::Database;
use rocket::fairing::AdHoc;
use rocket::fs::{FileServer, Options};
use rocket::{Build, Rocket};

/// How often the scheduler looks for due events, as Laravel's scheduler and WP-Cron's minimum.
const SCHEDULER_INTERVAL: Duration = Duration::from_secs(60);

/// The rank of the public site's catch-all route. Higher than [`ASSET_RANK`], so a theme's
/// built file is served before the site looks the path up.
const FRONT_RANK: isize = 20;

/// The rank of the REST routes that answer a family of paths (`/wp-json/<path..>`, the
/// `rest_route` query). Below [`FRONT_RANK`], above every route with a path of its own.
const REST_RANK: isize = 5;

/// The rank of a theme's built assets. A missing file forwards to the public site, which
/// answers with the theme's 404.
const ASSET_RANK: isize = 10;

/// What a site's own binary, or a test, can give the app besides its configuration.
#[derive(Default, Clone)]
pub struct AppOptions {
    /// A database the caller has opened already, such as a test database. `None` opens the one
    /// at `config.database_url`.
    pub database: Option<Arc<Database>>,
    /// Where signed-in users' roles come from. `None` gives every user no role
    /// ([`NoUserRoles`]), so a signed-in user can do nothing a visitor cannot, until Ignite's
    /// roles provide them (Task 8.2b).
    pub user_roles: Option<Arc<dyn UserRoles>>,
}

/// The Mission Control app for `config`. The database at `config.database_url` is opened at
/// ignition; ignition fails when it cannot be reached or no theme can be loaded.
pub fn rocket(config: Config) -> Rocket<Build> {
    rocket_with_options(config, AppOptions::default())
}

/// The Mission Control app on a database the caller has opened already, such as a test
/// database. `config.database_url` is not read.
pub fn rocket_with_database(config: Config, db: Arc<Database>) -> Rocket<Build> {
    rocket_with_options(
        config,
        AppOptions {
            database: Some(db),
            ..AppOptions::default()
        },
    )
}

/// The Mission Control app for `config` with `options`: a database opened already, a provider
/// of users' roles.
pub fn rocket_with_options(config: Config, options: AppOptions) -> Rocket<Build> {
    let AppOptions {
        database: db,
        user_roles,
    } = options;
    // Design 5.7: theme templates are capped in size and render work. Propel's defaults: a
    // template of at most 1 MiB, and a budget of 100,000 loop iterations and template entries
    // per render. Set once for the process, before any theme renders.
    propel_runtime::set_limits(propel_runtime::Limits::default());

    let hooks = Arc::new(Hooks::new());
    let shortcodes = Arc::new(Shortcodes::new());
    shortcodes.register_the_content(&hooks);
    let rest_routes = Arc::new(RestRoutes::new());
    let auth = rest::auth::AuthSettings {
        environment: config.environment,
        trusted_proxies: config.trusted_proxies.clone(),
        user_roles: user_roles.unwrap_or_else(|| Arc::new(NoUserRoles)),
    };

    rocket::build()
        .manage(Arc::clone(&hooks))
        .manage(shortcodes)
        .manage(rest_routes)
        .manage(auth)
        .mount("/", rest::routes(REST_RANK))
        .mount("/", rest::auth::refused_routes())
        .register("/", rest::auth::catchers())
        .attach(rest::auth::RestAuthFairing)
        .attach(AdHoc::try_on_ignite("Mission Control", move |rocket| {
            ignite(rocket, config, db, hooks)
        }))
        .attach(AdHoc::on_liftoff("Mission Control scheduler", |rocket| {
            Box::pin(async move {
                if let Some(scheduler) = rocket.state::<Arc<Scheduler>>() {
                    let scheduler = Arc::clone(scheduler);
                    rocket::tokio::spawn(async move { scheduler.run(SCHEDULER_INTERVAL).await });
                }
            })
        }))
}

/// Opens the database, loads the active theme and mounts the public site. The result is what
/// `AdHoc::try_on_ignite` takes: the app to go on with, or back on failure.
#[allow(clippy::result_large_err)]
async fn ignite(
    rocket: Rocket<Build>,
    config: Config,
    db: Option<Arc<Database>>,
    hooks: Arc<Hooks>,
) -> Result<Rocket<Build>, Rocket<Build>> {
    let db = match db {
        Some(db) => db,
        None => match Database::connect(&config.database_url).await {
            Ok(db) => Arc::new(db),
            Err(e) => {
                // Orbit's error names the driver at most, never the URL, which holds the
                // password.
                log::error!("Could not open the database in DATABASE_URL: {e}");
                return Err(rocket);
            }
        },
    };
    let theme = match load_active_theme(&db, &config.themes_dir).await {
        Ok(theme) => theme,
        Err(e) => {
            log::error!(
                "No theme could be loaded from `{}`: {e}",
                config.themes_dir.display()
            );
            return Err(rocket);
        }
    };

    let scheduler = Arc::new(Scheduler::new(Arc::clone(&db), Arc::clone(&hooks)));
    register_publish_future_post(&scheduler);

    let frontend = Frontend::new(Arc::clone(&db), hooks, theme);
    let mut rocket = rocket.mount("/", front::routes(FRONT_RANK));
    for (prefix, dir) in frontend.asset_dirs() {
        // `Missing`: a theme that has not been built has no folder yet. Dot files and index
        // pages are not served.
        rocket = rocket.mount(
            prefix,
            FileServer::new(dir, Options::Missing).rank(ASSET_RANK),
        );
    }
    Ok(rocket
        .manage(db)
        .manage(scheduler)
        .manage(Arc::new(frontend)))
}
