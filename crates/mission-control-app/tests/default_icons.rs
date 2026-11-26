//! The Ignite admin theme shows Mission Control's sidebar icon names as Tabler icons.
//!
//! Mission Control names its sidebar icons by what an entry is (`post`, `media`, ...,
//! `mission_control_core::admin::navigation`); each admin theme maps a name to its own classes.
//! This test locks in that the default theme (Ignite's, `ignite_panel::theme::IgniteTheme`) maps
//! every name Mission Control uses to a Tabler class, that a custom post type's Dashicons
//! `menu_icon` falls back to the theme's resource icon, and that a hostile `menu_icon` never
//! reaches the sidebar's markup.
//!
//! The rendered-sidebar part needs MySQL or MariaDB (Ruling R119): it runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, otherwise it says so and the rest
//! of the test still runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignite_panel::theme::{
    AdminTheme, IGNITE_RESOURCE_ICON, IgniteTheme, icon_classes, icons, ignite_icon,
};
use ignite_panel::{authz, users};
use mission_control::{AppOptions, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::admin::navigation::{
    APPEARANCE_ICON, COMMENTS_ICON, MEDIA_ICON, PAGE_ICON, PLUGINS_ICON, POST_ICON, SETTINGS_ICON,
    TOOLS_ICON, USERS_ICON,
};
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::types::{Origin, PostTypeDef, Registry};
use mission_control_testing::{ScratchDatabase, scratch_database};
use orbit_orm::Database;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};

const PASSWORD: &str = "correct horse battery staple";

/// A `menu_icon` that would break out of an HTML attribute if a theme printed it.
const HOSTILE_ICON: &str = "dashicons-x\" onmouseover=\"alert(1)\"><script>alert(2)</script>";

fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
}

fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

fn config(database_url: &str) -> Config {
    Config {
        database_url: database_url.to_owned(),
        secret_key: "unused: Ignite reads IGNITE_SECRET_KEY".to_owned(),
        debug: false,
        upload_max_bytes: 1024,
        themes_dir: shipped_themes(),
        uploads_dir: std::env::temp_dir().join("mc-unused-uploads"),
        plugins_dir: std::env::temp_dir().join("mc-unused-plugins"),
        environment: Environment::Local,
        trusted_proxies: TrustedProxies::None,
    }
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, NON_ALPHANUMERIC),
                utf8_percent_encode(v, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

async fn get(client: &Client, uri: &str) -> (Status, String) {
    let response = client.get(uri.to_owned()).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

async fn post<'c>(client: &'c Client, uri: &str, fields: &[(&str, &str)]) -> LocalResponse<'c> {
    client
        .post(uri.to_owned())
        .header(ContentType::Form)
        .body(form(fields))
        .dispatch()
        .await
}

/// The CSRF token of the client's browser session, read from the sign-in page.
async fn token(client: &Client) -> String {
    let (status, html) = get(client, "/mc-admin/login").await;
    assert_eq!(status, Status::Ok, "{html}");
    let marker = "name=\"_token\" value=\"";
    let start = html
        .find(marker)
        .unwrap_or_else(|| panic!("a form token in: {html}"))
        + marker.len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
}

