//! A theme's `theme.json` and the global stylesheet made from it, as WordPress's
//! `wp_get_global_stylesheet` makes it for a block theme.
//!
//! The file has WordPress's structure (`settings.color.palette`, `settings.typography.fontSizes`,
//! `settings.spacing.spacingSizes`, `settings.layout`, `settings.custom`, `styles`, `styles.elements`
//! and `styles.blocks`), so a theme ported from WordPress keeps its file. The stylesheet uses
//! WordPress's names, so content imported from WordPress keeps its colours and sizes:
//!
//! - the presets as custom properties: `--wp--preset--color--base`,
//!   `--wp--preset--font-size--medium`, `--wp--preset--font-family--manrope`,
//!   `--wp--preset--spacing--50`, and `settings.custom` as `--wp--custom--{path}`;
//! - the layout: `--wp--style--global--content-size` and `--wide-size`, the root padding, and the
//!   `is-layout-constrained`, `is-layout-flow` and `is-layout-flex` rules;
//! - `styles` on `body`, `styles.elements` on links, headings, buttons and captions, and
//!   `styles.blocks` on each block's `wp-block-*` class;
//! - the preset classes: `has-{slug}-color`, `has-{slug}-background-color`,
//!   `has-{slug}-border-color`, `has-{slug}-font-size` and `has-{slug}-font-family`.
//!
//! A fluid font size (`"fluid": {"min": .., "max": ..}`) becomes the `clamp()` WordPress's
//! `wp_get_computed_fluid_typography_value` computes, from a 320px viewport to the layout's wide
//! size.
//!
//! The file is read when the theme is loaded; an edit shows after a restart. Every value is
//! checked before it is written into the `<style>` element: a slug is lowercase letters, digits
//! and `-`, and a value has no `<`, `>`, `{`, `}`, `;`, `\` or control character, so a value cannot
//! end the rule or the element. A value that fails the check is left out.
//!
//! Not yet: style variations (`styles/*.json`), block style variations (`is-style-*`), section
//! styles, shadows, gradients, duotone and the aspect-ratio presets.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use serde_json::{Map, Value};

use super::Theme;

/// The file's name, in the theme's folder.
pub const THEME_JSON: &str = "theme.json";

/// The largest `theme.json` read, in bytes (512 KiB). A larger file is ignored.
pub const MAX_THEME_JSON_BYTES: u64 = 512 * 1024;

/// The smallest viewport a fluid font size scales from, as WordPress's default.
const MIN_VIEWPORT_PX: f64 = 320.0;

/// The largest viewport when neither `settings.typography.fluid.maxViewportWidth` nor
/// `settings.layout.wideSize` gives one, as WordPress's default.
const MAX_VIEWPORT_PX: f64 = 1600.0;

/// The pixels in a `rem`, to compare viewports with `rem` font sizes.
const ROOT_FONT_PX: f64 = 16.0;

/// The `theme.json` of `theme`, or of its parent when the child has none, as WordPress reads
/// the child's file over the parent's (the two are not merged yet). `None` when neither has a
/// readable file. A file that is not valid JSON, is too large or is a symbolic link is logged
/// and skipped.
pub fn theme_json(theme: &Theme) -> Option<(Value, &Theme)> {
    std::iter::once(theme)
        .chain(theme.parent())
        .find_map(|theme| read_json(&theme.dir().join(THEME_JSON)).map(|json| (json, theme)))
}

/// Reads a JSON file that is a regular file (not a link) of at most [`MAX_THEME_JSON_BYTES`].
fn read_json(path: &Path) -> Option<Value> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        log::warn!("`{}` is not a regular file; it is ignored.", path.display());
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_THEME_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_THEME_JSON_BYTES {
        log::warn!(
            "`{}` is over {MAX_THEME_JSON_BYTES} bytes; it is ignored.",
            path.display()
        );
        return None;
    }
    match serde_json::from_slice(&bytes) {
        Ok(value @ Value::Object(_)) => Some(value),
        Ok(_) | Err(_) => {
            log::warn!("`{}` is not a JSON object; it is ignored.", path.display());
            None
        }
    }
}

