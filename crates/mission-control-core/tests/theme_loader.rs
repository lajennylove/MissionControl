//! Themes: the `theme.toml` manifest, child themes and confined template lookup.

use std::fs;
use std::path::PathBuf;

use mission_control_core::theme::{Support, ThemeError, load_theme};
use propel_runtime::{RenderError, render};
use serde_json::json;

const SECRET: &str = "TOP-SECRET-OUTSIDE-THE-THEMES";

/// A temp `themes` folder, with a `secrets.propel` beside it that no theme may read.
struct Themes {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Themes {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("secrets.propel"), SECRET).unwrap();
        let root = temp.path().join("themes");
        fs::create_dir(&root).unwrap();
        Themes { _temp: temp, root }
    }

    /// Writes a theme folder: its `theme.toml` and `(file, source)` templates.
    fn theme(&self, slug: &str, manifest: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.root.join(slug);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("theme.toml"), manifest).unwrap();
        for (file, source) in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        dir
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

fn manifest_error(themes: &Themes, manifest: &str) -> ThemeError {
    let dir = themes.theme("broken", manifest, &[("index.propel", "")]);
    load_theme(&dir).expect_err("the manifest should be refused")
}

#[test]
fn a_valid_theme_loads_with_its_manifest() {
    let themes = Themes::new();
    let dir = themes.theme(
        "orbit-one",
        r#"
name = "Orbit One"
author = "RocketBoost"
author_uri = "https://rocketboost.rs"
version = "1.0.0"
description = "The default theme."
license = "GPL-2.0-or-later"
requires = "0.1"
supports = ["menus", "post-thumbnails", "custom-logo", "widgets", "html5"]
"#,
        &[("index.propel", "index")],
    );

    let theme = load_theme(&dir).unwrap();

    assert_eq!(theme.slug(), "orbit-one");
    assert!(theme.parent().is_none());
    let manifest = theme.manifest();
    assert_eq!(manifest.name, "Orbit One");
    assert_eq!(manifest.author.as_deref(), Some("RocketBoost"));
    assert_eq!(
        manifest.author_uri.as_deref(),
        Some("https://rocketboost.rs")
    );
    assert_eq!(manifest.version.as_deref(), Some("1.0.0"));
    assert_eq!(manifest.license.as_deref(), Some("GPL-2.0-or-later"));
    assert_eq!(manifest.requires.as_deref(), Some("0.1"));
    assert_eq!(manifest.template, None);
    assert_eq!(
        manifest.supports,
        [
            Support::Menus,
            Support::PostThumbnails,
            Support::CustomLogo,
            Support::Widgets,
            Support::Html5
        ]
    );
    assert!(manifest.supports(Support::Menus));
    assert!(!manifest.supports(Support::TitleTag));
}

#[test]
fn a_theme_without_a_name_is_refused() {
    let themes = Themes::new();
    let error = manifest_error(&themes, "version = \"1.0\"\n");
    assert!(matches!(error, ThemeError::MissingName { .. }), "{error:?}");
    assert!(error.to_string().contains("name"), "{error}");

    let error = manifest_error(&themes, "name = \"   \"\n");
    assert!(matches!(error, ThemeError::MissingName { .. }), "{error:?}");
}

