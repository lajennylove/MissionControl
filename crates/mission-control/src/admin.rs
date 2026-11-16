//! The admin: Ignite's panel at `/mc-admin` (Rulings R114 to R122).
//!
//! # When it is mounted
//!
//! Ignite 0.5.0 runs its sign-in, session and user queries on MySQL and MariaDB only, so the
//! panel is mounted only when the site's database is MySQL or MariaDB (Ruling R119). Ignite reads
//! its database from `DATABASE_URL` in the environment (after loading `.env`), not from the
//! app's configuration, so the panel is mounted only when that is the configuration's database
//! too; a site's own binary that builds its [`Config`] in code must set `DATABASE_URL` to the
//! same URL. Otherwise `/mc-admin` and every path under it answer a plain 503 page saying why,
//! and the rest of the site and the REST API run as before.
//!
//! # What is mounted
//!
//! - Ignite's panel ([`Ignite`]), with its sign-in, dashboard and profile. Its signing key is
//!   `IGNITE_SECRET_KEY`, which Ignite also reads from the environment. Its views are read from
//!   [`VIEWS_DIR`], a copy of Ignite 0.5.0's `views/ignite` with the Mission Control logo, mark
//!   and favicons in place of Ignite's (served under `/mc-includes/images/`, see
//!   [`crate::editor`]). Ignite reads a views directory in debug builds only: a release build
//!   renders the views compiled into Ignite, with Ignite's logo. `IGNITE_VIEWS_DIR`, when set,
//!   wins, as it does in Ignite.
//! - Every post type the admin shows, `post` and `page` included, as an Ignite runtime resource
//!   at `/mc-admin/<post type>` ([`PostTypeResources`], Ruling R114), with the active theme's page
//!   templates in the page form. The page `install.php` is reserved (Ruling R120).
//! - A fairing that authorizes the post type screens before Ignite's routes run (Ruling R115):
//!   the signed-in user must hold the type's whole capability set and `unfiltered_html`, and
//!   `edit_post` or `delete_post` on the post in the URL ([`authorize_post_screen`]). A refused
//!   request is answered with WordPress's 403 message. A visitor is let through, and Ignite sends
//!   them to its sign-in page. Ignite's own pages are not touched.
//!   It fails closed: a post type whose Ignite resource this app did not register (another app
//!   in the process holds it, or code took the slug first) answers 503, logged; and when this
//!   app cannot register `post` or `page`, it serves no post type screens at all.
//! - The New screen of a post type (Ruling R116): `GET /mc-admin/<post type>/new` creates an
//!   `auto-draft` owned by the signed-in user and redirects (303) to its edit form, as
//!   `post-new.php` does (`HEAD` creates nothing). Auto-drafts nobody saved are deleted after 7
//!   days by a daily scheduler event (Ruling R125). Ignite's create (`POST /mc-admin/<post type>`)
//!   is refused.
//!
//! - The classic editor for the rich text fields (Task 6.3): the editor script at
//!   [`editor_script_url`] is registered with Ignite, whose forms then load it.
//! - The media library (Task 6.4, Rulings R127 to R129, see the `media` submodule): the screen of
//!   `attachment` at `/mc-admin/attachment`, authorized by the same fairing with the
//!   `attachment` type's capabilities (those of `post`) and `unfiltered_html`, and `edit_post`
//!   or `delete_post` on the attachment; its upload page at `/mc-admin/attachment/new`; the
//!   picker's endpoints `/mc-admin/async-upload.php` and `/mc-admin/admin-ajax.php`; and the
//!   media library of Ignite's Image, File and Gallery fields.
//!
//! - The settings screens (Task 6.5, Ruling R132): General, Writing, Reading, Discussion, Media,
//!   Permalinks and Privacy at `/mc-admin/options-general` and the others, each an Ignite runtime
//!   resource with one record ([`SettingsScreen`]). The list address answers with a redirect
//!   (303) to the record's edit form, `/mc-admin/<screen>/1/edit`. The fairing checks
//!   `manage_options` (`manage_privacy_options` for Privacy) and refuses New, Ignite's create
//!   and Delete with WordPress's message for a page it does not have.
//! - The Menus screen (Task 6.5, Ruling R133) at `/mc-admin/nav-menus`: the `nav_menu` terms,
//!   their theme locations and their items; the fairing checks `edit_theme_options`.
//!
//! At start the five WordPress roles are made sure to exist as Ignite roles, and each post type
//! resource's five abilities are registered and granted to `administrator` and `editor`
//! (Ruling R121), so Ignite's own coarse checks let them in. The settings screens' `viewAny`,
//! `view` and `update` and the Menus screen's five abilities are granted to `administrator`
//! only (Ruling R132). Errors are logged, and the site starts anyway.
//!
//! The post types are read once, at start: a post type, taxonomy or field group saved or removed
//! later (by the admin or a plugin) reaches the admin's screens at the next start.

