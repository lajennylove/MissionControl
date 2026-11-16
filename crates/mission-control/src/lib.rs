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
//! migrations. Switching the theme takes effect at the next start. The post types, taxonomies
//! and field groups made in the admin are read from the database at ignition
//! ([`Registry::load_from_db`]) and served; when their tables cannot be read (a database
//! installed before them), the error is logged and the site starts without them.
//!
//! Then the installed plugins in the plugins folder (`MC_PLUGINS_DIR`, default `plugins`) that
//! the `active_plugins` option lists are loaded and started ([`Plugins::boot`]), after the
//! stored definitions so a plugin's types override the admin's; a plugin that fails is
//! deactivated with a notice for the owner, and the site starts anyway. A missing plugins folder
//! means no plugins. A task writes the failures of plugins at run time
//! ([`Plugins::spawn_recovery`]).
//!
//! The app manages, for code that extends it: the database (`Arc<Database>`), the hook registry
//! (`Arc<Hooks>`), the shortcode registry (`Arc<Shortcodes>`, run on `the_content`), the
//! scheduler (`Arc<Scheduler>`), the REST route registry (`Arc<RestRoutes>`), the post types the
//! REST API serves (`Arc<PostTypes>`), the taxonomies it serves (`Arc<Taxonomies>`), the
//! registry of custom post types, taxonomies and field groups (`Arc<Registry>`, which feeds
//! those two), the registered meta keys shown under `meta` (`Arc<MetaKeys>`, empty at start), the settings the
//! REST API reads and writes (`Arc<Settings>`, WordPress's core settings at start), the kses
//! limiter (`Arc<KsesLimiter>`), the uploads' storage (`Arc<dyn Storage>`, on the local disk in
//! the uploads folder, `MC_UPLOADS_DIR`), the image size registry (`Arc<ImageSizes>`, WordPress's
//! default sizes), the plugins (`Arc<Plugins>`) and the front end (`Arc<Frontend>`). The scheduler's loop starts at liftoff,
//! once the server listens, and runs due events every minute: scheduled posts, and the daily
//! deletion of auto-drafts older than 7 days.
//!
//! The REST API serves discovery at `/mc-json/` and `/?rest_route=/`, posts, pages and every
//! post type in [`PostTypes`] at `/mc-json/mc/v2/{rest_base}`, categories, tags and every
//! taxonomy in [`Taxonomies`] the same way, users at `/mc-json/mc/v2/users` (reads only),
//! media at `/mc-json/mc/v2/media`, the content model at `/mc-json/mc/v2/types`, `/taxonomies`
//! and `/statuses`, search at `/mc-json/mc/v2/search`, and the settings (administrators only) at
//! `/mc-json/mc/v2/settings`. Code that registers a post type, taxonomy or setting after the app
//! is built registers the routes again (`register_routes`), so discovery lists them. Uploads are served at `/mc-content/uploads/`, as data
//! (`nosniff`, a sandbox policy, the stored type). REST clients sign in with
//! application passwords; routes take the [`RestAuth`] or [`RestUser`] guard for the current
//! user, whose roles come from the [`UserRoles`] provider given in [`AppOptions`], by default
//! Ignite's roles ([`IgniteUserRoles`], Ruling R121).
//!
//! The admin is Ignite's panel at [`ADMIN_PATH`] (`/mc-admin`), with the screens of every post
//! type the admin shows; on a MySQL or MariaDB database only, for now: elsewhere `/mc-admin`
//! answers a 503 page (Ruling R119). Ignite reads `DATABASE_URL` and `IGNITE_SECRET_KEY` from the
//! environment (and `.env`), so they must be the configuration's. See the `admin` module's
//! documentation in the source for what is mounted and who may use it.

mod admin;
mod front;
mod rest;
mod uploads;

pub use admin::ADMIN_PATH;
pub use rest::auth::{RestAuth, RestUser};

use std::sync::Arc;
use std::time::Duration;

