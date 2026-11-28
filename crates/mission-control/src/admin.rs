//! The admin: Ignite's panel at `/mc-admin` (Rulings R114 to R122).
//!
//! # When it is mounted
//!
//! Ignite 0.6.1 runs its sign-in, session and user queries on MySQL and MariaDB only, so the
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
//!   `IGNITE_SECRET_KEY`, which Ignite also reads from the environment. Its brand is Mission
//!   Control's, through Ignite's brand options (as Filament's panel `brandName`, `brandLogo`,
//!   `brandLogoHeight` and `favicon`): the logo shows in place of the name, which is its
//!   alternative text, and the favicon replaces Ignite's; the images are served under
//!   `/mc-includes/images/` (see [`crate::editor`]). The options are compiled into Ignite's
//!   views, so they hold in a release build too.
//! - The sidebar in WordPress's admin menu order: the dashboard, then the
//!   `Content` group (Posts, Media, Pages, then the custom post types), then the `Site` group
//!   (Menus, then the settings screens), each entry at its WordPress `menu_position` and with
//!   its Ignite icon name (see `mission_control_core::admin::navigation`). Each admin theme maps
//!   the name to its own icon: the Ignite theme (the default) to Tabler, and the WordPress theme
//!   ([`wordpress_theme`]) to Dashicons. WordPress's Dashicons stylesheet is served at
//!   `/mc-includes/css/dashicons.css` with its fonts (see [`crate::editor`]), and only the
//!   WordPress theme loads it.
//! - Every post type the admin shows, `post` and `page` included, as an Ignite runtime resource
//!   at `/mc-admin/<post type>` ([`PostTypeResources`], Ruling R114), with the active theme's page
//!   templates in the page form. The page `install.php` is reserved (Ruling R120).
//! - Each post type's Ignite policy and user-aware queries (Task A1, see
//!   `mission_control_core::admin::posts`): the list and New need the type's `edit_posts`, a
//!   post's form and save `edit_post` (never on a post in the trash), the list's trash row
//!   actions (Trash, Restore, Delete Permanently) and Ignite's Delete (in the trash only)
//!   `delete_post`; authors and contributors see and edit their own posts, as WordPress lets
//!   them. Ignite answers a refusal with 403 and a post outside the user's list with 404.
//! - A fairing before Ignite's routes (Ruling R115, narrowed by Task A1): on a post type it
//!   checks the New screen and its save (Ignite's create) with the type's `edit_posts`, with
//!   WordPress's 403 messages ([`authorize_post_screen`]), and answers the form and the save of
//!   a post in the trash with `post.php`'s 409 message; on the media library it checks every
//!   screen (`upload_files`, then `edit_post` or `delete_post` on the attachment), and refuses
//!   Ignite's create, as files are uploaded. A visitor is let through, and Ignite sends
//!   them to its sign-in page. Ignite's own pages are not touched.
//!   It fails closed: a post type whose Ignite resource this app did not register (another app
//!   in the process holds it, or code took the slug first) answers 503, logged; and when this
//!   app cannot register `post` or `page`, it serves no post type screens at all.
//! - The New screen of a post type: a new post gets no id until it is saved (the owner's rule;
//!   WordPress's `post-new.php` creates an `auto-draft` with an id when the screen opens).
//!   `GET /mc-admin/<post type>/new` is Ignite's create form, with no record, and creates
//!   nothing. It posts to `POST /mc-admin/<post type>` (Ignite's create), whose save creates the
//!   post; the same fairing then sends the browser (303) to the new post's edit form,
//!   `/mc-admin/<post type>/<id>/edit`, as WordPress does after the first save, where Ignite's
//!   create would go to the list. Auto-drafts made by the earlier New screen are deleted after
//!   7 days by a daily scheduler event (Ruling R125).
//!
//! - The classic editor for the rich text fields (Task 6.3): the editor script at
//!   [`editor_script_url`] is registered with Ignite, whose forms then load it.
//! - The media library (Task 6.4, Rulings R127 to R129, see the `media` submodule): the screen of
//!   `attachment` at `/mc-admin/attachment`, authorized by the same fairing with
//!   `upload_files` (`upload.php`, `media-new.php`), and `edit_post` or `delete_post` on the
//!   attachment (`map_meta_cap`: an author edits and deletes their own files); its upload
//!   page at `/mc-admin/attachment/new`; the picker's endpoints `/mc-admin/async-upload.php`
//!   and `/mc-admin/admin-ajax.php`; and the
//!   media library of Ignite's Image, File and Gallery fields.
//!
//! - The settings screens (Task 6.5, Ruling R132): General, Writing, Reading, Discussion, Media,
//!   Permalinks and Privacy at `/mc-admin/options-general` and the others, each an Ignite runtime
//!   resource with one record ([`SettingsScreen`]). The list address answers with a redirect
//!   (303) to the record's edit form, `/mc-admin/<screen>/1/edit`. The fairing checks
//!   `manage_options` (`manage_privacy_options` for Privacy) and refuses New, Ignite's create
//!   and Delete with WordPress's message for a page it does not have.
//! - The Menus screen (Task 6.5, Ruling R133) at `/mc-admin/nav-menus`: the `nav_menu` terms,
//!   their theme locations and their items; the fairing checks `edit_theme_options`. Its forms
//!   get WordPress's menu builder (the `menus` submodule).
//! - The Categories and Tags screens (WordPress's Posts > Categories and Posts > Tags) at
//!   `/mc-admin/category` and `/mc-admin/post_tag` (see `mission_control_core::admin::terms`):
//!   their policy checks the taxonomy's `manage_terms`, `edit_terms`, `edit_term` and
//!   `delete_term`, so the default category is never deleted. A term slug another registration
//!   holds answers 503 (fail closed).
//!
//! - The Users screens (`users.php`, `user-new.php`, `user-edit.php`) and the Roles screen, in
//!   the Site group (see the `users` submodule), built on Ignite's. In the WordPress admin theme,
//!   the profile page is WordPress's `profile.php` with all its fields (the `profile`
//!   submodule); the Ignite theme keeps Ignite's.
//! - The dashboard's widgets (see `mission_control_core::admin::dashboard`): the At a Glance
//!   cards, then WordPress's At a Glance, Activity and Quick Draft boxes as Ignite's list and
//!   form widgets, in every admin theme. Quick Draft keeps its answer in its box ("Draft
//!   saved.", or the refusal with the values typed) and lists "Your Recent Drafts" (the
//!   `quick_draft` submodule).
//! - The pages' chrome (the `chrome` submodule): the top bar shows the user's display name, a
//!   refused profile picture's message is an error notice, and an address under the admin that
//!   nothing answers is a 404 page in the admin's look with a link to the dashboard.
//!
//! # Roles and capabilities
//!
//! WordPress's capabilities are Ignite abilities, and its five roles Ignite roles
//! (`Ignite::abilities` and `Ignite::role`, as `populate_roles` adds them, once, with
//! `mission_control_core::users`' capability lists): a role keeps the capabilities the Roles
//! screen gives it, and a user's capabilities are read from the stored roles on each request
//! (`IgniteUserRoles`). Each screen's Ignite abilities are the capabilities WordPress checks for
//! it (`ScreenCaps`): a post type's `edit_posts` and `delete_posts`, the media library's
//! `upload_files`, the Categories and Tags screens' `manage_categories`, the Menus screen's
//! `edit_theme_options`, the settings screens' `manage_options`, the Users screens' `list_users`,
//! `create_users`, `edit_users` and `delete_users`, and the Roles screen's `promote_users`. A user
//! without one sees neither the sidebar entry nor the screen (403); the fairing and each screen's
//! policy then check the finer capabilities (`edit_others_posts`, `edit_post`, ...). A role made
//! before its capabilities were stored (by the installer, or an earlier version) is given
//! WordPress's defaults at start. Errors are logged, and the site starts anyway.
//!
//! The post types are read once, at start: a post type, taxonomy or field group saved or removed
//! later (by the admin or a plugin) reaches the admin's screens at the next start.