use std::sync::{Arc, OnceLock};

use ignite_panel::authz::{self, AuthzError};
use ignite_panel::{AdminUser, Ignite, ignite_core};
use mission_control_core::admin::menus::{MenuObjectType, default_menu_object_types};
use mission_control_core::admin::{
    NAV_MENUS, PostScreen, Refusal, SETTINGS_RECORD, SettingsScreen, SettingsServices,
    attachment_post_type, authorize_menus_screen, authorize_post_screen, authorize_settings_screen,
    register_nav_menus_resource, register_settings_resources,
};
use mission_control_core::config::Config;
use mission_control_core::media::ImageSizes;
use mission_control_core::rest::KsesLimiter;
use mission_control_core::theme::Theme;
use mission_control_core::types::{PostTypeDef, PostTypeResources, Registry, create_auto_draft};
use mission_control_core::users::{ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, SUBSCRIBER};
use orbit_orm::{Database, Driver};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::uri::Origin;
use rocket::http::{Method, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::Redirect;
use rocket::response::content::RawHtml;
use rocket::route::{self, Handler, Route};
use rocket::{Build, Data, Request, Rocket, State};

use crate::editor::editor_script_url;
use crate::rest::auth::AuthSettings;

mod media;

/// Where the admin is mounted (Ruling R120).
pub const ADMIN_PATH: &str = "/mc-admin";

/// The admin's copy of Ignite's views, with Mission Control's logo (see the [module](self)).
const VIEWS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/views/ignite");

/// The variable Ignite reads a views directory from (Ignite's `VIEWS_DIR_ENV`, which it does not
/// export).
const IGNITE_VIEWS_DIR_ENV: &str = "IGNITE_VIEWS_DIR";

/// The first path segment of [`ADMIN_PATH`].
const ADMIN_SEGMENT: &str = "mc-admin";

/// The installer's page under the admin path (`/mc-admin/install.php`), which no resource may
/// take, so the path stays the installer's (Ruling R120).
const INSTALL_SLUG: &str = "install.php";

/// The internal path a refused admin request is sent to. Only answers a request the fairing
/// sent.
const REFUSED_PATH: &str = "/__mission-control/admin-refused";

/// The rank of the New route: before Ignite's `/<slug>/new`, whose default rank is between -12
/// and -1. It forwards every slug that is not a post type's, so Ignite's routes still answer.
const NEW_RANK: isize = -20;

/// The rank of the routes that answer under the admin path when the panel is not mounted:
/// before the public site's catch-all.
const UNAVAILABLE_RANK: isize = -20;

/// The WordPress roles the admin makes sure exist as Ignite roles (Ruling R121).
const WORDPRESS_ROLES: [&str; 5] = [ADMINISTRATOR, EDITOR, AUTHOR, CONTRIBUTOR, SUBSCRIBER];

/// The roles granted every ability of the post type resources, so Ignite's coarse check lets
/// them in; the fine checks are [`authorize_post_screen`]'s.
const GRANTED_ROLES: [&str; 2] = [ADMINISTRATOR, EDITOR];

/// The suffixes of a resource's five Ignite abilities (`post.viewAny`, ...).
const ABILITIES: [&str; 5] = ["viewAny", "view", "create", "update", "delete"];

/// The abilities of a settings screen granted to `administrator`: no create or delete, as a
/// screen has one record (Ruling R132).
const SETTINGS_ABILITIES: [&str; 3] = ["viewAny", "view", "update"];

/// The settings screens and the Menus screen this app registered with Ignite, so the fairing
/// refuses (fails closed) those another registration holds.
#[derive(Debug, Default)]
pub(crate) struct AdminScreens {
    /// The slugs of the settings screens registered.
    settings: Vec<&'static str>,
    /// Whether the Menus screen is registered.
    menus: bool,
}

/// Whether the admin can be mounted for this site, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminMode {
    /// Mount Ignite's panel.
    Panel,
    /// Answer the admin path with a 503 page.
    Unavailable(Unavailable),
}