use admin::AdminMode;
use mission_control_core::config::Config;
use mission_control_core::content::{
    Shortcodes, register_delete_auto_drafts, register_publish_future_post,
    schedule_auto_draft_deletion,
};
use mission_control_core::front::{Frontend, load_active_theme};
use mission_control_core::hooks::Hooks;
use mission_control_core::media::{ImageSizes, LocalStorage, Storage};
use mission_control_core::plugins::{PluginLimits, Plugins};
use mission_control_core::rest::meta::MetaKeys;
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::settings::Settings;
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::rest::{KsesLimiter, RestRoutes};
use mission_control_core::scheduler::Scheduler;
use mission_control_core::types::Registry;
use mission_control_core::users::{IgniteUserRoles, UserRoles};
use orbit_orm::Database;
use rocket::fairing::AdHoc;
use rocket::fs::{FileServer, Options};
use rocket::{Build, Rocket};

/// How often the scheduler looks for due events, as Laravel's scheduler and WP-Cron's minimum.
const SCHEDULER_INTERVAL: Duration = Duration::from_secs(60);

/// The rank of the public site's catch-all route. Higher than [`ASSET_RANK`], so a theme's
/// built file is served before the site looks the path up.
const FRONT_RANK: isize = 20;

/// The rank of the REST routes that answer a family of paths (`/mc-json/<path..>`, the
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
    ///
    /// The admin's panel (Ignite) opens its own connection from `DATABASE_URL` in the
    /// environment (or `.env`), not from this value: it is mounted only when that is
    /// `config.database_url`, which must then name this same database. Otherwise `/mc-admin`
    /// answers a 503 page and the log says why.
    pub database: Option<Arc<Database>>,
    /// Where signed-in users' roles come from, for the REST API and the admin. `None` reads
    /// them from Ignite's role tables ([`IgniteUserRoles`], Ruling R121).
    pub user_roles: Option<Arc<dyn UserRoles>>,
    /// How many kses filters the REST API runs at once, each on a blocking thread. `None` is
    /// [`KsesLimiter::DEFAULT_CONCURRENCY`].
    pub kses_concurrency: Option<usize>,
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
        kses_concurrency,
    } = options;
    // Design 5.7: theme templates are capped in size and render work. Propel's defaults: a
    // template of at most 1 MiB, and a budget of 100,000 loop iterations and template entries
    // per render. Set once for the process, before any theme renders.
    propel_runtime::set_limits(propel_runtime::Limits::default());

    let hooks = Arc::new(Hooks::new());
    let shortcodes = Arc::new(Shortcodes::new());
    shortcodes.register_the_content(&hooks);
    let rest_routes = Arc::new(RestRoutes::new());
    let post_types = Arc::new(PostTypes::new());
    post_types.register_routes(&rest_routes);
    let meta_keys = Arc::new(MetaKeys::new());
    let taxonomies = Arc::new(Taxonomies::new());
    taxonomies.register_routes(&rest_routes);
    mission_control_core::rest::users::register_routes(&rest_routes);
    mission_control_core::rest::media::register_routes(&rest_routes);
    mission_control_core::rest::meta_routes::register_routes(&rest_routes);
    mission_control_core::rest::search::register_routes(&rest_routes, &post_types, &taxonomies);
    let registry = Arc::new(Registry::new(
        Arc::clone(&post_types),
        Arc::clone(&taxonomies),
    ));
    let settings = Arc::new(Settings::new());
    settings.register_routes(&rest_routes);
    let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(config.uploads_dir.clone()));
    let image_sizes = Arc::new(ImageSizes::new());
    let upload_settings = rest::media::UploadSettings {
        max_bytes: config.upload_max_bytes,
    };
    let kses = Arc::new(KsesLimiter::new(
        kses_concurrency.unwrap_or(KsesLimiter::DEFAULT_CONCURRENCY),
    ));
    let auth = rest::auth::AuthSettings {
        environment: config.environment,
        trusted_proxies: config.trusted_proxies.clone(),
        user_roles: user_roles.unwrap_or_else(|| Arc::new(IgniteUserRoles)),
    };

    let types = Types {
        registry: Arc::clone(&registry),
        rest_routes: Arc::clone(&rest_routes),
    };
    rocket::build()
        .manage(Arc::clone(&hooks))
        .manage(shortcodes)
        .manage(rest_routes)
        .manage(post_types)
        .manage(taxonomies)
        .manage(registry)
        .manage(meta_keys)
        .manage(settings)
        .manage(kses)
        .manage(auth)
        .manage(storage)
        .manage(image_sizes)
        .manage(upload_settings)
        .mount("/", rest::routes(REST_RANK))
        .mount("/", uploads::routes())
        .mount("/", rest::auth::refused_routes())
        .register("/mc-json", rest::auth::catchers(&rest::auth::CATCHER_CODES))
        .register("/", rest::auth::catchers(&[403]))
        .attach(rest::auth::RestAuthFairing)
        .attach(AdHoc::try_on_ignite("Mission Control", move |rocket| {
            ignite(rocket, config, db, hooks, types)
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

/// The type registry and the route registry it registers the stored types' routes on.
struct Types {
    registry: Arc<Registry>,
    rest_routes: Arc<RestRoutes>,
}

impl Types {
    /// Loads the admin's post types, taxonomies and field groups from `db` into the registry,
    /// which registers them on the REST registries, and lists their routes in discovery. A
    /// table that cannot be read is logged, and the site starts without them; a stored
    /// definition that does not read is logged and skipped by the registry.
    async fn load(&self, db: &Database) {
        match self.registry.load_from_db(db).await {
            Ok(loaded) => log::debug!("Loaded {loaded} stored type definitions"),
            Err(e) => {
                log::error!(
                    "The stored post types, taxonomies and field groups were not loaded: {e}"
                );
                return;
            }
        }
        self.register_routes();
    }

    /// Lists the routes of every registered post type and taxonomy in discovery: after the
    /// stored definitions load, and again after the plugins have registered theirs.
    fn register_routes(&self) {
        let (post_types, taxonomies) = (self.registry.post_types(), self.registry.taxonomies());
        post_types.register_routes(&self.rest_routes);
        taxonomies.register_routes(&self.rest_routes);
        mission_control_core::rest::search::register_routes(
            &self.rest_routes,
            post_types,
            taxonomies,
        );
    }
}

/// Opens the database, loads the stored type definitions and the active theme, and mounts the
/// public site. The result is what `AdHoc::try_on_ignite` takes: the app to go on with, or back
/// on failure.
#[allow(clippy::result_large_err)]
async fn ignite(
    rocket: Rocket<Build>,
    config: Config,
    db: Option<Arc<Database>>,
    hooks: Arc<Hooks>,
    types: Types,
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
    types.load(&db).await;
    let plugins = match Plugins::new(
        Arc::clone(&hooks),
        Arc::clone(&db),
        Arc::clone(&types.registry),
        &config.plugins_dir,
        PluginLimits::default(),
    ) {
        Ok(plugins) => Arc::new(plugins),
        Err(e) => {
            log::error!("The plugin host could not start: {e}");
            return Err(rocket);
        }
    };
    match plugins.boot().await {
        Ok(failed) => {
            for (slug, e) in failed {
                log::error!("The plugin `{slug}` was deactivated at start: {e}");
            }
        }
        Err(e) => log::error!("The plugins did not all start: {e}"),
    }
    // Plugins register post types and taxonomies in `init`, so discovery lists them too.
    types.register_routes();
    plugins.spawn_recovery();
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
    // The admin, with the stored and the plugins' post types. Ignite registers its own 401 and
    // 500 catchers at `/`; without the panel, the REST catchers take those too.
    let rocket = match admin::admin_mode(&config, &db) {
        AdminMode::Panel => {
            admin::mount_panel(
                rocket,
                &db,
                Arc::clone(&types.registry),
                theme.page_templates(),
            )
            .await
        }
        AdminMode::Unavailable(reason) => rocket
            .register("/", rest::auth::catchers(&[401, 500]))
            .mount("/", admin::unavailable_routes(reason)),
    };

    let scheduler = Arc::new(Scheduler::new(Arc::clone(&db), Arc::clone(&hooks)));
    register_publish_future_post(&scheduler);
    // WordPress's daily `wp_scheduled_auto_draft_delete` (Ruling R125).
    register_delete_auto_drafts(&scheduler);
    if let Err(e) = schedule_auto_draft_deletion(&scheduler).await {
        log::error!("The daily deletion of old auto-drafts was not scheduled: {e}");
    }

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
        .manage(plugins)
        .manage(Arc::new(frontend)))
}
