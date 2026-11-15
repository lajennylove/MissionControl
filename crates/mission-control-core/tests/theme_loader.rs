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
