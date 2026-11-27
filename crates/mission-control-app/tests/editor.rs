//! The admin's classic editor (Task 6.3): TinyMCE and Mission Control's editor script, served at
//! `/mc-includes`, loaded by Ignite's form for the post content, and the HTML it sends stored as
//! sent.
//!
//! The files are served on every database; the test of them runs on SQLite. The form flow needs
//! the panel, which runs on MySQL and MariaDB only (Ruling R119): it runs when
//! `MC_TEST_DATABASE_URL` is a `mysql://` or `mariadb://` URL, on a database of its own
//! ([`scratch_database`]), dropped at the end; otherwise it says why it is skipped and passes.
//! Ignite reads its database from `DATABASE_URL`, so the flow sets it for the process; every
//! test here holds [`LOCK`] so none runs while it changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignite_panel::{authz, users};
use mission_control::{AppOptions, editor_script_url, editor_version, rocket_with_options};
use mission_control_app::install::run::run_migrations;
use mission_control_core::config::{Config, Environment, TrustedProxies};
use mission_control_core::content::Post;
use mission_control_core::migrations::migrator;
use mission_control_testing::{ScratchDatabase, create_users_table, scratch_database};
use orbit_orm::{Database, Model};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::{Client, LocalResponse};

/// Held by every test here: the flow test sets `DATABASE_URL` for the process.
static LOCK: Mutex<()> = Mutex::new(());

/// The password of the test user.
const PASSWORD: &str = "correct horse battery staple";

/// The editor's files in the repository.
fn assets() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/editor")
}

/// The themes shipped with Mission Control.
fn shipped_themes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes")
}

/// `MC_TEST_DATABASE_URL` when it names MySQL or MariaDB.
fn mysql_test_url() -> Option<String> {
    std::env::var(mission_control_testing::DATABASE_URL_VAR)
        .ok()
        .filter(|url| url.starts_with("mysql://") || url.starts_with("mariadb://"))
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

/// Every file under `dir`, as paths relative to it with `/` separators.
fn files_under(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).unwrap();
                let parts: Vec<String> = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn header<'a>(response: &'a LocalResponse<'_>, name: &str) -> &'a str {
    response.headers().get_one(name).unwrap_or_default()
}

#[test]
fn the_editor_files_are_served() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    rocket::async_test(files_are_served());
}