/// Why the admin is not mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// The database is SQLite or PostgreSQL (Ruling R119).
    NotMySql,
    /// `DATABASE_URL` in the environment, which Ignite reads, is not the configuration's
    /// database (or is not set).
    OtherDatabase,
}

impl Unavailable {
    /// What the 503 page says.
    fn message(self) -> &'static str {
        match self {
            Self::NotMySql => {
                "The admin needs a MySQL or MariaDB database for now. This site runs on another \
                 database, so the admin is not available; the site and its REST API work as \
                 usual."
            }
            Self::OtherDatabase => {
                "The admin could not start: its database setting (DATABASE_URL) is not the \
                 site's database. The site's owner can find the details in the server log."
            }
        }
    }
}

/// Decides whether the admin is mounted on `db`, the site's database. On MySQL and MariaDB it
/// loads `.env` into the environment as Ignite does (variables already set win), and compares
/// `DATABASE_URL` with the configuration's.
pub(crate) fn admin_mode(config: &Config, db: &Database) -> AdminMode {
    if db.driver() != Driver::MySql {
        return AdminMode::Unavailable(Unavailable::NotMySql);
    }
    ignite_panel::load_env();
    match std::env::var("DATABASE_URL") {
        Ok(url) if url == config.database_url => AdminMode::Panel,
        Ok(_) => {
            log::error!(
                "The admin is not mounted: DATABASE_URL in the environment (which Ignite reads) \
                 is not the database in the configuration. Set both to the same URL."
            );
            AdminMode::Unavailable(Unavailable::OtherDatabase)
        }
        Err(_) => {
            log::error!(
                "The admin is not mounted: Ignite reads its database from DATABASE_URL, which is \
                 not set in the environment or in .env."
            );
            AdminMode::Unavailable(Unavailable::OtherDatabase)
        }
    }
}

/// Mounts Ignite's panel at [`ADMIN_PATH`] with the post types of `registry` and the active
/// theme's `page_templates`, and the admin's authorization; see the [module](self).
///
/// # Panics
///
/// As [`Ignite::new`] does on a configuration error: an `IGNITE_TRUSTED_PROXIES` or
/// `IGNITE_APP_URL` that is not valid, or no valid `IGNITE_SECRET_KEY` in a release build.
pub(crate) async fn mount_panel(
    rocket: Rocket<Build>,
    db: &Database,
    registry: Arc<Registry>,
    theme: &Theme,
    image_sizes: Option<Arc<ImageSizes>>,
) -> Rocket<Build> {
    // Ignite first: it reserves its own pages' slugs (`login`, `profile`, ...) before a post type
    // can take one; such a type is then refused below instead of stopping the panel.
    let mut panel = Ignite::new().mount(ADMIN_PATH);
    if std::env::var_os(IGNITE_VIEWS_DIR_ENV).is_none() {
        panel = panel.views_dir(VIEWS_DIR);
    }
    ignite_core::register_editor(editor_script_url());
    if let Err(e) = ignite_core::reserve_slugs(&[INSTALL_SLUG]) {
        log::error!("The admin could not reserve `{INSTALL_SLUG}`: {e}");
    }
    // The settings screens and the Menus screen (Task 6.5), before the post types, so a post type
    // cannot take their slugs.
    // `sanitize_option`'s kses work runs under the app's limiter, as the REST API's does.
    let kses = rocket
        .state::<Arc<KsesLimiter>>()
        .map(|kses| KsesLimiter::clone(kses));
    let (settings, refused_settings) =
        register_settings_resources(SettingsServices { image_sizes, kses });
    for e in refused_settings {
        log::error!("A settings screen is not served by this app: {e}");
    }
    let resources =
        PostTypeResources::new(Arc::clone(&registry)).with_page_templates(theme.page_templates());
    let menus = match register_nav_menus_resource(theme, menu_object_types(&resources, &registry)) {
        Ok(()) => true,
        Err(e) => {
            log::error!("The Menus screen is not served by this app: {e}");
            false
        }
    };
    let refused = resources.sync();
    for refused in &refused {
        log::warn!("{refused}");
    }
    // Fail closed: without `post` and `page` of its own, this app serves no post type screens.
    // The fairing then refuses the slugs another registration holds.
    if refused
        .iter()
        .any(|r| matches!(r.post_type.as_str(), "post" | "page"))
    {
        log::error!(
            "THE ADMIN'S POST SCREENS ARE OFF: Ignite already has a resource at `post` or \
             `page` that this app did not register (a second app in the process, or code that \
             took the slug). Every post type screen answers 503 until the app starts alone."
        );
        for slug in resources.registered() {
            resources.remove(&slug);
        }
    }
    // The media library (Task 6.4): its screen, upload page and endpoints, and Ignite's media
    // fields' library.
    let (rocket, media_screen) = media::mount_media(rocket);
    let mut slugs = resources.registered();
    if media_screen.registered {
        slugs.push(media::ATTACHMENT.to_owned());
    }
    let mut administrator_only: Vec<(String, &[&str])> = settings
        .iter()
        .map(|slug| ((*slug).to_owned(), &SETTINGS_ABILITIES[..]))
        .collect();
    if menus {
        administrator_only.push((NAV_MENUS.to_owned(), &ABILITIES[..]));
    }
    provision(db, &slugs, &administrator_only).await;
    rocket
        .attach(panel.fairing())
        .attach(AdminAuthFairing)
        .mount(ADMIN_PATH, new_routes())
        .mount(ADMIN_PATH, settings_routes())
        .mount("/", refused_routes())
        .manage(Arc::new(resources))
        .manage(AdminScreens { settings, menus })
}