/// The global stylesheet for a `theme.json`, without the `<style>` element: the variables, the
/// layout, the styles and the preset classes, in WordPress's order.
pub fn global_stylesheet(json: &Value) -> String {
    let settings = json.get("settings");
    let styles = json.get("styles");
    let setting = |path: &[&str]| path.iter().try_fold(settings?, |v, key| v.get(key));
    let mut css = String::new();

    // The variables.
    let mut vars = Vec::new();
    for color in presets(setting(&["color", "palette"])) {
        if let Some(value) = color.get("color").and_then(css_value) {
            vars.push((format!("--wp--preset--color--{}", color.slug), value));
        }
    }
    let fluid = setting(&["typography", "fluid"]).is_some_and(|f| !matches!(f, Value::Bool(false)));
    let max_viewport = setting(&["typography", "fluid", "maxViewportWidth"])
        .or(setting(&["layout", "wideSize"]))
        .and_then(Value::as_str)
        .and_then(parse_length)
        .and_then(|(n, unit)| to_px(n, unit))
        .unwrap_or(MAX_VIEWPORT_PX);
    let min_viewport = setting(&["typography", "fluid", "minViewportWidth"])
        .and_then(Value::as_str)
        .and_then(parse_length)
        .and_then(|(n, unit)| to_px(n, unit))
        .unwrap_or(MIN_VIEWPORT_PX);
    for size in presets(setting(&["typography", "fontSizes"])) {
        let value = font_size_value(size.object, fluid, min_viewport, max_viewport);
        if let Some(value) = value {
            vars.push((format!("--wp--preset--font-size--{}", size.slug), value));
        }
    }
    for family in presets(setting(&["typography", "fontFamilies"])) {
        if let Some(value) = family.get("fontFamily").and_then(css_value) {
            vars.push((format!("--wp--preset--font-family--{}", family.slug), value));
        }
    }
    for spacing in presets(setting(&["spacing", "spacingSizes"])) {
        if let Some(value) = spacing.get("size").and_then(css_value) {
            vars.push((format!("--wp--preset--spacing--{}", spacing.slug), value));
        }
    }
    if let Some(Value::Object(custom)) = setting(&["custom"]) {
        custom_vars(custom, "--wp--custom", &mut vars);
    }
    if !vars.is_empty() {
        rule(&mut css, ":root", &vars);
    }

    // The layout.
    let content_size = setting(&["layout", "contentSize"]).and_then(css_value);
    let wide_size = setting(&["layout", "wideSize"]).and_then(css_value);
    let mut layout_vars = Vec::new();
    if let Some(size) = content_size {
        layout_vars.push(("--wp--style--global--content-size".to_owned(), size));
    }
    if let Some(size) = wide_size {
        layout_vars.push(("--wp--style--global--wide-size".to_owned(), size));
    }
    if !layout_vars.is_empty() {
        rule(&mut css, ":root", &layout_vars);
    }
    let block_gap = styles
        .and_then(|s| s.get("spacing"))
        .and_then(|s| s.get("blockGap"))
        .and_then(css_value)
        .unwrap_or_else(|| "24px".to_owned());
    css.push_str(&LAYOUT_CSS.replace("BLOCK_GAP", &block_gap));

    // `styles` on the body, with the root padding as variables.
    if let Some(Value::Object(root)) = styles {
        let mut body = declarations(root, &["spacing"]);
        let padding = root.get("spacing").and_then(|s| s.get("padding"));
        for side in ["top", "right", "bottom", "left"] {
            let value = padding
                .and_then(|p| p.get(side))
                .and_then(css_value)
                .unwrap_or_else(|| "0px".to_owned());
            body.push((format!("--wp--style--root--padding-{side}"), value));
        }
        rule(&mut css, "body", &body);
        if let Some(Value::Object(elements)) = root.get("elements") {
            element_rules(&mut css, elements, None);
        }
        if let Some(Value::Object(blocks)) = root.get("blocks") {
            for (name, block) in blocks {
                if let (Some(selector), Value::Object(block)) = (block_selector(name), block) {
                    block_rules(&mut css, name, &selector, block);
                }
            }
        }
    }

    // The preset classes.
    for color in presets(setting(&["color", "palette"])) {
        let slug = color.slug;
        let var = format!("var(--wp--preset--color--{slug}) !important");
        for (suffix, property) in [
            ("color", "color"),
            ("background-color", "background-color"),
            ("border-color", "border-color"),
        ] {
            rule(
                &mut css,
                &format!(".has-{slug}-{suffix}"),
                &[(property.to_owned(), var.clone())],
            );
        }
    }
    for size in presets(setting(&["typography", "fontSizes"])) {
        let slug = size.slug;
        rule(
            &mut css,
            &format!(".has-{slug}-font-size"),
            &[(
                "font-size".to_owned(),
                format!("var(--wp--preset--font-size--{slug}) !important"),
            )],
        );
    }
    for family in presets(setting(&["typography", "fontFamilies"])) {
        let slug = family.slug;
        rule(
            &mut css,
            &format!(".has-{slug}-font-family"),
            &[(
                "font-family".to_owned(),
                format!("var(--wp--preset--font-family--{slug}) !important"),
            )],
        );
    }
    css
}