async fn files_are_served() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("editor.db").display()
    );
    let db = Arc::new(Database::connect(&url).await.unwrap());
    create_users_table(&db).await;
    migrator().unwrap().run(&db).await.unwrap();
    let client = Client::tracked(rocket_with_options(
        config(&url),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");
    let ver = editor_version();

    // TinyMCE, the editor script, their styles: each with its type, `nosniff`, and a year's
    // cache when the URL carries the current version.
    for (path, file, content_type) in [
        (
            "js/tinymce/tinymce.min.js",
            "tinymce/tinymce.min.js",
            "text/javascript; charset=utf-8",
        ),
        (
            "js/editor.js",
            "editor.js",
            "text/javascript; charset=utf-8",
        ),
        ("css/editor.css", "editor.css", "text/css; charset=utf-8"),
        (
            "js/featured-image.js",
            "featured.js",
            "text/javascript; charset=utf-8",
        ),
        (
            "js/media-upload.js",
            "upload.js",
            "text/javascript; charset=utf-8",
        ),
        (
            "js/tinymce/skins/ui/oxide/skin.min.css",
            "tinymce/skins/ui/oxide/skin.min.css",
            "text/css; charset=utf-8",
        ),
        (
            "js/tinymce/license.md",
            "tinymce/license.md",
            "text/plain; charset=utf-8",
        ),
    ] {
        let uri = format!("/mc-includes/{path}?ver={ver}");
        let response = client.get(uri.clone()).dispatch().await;
        assert_eq!(response.status(), Status::Ok, "{uri}");
        assert_eq!(header(&response, "Content-Type"), content_type, "{uri}");
        assert_eq!(header(&response, "X-Content-Type-Options"), "nosniff");
        assert_eq!(
            header(&response, "Cache-Control"),
            "public, max-age=31536000, immutable",
            "{uri}"
        );
        let body = response.into_bytes().await.unwrap();
        assert_eq!(body, std::fs::read(assets().join(file)).unwrap(), "{uri}");
    }

    // The editor script loads the featured image box when the form has its field, and the
    // dialogs open centred over the page (the panel's reset takes their `margin: auto` away).
    let script = std::fs::read_to_string(assets().join("editor.js")).unwrap();
    assert!(script.contains("'featured-image.js'"), "editor.js");
    assert!(
        script.contains("[data-ignite-field=\"featured_media\"]"),
        "editor.js"
    );
    let css = std::fs::read_to_string(assets().join("editor.css")).unwrap();
    let centred = css
        .split_once(".mc-editor-dialog,\n.mc-media-dialog {")
        .map(|(_, rule)| rule.split_once('}').map_or("", |(rule, _)| rule))
        .unwrap_or_default();
    assert!(centred.contains("margin: auto;"), "{centred}");
    assert!(centred.contains("position: fixed;"), "{centred}");

    // HEAD answers with the same headers and no body.
    let response = client
        .head(format!("/mc-includes/js/tinymce/tinymce.min.js?ver={ver}"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        header(&response, "Content-Type"),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(header(&response, "X-Content-Type-Options"), "nosniff");
    assert_eq!(
        header(&response, "Cache-Control"),
        "public, max-age=31536000, immutable"
    );
    assert!(response.into_bytes().await.unwrap_or_default().is_empty());

    // Without the current version, the file is still sent, but not kept.
    for uri in [
        "/mc-includes/js/editor.js".to_owned(),
        "/mc-includes/js/editor.js?ver=7.9.3-000000000000".to_owned(),
    ] {
        let response = client.get(uri.clone()).dispatch().await;
        assert_eq!(response.status(), Status::Ok, "{uri}");
        assert_eq!(header(&response, "Cache-Control"), "no-cache", "{uri}");
        assert_eq!(header(&response, "X-Content-Type-Options"), "nosniff");
    }

    // Every file of TinyMCE's folder is served, unchanged: the bundle is the folder.
    let tinymce = assets().join("tinymce");
    let bundled = files_under(&tinymce);
    assert!(bundled.len() >= 14, "{bundled:?}");
    for file in &bundled {
        let uri = format!("/mc-includes/js/tinymce/{file}");
        let response = client.get(uri.clone()).dispatch().await;
        assert_eq!(response.status(), Status::Ok, "{uri}");
        let body = response.into_bytes().await.unwrap();
        assert_eq!(body, std::fs::read(tinymce.join(file)).unwrap(), "{uri}");
    }

    // Nothing outside the bundle: a traversal, encoded or not, a plugin not bundled, the folder
    // itself. The public site answers with the theme's 404 page.
    for uri in [
        "/mc-includes/js/tinymce/../../../Cargo.toml",
        "/mc-includes/js/tinymce/..%2f..%2f..%2fCargo.toml",
        "/mc-includes/js/%2e%2e/%2e%2e/%2e%2e/Cargo.toml",
        "/mc-includes/..%5c..%5cCargo.toml",
        "/mc-includes/js/tinymce/plugins/table/plugin.min.js",
        "/mc-includes/js/tinymce/tinymce.js",
        "/mc-includes/js/tinymce/",
        "/mc-includes/",
        "/mc-includes",
    ] {
        let response = client.get(uri).dispatch().await;
        assert_eq!(response.status(), Status::NotFound, "{uri}");
        assert_ne!(header(&response, "Cache-Control"), "no-cache", "{uri}");
        let body = response.into_string().await.unwrap_or_default();
        assert!(!body.contains("[workspace]"), "{uri}: {body}");
    }
    db.close().await;
}

/// A form body, URL-encoded.
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

/// The CSRF token in a page's form.
fn token_in(html: &str) -> String {
    let start = html
        .find("name=\"_token\" value=\"")
        .unwrap_or_else(|| panic!("a form token in: {html}"))
        + "name=\"_token\" value=\"".len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
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

#[test]
fn the_post_form_loads_the_editor_and_stores_its_html_as_sent() {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(_) = mysql_test_url() else {
        eprintln!(
            "skipped: the admin runs on MySQL or MariaDB only (Ruling R119); set \
             MC_TEST_DATABASE_URL to a mysql:// URL to run this test"
        );
        return;
    };
    let scratch = rocket::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scratch_database("editor"));
    // SAFETY: no other thread reads or writes the environment now. Every test of this process
    // holds `LOCK`, and the runtime above is gone; the next one starts after this call.
    unsafe { std::env::set_var("DATABASE_URL", scratch.url()) };
    rocket::async_test(form_flow(&scratch));
    // SAFETY: as above: the runtime of the flow has ended, and `LOCK` is held.
    unsafe { std::env::remove_var("DATABASE_URL") };
}

async fn form_flow(scratch: &ScratchDatabase) {
    run_migrations(scratch.url()).await.expect("migrates");
    let db = Arc::new(Database::connect(scratch.url()).await.expect("connects"));
    let client = Client::tracked(rocket_with_options(
        config(scratch.url()),
        AppOptions {
            database: Some(Arc::clone(&db)),
            ..AppOptions::default()
        },
    ))
    .await
    .expect("the app ignites");

    // An editor, signed in through Ignite's form (the roles exist once the app has started).
    let email = "edna@example.test";
    users::create_user(&db, "edna", email, PASSWORD)
        .await
        .expect("creates the user");
    authz::assign_role(&db, email, "editor")
        .await
        .expect("gives the role");
    let (_, login) = get(&client, "/mc-admin/login").await;
    let response = post(
        &client,
        "/mc-admin/login",
        &[
            ("_token", &token_in(&login)),
            ("email", email),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(response.status(), Status::SeeOther, "signs in");

    // New shows the create form, with no post yet: the content is a textarea marked for the
    // editor, and the form loads the registered editor script.
    let (status, new) = get(&client, "/mc-admin/post/new").await;
    assert_eq!(status, Status::Ok, "{new}");
    assert!(
        new.contains(r#"id="content" name="content" data-ignite-editor="wysiwyg""#),
        "{new}"
    );
    let script = format!(r#"<script src="{}" defer></script>"#, editor_script_url());
    assert!(new.contains(&script), "{script} in {new}");
    // The featured image's field, which the editor script turns into WordPress's box.
    assert!(
        new.contains(r#"<div data-ignite-field="featured_media">"#),
        "{new}"
    );
    assert!(
        new.contains(r#"name="featured_media" type="number""#),
        "{new}"
    );

    // The script it names is served, cached for a year.
    let response = client.get(editor_script_url()).dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        header(&response, "Cache-Control"),
        "public, max-age=31536000, immutable"
    );

    // Saving the form stores the HTML byte for byte: what TinyMCE produces (paragraphs, a list,
    // a link, an image from the uploads, the Read More tag, raw UTF-8), what the Text tab may
    // send (an iframe and a script kses would strip, an attribute without quotes), and the
    // CRLF line breaks a browser sends for a textarea.
    let html = "<p>Hello <strong>world</strong> — ñandú &amp; “quotes”</p>\r\n\
                <ul>\r\n<li>one</li>\r\n<li><a href=\"/mc-content/uploads/2026/11/a.pdf\" \
                target=\"_blank\" rel=\"noopener\">two</a></li>\r\n</ul>\r\n\
                <!--more Keep reading-->\r\n\
                <p><img src=\"/mc-content/uploads/2026/11/photo.jpg\" alt=\"A photo\" \
                width=\"300\" height=\"200\" /></p>\r\n\
                <iframe src=\"https://www.youtube.com/embed/x\" width=560 allowfullscreen>\
                </iframe>\r\n<script>console.log(1)</script>\r\n\
                <p style=\"text-align: center;\">  spaced  </p>\r\n";
    // The New screen's save creates the post and opens its edit form.
    let response = post(
        &client,
        "/mc-admin/post",
        &[
            ("_token", &token_in(&new)),
            ("title", "Written in the editor"),
            ("content", html),
            ("status", "draft"),
        ],
    )
    .await;
    let status = response.status();
    let edit = header(&response, "Location").to_owned();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::SeeOther, "saves: {body}");
    let id: i64 = edit
        .strip_prefix("/mc-admin/post/")
        .and_then(|rest| rest.strip_suffix("/edit"))
        .unwrap_or_else(|| panic!("redirects to the edit form: {edit}"))
        .parse()
        .unwrap();
    let saved = Post::query()
        .find(&*db, id)
        .await
        .expect("reads the post")
        .expect("the post exists");
    assert_eq!(saved.content, html);
    assert_eq!(saved.content.as_bytes(), html.as_bytes());

    // The form shows it again in the textarea, escaped, for the editor to load.
    let (status, page) = get(&client, &edit).await;
    assert_eq!(status, Status::Ok);
    assert!(page.contains("&lt;iframe src="), "{page}");
    assert!(page.contains("&lt;!--more Keep reading--&gt;"), "{page}");
    assert!(
        !page.contains("<iframe src=\"https://www.youtube.com"),
        "{page}"
    );

    db.close().await;
}

/// The Visual tab shows a `[caption]` as the captioned image (WordPress's wpcaption plugin,
/// `parseShortcode`), and writes the same shortcode back (`getShortcode`), so the saved content
/// is unchanged. Runs the script's two functions with Node when it is installed; otherwise
/// checks that the editor uses them.
#[test]
fn captions_show_as_images_in_the_visual_tab() {
    let path = assets().join("editor.js");
    let script = std::fs::read_to_string(&path).unwrap();
    assert!(
        script.contains("event.content = toCaptions(toPlaceholders(event.content));"),
        "editor.js"
    );
    assert!(
        script.contains("event.content = fromPlaceholders(fromCaptions(event.content));"),
        "editor.js"
    );
    let check = r#"
const src = require('fs').readFileSync(process.argv[1], 'utf8');
const take = (name) => {
  const start = src.indexOf('  function ' + name + '(');
  let depth = 0;
  for (let i = src.indexOf('{', start); ; i++) {
    if (src[i] === '{') depth++;
    if (src[i] === '}' && --depth === 0) return src.slice(start, i + 1);
  }
};
eval(take('escapeAttribute') + take('trim') + take('toCaptions') + take('fromCaptions'));
const shortcode = '[caption id="attachment_140" align="aligncenter" width="80"]' +
  '<img class="size-thumbnail wp-image-140" src="/a.png" alt="" width="80" height="80" />' +
  ' modal caption[/caption]';
const visual = toCaptions('<p>' + shortcode + '</p>');
if (visual.indexOf('[caption') !== -1) throw new Error('shortcode text shown: ' + visual);
if (visual.indexOf('<dl id="attachment_140" class="wp-caption aligncenter"') === -1 ||
    visual.indexOf('<dd class="wp-caption-dd">modal caption</dd>') === -1) {
  throw new Error('no captioned image: ' + visual);
}
const back = fromCaptions(visual);
if (back !== shortcode) throw new Error('not the same shortcode: ' + back);
"#;
    match std::process::Command::new("node")
        .arg("-e")
        .arg(check)
        .arg(&path)
        .output()
    {
        Ok(output) => assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(e) => eprintln!("Node is not installed ({e}); the caption round trip is not run"),
    }
}