/// What a menu item can link to (Ruling R133): a custom link, a page, a post, a category and a
/// tag, then the other post types the admin shows and the taxonomies, each when it can be added
/// to menus (`show_in_nav_menus`, which defaults to `public`).
fn menu_object_types(
    resources: &PostTypeResources,
    registry: &Registry,
) -> Vec<(MenuObjectType, String)> {
    let mut types = default_menu_object_types();
    let taken = |types: &Vec<(MenuObjectType, String)>, name: &str| {
        types.iter().any(|(t, _)| t.value() == name)
    };
    for def in resources.post_types() {
        if def.show_in_nav_menus.unwrap_or(def.public) && !taken(&types, &def.slug) {
            let label = def.labels.singular_name.clone();
            types.push((MenuObjectType::PostType(def.slug.clone()), label));
        }
    }
    for def in registry.all_taxonomies() {
        if def.show_in_nav_menus.unwrap_or(def.public) && !taken(&types, &def.slug) {
            let label = def.labels.singular_name.clone();
            types.push((MenuObjectType::Taxonomy(def.slug.clone()), label));
        }
    }
    types
}

/// The routes that answer the admin path with a 503 page when the panel is not mounted:
/// `/mc-admin` and every path under it, for every method a form or client sends.
pub(crate) fn unavailable_routes(reason: Unavailable) -> Vec<Route> {
    let handler = UnavailablePage(reason);
    [
        Method::Get,
        Method::Post,
        Method::Put,
        Method::Patch,
        Method::Delete,
    ]
    .into_iter()
    .map(|method| {
        Route::ranked(
            UNAVAILABLE_RANK,
            method,
            &format!("{ADMIN_PATH}/<_..>"),
            handler,
        )
    })
    .collect()
}

/// Ruling R121: the five WordPress roles as Ignite roles, and each post type resource's
/// abilities registered and granted to `administrator` and `editor`; Ruling R132: the
/// `administrator_only` resources' abilities registered and the ones listed granted to
/// `administrator` only. Logs what fails.
async fn provision(db: &Database, slugs: &[String], administrator_only: &[(String, &[&str])]) {
    for role in WORDPRESS_ROLES {
        match authz::create_role(db, role).await {
            Ok(()) | Err(AuthzError::Exists(_)) => {}
            Err(e) => log::error!("The admin could not create the role `{role}`: {e}"),
        }
    }
    let grants = slugs
        .iter()
        .map(|slug| (slug.as_str(), &GRANTED_ROLES[..], &ABILITIES[..]))
        .chain(
            administrator_only
                .iter()
                .map(|(slug, abilities)| (slug.as_str(), &[ADMINISTRATOR][..], *abilities)),
        );
    for (slug, roles, abilities) in grants {
        if let Err(e) = ignite_panel::register_runtime_permissions(db, slug).await {
            log::error!("The admin could not register the permissions of `{slug}`: {e}");
            continue;
        }
        for role in roles {
            for ability in abilities {
                let permission = format!("{slug}.{ability}");
                if let Err(e) = authz::grant_permission(db, role, &permission).await {
                    log::error!("The admin could not grant `{permission}` to `{role}`: {e}");
                }
            }
        }
    }
}