#[test]
fn an_unknown_key_is_refused_by_name() {
    let themes = Themes::new();
    let error = manifest_error(&themes, "name = \"X\"\nlicence = \"GPL\"\n");
    assert!(
        matches!(error, ThemeError::InvalidManifest { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("licence"), "{error}");
}

#[test]
fn an_unknown_support_is_refused_by_name() {
    let themes = Themes::new();
    let error = manifest_error(
        &themes,
        "name = \"X\"\nsupports = [\"menus\", \"jetpack\"]\n",
    );
    assert!(
        matches!(error, ThemeError::InvalidManifest { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("jetpack"), "{error}");
}

#[test]
fn a_folder_without_a_manifest_is_refused() {
    let themes = Themes::new();
    let dir = themes.root.join("bare");
    fs::create_dir(&dir).unwrap();
    let error = load_theme(&dir).unwrap_err();
    assert!(
        matches!(error, ThemeError::MissingManifest { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("theme.toml"), "{error}");
}

#[test]
fn an_oversized_manifest_is_refused_before_it_is_parsed() {
    let themes = Themes::new();
    let padding = "#".repeat(70 * 1024);
    let error = manifest_error(&themes, &format!("name = \"X\"\n{padding}\n"));
    assert!(
        matches!(error, ThemeError::ManifestTooLarge { .. }),
        "{error:?}"
    );
}

#[test]
fn a_standalone_theme_needs_an_index_template() {
    let themes = Themes::new();
    let dir = themes.theme(
        "no-index",
        "name = \"No Index\"\n",
        &[("single.propel", "")],
    );
    let error = load_theme(&dir).unwrap_err();
    assert!(
        matches!(error, ThemeError::MissingIndex { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("index.propel"), "{error}");
}

#[test]
fn a_folder_name_that_is_not_a_slug_is_refused() {
    let themes = Themes::new();
    let dir = themes.theme("my theme", "name = \"Spaces\"\n", &[("index.propel", "")]);
    let error = load_theme(&dir).unwrap_err();
    assert!(matches!(error, ThemeError::InvalidSlug { .. }), "{error:?}");
}

#[test]
fn a_child_theme_names_its_parent_and_inherits_its_templates() {
    let themes = Themes::new();
    themes.theme(
        "base",
        "name = \"Base\"\n",
        &[
            ("index.propel", "base index"),
            ("single.propel", "base single"),
            ("single-post.propel", "base single-post"),
            ("templates/wide.propel", "base wide"),
        ],
    );
    let child = themes.theme(
        "base-child",
        "name = \"Base Child\"\ntemplate = \"base\"\n",
        &[("single.propel", "child single")],
    );

    let theme = load_theme(&child).unwrap();
    assert_eq!(theme.manifest().template.as_deref(), Some("base"));
    let parent = theme.parent().expect("the parent theme is loaded");
    assert_eq!(parent.slug(), "base");
    assert_eq!(parent.manifest().name, "Base");

    let loader = theme.loader();

    // The child's override wins.
    let found = theme.locate_template(&names(&["single", "index"])).unwrap();
    assert_eq!(found.view, "base-child::single");
    assert_eq!(found.name, "single");
    assert_eq!(found.theme, "base-child");
    assert_eq!(
        render(&loader, &found.view, &json!({})).unwrap(),
        "child single"
    );

    // A template only the parent has is found there.
    let found = theme.locate_template(&names(&["index"])).unwrap();
    assert_eq!(found.view, "base::index");
    assert_eq!(found.theme, "base");
    assert_eq!(
        render(&loader, &found.view, &json!({})).unwrap(),
        "base index"
    );

    // As `locate_template`, the more specific name wins over the child: the parent's
    // `single-post` comes before the child's `single`.
    let found = theme
        .locate_template(&names(&[
            "single-post-hello",
            "single-post",
            "single",
            "index",
        ]))
        .unwrap();
    assert_eq!(found.view, "base::single-post");

    // A page template in a folder.
    let found = theme
        .locate_template(&names(&["templates/wide", "page", "index"]))
        .unwrap();
    assert_eq!(found.view, "base::templates.wide");
    assert!(found.path.ends_with("templates/wide.propel"));

    // Nothing found.
    assert!(theme.locate_template(&names(&["404"])).is_none());
}

#[test]
fn a_child_template_extends_a_parent_layout() {
    let themes = Themes::new();
    themes.theme(
        "base",
        "name = \"Base\"\n",
        &[
            ("index.propel", ""),
            (
                "layouts/app.propel",
                "<main>@yield('content')</main>@include('footer')",
            ),
            ("footer.propel", "<footer>base</footer>"),
            (
                "page.propel",
                "@extends('layouts.app')@section('content')base page@endsection",
            ),
        ],
    );
    let child = themes.theme(
        "kid",
        "name = \"Kid\"\ntemplate = \"base\"\n",
        &[
            (
                "single.propel",
                "@extends('layouts.app')@section('content')kid single@endsection",
            ),
            ("footer.propel", "<footer>kid</footer>"),
        ],
    );
    let theme = load_theme(&child).unwrap();
    let loader = theme.loader();

    let found = theme.locate_template(&names(&["single", "index"])).unwrap();
    assert_eq!(
        render(&loader, &found.view, &json!({})).unwrap(),
        "<main>kid single</main><footer>kid</footer>"
    );

    // A parent template rendered for the child still picks up the child's partials, as
    // WordPress's `get_template_part` does.
    let found = theme.locate_template(&names(&["page", "index"])).unwrap();
    assert_eq!(found.view, "base::page");
    assert_eq!(
        render(&loader, &found.view, &json!({})).unwrap(),
        "<main>base page</main><footer>kid</footer>"
    );
}

fn assert_confined(result: &Result<String, RenderError>) {
    assert!(
        matches!(result, Err(RenderError::Load { .. })),
        "{result:?}"
    );
    if let Ok(html) = result {
        assert!(!html.contains(SECRET), "secret leaked: {html}");
    }
}

#[test]
fn a_child_cannot_include_outside_the_theme_roots() {
    let themes = Themes::new();
    themes.theme("base", "name = \"Base\"\n", &[("index.propel", "")]);
    let child = themes.theme(
        "kid",
        "name = \"Kid\"\ntemplate = \"base\"\n",
        &[
            ("dotdot.propel", "@include('../../secrets')"),
            ("plain.propel", "@include('secrets')"),
            ("other.propel", "@include('other::secrets')"),
        ],
    );
    let theme = load_theme(&child).unwrap();
    let loader = theme.loader();

    for view in ["kid::dotdot", "kid::plain", "kid::other"] {
        assert_confined(&render(&loader, view, &json!({})));
    }
    // An unqualified top-level name never reaches a root outside the themes either.
    assert_confined(&render(&loader, "secrets", &json!({})));
}

#[cfg(unix)]
#[test]
fn a_template_symlinked_outside_the_theme_is_not_located() {
    let themes = Themes::new();
    let dir = themes.theme(
        "linked",
        "name = \"Linked\"\n",
        &[("index.propel", "index")],
    );
    std::os::unix::fs::symlink(
        themes.root.parent().unwrap().join("secrets.propel"),
        dir.join("single.propel"),
    )
    .unwrap();
    let theme = load_theme(&dir).unwrap();

    let found = theme.locate_template(&names(&["single", "index"])).unwrap();
    assert_eq!(found.view, "linked::index");
    assert_confined(&render(&theme.loader(), "linked::single", &json!({})));
}

#[test]
fn names_propel_cannot_map_are_skipped() {
    let themes = Themes::new();
    let dir = themes.theme("plain", "name = \"Plain\"\n", &[("index.propel", "")]);
    let theme = load_theme(&dir).unwrap();

    let found = theme
        .locate_template(&names(&[
            "page-caf%c3%a9",
            "page-café",
            "application-vnd.ms-excel",
            "a/b/c",
            "../index",
            "/index",
            "",
            "index",
        ]))
        .unwrap();
    assert_eq!(found.view, "plain::index");
}

#[test]
fn a_missing_parent_is_refused_by_name() {
    let themes = Themes::new();
    let child = themes.theme("orphan", "name = \"Orphan\"\ntemplate = \"gone\"\n", &[]);
    let error = load_theme(&child).unwrap_err();
    assert!(
        matches!(error, ThemeError::MissingParent { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("gone"), "{message}");
    assert!(message.contains("install"), "{message}");
}

#[test]
fn a_grandparent_chain_is_refused() {
    let themes = Themes::new();
    themes.theme("grand", "name = \"Grand\"\n", &[("index.propel", "")]);
    themes.theme("middle", "name = \"Middle\"\ntemplate = \"grand\"\n", &[]);
    let child = themes.theme("young", "name = \"Young\"\ntemplate = \"middle\"\n", &[]);
    let error = load_theme(&child).unwrap_err();
    assert!(
        matches!(error, ThemeError::InvalidParent { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("middle"), "{error}");
}

#[test]
fn a_theme_naming_itself_as_parent_is_refused() {
    let themes = Themes::new();
    let dir = themes.theme(
        "selfish",
        "name = \"Selfish\"\ntemplate = \"selfish\"\n",
        &[],
    );
    let error = load_theme(&dir).unwrap_err();
    assert!(matches!(error, ThemeError::OwnParent { .. }), "{error:?}");
}

#[test]
fn a_parent_slug_that_is_not_safe_is_refused_before_it_is_joined() {
    let themes = Themes::new();
    let dir = themes.theme(
        "sneaky",
        "name = \"Sneaky\"\ntemplate = \"../themes/base\"\n",
        &[],
    );
    let error = load_theme(&dir).unwrap_err();
    assert!(
        matches!(error, ThemeError::InvalidParentSlug { .. }),
        "{error:?}"
    );
}

#[test]
fn echo_escapes_and_raw_echo_does_not() {
    let themes = Themes::new();
    let dir = themes.theme(
        "escape",
        "name = \"Escape\"\n",
        &[("index.propel", "{{ $title }}|{!! $html !!}")],
    );
    let theme = load_theme(&dir).unwrap();
    let html = render(
        &theme.loader(),
        "escape::index",
        &json!({ "title": "<b>Hi</b>", "html": "<b>Hi</b>" }),
    )
    .unwrap();
    assert_eq!(html, "&lt;b&gt;Hi&lt;/b&gt;|<b>Hi</b>");
}

#[test]
fn a_theme_is_found_by_its_folder_even_through_a_relative_path() {
    let themes = Themes::new();
    let dir = themes.theme("dotted", "name = \"Dotted\"\n", &[("index.propel", "")]);
    let theme = load_theme(&dir.join(".")).unwrap();
    assert_eq!(theme.slug(), "dotted");
    assert_eq!(theme.dir(), dir);
}

#[cfg(unix)]
#[test]
fn a_manifest_symlinked_outside_the_theme_is_refused_unread() {
    let themes = Themes::new();
    let outside = themes.root.parent().unwrap().join("outside.toml");
    fs::write(&outside, format!("name = \"X\"\nleak = \"{SECRET}\"\n")).unwrap();
    let dir = themes.root.join("linked-manifest");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("index.propel"), "").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("theme.toml")).unwrap();

    let error = load_theme(&dir).unwrap_err();
    assert!(
        matches!(error, ThemeError::ManifestNotAFile { .. }),
        "{error:?}"
    );
    assert!(!error.to_string().contains(SECRET), "{error}");
    assert!(!error.to_string().contains("leak"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_fifo_manifest_is_refused_without_blocking() {
    let themes = Themes::new();
    let dir = themes.root.join("fifo");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("index.propel"), "").unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(dir.join("theme.toml"))
        .status()
        .unwrap();
    assert!(status.success());

    // If the loader opened the FIFO for reading it would block forever; run it on a thread and
    // give up after a few seconds.
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(load_theme(&dir).map(|_| ()));
    });
    let result = receive
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("loading a FIFO manifest blocked");
    let error = result.unwrap_err();
    assert!(
        matches!(error, ThemeError::ManifestNotAFile { .. }),
        "{error:?}"
    );
}

#[test]
fn manifest_errors_give_the_line_and_column_but_never_a_value() {
    let themes = Themes::new();

    let error = manifest_error(&themes, "name = \"X\"\n\nlicence = \"secret-value\"\n");
    let message = error.to_string();
    assert!(message.contains("licence"), "{message}");
    assert!(message.contains("line 3, column 1"), "{message}");
    assert!(!message.contains("secret-value"), "{message}");

    let error = manifest_error(&themes, "name = \"X\"\nsupports = \"secret-value\"\n");
    let message = error.to_string();
    assert!(message.contains("line 2"), "{message}");
    assert!(!message.contains("secret-value"), "{message}");

    let error = manifest_error(&themes, "name = \"X\"\nversion = 1.0\n");
    let message = error.to_string();
    assert!(message.contains("line 2"), "{message}");
    assert!(!message.contains("1.0"), "{message}");

    let error = manifest_error(&themes, "name = \"X\"\nauthor = \"secret-value\n");
    let message = error.to_string();
    assert!(message.contains("line 2"), "{message}");
    assert!(!message.contains("secret-value"), "{message}");
}

#[test]
fn a_child_cannot_extend_outside_the_theme_roots() {
    let themes = Themes::new();
    themes.theme("base", "name = \"Base\"\n", &[("index.propel", "")]);
    let child = themes.theme(
        "kid",
        "name = \"Kid\"\ntemplate = \"base\"\n",
        &[
            ("dotdot.propel", "@extends('../../secrets')"),
            ("other.propel", "@extends('other::secrets')"),
        ],
    );
    let theme = load_theme(&child).unwrap();
    let loader = theme.loader();

    for view in ["kid::dotdot", "kid::other"] {
        assert_confined(&render(&loader, view, &json!({})));
    }
}

#[test]
fn menu_locations_and_sidebars_are_read_in_order_and_imply_support() {
    let themes = Themes::new();
    let dir = themes.theme(
        "areas",
        r#"
name = "Areas"
supports = ["title-tag"]

[menus]
primary = "Primary menu"
footer_links = "Footer links"
alpha = "Comes last"

[sidebars]
sidebar = "Sidebar"
footer = "Footer"
"#,
        &[("index.propel", "")],
    );
    let theme = load_theme(&dir).unwrap();
    let manifest = theme.manifest();

    let menus: Vec<(&str, &str)> = manifest
        .menus
        .iter()
        .map(|m| (m.location.as_str(), m.description.as_str()))
        .collect();
    assert_eq!(
        menus,
        [
            ("primary", "Primary menu"),
            ("footer_links", "Footer links"),
            ("alpha", "Comes last")
        ]
    );
    let sidebars: Vec<(&str, &str)> = manifest
        .sidebars
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();
    assert_eq!(sidebars, [("sidebar", "Sidebar"), ("footer", "Footer")]);
    // `register_nav_menus` and `register_sidebar` add the theme support, as in WordPress.
    assert!(manifest.supports(Support::Menus));
    assert!(manifest.supports(Support::Widgets));
}

#[test]
fn a_child_theme_inherits_its_parents_menu_locations_and_sidebars() {
    let themes = Themes::new();
    themes.theme(
        "base",
        "name = \"Base\"\n[menus]\nprimary = \"Primary\"\nsocial = \"Social\"\n[sidebars]\nfooter = \"Footer\"\n",
        &[("index.propel", "")],
    );
    let child = themes.theme(
        "kid",
        "name = \"Kid\"\ntemplate = \"base\"\n[menus]\nprimary = \"Main menu\"\n",
        &[],
    );
    let theme = load_theme(&child).unwrap();

    let menus: Vec<(String, String)> = theme
        .menu_locations()
        .into_iter()
        .map(|m| (m.location, m.description))
        .collect();
    assert_eq!(
        menus,
        [
            ("primary".to_owned(), "Main menu".to_owned()),
            ("social".to_owned(), "Social".to_owned())
        ]
    );
    let sidebars: Vec<String> = theme.sidebars().into_iter().map(|s| s.id).collect();
    assert_eq!(sidebars, ["footer"]);
}

#[test]
fn menu_and_sidebar_tables_are_strict() {
    let themes = Themes::new();
    for (manifest, expected) in [
        // Ids are template names: `menus.primary`, so no `-` and no capitals.
        ("name = \"X\"\n[menus]\nsidebar-1 = \"A\"\n", "sidebar-1"),
        ("name = \"X\"\n[sidebars]\nFooter = \"A\"\n", "Footer"),
        ("name = \"X\"\n[sidebars]\n1st = \"A\"\n", "1st"),
        ("name = \"X\"\n[menus]\nprimary = 3\n", "string"),
        (
            "name = \"X\"\n[sidebars]\nfooter = { name = \"A\" }\n",
            "string",
        ),
        ("name = \"X\"\nmenus = [\"primary\"]\n", "expected"),
        ("name = \"X\"\n[sidebars]\nfooter = \"\"\n", "empty"),
    ] {
        let error = manifest_error(&themes, manifest);
        assert!(
            matches!(error, ThemeError::InvalidManifest { .. }),
            "{manifest}: {error:?}"
        );
        assert!(error.to_string().contains(expected), "{manifest}: {error}");
    }
    // A key that is not safe to repeat is not echoed.
    let error = manifest_error(&themes, "name = \"X\"\n[menus]\n\"<script>\" = \"A\"\n");
    assert!(!error.to_string().contains("<script>"), "{error}");
}

#[test]
fn reserved_widget_area_ids_are_refused() {
    let themes = Themes::new();
    for id in ["wp_inactive_widgets", "array_version"] {
        let error = manifest_error(
            &themes,
            &format!("name = \"X\"\n[sidebars]\n{id} = \"A\"\n"),
        );
        assert!(error.to_string().contains("reserved"), "{error}");
    }
}

#[test]
fn page_templates_are_found_by_their_header_and_merged_with_the_parents() {
    let themes = Themes::new();
    themes.theme(
        "base",
        "name = \"Base\"\n",
        &[
            ("index.propel", ""),
            (
                "templates/wide.propel",
                "{{-- Template Name: Full width --}}\n@extends('layouts.app')\n",
            ),
            (
                "landing.propel",
                "{{--\nTemplate Name: Landing\n--}}\nlanding",
            ),
            // No header: not a page template.
            ("plain.propel", "{{-- Template Name: Parent plain --}}"),
            // A template for another post type only.
            (
                "single-book.propel",
                "{{-- Template Name: Book\n     Template Post Type: book, Movie --}}\n",
            ),
            // Two folders down: not scanned, as WordPress looks one folder deep.
            ("a/b/deep.propel", "{{-- Template Name: Deep --}}"),
            ("node_modules/x.propel", "{{-- Template Name: Vendored --}}"),
            ("vendor/x.propel", "{{-- Template Name: Vendored --}}"),
            // A header past the first 8 KiB is not read.
            (
                "late.propel",
                &format!("{}\n{{{{-- Template Name: Late --}}}}", "x".repeat(9000)),
            ),
            (".hidden/x.propel", "{{-- Template Name: Hidden --}}"),
        ],
    );
    let child = themes.theme(
        "kid",
        "name = \"Kid\"\ntemplate = \"base\"\n",
        &[
            (
                "landing.propel",
                "{{-- template name:   A landing page   --}}",
            ),
            (
                "templates/sidebar.propel",
                "{{-- Template Name: Blog sidebar --}}\n",
            ),
            // A child file without a header hides the parent's file of the same name.
            ("plain.propel", "child plain"),
            (
                "templates/both.propel",
                "{{-- Template Name: Both\nTemplate Post Type: post, page --}}",
            ),
        ],
    );
    let theme = load_theme(&child).unwrap();

    let templates: Vec<(String, String)> = theme
        .page_templates()
        .into_iter()
        .map(|t| (t.name, t.title))
        .collect();
    // The child's file wins; sorted by the name the admin shows.
    assert_eq!(
        templates,
        [
            ("landing".to_owned(), "A landing page".to_owned()),
            ("templates/sidebar".to_owned(), "Blog sidebar".to_owned()),
            ("templates/both".to_owned(), "Both".to_owned()),
            ("templates/wide".to_owned(), "Full width".to_owned()),
        ]
    );
}

#[test]
fn the_templates_table_is_gone() {
    // Page templates come from the files' headers (G48); the old table is an unknown key.
    let themes = Themes::new();
    let error = manifest_error(
        &themes,
        "name = \"X\"\n[templates]\n\"templates/wide\" = \"Full width\"\n",
    );
    assert!(
        matches!(error, ThemeError::InvalidManifest { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("templates"), "{error}");
}

#[test]
fn template_headers_are_read_as_wordpress_reads_them() {
    use mission_control_core::theme::page_template_header;
    let header = |text: &str| page_template_header(text);
    assert_eq!(
        header("{{-- Template Name: Wide --}}"),
        Some(("Wide".to_owned(), None))
    );
    assert_eq!(
        header(" * Template Name: Wide */"),
        Some(("Wide".to_owned(), None))
    );
    assert_eq!(
        header("<?php /* Template Name: Wide */ ?>"),
        Some(("Wide".to_owned(), None))
    );
    assert_eq!(
        header("<? /* Template Name: Short */"),
        Some(("Short".to_owned(), None))
    );
    // A carriage return alone ends a line, as `get_file_data` reads it.
    assert_eq!(
        header("{{-- Template Name: X\rTemplate Post Type: post --}}"),
        Some(("X".to_owned(), Some(vec!["post".to_owned()])))
    );
    // A post type that cleans to nothing is not `page`.
    assert_eq!(
        header("{{-- Template Name: X\nTemplate Post Type: !! --}}"),
        Some(("X".to_owned(), Some(vec![String::new()])))
    );
    assert_eq!(header("<p>Template Name: Wide</p>"), None);
    assert_eq!(header("{{-- Template Name:   --}}"), None);
    assert_eq!(
        header("{{-- Template Name: W\r\nTemplate Post Type: Page , my-type --}}"),
        Some((
            "W".to_owned(),
            Some(vec!["page".to_owned(), "my-type".to_owned()])
        ))
    );
}