/// The `@font-face` rules for the fonts the `theme.json` bundles
/// (`settings.typography.fontFamilies[].fontFace`), as WordPress's `wp_print_font_faces`. Each
/// `src` written as `file:./path` is turned into a URL by `url`, which returns `None` for a file
/// that is not served (the font is then left out). `font-display: fallback`, as WordPress's
/// default.
pub fn font_faces(json: &Value, url: impl Fn(&str) -> Option<String>) -> String {
    let families = json
        .get("settings")
        .and_then(|s| s.get("typography"))
        .and_then(|t| t.get("fontFamilies"))
        .and_then(Value::as_array);
    let mut css = String::new();
    for face in families
        .into_iter()
        .flatten()
        .filter_map(|family| family.get("fontFace").and_then(Value::as_array))
        .flatten()
    {
        let Some(family) = face.get("fontFamily").and_then(css_value) else {
            continue;
        };
        let sources: Vec<String> = match face.get("src") {
            Some(Value::String(src)) => vec![src.clone()],
            Some(Value::Array(srcs)) => srcs
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let urls: Vec<String> = sources
            .iter()
            .filter_map(|src| src.strip_prefix("file:./"))
            .filter(|rel| is_relative_file(rel))
            .filter_map(&url)
            .filter(|url| css_value(&Value::String(url.clone())).is_some() && !url.contains('"'))
            .map(|url| {
                let format = if url.ends_with(".woff2") {
                    "woff2"
                } else if url.ends_with(".woff") {
                    "woff"
                } else {
                    "truetype"
                };
                format!("url(\"{url}\") format(\"{format}\")")
            })
            .collect();
        if urls.is_empty() {
            continue;
        }
        let mut declarations = vec![("font-family".to_owned(), family)];
        for (key, property) in [
            ("fontStyle", "font-style"),
            ("fontWeight", "font-weight"),
            ("fontStretch", "font-stretch"),
        ] {
            if let Some(value) = face.get(key).and_then(css_value) {
                declarations.push((property.to_owned(), value));
            }
        }
        declarations.push(("font-display".to_owned(), "fallback".to_owned()));
        declarations.push(("src".to_owned(), urls.join(", ")));
        rule(&mut css, "@font-face", &declarations);
    }
    css
}

/// Whether `rel` is a path inside the theme: segments of ASCII letters, digits, `.`, `-` and
/// `_`, none of them empty, `.` or `..`.
fn is_relative_file(rel: &str) -> bool {
    rel.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    })
}

/// A preset of `settings`: an entry with a safe `slug`.
struct Preset<'a> {
    slug: &'a str,
    object: &'a Map<String, Value>,
}

impl Preset<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.object.get(key)
    }
}

/// The entries of a preset list that have a safe slug, in order.
fn presets(list: Option<&Value>) -> Vec<Preset<'_>> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let object = entry.as_object()?;
            let slug = object.get("slug")?.as_str().filter(|s| is_slug(s))?;
            Some(Preset { slug, object })
        })
        .collect()
}

/// Whether `slug` names a preset: lowercase ASCII letters, digits and `-`, at most 64.
fn is_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `settings.custom` as `--wp--custom--{path}` variables, as `WP_Theme_JSON::compute_theme_vars`:
/// each key in kebab case, nested keys joined by `--`.
fn custom_vars(object: &Map<String, Value>, prefix: &str, vars: &mut Vec<(String, String)>) {
    for (key, value) in object {
        let name = kebab(key);
        if !is_slug(&name) {
            continue;
        }
        let name = format!("{prefix}--{name}");
        match value {
            Value::Object(nested) => custom_vars(nested, &name, vars),
            value => {
                if let Some(value) = css_value(value) {
                    vars.push((name, value));
                }
            }
        }
    }
}