/// Answers a request under the admin path while the panel is not mounted.
#[derive(Debug, Clone, Copy)]
struct UnavailablePage(Unavailable);

#[rocket::async_trait]
impl Handler for UnavailablePage {
    async fn handle<'r>(&self, request: &'r Request<'_>, _: Data<'r>) -> route::Outcome<'r> {
        let page = error_page("The admin is not available", None, self.0.message());
        route::Outcome::from(request, (Status::ServiceUnavailable, page))
    }
}

/// A plain error page, as WordPress's `wp_die` shows one: an optional heading and a message.
/// The text is the admin's own, never the request's, so it is not escaped.
fn error_page(title: &str, heading: Option<&str>, message: &str) -> RawHtml<String> {
    let heading = heading.map_or_else(String::new, |h| format!("<h1>{h}</h1>\n"));
    RawHtml(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{title}</title>
</head>
<body>
<main>
{heading}<p>{message}</p>
</main>
</body>
</html>"#
    ))
}

/// Why the fairing ended an admin request.
#[derive(Debug, Clone, Copy)]
enum AdminRefusal {
    /// Refused by the post type screens' rules.
    Refused(Refusal),
    /// The user's roles or the post could not be read.
    Failed,
    /// The slug is a post type whose Ignite resource this app did not register: its rules are
    /// not this app's to check, so the screens are refused (fail closed).
    NotServed,
}

/// The refusal the fairing ended the request with.
#[derive(Default)]
struct PendingRefusal(OnceLock<AdminRefusal>);

/// Authorizes the post type screens before Ignite's routes run (Ruling R115); see the
/// [module](self).
struct AdminAuthFairing;

#[rocket::async_trait]
impl Fairing for AdminAuthFairing {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control admin authorization",
            kind: Kind::Request,
        }
    }

    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        let Some(refusal) = refusal(request).await else {
            return;
        };
        let _ = request.local_cache(PendingRefusal::default).0.set(refusal);
        // HEAD stays HEAD: Rocket answers it with the GET route and drops the body.
        if request.method() != Method::Head {
            request.set_method(Method::Get);
        }
        request.set_uri(Origin::parse(REFUSED_PATH).expect("a valid path"));
    }
}

/// Why the request must not reach the post type screens, or `None`: it is not for a post type's
/// screens, nobody is signed in (Ignite sends the visitor to its sign-in page), or it is
/// allowed.
async fn refusal(request: &Request<'_>) -> Option<AdminRefusal> {
    let mut segments = request.uri().path().segments();
    if segments.next()? != ADMIN_SEGMENT {
        return None;
    }
    let slug = segments.next()?;
    // A screen has at most two segments after the post type; more is no screen (404).
    let rest: Vec<&str> = segments.take(3).collect();
    let rocket = request.rocket();
    if let Some(refusal) = theme_or_settings_refusal(request, slug, &rest).await {
        return refusal;
    }
    let resources = rocket.state::<Arc<PostTypeResources>>()?;
    let def = if slug == media::ATTACHMENT {
        // The media library's screen (Ruling R127), when this app registered it; otherwise,
        // when another registration holds the slug, it is refused (fail closed).
        match rocket.state::<media::MediaScreen>() {
            Some(screen) if screen.registered => Some(attachment_post_type()),
            _ if ignite_core::runtime_resource(slug).is_some() => {
                log::error!(
                    "Refused `{}`: Ignite's resource `{slug}` was not registered by this app.",
                    request.uri().path()
                );
                return Some(AdminRefusal::NotServed);
            }
            _ => None,
        }
    } else {
        resources.post_type(slug)
    };
    let Some(def) = def else {
        if resources.is_unowned(slug) {
            log::error!(
                "Refused `{}`: Ignite's resource `{slug}` was not registered by this app.",
                request.uri().path()
            );
            return Some(AdminRefusal::NotServed);
        }
        return None;
    };
    let (db, caps) = match signed_in_caps(request).await? {
        Ok(found) => found,
        Err(failed) => return Some(failed),
    };
    let screen = PostScreen::parse(request.method().as_str(), &rest);
    // The media library's upload page is `media-new.php`: its refusal is WordPress's for it.
    let upload_page = slug == media::ATTACHMENT && rest == ["new"];
    match authorize_post_screen(&db, &caps, &def, screen).await {
        Ok(Some(_)) if upload_page => Some(AdminRefusal::Refused(Refusal::UploadFiles)),
        Ok(refused) => refused.map(AdminRefusal::Refused),
        Err(e) => {
            log::error!("Could not check the admin request {screen:?} on `{slug}`: {e}");
            Some(AdminRefusal::Failed)
        }
    }
}