async fn sign_in(client: &Client, email: &str) {
    let csrf = token(client).await;
    let response = post(
        client,
        "/mc-admin/login",
        &[("_token", &csrf), ("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs {email} in");
}

#[test]
fn the_default_theme_shows_mission_controls_icon_names_as_tabler_icons() {
    // Every name Mission Control uses, plus the dashboard's, which Ignite names itself.
    let names = [
        icons::DASHBOARD,
        POST_ICON,
        MEDIA_ICON,
        PAGE_ICON,
        COMMENTS_ICON,
        APPEARANCE_ICON,
        PLUGINS_ICON,
        USERS_ICON,
        TOOLS_ICON,
        SETTINGS_ICON,
    ];
    for name in names {
        let classes = ignite_icon(name).unwrap_or_else(|| panic!("`{name}` has no Ignite icon"));
        assert!(
            classes.starts_with("ti ti-") && classes.len() > "ti ti-".len(),
            "`{name}` maps to `{classes}`"
        );
        assert_eq!(
            icon_classes(&IgniteTheme, Some(name)),
            classes,
            "the theme resolves `{name}`"
        );
        assert_ne!(
            classes, IGNITE_RESOURCE_ICON,
            "`{name}` is not the fallback"
        );
    }
    // A Dashicons name, or a value that would break out of an attribute, is not a known name:
    // the theme shows its resource icon, which is a fixed class string.
    for name in ["dashicons-calendar", HOSTILE_ICON] {
        assert_eq!(IgniteTheme.icon(name), None, "{name}");
        assert_eq!(icon_classes(&IgniteTheme, Some(name)), "ti ti-table");
    }
    assert_eq!(IGNITE_RESOURCE_ICON, "ti ti-table");
    assert_eq!(icon_classes(&IgniteTheme, None), "ti ti-table");

    // The rendered sidebar (needs MySQL or MariaDB).
    let Some(_) = mysql_test_url() else {
        eprintln!(
            "skipped the rendered sidebar: the admin runs on MySQL or MariaDB only (Ruling \
             R119); set MC_TEST_DATABASE_URL to a mysql:// URL to run it"
        );
        return;
    };
    let scratch = rocket::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scratch_database("icons"));
    rocket::async_test(async { run_migrations(scratch.url()).await.expect("migrates") });
    // SAFETY: this is the only test of this binary, and no other thread reads or writes the
    // environment: the runtimes above have ended.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(sidebar(&scratch));
    // SAFETY: as above; the runtime of the sidebar flow has ended.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

/// A public post type with a `menu_icon`.
fn post_type(slug: &str, plural: &str, icon: &str) -> PostTypeDef {
    PostTypeDef::from_json(
        &serde_json::json!({
            "slug": slug,
            "public": true,
            "menu_icon": icon,
            "labels": {"name": plural, "singular_name": slug},
        })
        .to_string(),
        Origin::Admin,
    )
    .expect("a valid post type")
}

async fn sidebar(scratch: &ScratchDatabase) {
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
    // Two stored types are read at ignition: a Dashicons icon, and a hostile one.
    // Three admin-made types, stored in the database through a registry of their own, so that
    // no app of this process holds their resources: the app below reads them at ignition.
    let registry = Registry::new(Arc::new(PostTypes::new()), Arc::new(Taxonomies::new()));
    for def in [
        post_type("event", "Events", "dashicons-calendar"),
        post_type("trick", "Tricks", HOSTILE_ICON),
        post_type("plain", "Plains", "none"),
    ] {
        registry
            .save_admin_definition(&db, def)
            .await
            .expect("saves the admin's type");
    }
    // A fresh app on the same database reads the stored types at ignition.
    let client = Client::tracked(rocket_with_options(
        config(scratch.url()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    eprintln!(
        "DEBUG {:?} {:?}",
        client
            .rocket()
            .state::<Arc<Registry>>()
            .unwrap()
            .all_post_types()
            .iter()
            .map(|d| d.slug.clone())
            .collect::<Vec<_>>(),
        0
    );
    let email = "ada@example.test";
    users::create_user(&db, "ada", email, PASSWORD)
        .await
        .expect("creates the user");
    authz::assign_role(&db, email, "administrator")
        .await
        .expect("gives the role");
    sign_in(&client, email).await;
    let (status, dashboard) = get(&client, "/mc-admin").await;
    assert_eq!(status, Status::Ok, "{dashboard}");
    let start = dashboard.find("aria-label=\"Main\"").expect("the sidebar");
    let nav = &dashboard[start..dashboard[start..].find("</nav>").unwrap() + start];

    // Mission Control's own entries and the dashboard are Tabler icons.
    for entry in [
        "<i class=\"ti ti-layout-dashboard text-lg\"></i>Dashboard",
        "<i class=\"ti ti-pin text-lg\"></i>Posts",
        "<i class=\"ti ti-photo text-lg\"></i>Media",
        "<i class=\"ti ti-file-text text-lg\"></i>Pages",
        "<i class=\"ti ti-brush text-lg\"></i>Menus",
        "<i class=\"ti ti-settings text-lg\"></i>Settings</a>",
    ] {
        assert!(nav.contains(entry), "`{entry}` in {nav}");
    }
    // A custom type's Dashicons name shows the resource icon, and no markup carries the name.
    assert!(
        nav.contains("<i class=\"ti ti-table text-lg\"></i>Events"),
        "{nav}"
    );
    assert!(!nav.contains("dashicons"), "{nav}");
    // A hostile or non-name icon is the post icon (`post_type_icon`), with no markup of its own.
    for label in ["Tricks", "Plains"] {
        assert!(
            nav.contains(&format!("<i class=\"ti ti-pin text-lg\"></i>{label}")),
            "{label} in {nav}"
        );
    }
    for hostile in ["onmouseover", "<script>alert", "alert(", "dashicons-x"] {
        assert!(!dashboard.contains(hostile), "`{hostile}` in {dashboard}");
    }
}