/// `_wp_to_kebab_case`: `lineHeight` is `line-height`, `body_2` is `body-2`.
fn kebab(key: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            if previous_lower {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            previous_lower = false;
        } else if c == '_' || c == ' ' {
            out.push('-');
            previous_lower = false;
        } else {
            out.push(c);
            previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// A value as CSS: `var:preset|color|base` is `var(--wp--preset--color--base)` and
/// `var:custom|x|y` is `var(--wp--custom--x--y)`; text and numbers are kept. `None` for a value
/// that could end the rule or the `<style>` element, or that is not text or a number.
fn css_value(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    if text.is_empty()
        || text
            .chars()
            .any(|c| c.is_control() || "<>{};\\".contains(c))
        || text.contains("/*")
    {
        return None;
    }
    if let Some(reference) = text.strip_prefix("var:") {
        let parts: Vec<&str> = reference.split('|').collect();
        if parts.len() < 2 || !parts.iter().all(|p| is_slug(p)) {
            return None;
        }
        return Some(format!("var(--wp--{})", parts.join("--")));
    }
    Some(text)
}

/// A preset font size's value: its `clamp()` when it is fluid, otherwise its `size`.
fn font_size_value(
    size: &Map<String, Value>,
    fluid: bool,
    min_viewport: f64,
    max_viewport: f64,
) -> Option<String> {
    let fixed = size.get("size").and_then(css_value);
    if !fluid {
        return fixed;
    }
    let Some(Value::Object(range)) = size.get("fluid") else {
        return fixed;
    };
    let min = range
        .get("min")
        .and_then(Value::as_str)
        .and_then(parse_length);
    let max = range
        .get("max")
        .and_then(Value::as_str)
        .and_then(parse_length);
    match (min, max) {
        (Some(min), Some(max)) if min.1 == max.1 => {
            fluid_clamp(min, max, min_viewport, max_viewport).or(fixed)
        }
        _ => fixed,
    }
}

/// `wp_get_computed_fluid_typography_value`: `clamp(min, min + ((1vw - offset) * factor), max)`,
/// with the viewports in the font size's unit.
fn fluid_clamp(
    (min, unit): (f64, &str),
    (max, _): (f64, &str),
    min_viewport_px: f64,
    max_viewport_px: f64,
) -> Option<String> {
    let scale = match unit {
        "rem" | "em" => ROOT_FONT_PX,
        "px" => 1.0,
        _ => return None,
    };
    let min_viewport = min_viewport_px / scale;
    let max_viewport = max_viewport_px / scale;
    if max_viewport <= min_viewport {
        return None;
    }
    let offset = round3(min_viewport / 100.0);
    let factor = round3(100.0 * ((max - min) / (max_viewport - min_viewport)));
    let factor = if factor == 0.0 { 1.0 } else { factor };
    let min = format!("{}{unit}", round3(min));
    let max = format!("{}{unit}", round3(max));
    Some(format!(
        "clamp({min}, {min} + ((1vw - {offset}{unit}) * {factor}), {max})"
    ))
}

fn round3(n: f64) -> f64 {
    (n * 1000.0).round() / 1000.0
}

/// A length such as `1.125rem` or `1340px`: its number and unit.
fn parse_length(text: &str) -> Option<(f64, &str)> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    matches!(unit, "px" | "rem" | "em").then_some((number, unit))
}

fn to_px(n: f64, unit: &str) -> Option<f64> {
    match unit {
        "px" => Some(n),
        "rem" | "em" => Some(n * ROOT_FONT_PX),
        _ => None,
    }
}

/// The CSS declarations of a style object (`color`, `typography`, `spacing`, `border`,
/// `outline`), leaving out the groups in `skip`.
fn declarations(style: &Map<String, Value>, skip: &[&str]) -> Vec<(String, String)> {
    const GROUPS: &[(&str, &[(&str, &str)])] = &[
        (
            "color",
            &[
                ("background", "background-color"),
                ("gradient", "background"),
                ("text", "color"),
            ],
        ),
        (
            "typography",
            &[
                ("fontFamily", "font-family"),
                ("fontSize", "font-size"),
                ("fontStyle", "font-style"),
                ("fontWeight", "font-weight"),
                ("letterSpacing", "letter-spacing"),
                ("lineHeight", "line-height"),
                ("textDecoration", "text-decoration"),
                ("textTransform", "text-transform"),
            ],
        ),
        (
            "border",
            &[
                ("color", "border-color"),
                ("radius", "border-radius"),
                ("style", "border-style"),
                ("width", "border-width"),
            ],
        ),
        (
            "outline",
            &[
                ("color", "outline-color"),
                ("offset", "outline-offset"),
                ("style", "outline-style"),
                ("width", "outline-width"),
            ],
        ),
    ];
    let mut out = Vec::new();
    for (group, properties) in GROUPS {
        if skip.contains(group) {
            continue;
        }
        let Some(Value::Object(values)) = style.get(*group) else {
            continue;
        };
        for (key, property) in *properties {
            if let Some(value) = values.get(*key).and_then(css_value) {
                out.push(((*property).to_owned(), value));
            }
        }
    }
    if !skip.contains(&"spacing")
        && let Some(Value::Object(spacing)) = style.get("spacing")
    {
        for box_property in ["padding", "margin"] {
            match spacing.get(box_property) {
                Some(Value::Object(sides)) => {
                    for side in ["top", "right", "bottom", "left"] {
                        if let Some(value) = sides.get(side).and_then(css_value) {
                            out.push((format!("{box_property}-{side}"), value));
                        }
                    }
                }
                Some(value) => {
                    if let Some(value) = css_value(value) {
                        out.push((box_property.to_owned(), value));
                    }
                }
                None => {}
            }
        }
    }
    out
}

/// The selectors of WordPress's elements (`WP_Theme_JSON::ELEMENTS`), each inside `scope` (a
/// block's selector) when given.
fn element_selector(element: &str, scope: Option<&str>) -> Option<String> {
    let selectors: &[&str] = match element {
        "link" => &["a:where(:not(.wp-element-button))"],
        "heading" => &["h1", "h2", "h3", "h4", "h5", "h6"],
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "cite" => return Some(scoped(&[element], scope)),
        "button" => &[".wp-element-button", ".wp-block-button__link"],
        "caption" => &[
            ".wp-element-caption",
            ".wp-block-audio figcaption",
            ".wp-block-embed figcaption",
            ".wp-block-gallery figcaption",
            ".wp-block-image figcaption",
            ".wp-block-table figcaption",
            ".wp-block-video figcaption",
            ".wp-caption-text",
        ],
        _ => return None,
    };
    Some(scoped(selectors, scope))
}

fn scoped(selectors: &[&str], scope: Option<&str>) -> String {
    selectors
        .iter()
        .map(|s| match scope {
            Some(scope) => format!("{scope} {s}"),
            None => (*s).to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The rules of `styles.elements`, and of a block's `elements` inside `scope`.
fn element_rules(css: &mut String, elements: &Map<String, Value>, scope: Option<&str>) {
    // `heading` first, so `h1` to `h6` override it, as WordPress orders them.
    let mut ordered: Vec<(&String, &Value)> = elements.iter().collect();
    ordered.sort_by_key(|(element, _)| element.as_str() != "heading");
    for (element, style) in ordered {
        let (Some(selector), Value::Object(style)) = (element_selector(element, scope), style)
        else {
            continue;
        };
        let wrap = |selector: &str| format!(":root :where({selector})");
        rule(css, &wrap(&selector), &declarations(style, &[]));
        for pseudo in [":hover", ":focus", ":focus-visible", ":active", ":visited"] {
            if let Some(Value::Object(state)) = style.get(pseudo) {
                let with_state = selector
                    .split(", ")
                    .map(|s| format!("{s}{pseudo}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                rule(css, &wrap(&with_state), &declarations(state, &[]));
            }
        }
        if let Some(Value::String(extra)) = style.get("css") {
            custom_css(css, &selector, extra);
        }
    }
}

/// The class of a core block (`core/post-title` is `.wp-block-post-title`), with WordPress's
/// exceptions. The blocks classic content has an equivalent for (a list, a quote, code, a
/// separator) also style that HTML inside a post's content, so TinyMCE content looks like
/// block content. `None` for another namespace's block, or a name that is not a slug.
fn block_selector(name: &str) -> Option<String> {
    let block = name.strip_prefix("core/").filter(|b| is_slug(b))?;
    Some(match block {
        "button" => ".wp-block-button .wp-block-button__link".to_owned(),
        "search" => ".wp-block-search".to_owned(),
        "list" => ".wp-block-list, .wp-block-post-content ol, .wp-block-post-content ul".to_owned(),
        "quote" => ".wp-block-quote, .wp-block-post-content blockquote".to_owned(),
        "code" => ".wp-block-code, .wp-block-post-content pre".to_owned(),
        "separator" => ".wp-block-separator, .wp-block-post-content hr".to_owned(),
        "navigation" => ".wp-block-navigation".to_owned(),
        block => format!(".wp-block-{block}"),
    })
}

/// The rules of one block of `styles.blocks`.
fn block_rules(css: &mut String, name: &str, selector: &str, style: &Map<String, Value>) {
    let wrapped = format!(":root :where({selector})");
    rule(css, &wrapped, &declarations(style, &[]));
    // A flex block's gap; a flow block's gap is its children's margin.
    if let Some(gap) = style
        .get("spacing")
        .and_then(|s| s.get("blockGap"))
        .and_then(css_value)
    {
        if matches!(name, "core/columns" | "core/buttons") {
            rule(css, &wrapped, &[("gap".to_owned(), gap)]);
        } else {
            rule(
                css,
                &format!(":root :where({selector}) > * + *"),
                &[("margin-block-start".to_owned(), gap)],
            );
        }
    }
    if let Some(Value::Object(elements)) = style.get("elements") {
        element_rules(css, elements, Some(selector));
    }
    if let Some(Value::String(extra)) = style.get("css") {
        custom_css(css, selector, extra);
    }
}

/// A block's `css`, as `WP_Theme_JSON::process_blocks_custom_css`: declarations before the
/// first `&` apply to the block, and each `& ...{...}` is a nested rule with `&` the block. Left
/// out when it could end the `<style>` element or its braces do not balance.
fn custom_css(css: &mut String, selector: &str, extra: &str) {
    let balanced = extra.chars().try_fold(0i32, |depth, c| match c {
        '{' => Some(depth + 1),
        '}' => (depth > 0).then_some(depth - 1),
        _ => Some(depth),
    }) == Some(0);
    if !balanced
        || extra
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        || extra.contains(['<', '\\'])
    {
        return;
    }
    let mut parts = extra.split('&');
    let own = parts.next().unwrap_or_default().trim();
    if !own.is_empty() {
        let _ = write!(css, ":root :where({selector}){{{own}}}");
    }
    for nested in parts {
        let nested = nested.trim_end();
        if nested.is_empty() {
            continue;
        }
        let _ = write!(css, ":root :where({selector}){nested}");
    }
}

/// Writes `selector{a: b;c: d;}`, or nothing when there is no declaration.
fn rule(css: &mut String, selector: &str, declarations: &[(String, String)]) {
    if declarations.is_empty() {
        return;
    }
    css.push_str(selector);
    css.push('{');
    for (property, value) in declarations {
        let _ = write!(css, "{property}: {value};");
    }
    css.push('}');
}

/// WordPress's base layout rules (`WP_Theme_JSON::get_root_layout_rules` and the layout
/// definitions), with `BLOCK_GAP` replaced by `styles.spacing.blockGap`.
const LAYOUT_CSS: &str = ":where(body) { margin: 0; }\
.wp-site-blocks { padding-top: var(--wp--style--root--padding-top); padding-bottom: var(--wp--style--root--padding-bottom); }\
.has-global-padding { padding-right: var(--wp--style--root--padding-right); padding-left: var(--wp--style--root--padding-left); }\
.has-global-padding > .alignfull { margin-right: calc(var(--wp--style--root--padding-right) * -1); margin-left: calc(var(--wp--style--root--padding-left) * -1); }\
.has-global-padding :where(:not(.alignfull.is-layout-flow) > .has-global-padding:not(.wp-block-block, .alignfull)) { padding-right: 0; padding-left: 0; }\
.has-global-padding :where(:not(.alignfull.is-layout-flow) > .has-global-padding:not(.wp-block-block, .alignfull)) > .alignfull { margin-left: 0; margin-right: 0; }\
.wp-site-blocks > .alignleft { float: left; margin-right: 2em; }\
.wp-site-blocks > .alignright { float: right; margin-left: 2em; }\
.wp-site-blocks > .aligncenter { justify-content: center; margin-left: auto; margin-right: auto; }\
:where(.wp-site-blocks) > * { margin-block-start: BLOCK_GAP; margin-block-end: 0; }\
:where(.wp-site-blocks) > :first-child { margin-block-start: 0; }\
:where(.wp-site-blocks) > :last-child { margin-block-end: 0; }\
:root { --wp--style--block-gap: BLOCK_GAP; }\
:root :where(.is-layout-flow) > :first-child{margin-block-start: 0;}\
:root :where(.is-layout-flow) > :last-child{margin-block-end: 0;}\
:root :where(.is-layout-flow) > *{margin-block-start: BLOCK_GAP;margin-block-end: 0;}\
:root :where(.is-layout-constrained) > :first-child{margin-block-start: 0;}\
:root :where(.is-layout-constrained) > :last-child{margin-block-end: 0;}\
:root :where(.is-layout-constrained) > *{margin-block-start: BLOCK_GAP;margin-block-end: 0;}\
:root :where(.is-layout-flex){gap: BLOCK_GAP;}\
.is-layout-flow > .alignleft{float: left;margin-inline-start: 0;margin-inline-end: 2em;}\
.is-layout-flow > .alignright{float: right;margin-inline-start: 2em;margin-inline-end: 0;}\
.is-layout-flow > .aligncenter{margin-left: auto !important;margin-right: auto !important;}\
.is-layout-constrained > .alignleft{float: left;margin-inline-start: 0;margin-inline-end: 2em;}\
.is-layout-constrained > .alignright{float: right;margin-inline-start: 2em;margin-inline-end: 0;}\
.is-layout-constrained > .aligncenter{margin-left: auto !important;margin-right: auto !important;}\
.is-layout-constrained > :where(:not(.alignleft):not(.alignright):not(.alignfull)){max-width: var(--wp--style--global--content-size);margin-left: auto !important;margin-right: auto !important;}\
.is-layout-constrained > .alignwide{max-width: var(--wp--style--global--wide-size);}\
body .is-layout-flex{display: flex;}\
.is-layout-flex{flex-wrap: wrap;align-items: center;}\
.is-layout-flex > :is(*, div){margin: 0;}";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fluid_sizes_match_wordpress() {
        let clamp = |min, max| fluid_clamp(min, max, 320.0, 1340.0).unwrap();
        assert_eq!(
            clamp((1.0, "rem"), (1.125, "rem")),
            "clamp(1rem, 1rem + ((1vw - 0.2rem) * 0.196), 1.125rem)"
        );
        assert_eq!(
            clamp((2.15, "rem"), (3.0, "rem")),
            "clamp(2.15rem, 2.15rem + ((1vw - 0.2rem) * 1.333), 3rem)"
        );
    }

    #[test]
    fn values_that_could_escape_are_dropped() {
        assert_eq!(css_value(&json!("#fff")), Some("#fff".to_owned()));
        assert_eq!(
            css_value(&json!("var:preset|color|base")),
            Some("var(--wp--preset--color--base)".to_owned())
        );
        for bad in [
            "red;}body{x:y",
            "</style><script>",
            "a\\62",
            "x /* y */",
            "var:preset|a b",
        ] {
            assert_eq!(css_value(&json!(bad)), None, "{bad}");
        }
    }

    #[test]
    fn custom_settings_are_kebab_case_variables() {
        let css = global_stylesheet(&json!({
            "settings": {"custom": {"lineHeight": {"body": 1.7}, "spacing_unit": "4px"}}
        }));
        assert!(css.contains("--wp--custom--line-height--body: 1.7;"));
        assert!(css.contains("--wp--custom--spacing-unit: 4px;"));
    }

    #[test]
    fn custom_css_nests_under_the_block() {
        let mut css = String::new();
        custom_css(&mut css, ".wp-block-list", "& li{margin-top: 0.5rem;}");
        assert_eq!(css, ":root :where(.wp-block-list) li{margin-top: 0.5rem;}");
        let mut css = String::new();
        custom_css(&mut css, ".x", "}</style><script>{");
        assert!(css.is_empty());
    }
}