/// The signed-in user's database and capabilities: `None` for a visitor (Ignite sends them to
/// its sign-in page), or the refusal when they cannot be read.
async fn signed_in_caps(
    request: &Request<'_>,
) -> Option<Result<(Arc<Database>, mission_control_core::users::UserCaps), AdminRefusal>> {
    let Outcome::Success(user) = request.guard::<AdminUser>().await else {
        return None;
    };
    let rocket = request.rocket();
    let (Some(db), Some(settings)) = (
        rocket.state::<Arc<Database>>(),
        rocket.state::<AuthSettings>(),
    ) else {
        log::error!("The admin's authorization ran before the app was ready");
        return Some(Err(AdminRefusal::Failed));
    };
    Some(match settings.user_roles.roles(db, user.user_id()).await {
        Ok(assignment) => Ok((Arc::clone(db), assignment.user_caps(user.user_id()))),
        Err(e) => {
            log::error!("Could not read the roles of user {}: {e}", user.user_id());
            Err(AdminRefusal::Failed)
        }
    })
}

/// The fairing's answer for the settings screens and the Menus screen (Rulings R132 and R133):
/// `None` when `slug` is neither (the post type screens' rules decide), `Some(None)` when the
/// request may go on, and `Some(Some(refusal))` when it is refused. A screen this app did not
/// register while Ignite has a resource at its slug answers 503 (fail closed).
async fn theme_or_settings_refusal(
    request: &Request<'_>,
    slug: &str,
    rest: &[&str],
) -> Option<Option<AdminRefusal>> {
    let settings = SettingsScreen::from_slug(slug);
    if settings.is_none() && slug != NAV_MENUS {
        return None;
    }
    let screens = request.rocket().state::<AdminScreens>();
    let ours = screens.is_some_and(|screens| match settings {
        Some(screen) => screens.settings.contains(&screen.slug()),
        None => screens.menus,
    });
    if !ours {
        if ignite_core::runtime_resource(slug).is_some() {
            log::error!(
                "Refused `{}`: Ignite's resource `{slug}` was not registered by this app.",
                request.uri().path()
            );
            return Some(Some(AdminRefusal::NotServed));
        }
        return Some(None);
    }
    let caps = match signed_in_caps(request).await {
        None => return Some(None),
        Some(Err(failed)) => return Some(Some(failed)),
        Some(Ok((_, caps))) => caps,
    };
    let screen = PostScreen::parse(request.method().as_str(), rest);
    let refusal = match settings {
        Some(settings) => authorize_settings_screen(&caps, settings, screen),
        None => authorize_menus_screen(&caps, screen),
    };
    Some(refusal.map(AdminRefusal::Refused))
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AdminRefusal {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        match request.local_cache(PendingRefusal::default).0.get() {
            Some(refusal) => Outcome::Success(*refusal),
            None => Outcome::Forward(Status::NotFound),
        }
    }
}

/// The route that answers a request the fairing refused, at the lowest rank Rocket has, so no
/// other route answers it first.
fn refused_routes() -> Vec<Route> {
    let mut routes = rocket::routes![refused];
    for route in &mut routes {
        route.rank = isize::MIN;
    }
    routes
}