use std::sync::{Arc, OnceLock};

use ignite_panel::authz::{self, AuthzError};
use ignite_panel::{AdminUser, Ignite, NavigationItem, Role, ignite_core};
use mission_control_core::admin::menus::{MenuObjectType, default_menu_object_types};
use mission_control_core::admin::{
    NAV_MENUS, PostScreen, Refusal, SETTINGS_RECORD, SettingsScreen, SettingsServices,
    TERM_SCREENS, attachment_post_type, authorize_menus_screen, authorize_post_screen,
    authorize_settings_screen, authorize_term_screen, may_upload, register_nav_menus_resource,
    register_settings_resources, register_term_resources,
};
use mission_control_core::admin::{dashboard, navigation};
use mission_control_core::config::Config;
use mission_control_core::media::ImageSizes;
use mission_control_core::rest::KsesLimiter;
use mission_control_core::theme::Theme;
use mission_control_core::types::{Access, PostTypeResources, Registry};
use mission_control_core::users::{all_capabilities, default_roles, is_capability_name};
use orbit_orm::{Database, Driver};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::uri::Origin;
use rocket::http::{Method, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::Redirect;
use rocket::response::content::RawHtml;
use rocket::route::{self, Handler, Route};
use rocket::{Build, Data, Request, Response, Rocket};

use crate::editor::editor_script_url;
use crate::rest::auth::AuthSettings;

mod chrome;
mod forms;
mod locks;
mod media;
mod menus;
mod profile;
mod quick_draft;
mod users;
mod wordpress_theme;

/// Where the admin is mounted (Ruling R120).
pub const ADMIN_PATH: &str = "/mc-admin";

/// The admin's brand name: the sidebar logo's alternative text and the end of each page title.
pub(crate) const BRAND_NAME: &str = "RocketBoost Mission Control";

/// The admin's logo, served from `assets/brand/` (see [`crate::editor`]).
pub(crate) const BRAND_LOGO: &str = "/mc-includes/images/mission-control-logo.webp";

/// The logo's height. The logo is 661x571 px; 4.3rem (69px) shows it about 80px wide, half the
/// 160 x 138 px the sidebar showed before, as the owner asked for the sign-in and the sidebar.
pub(crate) const BRAND_LOGO_HEIGHT: &str = "4.3rem";

/// The admin's favicon, served from `assets/brand/` (see [`crate::editor`]).
pub(crate) const FAVICON: &str = "/mc-includes/images/favicon-32.png";

/// The first path segment of [`ADMIN_PATH`].
const ADMIN_SEGMENT: &str = "mc-admin";

/// The installer's page under the admin path (`/mc-admin/install.php`), which no resource may
/// take, so the path stays the installer's (Ruling R120).
const INSTALL_SLUG: &str = "install.php";

/// The internal path a refused admin request is sent to. Only answers a request the fairing
/// sent.
const REFUSED_PATH: &str = "/__mission-control/admin-refused";

/// The rank of the settings screens' list route: before Ignite's `/<slug>`, whose default rank
/// is between -12 and -1. It forwards every slug that is not a settings screen's, so Ignite's
/// routes still answer.
const SETTINGS_RANK: isize = -20;

/// The rank of the routes that answer under the admin path when the panel is not mounted:
/// before the public site's catch-all.
const UNAVAILABLE_RANK: isize = -20;

/// The settings screens and the Menus screen this app registered with Ignite, so the fairing
/// refuses (fails closed) those another registration holds.
#[derive(Debug, Default)]
pub(crate) struct AdminScreens {
    /// The slugs of the settings screens registered.
    settings: Vec<&'static str>,
    /// Whether the Menus screen is registered.
    menus: bool,
    /// The slugs of the Categories and Tags screens registered.
    terms: Vec<&'static str>,
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
    // The WordPress theme is registered before the panel is built, so a user can choose it.
    wordpress_theme::register();
    // Ignite first: it reserves its own pages' slugs (`login`, `profile`, ...) before a post type
    // can take one; such a type is then refused below instead of stopping the panel.
    let mut panel = Ignite::new()
        .mount(ADMIN_PATH)
        .brand_name(BRAND_NAME)
        .brand_logo(BRAND_LOGO)
        .brand_logo_height(BRAND_LOGO_HEIGHT)
        .favicon(FAVICON)
        .navigation_groups(navigation::GROUPS)
        // WordPress's Settings menu: a label-only entry in Site, after Menus, that opens its
        // first screen (General) and lists the seven settings screens under it.
        .navigation_items([NavigationItem::make(navigation::SETTINGS_PARENT)
            .group(navigation::SITE)
            .sort(navigation::SETTINGS)
            .icon(navigation::SETTINGS_ICON)])
        // WordPress's dashboard: the At a Glance cards, then its At a Glance, Activity and
        // Quick Draft boxes.
        .widget(dashboard::AtAGlance)
        .list_widget(dashboard::at_a_glance_widget(ADMIN_PATH))
        .list_widget(dashboard::activity_widget(ADMIN_PATH))
        .form_widget(dashboard::quick_draft_widget())
        // WordPress's Users screens and the Roles screen, in Site.
        .runtime_resource(users::users_screen())
        .runtime_resource(users::profile_entry())
        .runtime_resource(users::roles_screen())
        // WordPress's capabilities as abilities, and its five roles with their capabilities.
        .abilities(all_capabilities())
        // Ask before a changed form is left unsaved (posts, pages, settings, menus).
        .unsaved_changes_alerts();
    for role in default_roles() {
        let caps: Vec<String> = role.granted().map(str::to_owned).collect();
        panel = panel.role(
            Role::make(role.name.as_str())
                .label(role.display_name.as_str())
                .abilities(caps),
        );
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
    let (settings, refused_settings) = register_settings_resources(SettingsServices {
        image_sizes,
        kses: kses.clone(),
    });
    for e in refused_settings {
        log::error!("A settings screen is not served by this app: {e}");
    }
    // The post type screens read the acting user's capabilities as the REST API does: the app's
    // roles provider and kses limiter (Task A1).
    let access = Access::new(
        rocket.state::<AuthSettings>().map_or_else(
            || Arc::new(mission_control_core::users::IgniteUserRoles) as Arc<_>,
            |auth| Arc::clone(&auth.user_roles),
        ),
        kses.clone().unwrap_or_default(),
    );
    // The Categories and Tags screens, before the post types, so a post type cannot take their
    // slugs.
    let (terms, refused_terms) = register_term_resources(&access);
    for e in refused_terms {
        log::error!("A term screen is not served by this app: {e}");
    }
    let resources = PostTypeResources::new(Arc::clone(&registry))
        .with_page_templates(theme.page_templates())
        .with_access(access.clone());
    let menu_types = menu_object_types(&resources, &registry);
    let menu_sources = menus::menu_sources(&menu_types, &resources, &registry);
    let menus = match register_nav_menus_resource(theme, menu_types) {
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
    let (rocket, media_screen) = media::mount_media(rocket, access);
    // Each screen's abilities are WordPress capabilities (a custom post type's `edit_books`,
    // ...): registered as permissions, so the Roles screen offers them.
    let mut slugs: Vec<String> = resources.registered();
    if media_screen.registered {
        slugs.push(media::ATTACHMENT.to_owned());
    }
    slugs.extend(terms.iter().map(|slug| (*slug).to_owned()));
    slugs.extend(settings.iter().map(|slug| (*slug).to_owned()));
    if menus {
        slugs.push(NAV_MENUS.to_owned());
    }
    provision(db, &slugs).await;
    rocket
        .attach(panel.fairing())
        .attach(AdminAuthFairing)
        .attach(quick_draft::QuickDraft)
        .manage(quick_draft::QuickDraftNotices::default())
        .attach(menus::MenuBuilderViews)
        .attach(forms::FormViews)
        .attach(users::UserFormViews)
        .attach(profile::ProfileView)
        .attach(chrome::ChromeViews)
        .manage(profile::ProfileNotices::default())
        .register(
            format!("{ADMIN_PATH}/profile"),
            rocket::catchers![picture_too_large],
        )
        .register(ADMIN_PATH, rocket::catchers![chrome::not_found])
        .mount(ADMIN_PATH, locks::routes())
        .mount(ADMIN_PATH, menus::routes())
        .mount("/", menus::include_routes())
        .manage(menu_sources)
        .mount(ADMIN_PATH, settings_routes())
        .mount(ADMIN_PATH, users::routes())
        .mount(ADMIN_PATH, profile::routes())
        .mount(ADMIN_PATH, chrome::routes())
        .mount("/", refused_routes())
        .manage(Arc::new(resources))
        .manage(AdminScreens {
            settings,
            menus,
            terms,
        })
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

/// WordPress's capabilities registered as Ignite permissions, the five roles made before their
/// capabilities were stored given WordPress's defaults (a role Ignite has not made yet is made
/// by `Ignite::role` when the panel starts, with them), and each screen of `slugs` given its
/// abilities as permissions. Logs what fails.
async fn provision(db: &Database, slugs: &[String]) {
    if let Err(e) = ignite_panel::register_abilities(db, all_capabilities()).await {
        log::error!("The admin could not register WordPress's capabilities: {e}");
    }
    for role in default_roles() {
        match authz::role_permissions(db, &role.name).await {
            Ok(stored) if !stored.iter().any(|name| is_capability_name(name)) => {
                let caps: Vec<String> = role.granted().map(str::to_owned).collect();
                if let Err(e) = authz::sync_role_permissions(db, &role.name, &caps).await {
                    log::error!(
                        "The admin could not give the role `{}` its capabilities: {e}",
                        role.name
                    );
                }
            }
            Ok(_) | Err(AuthzError::NotFound(_)) => {}
            Err(e) => log::error!("The admin could not read the role `{}`: {e}", role.name),
        }
    }
    for slug in slugs {
        if let Err(e) = ignite_panel::register_runtime_permissions(db, slug).await {
            log::error!("The admin could not register the permissions of `{slug}`: {e}");
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

/// Ignite's refusal of a profile picture it does not keep, under the picture field.
pub const PICTURE_REFUSED: &str = "Choose a PNG, JPEG or WebP picture of up to 1 MB.";

/// A profile request whose body is larger than Rocket lets through
/// ([`crate::PROFILE_PICTURE_BODY_LIMIT`]): Ignite's message for a picture it does not keep,
/// with the way back to the profile, in place of Rocket's bare "413: Payload Too Large". A
/// picture up to the limit reaches Ignite, which answers the same message on the profile.
#[rocket::catch(413)]
fn picture_too_large() -> (Status, RawHtml<String>) {
    let page = error_page("Error", None, PICTURE_REFUSED);
    let back = format!("<p><a href=\"{ADMIN_PATH}/profile\">Back to your profile</a></p></body>");
    (
        Status::PayloadTooLarge,
        RawHtml(page.0.replacen("</body>", &back, 1)),
    )
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

/// Authorizes the post type screens before Ignite's routes run (Ruling R115), and sends the New
/// screen's first save to the new post's edit form; see the [module](self).
struct AdminAuthFairing;

#[rocket::async_trait]
impl Fairing for AdminAuthFairing {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control admin authorization",
            kind: Kind::Request | Kind::Response,
        }
    }

    /// Ignite's create answers a saved record with a redirect (303) to the list. On a post type,
    /// the browser is sent to the new post's edit form instead, as WordPress does after the New
    /// screen's first save.
    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if request.method() != Method::Post || response.status() != Status::SeeOther {
            return;
        }
        let mut segments = request.uri().path().segments();
        let (Some(ADMIN_SEGMENT), Some(slug), None) =
            (segments.next(), segments.next(), segments.next())
        else {
            return;
        };
        let Some(resources) = request.rocket().state::<Arc<PostTypeResources>>() else {
            return;
        };
        if resources.post_type(slug).is_none() {
            return;
        }
        let Outcome::Success(user) = request.guard::<AdminUser>().await else {
            return;
        };
        // A dashboard form (Quick Draft) comes back to the dashboard, as WordPress's does.
        let to_dashboard = response
            .headers()
            .get_one("Location")
            .is_some_and(|to| to.trim_end_matches('/') == ADMIN_PATH);
        if let Some(id) = resources.take_created(user.user_id(), slug)
            && !to_dashboard
        {
            response.set_raw_header("Location", format!("{ADMIN_PATH}/{slug}/{id}/edit"));
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
    if TERM_SCREENS.contains(&slug) {
        return term_screen_refusal(request, slug, &rest).await;
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
    let screen = PostScreen::parse(request.method().as_str(), &rest);
    // A post type's list, forms and Delete are decided by the type's policy and its user-aware
    // queries (Task A1). The fairing also checks the New screen and its save (Ignite's create),
    // so a refusal there has WordPress's message, and the edit form and its save of a post in
    // the trash, which `post.php` refuses with 409 and its own message.
    let edit = matches!(screen, PostScreen::Edit(_) | PostScreen::Update(_));
    if slug != media::ATTACHMENT && !edit && !matches!(screen, PostScreen::New | PostScreen::Create)
    {
        return None;
    }
    let (db, caps) = match signed_in_caps(request).await? {
        Ok(found) => found,
        Err(failed) => return Some(failed),
    };
    // A file is added by uploading it (Ruling R129): the media library has no create form.
    if slug == media::ATTACHMENT && screen == PostScreen::Create {
        return Some(AdminRefusal::Refused(Refusal::NoSuchPage));
    }
    // The media library's upload page is `media-new.php`: its refusal is WordPress's for it.
    let upload_page = slug == media::ATTACHMENT && rest == ["new"];
    // The media library (`upload.php`, `media-new.php`) needs `upload_files`; an attachment's
    // edit form, save and Delete need `edit_post` or `delete_post` on it, as for any post (an
    // author's own files).
    if slug == media::ATTACHMENT && !may_upload(&caps) {
        let refusal = match screen {
            _ if upload_page => Refusal::UploadFiles,
            PostScreen::New => Refusal::NewScreen,
            _ => Refusal::Screen,
        };
        return Some(AdminRefusal::Refused(refusal));
    }
    if slug == media::ATTACHMENT
        && matches!(
            screen,
            PostScreen::List | PostScreen::New | PostScreen::Other
        )
    {
        return None;
    }
    match authorize_post_screen(&db, &caps, &def, screen).await {
        // The other refusals of a post type's form are the policy's (403, or 404 for a post
        // outside the user's list).
        Ok(Some(refused)) if edit && slug != media::ATTACHMENT => {
            (refused == Refusal::InTrash).then_some(AdminRefusal::Refused(Refusal::InTrash))
        }
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

/// The fairing's answer for the Categories and Tags screens: their policy decides each action,
/// unless this app did not register the screen while Ignite has a resource at its slug, which
/// answers 503 (fail closed). Delete on the default category is refused here, before anything
/// changes, with its own message ([`authorize_term_screen`]).
async fn term_screen_refusal(
    request: &Request<'_>,
    slug: &str,
    rest: &[&str],
) -> Option<AdminRefusal> {
    let ours = request
        .rocket()
        .state::<AdminScreens>()
        .is_some_and(|screens| screens.terms.contains(&slug));
    if !ours && ignite_core::runtime_resource(slug).is_some() {
        log::error!(
            "Refused `{}`: Ignite's resource `{slug}` was not registered by this app.",
            request.uri().path()
        );
        return Some(AdminRefusal::NotServed);
    }
    let screen = PostScreen::parse(request.method().as_str(), rest);
    if !matches!(screen, PostScreen::Delete(_)) {
        return None;
    }
    // A visitor is sent to the sign-in page by Ignite.
    request.guard::<AdminUser>().await.succeeded()?;
    let Some(db) = request.rocket().state::<Arc<Database>>() else {
        log::error!("The admin's authorization ran before the app was ready");
        return Some(AdminRefusal::Failed);
    };
    match authorize_term_screen(db, slug, screen).await {
        Ok(refused) => refused.map(AdminRefusal::Refused),
        Err(e) => {
            log::error!("Could not check the admin request {screen:?} on `{slug}`: {e}");
            Some(AdminRefusal::Failed)
        }
    }
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

/// Answers a refused admin request: WordPress's message with its status (403, or 409 for a post
/// in the trash), or a 500 page when the check could not be made.
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

/// The settings screens' list address, at [`SETTINGS_RANK`], before Ignite's list.
fn settings_routes() -> Vec<Route> {
    let mut routes = rocket::routes![settings_list];
    for route in &mut routes {
        route.rank = SETTINGS_RANK;
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