/// Answers a refused admin request: WordPress's 403 message, or a 500 page when the check could
/// not be made.
#[rocket::get("/__mission-control/admin-refused")]
fn refused(refusal: AdminRefusal) -> (Status, RawHtml<String>) {
    match refusal {
        AdminRefusal::Refused(refusal) => (
            Status::from_code(refusal.status()).unwrap_or(Status::Forbidden),
            error_page("Error", refusal.heading(), refusal.message()),
        ),
        AdminRefusal::NotServed => (
            Status::ServiceUnavailable,
            error_page(
                "The admin is not available",
                None,
                "These screens are not available: the admin could not take charge of them. The \
                 site's owner can find the details in the server log.",
            ),
        ),
        AdminRefusal::Failed => (
            Status::InternalServerError,
            error_page(
                "Error",
                None,
                "Something went wrong. Try again in a moment.",
            ),
        ),
    }
}

/// The post type whose screens the request is for: the first segment after the admin path,
/// when it is a post type's resource. Any other slug forwards, to Ignite's own routes.
struct PostTypeScreen(PostTypeDef);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for PostTypeScreen {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let def = request.routed_segment(0).and_then(|slug| {
            request
                .rocket()
                .state::<Arc<PostTypeResources>>()?
                .post_type(slug)
        });
        match def {
            Some(def) => Outcome::Success(Self(def)),
            None => Outcome::Forward(Status::NotFound),
        }
    }
}

/// The New route, at [`NEW_RANK`].
fn new_routes() -> Vec<Route> {
    let mut routes = rocket::routes![new_post, new_post_head];
    for route in &mut routes {
        route.rank = NEW_RANK;
    }
    routes
}

/// The New screen of a post type (Ruling R116): an `auto-draft` owned by the signed-in user, and
/// a redirect (303) to its edit form, as `post-new.php` does. The fairing has checked the user's
/// capabilities; a visitor fails [`AdminUser`] with 401, which Ignite sends to its sign-in page.
/// `HEAD` is [`new_post_head`]'s, so it creates nothing.
#[rocket::get("/<_>/new")]
async fn new_post(
    screen: PostTypeScreen,
    user: AdminUser,
    db: &State<Arc<Database>>,
) -> Result<Redirect, Status> {
    let slug = &screen.0.slug;
    match create_auto_draft(db, slug, user.user_id()).await {
        Ok(post) => Ok(Redirect::to(format!(
            "{ADMIN_PATH}/{slug}/{}/edit",
            post.id
        ))),
        Err(e) => {
            log::error!("Could not create the auto-draft of a new `{slug}`: {e}");
            Err(Status::InternalServerError)
        }
    }
}

/// `HEAD` on the New screen (Ruling R125): 200 with no body, and nothing created. Without it,
/// Rocket would answer `HEAD` with [`new_post`], which creates an auto-draft.
#[rocket::head("/<_>/new")]
fn new_post_head(_screen: PostTypeScreen, _user: AdminUser) -> Status {
    Status::Ok
}

/// The settings screen a request is for: the first segment after the admin path, when it is a
/// settings screen this app registered. Any other slug forwards, to Ignite's own routes.
struct SettingsSlug(SettingsScreen);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SettingsSlug {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let screen = request
            .routed_segment(0)
            .and_then(SettingsScreen::from_slug)
            .filter(|screen| {
                request
                    .rocket()
                    .state::<AdminScreens>()
                    .is_some_and(|screens| screens.settings.contains(&screen.slug()))
            });
        match screen {
            Some(screen) => Outcome::Success(Self(screen)),
            None => Outcome::Forward(Status::NotFound),
        }
    }
}

/// The settings screens' list address, at [`NEW_RANK`], before Ignite's list.
fn settings_routes() -> Vec<Route> {
    let mut routes = rocket::routes![settings_list];
    for route in &mut routes {
        route.rank = NEW_RANK;
    }
    routes
}

/// A settings screen's list address (Ruling R132): a redirect (303) to its one record's edit
/// form, as WordPress's Settings menu opens the screen. Ignite's save lands here too, so its
/// "Changes saved." message shows on the form. The fairing has checked the user's capability;
/// a visitor fails [`AdminUser`] with 401, which Ignite sends to its sign-in page.
#[rocket::get("/<_>")]
fn settings_list(screen: SettingsSlug, _user: AdminUser) -> Redirect {
    Redirect::to(format!(
        "{ADMIN_PATH}/{}/{SETTINGS_RECORD}/edit",
        screen.0.slug()
    ))
}
