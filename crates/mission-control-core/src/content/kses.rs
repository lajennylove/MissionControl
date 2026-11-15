//! kses, WordPress's HTML filter on save: the markup a user without the `unfiltered_html`
//! capability may store in a post.
//!
//! WordPress runs it from `kses_init_filters` when the acting user lacks `unfiltered_html`: the
//! content and the excerpt go through `wp_filter_post_kses` (the `$allowedposttags` list), and
//! the title through `wp_filter_kses` (the smaller `$allowedtags` list). Administrators and
//! editors hold `unfiltered_html` on a single site, so what they send is stored as sent.
//! Mission Control departs from WordPress for titles (Ruling R39): a title is text, so
//! [`kses_title`] strips every tag and decodes entities once, and themes escape it.
//!
//! **Every path that writes a post's title, content or excerpt for a user must pass the fields
//! through [`kses_post_fields`] first** (the REST API's and the admin's create, update and
//! autosave routes, and any import that acts for a user). [`Post::create`](super::Post::create),
//! [`Post::save`](super::Post::save), [`save_revision`](super::save_revision),
//! [`create_post_autosave`](super::create_post_autosave) and the attachment writers store what
//! they are given; they do not know who is acting. Because rows can still be written without
//! kses (by an import, or by code before this filter existed), the front end keeps escaping
//! manual excerpts as text (Ruling R34), and a post's content is trusted as WordPress trusts it.
//!
//! The HTML is parsed and written back with [`ammonia`] (html5ever, the parser of the HTML
//! standard), configured with WordPress's lists:
//!
//! - the tags and attributes of `$allowedposttags` (WordPress 6.8), each with the global
//!   attributes of `_wp_add_global_attributes` (`aria-*` of the list, `class`, `data-*`, `dir`,
//!   `hidden`, `id`, `lang`, `style`, `title`, `role`, `xml:lang`); `<object>` is left out,
//!   because WordPress only keeps it for a PDF in the site's own uploads
//!   (`_wp_kses_allow_pdf_objects`);
//! - the tags and attributes of `$allowedtags`, without global attributes ([`kses_data`]);
//! - the protocols of `wp_allowed_protocols` in every URL attribute (`href`, `src`, `cite`,
//!   `longdesc`, `usemap`, `poster`); relative URLs are kept;
//! - the `style` attribute through [`safecss_filter_attr`].
//!
//! Where it differs from WordPress, it removes more: the content of `<script>` and `<style>`
//! goes with the tags; HTML comments are removed, except the page markers WordPress reads from
//! the content (`<!--more-->`, with its optional link text, `<!--noteaser-->` and
//! `<!--nextpage-->`), so block-editor comments (`<!-- wp:paragraph -->`) do not survive; and
//! the output is well-formed (tags closed, attributes quoted, entities written once: `&` as
//! `&amp;`, `&eacute;` as `é`).
//!
//! ## Bounds and known refusals
//!
//! - **Size (Ruling R42).** [`kses_post_fields`] refuses a field longer than
//!   [`MAX_FILTERED_BYTES`] (256 KB) for a user without `unfiltered_html`, with [`FieldTooLarge`];
//!   the save reports it and stores nothing. Parsing is linear but not free (about 0.35 s and
//!   340 MB for 480 KB), so this is the real bound on a save. WordPress has no such cap.
//!   [`kses_post`], [`kses_data`] and [`kses_title`] themselves take any size; call them through
//!   the helper, or check the size first.
//! - **Shape.** Input nested deeper than [`MAX_NESTING`] elements (the parser's time grows with
//!   length times depth), holding more than [`MAX_ACTIVE_FORMATTING`] formatting elements active
//!   at once, with a formatting element (`<a>`, `<b>`, `<em>`…) whose start tag has more than
//!   [`MAX_FORMATTING_ATTRIBUTES`] (4 KB) of attributes, even when it is closed at once, or that
//!   would make the parser copy more start-tag bytes than about the input's length (it reopens
//!   every unclosed formatting element, attributes and all, in each new block, and clones
//!   misnested ones), is not parsed: it is stored as escaped text, so nothing
//!   runs and nothing typed is lost (a title is kept as typed). So is input whose length times
//!   its number of `&` and `0xC2` bytes passes ten billion (html5ever 0.40's serialiser rescans
//!   a value after each of them).
//! - **Output.** Output longer than eight times the input is refused the same way, as a backstop.
//! - **Malformed but valid HTML** can be refused, because the pre-scan counts conservatively:
//!   more than 32 formatting elements left unclosed or misnested (`<b><p></b>`), more than 512
//!   elements left unclosed, or an unclosed formatting element with long attributes followed by
//!   many blocks. One unclosed link with a long `href` (or a long `style`) followed by many
//!   paragraphs is refused by the pre-scan, cheaply, since the parser would copy the `href` into
//!   every paragraph; a short unclosed `<b>` across thousands of paragraphs is kept, as browsers
//!   show it. A link whose `href` (or a formatting element whose `style`, `title`…) passes 4 KB
//!   is refused even when it is well-formed; other elements (`<span>`, `<img>`, `<p>`…) keep
//!   long attributes. A formatting end tag that does not close the last element open (such as
//!   `</b>` after `<b><p><b></p>`) leaves every `<b>` counted as active, so sloppy markup of that
//!   kind followed by many blocks can be refused too.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use ammonia::{Builder, Url, UrlRelative};

use crate::users::{UserCaps, has_cap};

/// The protocols a URL may use, as WordPress's `wp_allowed_protocols`.
const ALLOWED_PROTOCOLS: &[&str] = &[
    "http", "https", "ftp", "ftps", "mailto", "news", "irc", "irc6", "ircs", "gopher", "nntp",
    "feed", "telnet", "mms", "rtsp", "sms", "svn", "tel", "fax", "xmpp", "webcal", "urn",
];

/// The attributes every tag of `$allowedposttags` also allows, as `_wp_add_global_attributes`;
/// `data-*` is checked by name in [`filter_attribute`].
const GLOBAL_ATTRIBUTES: &[&str] = &[
    "aria-controls",
    "aria-current",
    "aria-describedby",
    "aria-details",
    "aria-expanded",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-live",
    "class",
    "dir",
    "hidden",
    "id",
    "lang",
    "style",
    "title",
    "role",
    "xml:lang",
];

/// WordPress's `$allowedposttags`: each tag and its own attributes. `<object>` is left out (see
/// the module documentation).
const ALLOWED_POST_TAGS: &[(&str, &[&str])] = &[
    ("address", &[]),
    ("a", &["href", "rel", "rev", "name", "target", "download"]),
    ("abbr", &[]),
    ("acronym", &[]),
    (
        "area",
        &["alt", "coords", "href", "nohref", "shape", "target"],
    ),
    ("article", &["align"]),
    ("aside", &["align"]),
    (
        "audio",
        &["autoplay", "controls", "loop", "muted", "preload", "src"],
    ),
    ("b", &[]),
    ("bdo", &[]),
    ("big", &[]),
    ("blockquote", &["cite"]),
    ("br", &[]),
    ("button", &["disabled", "name", "type", "value"]),
    ("caption", &["align"]),
    ("cite", &[]),
    ("code", &[]),
    (
        "col",
        &["align", "char", "charoff", "span", "valign", "width"],
    ),
    (
        "colgroup",
        &["align", "char", "charoff", "span", "valign", "width"],
    ),
    ("del", &["datetime"]),
    ("dd", &[]),
    ("dfn", &[]),
    ("details", &["align", "open"]),
    ("div", &["align"]),
    ("dl", &[]),
    ("dt", &[]),
    ("em", &[]),
    ("fieldset", &[]),
    ("figure", &["align"]),
    ("figcaption", &["align"]),
    ("font", &["color", "face", "size"]),
    ("footer", &["align"]),
    ("h1", &["align"]),
    ("h2", &["align"]),
    ("h3", &["align"]),
    ("h4", &["align"]),
    ("h5", &["align"]),
    ("h6", &["align"]),
    ("header", &["align"]),
    ("hgroup", &["align"]),
    ("hr", &["align", "noshade", "size", "width"]),
    ("i", &[]),
    (
        "img",
        &[
            "alt", "align", "border", "height", "hspace", "loading", "longdesc", "vspace", "src",
            "usemap", "width",
        ],
    ),
    ("ins", &["datetime", "cite"]),
    ("kbd", &[]),
    ("label", &["for"]),
    ("legend", &["align"]),
    ("li", &["align", "value"]),
    ("main", &["align"]),
    ("map", &["name"]),
    ("mark", &[]),
    ("menu", &["type"]),
    ("nav", &["align"]),
    ("p", &["align"]),
    ("pre", &["width"]),
    ("q", &["cite"]),
    ("rb", &[]),
    ("rp", &[]),
    ("rt", &[]),
    ("rtc", &[]),
    ("ruby", &[]),
    ("s", &[]),
    ("samp", &[]),
    ("span", &["align"]),
    ("section", &["align"]),
    ("small", &[]),
    ("strike", &[]),
    ("strong", &[]),
    ("sub", &[]),
    ("summary", &["align"]),
    ("sup", &[]),
    (
        "table",
        &[
            "align",
            "bgcolor",
            "border",
            "cellpadding",
            "cellspacing",
            "rules",
            "summary",
            "width",
        ],
    ),
    ("tbody", &["align", "char", "charoff", "valign"]),
    (
        "td",
        &[
            "abbr", "align", "axis", "bgcolor", "char", "charoff", "colspan", "headers", "height",
            "nowrap", "rowspan", "scope", "valign", "width",
        ],
    ),
    (
        "textarea",
        &["cols", "rows", "disabled", "name", "readonly"],
    ),
    ("tfoot", &["align", "char", "charoff", "valign"]),
    (
        "th",
        &[
            "abbr", "align", "axis", "bgcolor", "char", "charoff", "colspan", "headers", "height",
            "nowrap", "rowspan", "scope", "valign", "width",
        ],
    ),
    ("thead", &["align", "char", "charoff", "valign"]),
    ("title", &[]),
    ("tr", &["align", "bgcolor", "char", "charoff", "valign"]),
    ("track", &["default", "kind", "label", "src", "srclang"]),
    ("tt", &[]),
    ("u", &[]),
    ("ul", &["type"]),
    ("ol", &["start", "type", "reversed"]),
    ("var", &[]),
    (
        "video",
        &[
            "autoplay",
            "controls",
            "height",
            "loop",
            "muted",
            "playsinline",
            "poster",
            "preload",
            "src",
            "width",
        ],
    ),
];

/// WordPress's `$allowedtags`, for titles: each tag and its attributes, with no global
/// attributes.
const ALLOWED_TAGS: &[(&str, &[&str])] = &[
    ("a", &["href", "title"]),
    ("abbr", &["title"]),
    ("acronym", &["title"]),
    ("b", &[]),
    ("blockquote", &["cite"]),
    ("cite", &[]),
    ("code", &[]),
    ("del", &["datetime"]),
    ("em", &[]),
    ("i", &[]),
    ("q", &["cite"]),
    ("s", &[]),
    ("strike", &[]),
    ("strong", &[]),
];

/// The attributes, among those allowed, that hold a URL, as WordPress's
/// `wp_kses_uri_attributes`; each must use an allowed protocol or be relative.
const URI_ATTRIBUTES: &[&str] = &["cite", "href", "longdesc", "poster", "src", "usemap"];

/// The elements removed with their content rather than unwrapped.
const CLEAN_CONTENT_TAGS: &[&str] = &["script", "style"];

/// The CSS properties `safecss_filter_attr` allows (its `safe_style_css` list, WordPress 6.8);
/// custom properties (`--name`) are allowed too.
const SAFE_STYLE_CSS: &[&str] = &[
    "background",
    "background-color",
    "background-image",
    "background-position",
    "background-repeat",
    "background-size",
    "background-attachment",
    "background-blend-mode",
    "border",
    "border-radius",
    "border-width",
    "border-color",
    "border-style",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "border-bottom-style",
    "border-bottom-width",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-top",
    "border-top-color",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-top-style",
    "border-top-width",
    "border-spacing",
    "border-collapse",
    "caption-side",
    "columns",
    "column-count",
    "column-fill",
    "column-gap",
    "column-rule",
    "column-span",
    "column-width",
    "color",
    "filter",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-variant",
    "font-weight",
    "letter-spacing",
    "line-height",
    "text-align",
    "text-decoration",
    "text-indent",
    "text-transform",
    "height",
    "min-height",
    "max-height",
    "width",
    "min-width",
    "max-width",
    "margin",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "margin-top",
    "margin-block-start",
    "margin-block-end",
    "margin-inline-start",
    "margin-inline-end",
    "padding",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "padding-top",
    "padding-block-start",
    "padding-block-end",
    "padding-inline-start",
    "padding-inline-end",
    "flex",
    "flex-basis",
    "flex-direction",
    "flex-flow",
    "flex-grow",
    "flex-shrink",
    "flex-wrap",
    "gap",
    "row-gap",
    "grid-template-columns",
    "grid-auto-columns",
    "grid-column-start",
    "grid-column-end",
    "grid-column",
    "grid-column-gap",
    "grid-template-rows",
    "grid-auto-rows",
    "grid-row-start",
    "grid-row-end",
    "grid-row",
    "grid-row-gap",
    "grid-gap",
    "justify-content",
    "justify-items",
    "justify-self",
    "align-content",
    "align-items",
    "align-self",
    "clear",
    "cursor",
    "direction",
    "float",
    "list-style-type",
    "object-fit",
    "object-position",
    "opacity",
    "overflow",
    "vertical-align",
    "writing-mode",
    "position",
    "top",
    "right",
    "bottom",
    "left",
    "z-index",
    "box-shadow",
    "aspect-ratio",
    "container-type",
];

/// The CSS properties whose value may hold `url()`, as `safecss_filter_attr`'s
/// `$css_url_data_types`.
const CSS_URL_DATA_TYPES: &[&str] = &[
    "background",
    "background-image",
    "cursor",
    "filter",
    "list-style",
    "list-style-image",
];

/// The CSS properties whose value may be a gradient, as `$css_gradient_data_types`.
const CSS_GRADIENT_DATA_TYPES: &[&str] = &["background", "background-image"];

/// The CSS functions `safecss_filter_attr` allows anywhere in a value, longest first so that
/// `minmax` is not read as `min`.
const CSS_FUNCTIONS: &[&str] = &["minmax", "repeat", "clamp", "calc", "var", "min", "max"];

/// Text no declaration may contain, in any case, wherever it is (even inside an allowed
/// function or `url()`): old browsers' script hooks in CSS, and escapes and comments that could
/// hide them. WordPress's own checks reject most of these outside functions; this list also
/// covers the inside.
const CSS_BANNED: &[&str] = &[
    "expression",
    "javascript:",
    "vbscript:",
    "behavior",
    "-moz-binding",
    "\\",
    "/*",
];

/// The deepest nesting of elements kses parses, as browsers cap it (512). Past it, the input is
/// not parsed: [`kses_post`] and [`kses_data`] store it as escaped text, [`kses_title`] as
/// typed. It bounds the cost of the parser's scans of its open elements.
pub const MAX_NESTING: usize = 512;

/// The sanitiser for post content and excerpts.
static POST_SANITIZER: LazyLock<Builder<'static>> =
    LazyLock::new(|| sanitizer(ALLOWED_POST_TAGS, GLOBAL_ATTRIBUTES, true));

/// The sanitiser for `$allowedtags` ([`kses_data`]).
static DATA_SANITIZER: LazyLock<Builder<'static>> =
    LazyLock::new(|| sanitizer(ALLOWED_TAGS, &[], false));

/// The sanitiser for titles ([`kses_title`]): no tags at all.
static TEXT_SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| sanitizer(&[], &[], false));

/// A sanitiser that keeps `tags` with their attributes, `global` attributes on every tag (and
/// `data-*` when `data_attributes`), URLs with an allowed protocol, and filtered styles.
fn sanitizer(
    tags: &'static [(&'static str, &'static [&'static str])],
    global: &'static [&'static str],
    data_attributes: bool,
) -> Builder<'static> {
    let mut builder = Builder::empty();
    builder
        .tags(tags.iter().map(|(tag, _)| *tag).collect())
        .tag_attributes(
            tags.iter()
                .map(|(tag, attributes)| (*tag, attributes.iter().copied().collect()))
                .collect::<HashMap<_, HashSet<_>>>(),
        )
        .generic_attributes(global.iter().copied().collect())
        .clean_content_tags(CLEAN_CONTENT_TAGS.iter().copied().collect())
        .url_schemes(ALLOWED_PROTOCOLS.iter().copied().collect())
        .url_relative(UrlRelative::PassThrough)
        // WordPress keeps the `rel` an author writes and adds none.
        .link_rel(None)
        .strip_comments(true)
        .attribute_filter(filter_attribute);
    if data_attributes {
        builder.generic_attribute_prefixes(HashSet::from(["data-"]));
    }
    builder
}

/// The value an allowed attribute keeps, or `None` to remove it: a `data-*` name must be
/// `data-` and lower-case letters, digits, `_` or `-` (`wp_kses_attr_check`), a URL attribute
/// must use an allowed protocol or be relative, and `style` goes through
/// [`safecss_filter_attr`] and is removed when nothing is left.
fn filter_attribute<'u>(_element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
    if let Some(rest) = attribute.strip_prefix("data-") {
        let valid = !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        return valid.then_some(Cow::Borrowed(value));
    }
    if URI_ATTRIBUTES.contains(&attribute) {
        return allowed_url(value).then_some(Cow::Borrowed(value));
    }
    if attribute == "style" {
        let css = safecss_filter_attr(value);
        return (!css.is_empty()).then_some(Cow::Owned(css));
    }
    Some(Cow::Borrowed(value))
}

/// Whether `url` is relative or uses one of [`ALLOWED_PROTOCOLS`], read as a browser reads it
/// (leading spaces and control characters, tabs and line breaks ignored; the scheme in any
/// case), as WordPress's `wp_kses_bad_protocol` decides.
fn allowed_url(url: &str) -> bool {
    match Url::parse(url) {
        Ok(url) => ALLOWED_PROTOCOLS.contains(&url.scheme()),
        Err(ammonia::url::ParseError::RelativeUrlWithoutBase) => true,
        Err(_) => false,
    }
}

/// `html` filtered for a post's content or excerpt, as WordPress's `wp_kses_post`: only the tags
/// and attributes of `$allowedposttags` (with the global attributes), URLs with an allowed
/// protocol, `style` through [`safecss_filter_attr`], and the page markers `<!--more-->`,
/// `<!--noteaser-->` and `<!--nextpage-->`. Everything else is removed: other tags are
/// unwrapped (their text kept), `<script>` and `<style>` go with their content, and other
/// comments go. Unlike WordPress, the output is well-formed HTML (tags closed, attributes
/// quoted, entities written once), and block-editor comments (`<!-- wp:… -->`) are removed.
/// Input nested deeper than [`MAX_NESTING`], with more than [`MAX_ACTIVE_FORMATTING`]
/// formatting elements open at once, with too many `&` for its length, or whose output would
/// pass the output budget, is stored as escaped text.
pub fn kses_post(html: &str) -> String {
    if !parse_is_bounded(html) {
        return escape_text(html);
    }
    let (protected, token, markers) = protect_markers(html);
    let Some(cleaned) = serialize_within(&POST_SANITIZER.clean(&protected), output_budget(html))
    else {
        return escape_text(html);
    };
    restore_markers(&cleaned, &token, &markers)
}

/// `html`, the sanitiser's output, with each placeholder replaced by its marker where the
/// marker is a comment between elements, and removed where it is not: inside a tag (an
/// attribute value) or inside the text of `<textarea>` or `<title>`, where a marker would be
/// text, and splitting the content at it (for a teaser or a page) would cut the tag open.
///
/// The output is well-formed, so the scan is simple: in text, `<` only opens a tag (text and
/// `<textarea>`/`<title>` text write it as `&lt;`; comments, `<script>` and `<style>` are gone);
/// in a tag, every attribute value is in `"` and writes its own `"` as `&quot;`.
fn restore_markers(html: &str, token: &str, markers: &[String]) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_quote = false;
    // The closing tag that ends the `<textarea>` or `<title>` text we are in.
    let mut text_element: Option<&str> = None;
    let mut i = 0;
    while i < html.len() {
        let rest = &html[i..];
        if let Some(after) = rest.strip_prefix(token)
            && let Some(end) = after.find('x')
            && let Ok(index) = after[..end].parse::<usize>()
            && let Some(marker) = markers.get(index)
        {
            if !in_tag && text_element.is_none() {
                out.push_str(marker);
            }
            i += token.len() + end + 1;
            continue;
        }
        let c = rest.chars().next().expect("i is on a char boundary");
        if in_tag {
            if c == '"' {
                in_quote = !in_quote;
            } else if c == '>' && !in_quote {
                in_tag = false;
            }
        } else if c == '<' {
            in_tag = true;
            if let Some(close) = text_element {
                if rest.starts_with(close) {
                    text_element = None;
                }
            } else if starts_tag(rest, "textarea") {
                text_element = Some("</textarea");
            } else if starts_tag(rest, "title") {
                text_element = Some("</title");
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Whether `html` starts with the start tag of element `name` (`<name>` or `<name …>`).
fn starts_tag(html: &str, name: &str) -> bool {
    html.strip_prefix('<')
        .and_then(|rest| rest.strip_prefix(name))
        .is_some_and(|rest| rest.starts_with(['>', ' ', '/']))
}

/// `html` filtered as WordPress's `wp_filter_kses` filters titles and comments: only the tags
/// and attributes of `$allowedtags` (`a` with `href` and `title`, `abbr`, `acronym`, `b`,
/// `blockquote`, `cite`, `code`, `del`, `em`, `i`, `q`, `s`, `strike`, `strong`), with URLs
/// checked as in [`kses_post`]. Comments are removed. Mission Control's titles use
/// [`kses_title`] instead (Ruling R39).
pub fn kses_data(html: &str) -> String {
    if !parse_is_bounded(html) {
        return escape_text(html);
    }
    serialize_within(&DATA_SANITIZER.clean(html), output_budget(html))
        .unwrap_or_else(|| escape_text(html))
}

/// `title` as plain text, for a user without `unfiltered_html` (Ruling R39: a title is text,
/// and themes escape it when they print it): every tag is stripped (`<script>` and `<style>`
/// with their content, and comments), and entities are decoded once, so `A & <em>B</em>` and
/// `A &amp; B` are both stored as `A & B`, and `&amp;lt;` as `&lt;`.
///
/// WordPress instead keeps the `$allowedtags` markup in titles ([`kses_data`]) and prints them
/// raw; Ruling R39 departs from it.
pub fn kses_title(title: &str) -> String {
    if !parse_is_bounded(title) {
        // Not parsed: kept as typed, which is text, as a title is.
        return title.to_owned();
    }
    match serialize_within(&TEXT_SANITIZER.clean(title), output_budget(title)) {
        Some(text) => decode_serialized_text(&text),
        None => title.to_owned(),
    }
}

/// `html` as text, escaped as the sanitiser escapes text (`&`, `<`, `>` and the no-break
/// space), so that it reads back as the same text and contains no markup. What kses stores for
/// input past [`MAX_NESTING`], [`MAX_ACTIVE_FORMATTING`], the escaping work or the output
/// budget.
fn escape_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + html.len() / 4);
    for c in html.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
    out
}

/// The elements that never have content, so never stay open (HTML's void elements).
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "image", "img",
    "input", "keygen", "link", "meta", "param", "source", "track", "wbr",
];

/// The elements [`nesting_within_cap`] does not count toward the depth (outside SVG and
/// MathML): the parser closes them itself when a sibling starts or their parent ends (`p`;
/// table sections, rows and cells, which only open inside a counted `<table>` or `<template>`),
/// or merges them into the document's own (`html`, `head`, `body`). They can only nest with a
/// counted element in between (a cell inside a table inside a cell, a paragraph inside a
/// button), so leaving them out lets the parser's real depth reach about four times the cap,
/// and keeps unclosed `<p>` and table cells from adding up. (`li`, `dd`, `dt`, `option` and
/// ruby text are counted: they close a sibling only in some contexts.)
const UNCOUNTED_ELEMENTS: &[&str] = &[
    "html", "head", "body", "p", "tr", "td", "th", "tbody", "thead", "tfoot", "caption", "colgroup",
];

/// The formatting elements the parser keeps in its list of active formatting elements and
/// reopens in every new block until they are closed.
const FORMATTING_ELEMENTS: &[&str] = &[
    "a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong", "tt", "u",
];

/// The most formatting elements (`<b>`, `<em>`, `<a>`…) kses lets stay open at once. The parser
/// reopens every unclosed one in each new block, so the output can grow by this many elements
/// per paragraph; past it, the input is not parsed, as past [`MAX_NESTING`]. Post content needs
/// a handful at most.
pub const MAX_ACTIVE_FORMATTING: usize = 32;

/// The most attribute bytes (everything between the name and the closing `>`) a formatting
/// element's start tag may have; past it, the input is not parsed, as past
/// [`MAX_NESTING`]. The parser copies these bytes each time it reopens or clones the element, so
/// this bounds the cost of each copy even where the pre-scan's model of the tree is wrong. A
/// link's `href` or a `style` longer than 4 KB is rare in post content.
pub const MAX_FORMATTING_ATTRIBUTES: usize = 4 * 1024;

/// How much larger than its input kses's output may be: this many times the input, plus
/// [`OUTPUT_ALLOWANCE`]. Escaping text makes at most five times (`&` as `&amp;`); a parse that
/// writes more is reopening elements, and is refused like input past the caps.
const OUTPUT_FACTOR: usize = 8;

/// The bytes kses's output may have beyond [`OUTPUT_FACTOR`] times its input, for short input
/// whose closing tags the sanitiser adds.
const OUTPUT_ALLOWANCE: usize = 1024;

/// The most work kses lets the serialiser do: the input's length times the number of `&` and
/// `0xC2` bytes in it. html5ever 0.40's serialiser searches the rest of a text or attribute
/// value for `<`, `>` (and `"`) again after every `&` and every `0xC2` byte (the lead byte of
/// U+0080 to U+00BF: the no-break space, `©`, `°`, `«`…), so its time grows with a value's
/// length times those; a value is never longer than the input, and each of them in it comes
/// from one in the input (an entity such as `&nbsp;` has its own `&`). Ten billion is a fraction
/// of a second, and lets a 256 KB post hold 38,000 of them.
const MAX_ESCAPE_WORK: u128 = 10_000_000_000;

/// Whether kses may parse `html`: its serialising work is within [`MAX_ESCAPE_WORK`], and its
/// nesting and open formatting elements are within [`MAX_NESTING`] and
/// [`MAX_ACTIVE_FORMATTING`] ([`nesting_within_cap`]).
fn parse_is_bounded(html: &str) -> bool {
    let restarts = html.bytes().filter(|&b| b == b'&' || b == 0xC2).count();
    (html.len() as u128) * (restarts as u128) <= MAX_ESCAPE_WORK && nesting_within_cap(html)
}

/// The start-tag bytes the pre-scan lets the parser reopen or clone beyond one per byte of
/// input, for short input.
const REOPEN_ALLOWANCE: usize = 10_000;

/// The elements whose content the tokenizer reads as text up to their end tag (raw text and
/// escapable raw text, with scripting on, as html5ever parses for ammonia).
const RAW_TEXT_ELEMENTS: &[&str] = &[
    "script", "style", "xmp", "iframe", "noembed", "noframes", "noscript", "textarea", "title",
];

/// Writes `document` as HTML, or `None` when it would be longer than `budget` bytes; writing
/// stops there, so a refused document costs no more than the budget.
fn serialize_within(document: &ammonia::Document, budget: usize) -> Option<String> {
    /// A writer that refuses to grow past its budget.
    struct Budgeted {
        out: Vec<u8>,
        budget: usize,
    }
    impl std::io::Write for Budgeted {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.out.len() + buf.len() > self.budget {
                return Err(std::io::Error::other("kses output budget"));
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Budgeted {
        out: Vec::new(),
        budget,
    };
    document.write_to(&mut writer).ok()?;
    // The serialiser writes UTF-8.
    String::from_utf8(writer.out).ok()
}

/// The output budget for `input`: [`OUTPUT_FACTOR`] times its length plus
/// [`OUTPUT_ALLOWANCE`].
fn output_budget(input: &str) -> usize {
    input
        .len()
        .saturating_mul(OUTPUT_FACTOR)
        .saturating_add(OUTPUT_ALLOWANCE)
}

/// Whether `html` stays within [`MAX_NESTING`] and [`MAX_ACTIVE_FORMATTING`], found in one
/// linear pass before html5ever parses it.
///
/// html5ever (as browsers' parsers) scans its stack of open elements for many tags, so its time
/// grows with the input's length times its nesting: 100,000 nested `<div>` take seconds. The
/// nesting cap bounds the cost of those scans. The parser also reopens every unclosed formatting
/// element in each new block, attributes and all, so 512 unclosed `<b>` and 60,000 paragraphs
/// wrote 516 MB from 486 KB, and one `<b>` with a 120 KB `style` in 30,000 paragraphs took 69 s
/// (the sanitiser filters every copy); its adoption agency clones misnested ones too. The
/// formatting cap bounds how many are reopened at once; a reopening budget, in bytes, bounds
/// the total before anything is built: the start-tag bytes the parser may copy must stay within
/// the input's length plus [`REOPEN_ALLOWANCE`] (each tag is charged the active formatting
/// elements' start tags, which it may close for a later tag or text to reopen, and each run of
/// the adoption agency that may clone, [`ADOPTION_ROUNDS`] times the ones it may clone; see
/// `TreeModel::reopened`). An output budget ([`OUTPUT_FACTOR`]) is the backstop. (The
/// serialiser's escaping work is bounded separately, in [`parse_is_bounded`], and the input's
/// size by [`kses_post_fields`], Ruling R42.)
///
/// The pass tokenizes as the HTML standard does (tags, quoted and unquoted attribute values,
/// comments with all their endings, bogus comments, `DOCTYPE`, raw text elements, CDATA), and
/// models the tree conservatively, so that it can count more open and active formatting
/// elements than the parser keeps, never fewer:
///
/// - every start tag of an element that is not void opens one, counted toward the depth unless
///   it is [uncounted](UNCOUNTED_ELEMENTS) (every start tag at all counts when the input has an
///   `<svg` or `<math` tag, since in SVG and MathML those names are ordinary elements); `<li>`
///   closes an open `<li>`, `<dd>` or `<dt>` an open `<dd>` or `<dt>`, and `<p>` an open `<p>`;
/// - a [formatting element](FORMATTING_ELEMENTS) also counts as active, with the length of its
///   start tag (whose attributes may not pass [`MAX_FORMATTING_ATTRIBUTES`]), and stops counting
///   only when its end tag comes while it is both the last element open and the last active
///   one with its name (the entry the parser's adoption agency acts on); after any other end,
///   the parser may keep it active and reopen it, so the model keeps every entry;
/// - an end tag closes only the element it names when that element is the last counted one open
///   (and `</ul>`, `</ol>`, `</menu>` and `</dl>` their open items first; `</p>` its paragraph
///   when only formatting elements are open in it), and only when no uncounted element other
///   than a paragraph is open in it (any, for a table), since the parser ignores it then; an
///   end tag the parser would ignore, or one inside a comment, an attribute value, raw text or
///   CDATA, closes nothing;
/// - text that may be markup for the parser counts each `<` followed by a letter as an element
///   that never closes, toward both caps, active with its tag's bytes: CDATA (a bogus comment outside SVG and MathML, which
///   ends sooner), and, when the input has an `<svg` or `<math` tag anywhere, raw text and the
///   rest of the input after `<plaintext>` (in SVG and MathML, `<style>`, `<title>` or
///   `<plaintext>` hold markup); and after a `<script>` whose text holds `<!--` (where the
///   parser's end of the script is hard to tell), no end tag closes anything.
///
/// Past a cap, the input is not parsed: [`kses_post`] and [`kses_data`] store it as escaped
/// text (every `<` as `&lt;`), so nothing runs and nothing typed is lost, and [`kses_title`]
/// keeps it as typed, as the text a title is.
fn nesting_within_cap(html: &str) -> bool {
    let bytes = html.as_bytes();
    let length = bytes.len();
    let mut tree = TreeModel::default();
    // The bytes of start tags the parser may copy, reopening or cloning formatting elements
    // (see `TreeModel::reopened`): about one per byte of input at most.
    let reopen_budget = length.saturating_add(REOPEN_ALLOWANCE);
    // Once set, no end tag closes anything.
    let mut no_closing = false;
    // Whether the parser may be in SVG or MathML anywhere, where raw text is markup.
    let foreign = contains_ignoring_case(bytes, b"<svg") || contains_ignoring_case(bytes, b"<math");
    let mut i = 0;
    while let Some(at) = bytes[i..].iter().position(|&b| b == b'<').map(|at| i + at) {
        if !tree.within_caps() || tree.reopened > reopen_budget {
            return false;
        }
        i = at + 1;
        let Some(&next) = bytes.get(i) else { break };
        if next.is_ascii_alphabetic() {
            let (name_end, after) = scan_tag(bytes, i);
            let name = html[i..name_end].to_ascii_lowercase();
            if FORMATTING_ELEMENTS.contains(&name.as_str())
                && attribute_bytes(bytes, name_end, after) > MAX_FORMATTING_ATTRIBUTES
            {
                return false;
            }
            i = after;
            tree.charge_start(&name, foreign);
            if name == "plaintext" && !foreign {
                // The rest is text for the parser.
                break;
            }
            if name == "plaintext" {
                // The rest is markup in SVG and MathML.
                tree.unclosable += 1;
                if !tree.markup_in_text(bytes, i, length) {
                    return false;
                }
                break;
            }
            if RAW_TEXT_ELEMENTS.contains(&name.as_str()) {
                let close = find_raw_text_end(bytes, i, &name).unwrap_or(length);
                if foreign && !tree.markup_in_text(bytes, i, close) {
                    return false;
                }
                if name == "script" && contains(&bytes[i..close], b"<!--") {
                    no_closing = true;
                }
                // The end tag is read as a tag on the next turn.
                i = close;
            }
            tree.start(name, after - at, foreign);
        } else if next == b'/' {
            match bytes.get(i + 1) {
                Some(c) if c.is_ascii_alphabetic() => {
                    let (name_end, after) = scan_tag(bytes, i + 1);
                    let name = html[i + 1..name_end].to_ascii_lowercase();
                    i = after;
                    if !no_closing {
                        tree.end(&name);
                    }
                    tree.charge_end(&name);
                }
                Some(b'>') => i += 2,
                // A bogus comment, up to the next `>`.
                _ => i = skip_past(bytes, i, b">"),
            }
        } else if next == b'!' {
            if bytes[i..].starts_with(b"!--") {
                i = comment_end(bytes, i + 3);
            } else if bytes[i..].starts_with(b"![CDATA[") {
                // CDATA in SVG and MathML, a bogus comment elsewhere: read it as the longer one,
                // counting what may be markup.
                let end = find(bytes, i, b"]]>").map_or(length, |at| at + 3);
                if !tree.markup_in_text(bytes, i, end) {
                    return false;
                }
                i = end;
            } else {
                // `DOCTYPE` or a bogus comment, up to the next `>`.
                i = skip_past(bytes, i, b">");
            }
        } else if next == b'?' {
            i = skip_past(bytes, i, b">");
        }
    }
    tree.within_caps() && tree.reopened <= reopen_budget
}

/// The phrasing elements other than the formatting ones. With the
/// [formatting elements](FORMATTING_ELEMENTS), their start tags never open an element that stays
/// open as a [special] element (`<img>`, `<br>` and `<wbr>` are special, but void, so closed at
/// once), so they do not expose the active formatting elements to the adoption agency
/// (`TreeModel::last_block`).
///
/// They can close elements, which the model does not follow (it keeps those open: more, never
/// fewer): `<a>` and `<nobr>` run the adoption agency for an active one (charged separately);
/// in a `<colgroup>`, any of them closes the colgroup (uncounted, so kept open by the model);
/// and in SVG or MathML many of them (`<b>`, `<span>`, `<sub>`…) close the foreign elements open
/// (when the input has an `<svg` or `<math` tag, the model treats every start tag as one that
/// may open a special element).
///
/// [special]: https://html.spec.whatwg.org/multipage/parsing.html#special
const PHRASING_ELEMENTS: &[&str] = &[
    "abbr", "acronym", "bdi", "bdo", "br", "cite", "data", "del", "dfn", "img", "ins", "kbd",
    "label", "mark", "q", "samp", "span", "sub", "sup", "time", "var", "wbr",
];

/// The elements whose end tag the parser handles by closing everything open after them (when
/// they are in scope), so they close with the paragraphs open in them; any other element's end
/// tag is ignored when a special element, such as a paragraph or a cell, is open after it.
const CLOSING_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "li",
    "listing",
    "main",
    "menu",
    "nav",
    "ol",
    "pre",
    "search",
    "section",
    "summary",
    "ul",
];

/// How many times html5ever's adoption agency runs its outer loop at most, cloning the
/// formatting element and up to three others each time.
const ADOPTION_ROUNDS: usize = 8;

/// A counted element open in the model of [`nesting_within_cap`], with the uncounted elements
/// open in it after it.
struct OpenElement {
    /// The tag name, in lower case.
    name: String,
    /// Whether it is a formatting element, counted as active.
    formatting: bool,
    /// The order of its start tag, which names its entry in [`TreeModel::active`].
    seq: usize,
    /// The [uncounted](UNCOUNTED_ELEMENTS) elements open after it, last opened last.
    uncounted: Vec<&'static str>,
}

impl OpenElement {
    /// Whether the parser closes this element at its end tag (or at a new item, for a list
    /// item), with the uncounted elements open in it: when none are, or when only paragraphs
    /// are and its end tag closes everything open after it. A table closes with its rows, cells
    /// and paragraphs.
    fn closes_with_uncounted(&self) -> bool {
        self.uncounted.is_empty()
            || self.name == "table"
            || (CLOSING_ELEMENTS.contains(&self.name.as_str())
                && self.uncounted.iter().all(|&u| u == "p"))
    }
}

/// An active formatting element in the model of [`nesting_within_cap`].
struct Active {
    /// The tag name.
    name: &'static str,
    /// The length of its start tag, name and attributes: what the parser copies when it reopens
    /// or clones the element.
    bytes: usize,
    /// The order of its start tag.
    seq: usize,
}

/// The conservative model of the parser's tree in [`nesting_within_cap`]. Every operation looks
/// at the last element open, or walks down over open formatting elements only, or over the
/// active ones (at most [`MAX_ACTIVE_FORMATTING`] and one more, since the pass stops past the
/// cap), so the pass stays linear.
///
/// Where the model cannot tell what the parser does, it keeps elements open and entries active
/// (more, never fewer). It lets an active entry go at one point only, in [`TreeModel::end`]: a
/// formatting end tag whose name's last active entry is the element open last, with nothing
/// open after it, and no element counted in text that may be markup. The parser then acts on
/// the same entry: its adoption agency picks the last entry with the name, and the model's
/// entries are a superset of the parser's in the same order per name (the parser only drops
/// entries, replaces one with its clone in place, or moves the one it acts on later, past
/// entries of other names only); that element is the parser's current node, or has only
/// reopened formatting elements after it, so no special element is open after it and the
/// parser closes it and drops its entry. So [`TreeModel::active`] holds every element the
/// parser may reopen or clone. Elements are closed (`end`, `end_paragraph`, the item pops in
/// `start`) only as the parser closes them; closing never drops an active entry.
#[derive(Default)]
struct TreeModel {
    /// The uncounted elements open before any counted one.
    root: Vec<&'static str>,
    /// The counted elements open, last opened last; their number is the depth.
    open: Vec<OpenElement>,
    /// The active formatting elements: open, or closed by another element's end tag, so
    /// reopened by the parser. Started first, first.
    active: Vec<Active>,
    /// Elements counted in text that may be markup; they never close.
    unclosable: usize,
    /// The bytes of the tags of [`TreeModel::unclosable`] elements, which may be active
    /// formatting elements; they never stop being active.
    unclosable_bytes: usize,
    /// How many start tags have come.
    seq: usize,
    /// The order of the last start tag that is not [phrasing](PHRASING_ELEMENTS), which may
    /// have opened a special element. An active element started before it may be cloned by the
    /// adoption agency ("exposed").
    last_block: usize,
    /// The bytes of start tags the parser may copy, as reopened or cloned formatting elements.
    ///
    /// The parser reopens the active elements that are not open at a tag or a run of text, but
    /// only the ones closed since it last reopened them, and only a tag closes elements. So each
    /// tag is charged the active elements' bytes, after an end tag (the ones it may have closed
    /// and that stay active) and before a start tag (the new element is open). An end tag of a
    /// formatting element, and `<a>` or `<nobr>` while one is active, run the adoption agency,
    /// which clones only when a special element is open after the formatting element: those
    /// are charged [`ADOPTION_ROUNDS`] times the bytes of the exposed active elements.
    reopened: usize,
}

impl TreeModel {
    /// Whether the model is within [`MAX_NESTING`] and [`MAX_ACTIVE_FORMATTING`].
    fn within_caps(&self) -> bool {
        self.open.len() + self.unclosable <= MAX_NESTING
            && self.formatting() + self.unclosable <= MAX_ACTIVE_FORMATTING
    }

    /// How many formatting elements are active.
    fn formatting(&self) -> usize {
        self.active.len()
    }

    /// The bytes of the active formatting elements' start tags.
    fn active_bytes(&self) -> usize {
        self.active
            .iter()
            .fold(self.unclosable_bytes, |sum, a| sum.saturating_add(a.bytes))
    }

    /// The bytes of the start tags of the active formatting elements the adoption agency may
    /// clone: those started before a special element may have been opened.
    fn exposed_bytes(&self) -> usize {
        self.active
            .iter()
            .filter(|a| a.seq < self.last_block)
            .fold(self.unclosable_bytes, |sum, a| sum.saturating_add(a.bytes))
    }

    /// Whether the adoption agency for `name` may clone: an active element `name` is exposed.
    fn may_clone(&self, name: &str) -> bool {
        self.unclosable_bytes > 0
            || self
                .active
                .iter()
                .any(|a| a.name == name && a.seq < self.last_block)
    }

    /// Charges the start tag `name`, before the model opens it.
    fn charge_start(&mut self, name: &str, foreign: bool) {
        self.reopened = self.reopened.saturating_add(self.active_bytes());
        if matches!(name, "a" | "nobr") && self.may_clone(name) {
            self.charge_adoption();
        }
        self.seq += 1;
        if foreign || !(FORMATTING_ELEMENTS.contains(&name) || PHRASING_ELEMENTS.contains(&name)) {
            self.last_block = self.seq;
        }
    }

    /// Charges the end tag `name`, after the model closed what it closes.
    fn charge_end(&mut self, name: &str) {
        self.reopened = self.reopened.saturating_add(self.active_bytes());
        if FORMATTING_ELEMENTS.contains(&name) && self.may_clone(name) {
            self.charge_adoption();
        }
    }

    /// Charges one run of the adoption agency.
    fn charge_adoption(&mut self) {
        self.reopened = self
            .reopened
            .saturating_add(ADOPTION_ROUNDS.saturating_mul(self.exposed_bytes()));
    }

    /// Text in `bytes[from..to]` that may be markup for the parser: each `<` followed by a
    /// letter counts as an element that never closes, active with its tag's bytes, and as a
    /// tag that may close elements, reopen and clone them. Returns `false` when that passes
    /// [`MAX_ACTIVE_FORMATTING`] (then the tags are not read, so the pass stays linear).
    fn markup_in_text(&mut self, bytes: &[u8], from: usize, to: usize) -> bool {
        let text = &bytes[from..to];
        let count = count_tag_opens(text);
        if count == 0 {
            return true;
        }
        self.unclosable += count;
        if !self.within_caps() {
            return false;
        }
        for at in 0..text.len() - 1 {
            if text[at] == b'<' && text[at + 1].is_ascii_alphabetic() {
                let (name_end, after) = scan_tag(bytes, from + at + 1);
                let name = &bytes[from + at + 1..name_end];
                if FORMATTING_ELEMENTS
                    .iter()
                    .any(|f| f.as_bytes().eq_ignore_ascii_case(name))
                    && attribute_bytes(bytes, name_end, after) > MAX_FORMATTING_ATTRIBUTES
                {
                    return false;
                }
                self.unclosable_bytes = self.unclosable_bytes.saturating_add(after - (from + at));
            }
        }
        self.seq += 1;
        self.last_block = self.seq;
        // Each of those tags may close elements and run the adoption agency.
        let per_tag = self
            .active_bytes()
            .saturating_add(ADOPTION_ROUNDS.saturating_mul(self.exposed_bytes()));
        self.reopened = self.reopened.saturating_add(count.saturating_mul(per_tag));
        true
    }

    /// The uncounted elements open after the last counted one.
    fn last_uncounted(&mut self) -> &mut Vec<&'static str> {
        match self.open.last_mut() {
            Some(element) => &mut element.uncounted,
            None => &mut self.root,
        }
    }

    /// A start tag `name`, `bytes` long; `foreign` when the input may hold SVG or MathML.
    fn start(&mut self, name: String, bytes: usize, foreign: bool) {
        let n = name.as_str();
        if !foreign && (VOID_ELEMENTS.contains(&n) || matches!(n, "html" | "head" | "body")) {
            return;
        }
        if !foreign && let Some(&uncounted) = UNCOUNTED_ELEMENTS.iter().find(|&&u| u == n) {
            let open = self.last_uncounted();
            if uncounted == "p" && open.last() == Some(&"p") {
                // A paragraph closes the open paragraph.
                open.pop();
            }
            open.push(uncounted);
            return;
        }
        if let Some(top) = self.open.last()
            && top.closes_with_uncounted()
        {
            let top = top.name.as_str();
            if (n == "li" && top == "li")
                || (matches!(n, "dd" | "dt") && matches!(top, "dd" | "dt"))
            {
                self.open.pop();
            }
        }
        let formatting = FORMATTING_ELEMENTS.iter().find(|&&f| f == n);
        if let Some(&formatting) = formatting {
            self.active.push(Active {
                name: formatting,
                bytes,
                seq: self.seq,
            });
        }
        self.open.push(OpenElement {
            name,
            formatting: formatting.is_some(),
            seq: self.seq,
            uncounted: Vec::new(),
        });
    }

    /// An end tag `name`.
    fn end(&mut self, name: &str) {
        if FORMATTING_ELEMENTS.contains(&name) {
            // The adoption agency acts on the last active entry with this name (the parser's
            // "formatting element"), not on the element open last. Only when that entry is the
            // element open last, with nothing open after it, does the parser certainly close it
            // and drop its entry; the model does the same. In every other case (the entry is not
            // open, so the parser drops that entry and keeps the open one; or something is open
            // after it, so the parser may clone it), the model drops nothing and closes nothing.
            // Elements counted in text that may be markup could be later entries with this name,
            // so with any of them, nothing is dropped either.
            let last = self.active.iter().rev().find(|a| a.name == name);
            if let (Some(last), Some(top)) = (last, self.open.last())
                && self.unclosable == 0
                && top.formatting
                && top.uncounted.is_empty()
                && top.seq == last.seq
            {
                let seq = last.seq;
                self.open.pop();
                self.active.retain(|a| a.seq != seq);
            }
            return;
        }
        if name == "p" {
            self.end_paragraph();
            return;
        }
        let open = self.last_uncounted();
        if open.last() == Some(&name) {
            open.pop();
            return;
        }
        // A counted element closes with the uncounted ones open in it, and the lists with their
        // open item (two items of one list are never open in a row: an item closes the last).
        let items: &[&str] = match name {
            "ul" | "ol" | "menu" => &["li"],
            "dl" => &["dd", "dt"],
            _ => &[],
        };
        // The parser ignores the end tag when a special element the model does not count (a
        // cell, say) is open in the element; then the model keeps it open too.
        let mut at = self.open.len();
        if at > 0 && items.contains(&self.open[at - 1].name.as_str()) {
            if !self.open[at - 1].closes_with_uncounted() {
                return;
            }
            at -= 1;
        }
        if at > 0 && self.open[at - 1].name == name && self.open[at - 1].closes_with_uncounted() {
            self.open.truncate(at - 1);
        }
    }

    /// `</p>`: closes the last open paragraph and the formatting elements open in it (they stay
    /// active), when nothing else is open in it. Walks down over formatting elements only.
    fn end_paragraph(&mut self) {
        for at in (0..self.open.len()).rev() {
            let element = &mut self.open[at];
            if let Some(&last) = element.uncounted.last() {
                if last == "p" {
                    element.uncounted.pop();
                    self.open.truncate(at + 1);
                }
                return;
            }
            if element.name == "p" {
                // Counted, in SVG or MathML.
                self.open.truncate(at);
                return;
            }
            if !element.formatting {
                return;
            }
        }
        if self.root.last() == Some(&"p") {
            self.root.pop();
            self.open.clear();
        }
    }
}

/// Whether `byte` is whitespace for the HTML tokenizer (with `\r`, which the parser turns into
/// `\n` before tokenizing).
fn is_html_space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

/// Reads a start or end tag whose name starts at `name_start`, as the HTML tokenizer reads it:
/// the name, then attributes (a value is quoted only when `"` or `'` follows the `=`), up to
/// the `>` outside any value. Returns the end of the name and the index after the `>` (the
/// input's length when the tag does not end).
fn scan_tag(bytes: &[u8], name_start: usize) -> (usize, usize) {
    let length = bytes.len();
    let mut i = name_start;
    while i < length && !is_html_space(bytes[i]) && !matches!(bytes[i], b'/' | b'>') {
        i += 1;
    }
    let name_end = i;
    loop {
        // Before an attribute name (a `/` not before `>` is read again here).
        while i < length && (is_html_space(bytes[i]) || bytes[i] == b'/') {
            i += 1;
        }
        if i >= length {
            return (name_end, length);
        }
        if bytes[i] == b'>' {
            return (name_end, i + 1);
        }
        // The attribute name: its first character may be anything, even `=`.
        i += 1;
        while i < length && !is_html_space(bytes[i]) && !matches!(bytes[i], b'/' | b'>' | b'=') {
            i += 1;
        }
        while i < length && is_html_space(bytes[i]) {
            i += 1;
        }
        if i < length && bytes[i] == b'=' {
            i += 1;
            while i < length && is_html_space(bytes[i]) {
                i += 1;
            }
            match bytes.get(i) {
                None => return (name_end, length),
                Some(&quote @ (b'"' | b'\'')) => match find(bytes, i + 1, &[quote]) {
                    Some(at) => i = at + 1,
                    None => return (name_end, length),
                },
                Some(b'>') => return (name_end, i + 1),
                Some(_) => {
                    while i < length && !is_html_space(bytes[i]) && bytes[i] != b'>' {
                        i += 1;
                    }
                }
            }
        }
    }
}

/// The attribute bytes of the tag whose name ends at `name_end` and that ends before `after`
/// ([`scan_tag`]): everything between the name and the closing `>` (to the end of the input
/// when the tag does not end).
fn attribute_bytes(bytes: &[u8], name_end: usize, after: usize) -> usize {
    let closed = after > name_end && bytes[after - 1] == b'>';
    after - name_end - usize::from(closed)
}

/// The index after the comment whose text starts at `start` (after `<!--`), as the HTML
/// tokenizer ends it: `<!-->`, `<!--->`, `-->` or `--!>`; the input's length when it does not
/// end.
fn comment_end(bytes: &[u8], start: usize) -> usize {
    #[derive(Clone, Copy)]
    enum State {
        Start,
        StartDash,
        Text,
        EndDash,
        End,
        Bang,
    }
    let mut state = State::Start;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        state = match (state, byte) {
            (State::Start | State::StartDash | State::End | State::Bang, b'>') => {
                return start + offset + 1;
            }
            (State::Start, b'-') => State::StartDash,
            (State::StartDash | State::EndDash | State::End, b'-') => State::End,
            (State::Text | State::Bang, b'-') => State::EndDash,
            (State::End, b'!') => State::Bang,
            _ => State::Text,
        };
    }
    bytes.len()
}

/// The index of the end tag `</name` (any case, followed by whitespace, `/` or `>`) at or after
/// `from`, where raw text of element `name` ends.
fn find_raw_text_end(bytes: &[u8], from: usize, name: &str) -> Option<usize> {
    let mut i = from;
    while let Some(at) = find(bytes, i, b"</") {
        let after = at + 2 + name.len();
        if bytes.len() > after
            && bytes[at + 2..after].eq_ignore_ascii_case(name.as_bytes())
            && (is_html_space(bytes[after]) || matches!(bytes[after], b'/' | b'>'))
        {
            return Some(at);
        }
        i = at + 2;
    }
    None
}

/// How many times `<` followed by an ASCII letter (what may open an element) occurs in `text`.
fn count_tag_opens(text: &[u8]) -> usize {
    text.windows(2)
        .filter(|pair| pair[0] == b'<' && pair[1].is_ascii_alphabetic())
        .count()
}

/// The index of the first `needle` at or after `from`.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

/// Whether `needle` occurs in `bytes`.
fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    find(bytes, 0, needle).is_some()
}

/// Whether `needle` (lower-case ASCII) occurs in `bytes` in any case.
fn contains_ignoring_case(bytes: &[u8], needle: &[u8]) -> bool {
    bytes
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// The index after the first `needle` at or after `from`, or the input's length.
fn skip_past(bytes: &[u8], from: usize, needle: &[u8]) -> usize {
    find(bytes, from, needle).map_or(bytes.len(), |at| at + needle.len())
}

/// `text`, a text node as the sanitiser writes it, with its four escapes (`&amp;`, `&lt;`,
/// `&gt;`, `&nbsp;`) decoded in one pass, so `&amp;lt;` gives `&lt;`.
fn decode_serialized_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let entity = &rest[at..];
        let (decoded, length) = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&nbsp;", '\u{a0}'),
        ]
        .iter()
        .find(|(name, _)| entity.starts_with(name))
        .map_or(('&', 1), |(name, c)| (*c, name.len()));
        out.push(decoded);
        rest = &entity[length..];
    }
    out.push_str(rest);
    out
}

/// The fields of a post that kses filters on save: its title, content and excerpt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostFields {
    /// The title (`post_title`).
    pub title: String,
    /// The body (`post_content`).
    pub content: String,
    /// The excerpt (`post_excerpt`).
    pub excerpt: String,
}

/// `fields` as they may be stored for `user`, as WordPress's `kses_init_filters` filters a
/// save: when `user` lacks `unfiltered_html`, the content and the excerpt go through
/// [`kses_post`] and the title through [`kses_title`] (plain text, Ruling R39); otherwise
/// (administrators and editors on a single site) they are returned as sent.
///
/// For a user without `unfiltered_html`, a field longer than [`MAX_FILTERED_BYTES`] is refused
/// with [`FieldTooLarge`] (Ruling R42): the save reports the error and stores nothing.
///
/// No `rel` is added to links with a `target`: WordPress 5.1 to 6.6 added `rel="noopener"` on
/// save (`wp_targeted_link_rel`), but WordPress 6.7 deprecated it and no longer runs it.
///
/// Every path that saves a post's fields for a user calls this first (the REST API's and the
/// admin's create, update and autosave routes, and imports that act for a user):
/// [`Post::create`](super::Post::create), [`Post::save`](super::Post::save) and the revision
/// and attachment writers store what they are given.
///
/// ```ignore
/// let fields = kses_post_fields(&user, PostFields { title, content, excerpt })?;
/// let post = Post::create(&db, NewPost {
///     title: fields.title,
///     content: fields.content,
///     excerpt: fields.excerpt,
///     ..NewPost::new(user.id(), "post")
/// })
/// .await?;
/// ```
///
/// # Errors
///
/// [`FieldTooLarge`] when `user` lacks `unfiltered_html` and a field is longer than
/// [`MAX_FILTERED_BYTES`].
pub fn kses_post_fields(user: &UserCaps, fields: PostFields) -> Result<PostFields, FieldTooLarge> {
    if has_cap(user, "unfiltered_html") {
        return Ok(fields);
    }
    for (field, value) in [
        ("title", &fields.title),
        ("content", &fields.content),
        ("excerpt", &fields.excerpt),
    ] {
        if value.len() > MAX_FILTERED_BYTES {
            return Err(FieldTooLarge {
                field,
                size: value.len(),
            });
        }
    }
    Ok(PostFields {
        title: kses_title(&fields.title),
        content: kses_post(&fields.content),
        excerpt: kses_post(&fields.excerpt),
    })
}

/// The largest title, content or excerpt, in bytes, that kses filters for a user without
/// `unfiltered_html` (Ruling R42): 256 KB. Parsing is linear, but it still takes about 0.35 s and
/// 340 MB for 480 KB of plain `<div>x</div>`, so this cap is what bounds a save's cost.
/// WordPress has no such cap; it is a known limit of Mission Control.
pub const MAX_FILTERED_BYTES: usize = 256 * 1024;

/// A post field too long to filter for a user without `unfiltered_html`
/// ([`MAX_FILTERED_BYTES`]); the save is refused and nothing is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldTooLarge {
    /// The field: `title`, `content` or `excerpt`.
    pub field: &'static str,
    /// Its length, in bytes.
    pub size: usize,
}

impl std::fmt::Display for FieldTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the {} is {} bytes; at most {} bytes can be saved without the unfiltered_html \
             capability",
            self.field, self.size, MAX_FILTERED_BYTES
        )
    }
}

impl std::error::Error for FieldTooLarge {}

/// `html` with each page marker (`<!--more…-->`, `<!--noteaser-->`, `<!--nextpage-->`)
/// replaced by a placeholder no input can guess, the placeholders' random token, and the
/// markers to put back, in placeholder order. A marker is written back in a safe form: the
/// "more" link text loses the characters that could end an attribute or a comment.
fn protect_markers(html: &str) -> (String, String, Vec<String>) {
    let token = random_token();
    let mut out = String::with_capacity(html.len());
    let mut markers = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        let comment = &rest[start..];
        let marker = if comment.starts_with("<!--nextpage-->") {
            Some(("<!--nextpage-->".to_owned(), "<!--nextpage-->".len()))
        } else if comment.starts_with("<!--noteaser-->") {
            Some(("<!--noteaser-->".to_owned(), "<!--noteaser-->".len()))
        } else if let Some(after) = comment.strip_prefix("<!--more") {
            // As WordPress's `<!--more(.*?)?-->`: the text up to the first `-->`.
            after.find("-->").map(|end| {
                let marker = format!("<!--more{}-->", more_text(&after[..end]));
                (marker, "<!--more".len() + end + "-->".len())
            })
        } else {
            None
        };
        match marker {
            Some((marker, length)) => {
                out.push_str(&placeholder(&token, markers.len()));
                markers.push(marker);
                rest = &comment[length..];
            }
            None => {
                out.push_str("<!--");
                rest = &comment["<!--".len()..];
            }
        }
    }
    out.push_str(rest);
    (out, token, markers)
}

/// The "more" link text, safe to write inside a comment or an attribute value: without
/// `< > " ' & `` ` `` or control characters, with runs of `-` made one, and without a `-` at the
/// end (so the comment cannot end early or run on).
fn more_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '<' | '>' | '"' | '\'' | '&' | '`') || c.is_control() {
            continue;
        }
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The placeholder of marker `index`: the token, the index and an `x`, so that marker 1's
/// placeholder is not the start of marker 10's. The token (`mckses`, hex digits, `m`) never
/// contains an `x`, so the first `x` after it ends the index.
fn placeholder(token: &str, index: usize) -> String {
    format!("{token}{index}x")
}

/// 128 random bits as letters and digits, from the operating system's generator. Should it
/// fail, a fixed token is used: a guessed placeholder can only add a page marker, which is safe.
fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = *b"mission-control!";
    }
    let mut token = String::from("mckses");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token.push('m');
    token
}

/// `css`, a `style` attribute's value, with only its safe declarations, as WordPress's
/// `safecss_filter_attr`: line breaks and tabs are removed; each `;`-separated declaration is
/// kept only when its property is in WordPress's `safe_style_css` list (or is a custom property
/// `--name`); a `url()` only in a property that takes one and with an allowed protocol or a
/// relative URL; a gradient only in `background` and `background-image`; and, outside `url()`,
/// gradients and the functions `var`, `calc`, `min`, `max`, `minmax`, `clamp` and `repeat`, no
/// `\`, `(`, `&`, `=`, `}` or `/*`. The kept declarations are joined with `;`.
///
/// It is stricter than WordPress in two ways: a declaration without a `:` is removed, and one
/// that contains `expression`, `javascript:`, `vbscript:`, `behavior`, `-moz-binding`, `\` or
/// `/*` anywhere, even inside a function, is removed.
pub fn safecss_filter_attr(css: &str) -> String {
    let css: String = css
        .chars()
        .filter(|c| !matches!(c, '\0' | '\n' | '\r' | '\t'))
        .collect();
    let mut kept: Vec<&str> = Vec::new();
    for item in css.trim().split(';') {
        let item = item.trim();
        if !item.is_empty() && safe_declaration(item) {
            kept.push(item);
        }
    }
    kept.join(";")
}

/// Whether one declaration (`property: value`, trimmed) passes [`safecss_filter_attr`].
fn safe_declaration(item: &str) -> bool {
    let lower = item.to_ascii_lowercase();
    if CSS_BANNED.iter().any(|banned| lower.contains(banned)) {
        return false;
    }
    let Some((property, value)) = item.split_once(':') else {
        return false;
    };
    let property = property.trim();
    let custom = property.len() > 2
        && property.starts_with("--")
        && property[2..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !custom && !SAFE_STYLE_CSS.contains(&property) {
        return false;
    }
    let (url_type, gradient_type) = if custom {
        let value = value.trim();
        (value.starts_with("url("), value.contains("-gradient("))
    } else {
        (
            CSS_URL_DATA_TYPES.contains(&property),
            CSS_GRADIENT_DATA_TYPES.contains(&property),
        )
    };
    let mut test = item.to_owned();
    if url_type {
        // The declaration without its `url()`s, each checked first.
        let value_start = item.len() - value.len();
        let mut without = String::with_capacity(item.len());
        let mut from = 0;
        for (start, end) in css_urls(value) {
            if !css_url_allowed(&value[start..end]) {
                return false;
            }
            without.push_str(&item[from..value_start + start]);
            from = value_start + end;
        }
        without.push_str(&item[from..]);
        test = without;
    }
    if gradient_type {
        let value = value.trim();
        if is_gradient(value) {
            test = test.replace(value, "");
        }
    }
    let test = remove_css_functions(&test);
    !(test.contains(['\\', '(', '&', '=', '}']) || test.contains("/*"))
}

/// The start and end of each `url(…)` in `value`, as WordPress's `url\([^)]+\)`: `url(` and at
/// least one character up to the next `)`.
fn css_urls(value: &str) -> Vec<(usize, usize)> {
    let mut urls = Vec::new();
    let mut from = 0;
    while let Some(start) = value[from..].find("url(").map(|i| from + i) {
        let inner = start + "url(".len();
        match value[inner..].find(')') {
            Some(0) => from = inner,
            Some(end) => {
                urls.push((start, inner + end + 1));
                from = inner + end + 1;
            }
            None => break,
        }
    }
    urls
}

/// Whether a matched `url(…)` holds a non-empty URL, optionally quoted with `'` or `"`, with an
/// allowed protocol or none.
fn css_url_allowed(url: &str) -> bool {
    let inner = &url["url(".len()..url.len() - 1];
    let inner = inner.trim_start();
    let inner = inner.trim_end();
    let unquoted = match inner.chars().next() {
        Some(quote @ ('\'' | '"')) => {
            match inner[1..].strip_suffix(quote) {
                Some(unquoted) => unquoted,
                // An opening quote without its closing one is part of the URL in WordPress's
                // pattern; refuse it rather than read it as a URL.
                None => return false,
            }
        }
        _ => inner,
    };
    let unquoted = unquoted.trim();
    !unquoted.is_empty() && !unquoted.contains(['"', '\'']) && allowed_url(unquoted)
}

/// Whether `value` is one gradient, as WordPress's
/// `^(repeating-)?(linear|radial|conic)-gradient\(([^()]|rgb[a]?\([^()]*\))*\)$`.
fn is_gradient(value: &str) -> bool {
    let value = value.strip_prefix("repeating-").unwrap_or(value);
    let Some(body) = ["linear", "radial", "conic"]
        .iter()
        .find_map(|kind| value.strip_prefix(kind))
        .and_then(|rest| rest.strip_prefix("-gradient("))
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    let mut rest = body;
    while let Some(c) = rest.chars().next() {
        if let Some(args) = rest
            .strip_prefix("rgba(")
            .or_else(|| rest.strip_prefix("rgb("))
        {
            match args.find(['(', ')']) {
                Some(end) if args.as_bytes()[end] == b')' => rest = &args[end + 1..],
                _ => return false,
            }
        } else if c == '(' || c == ')' {
            return false;
        } else {
            rest = &rest[c.len_utf8()..];
        }
    }
    true
}

/// `css` without each balanced call of an allowed function (nested calls and parentheses
/// included), as WordPress's `\b(?:var|calc|min|max|minmax|clamp|repeat)(\((?:[^()]|(?1))*\))`.
/// A call whose parentheses do not balance is left in.
fn remove_css_functions(css: &str) -> String {
    let bytes = css.as_bytes();
    let closes = matching_closes(bytes);
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    while i < css.len() {
        let at_boundary = i == 0 || !is_word_byte(bytes[i - 1]);
        let call_end = if at_boundary {
            CSS_FUNCTIONS.iter().find_map(|name| {
                css[i..]
                    .strip_prefix(name)
                    .filter(|rest| rest.starts_with('('))
                    .and_then(|_| closes[i + name.len()])
                    .map(|close| close + 1)
            })
        } else {
            None
        };
        match call_end {
            Some(end) => i = end,
            None => {
                let c = css[i..].chars().next().expect("i is on a char boundary");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// For each byte of `css`, the index of the `)` that closes it when it is a `(` that is closed,
/// else `None`; found in one pass, so a long run of unclosed calls stays linear.
fn matching_closes(css: &[u8]) -> Vec<Option<usize>> {
    let mut closes = vec![None; css.len()];
    let mut open = Vec::new();
    for (i, byte) in css.iter().enumerate() {
        match byte {
            b'(' => open.push(i),
            b')' => {
                if let Some(start) = open.pop() {
                    closes[start] = Some(i);
                }
            }
            _ => {}
        }
    }
    closes
}

/// Whether `byte` is a word character for a regular expression's `\b`: an ASCII letter or
/// digit, or `_`.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    //! Which bound refuses an input, independent of how fast the build is.

    use super::*;

    /// `count` distinct unclosed `<b>`.
    fn unclosed(count: usize) -> String {
        (0..count).map(|k| format!("<b id=a{k}>")).collect()
    }

    #[test]
    fn the_reopening_budget_refuses_amplification_before_parsing() {
        let caps = MAX_ACTIVE_FORMATTING;
        for html in [
            format!("<p>{}</p>{}", unclosed(caps), "<p>x</p>".repeat(5_000)),
            // Implicit closes: a paragraph, a block or a heading closes the open paragraph.
            format!("<p>{}{}", unclosed(caps), "<p>x".repeat(5_000)),
            format!("<p>{}{}", unclosed(caps), "<div>x</div>".repeat(5_000)),
            format!("<p>{}{}", unclosed(caps), "<h1>x</h1>".repeat(5_000)),
            format!(
                "<p>{}<div></div>{}",
                unclosed(caps),
                "<p>x</p>".repeat(5_000)
            ),
            format!("<p>{}</p>{}", unclosed(4), "<p>x</p>".repeat(20_000)),
        ] {
            assert!(!nesting_within_cap(&html), "{}", &html[..60]);
        }
        // Within the caps and the budget.
        let one = format!("<p><b>x</p>{}", "<p>y</p>".repeat(20_000));
        assert!(nesting_within_cap(&one));
        let at_cap = format!("<p>{}</p><p>x</p>", unclosed(caps));
        assert!(nesting_within_cap(&at_cap));
        assert!(!nesting_within_cap(&format!(
            "<p>{}</p>",
            unclosed(caps + 1)
        )));
    }

    #[test]
    fn the_reopening_budget_counts_the_bytes_of_the_tags_reopened() {
        // One formatting element with a long attribute, reopened in every paragraph: the parser
        // copies the attribute each time, and filters every copy.
        let style = format!("color: red;{}", " ".repeat(12_000));
        let href = format!("/{}", "a".repeat(12_000));
        for html in [
            format!("<p><b style=\"{style}\">{}", "<p>x".repeat(3_000)),
            format!("<p><a href=\"{href}\">{}", "<p>x".repeat(3_000)),
            format!("<p><b style=\"{style}\">x</p>{}", "<p>x</p>".repeat(3_000)),
            format!("<p><em title=\"{href}\">{}", "<div>x</div>".repeat(3_000)),
            // Inside markup the pass does not read as tags (SVG `<style>` holds markup).
            format!(
                "<p><svg><style><b style=\"{style}\"></style></svg>{}",
                "<p>x</p>".repeat(3_000)
            ),
        ] {
            assert!(!nesting_within_cap(&html), "{}", &html[..30]);
        }
        // Element counts would have let these through: one element per paragraph.
        assert!(3_000 * 2 < "<p>x".repeat(3_000).len() + REOPEN_ALLOWANCE);

        // The adoption agency clones a formatting element when a block is open in it.
        let long = format!("<b style=\"color: red;{}\">", " ".repeat(2_000));
        assert!(!nesting_within_cap(&format!("{long}<div>x</b>")));
        // A formatting element closed while it is the last one open is not cloned.
        for html in [
            format!("{long}<div>x</div></b>"),
            format!("{long}<span>x</span></b><div>y</div>"),
            format!("<p>{long}x</b></p>{}", "<p>y</p>".repeat(3_000)),
        ] {
            assert!(nesting_within_cap(&html), "{}", &html[html.len() - 20..]);
        }
        // A short unclosed element is still reopened in thousands of paragraphs, as browsers do.
        assert!(nesting_within_cap(&format!(
            "<p><b>x</p>{}",
            "<p>y</p>".repeat(20_000)
        )));
    }

    #[test]
    fn the_tree_model_keeps_an_element_the_parser_cannot_close() {
        // `</span>` is ignored by the parser while a paragraph is open in the span.
        let mut tree = TreeModel::default();
        for (start, name) in [(true, "span"), (true, "p"), (false, "span")] {
            if start {
                tree.start(name.to_owned(), 6, false);
            } else {
                tree.end(name);
            }
        }
        assert_eq!(tree.open.len(), 1, "the span stays open");
        // A link closed while it is the last element open stops being active.
        let mut tree = TreeModel::default();
        tree.charge_start("div", false);
        tree.start("div".to_owned(), 5, false);
        tree.charge_start("a", false);
        tree.start("a".to_owned(), 40, false);
        tree.end("a");
        tree.charge_end("a");
        assert_eq!(tree.formatting(), 0);
        assert_eq!(tree.reopened, 0, "nothing to reopen or clone");
    }

    #[test]
    fn a_formatting_end_tag_lets_go_only_of_the_entry_the_parser_lets_go() {
        // `<div><b BIG><p><b></p></b></div>`: `</p>` closes the inner `<b>` (it stays active),
        // and `</b>` then acts on the last active `<b>`, the inner one, which is not open: the
        // parser drops that entry and keeps BIG open and active. The model must not drop BIG.
        let mut tree = TreeModel::default();
        tree.start("div".to_owned(), 5, false);
        tree.start("b".to_owned(), 2_000, false);
        tree.seq += 1;
        tree.start("p".to_owned(), 3, false);
        tree.start("b".to_owned(), 3, false);
        tree.seq += 1;
        tree.end("p");
        tree.end("b");
        assert!(
            tree.active.iter().any(|a| a.bytes == 2_000),
            "BIG stays active"
        );
        tree.end("div");
        assert!(tree.active.iter().any(|a| a.bytes == 2_000));

        // So every later paragraph reopens BIG, and the pass charges it.
        let style = format!("color: red;{}", " ".repeat(2_000));
        let href = format!("/{}", "a".repeat(2_000));
        let paragraphs = "<p>x".repeat(3_000);
        for html in [
            format!("<div><b style=\"{style}\"><p><b></p></b></div>{paragraphs}"),
            format!("<ul><li><b style=\"{style}\"><p><b></p></b></li></ul>{paragraphs}"),
            format!("<b style=\"{style}\"><p><b></p></b>{paragraphs}"),
            format!("<div><a href=\"{href}\"><p><a></p></a></div>{paragraphs}"),
            format!("<div><em title=\"{href}\"><p><em></p></em></div>{paragraphs}"),
            format!(
                "<div><b style=\"{style}\"><p><b></p></b></div>\
                 <div><i style=\"{style}\"><p><i></p></i></div>{paragraphs}"
            ),
        ] {
            assert!(!nesting_within_cap(&html), "{}", &html[..12]);
        }
        // The same element closed while it is the last open still stops being active.
        assert!(nesting_within_cap(&format!(
            "<div><b style=\"{style}\"><p>x</p></b></div>{paragraphs}"
        )));
    }

    #[test]
    fn formatting_start_tags_longer_than_the_cap_are_refused() {
        let at_cap = format!(
            "<b title=\"{}\">",
            "a".repeat(MAX_FORMATTING_ATTRIBUTES - 9)
        );
        assert_eq!(at_cap.len(), MAX_FORMATTING_ATTRIBUTES + 3);
        assert!(nesting_within_cap(&format!("<p>{at_cap}x</b></p>")));
        let past = format!(
            "<b title=\"{}\">",
            "a".repeat(MAX_FORMATTING_ATTRIBUTES - 8)
        );
        assert!(!nesting_within_cap(&format!("<p>{past}x</b></p>")));
        for name in FORMATTING_ELEMENTS {
            let html = format!("<{name} title=\"{}\">x</{name}>", "a".repeat(5_000));
            assert!(!nesting_within_cap(&html), "{name}");
        }
        // Other elements keep long attributes.
        let html = format!("<span title=\"{}\">x</span>", "a".repeat(20_000));
        assert!(nesting_within_cap(&html));
    }

    #[test]
    fn the_serialising_work_counts_ampersands_and_0xc2_bytes() {
        for html in [
            "&".repeat(150_000),
            "\u{a0}".repeat(150_000),
            "\u{b0}".repeat(150_000),
            format!("<span title=\"{}\">x</span>", "\u{a9}".repeat(150_000)),
        ] {
            assert!(nesting_within_cap(&html), "the shape is fine");
            assert!(!parse_is_bounded(&html), "the serialising work is not");
        }
        assert!(parse_is_bounded(&"caf\u{e9} \u{b0} &amp; ".repeat(1_000)));
    }

    #[test]
    fn the_tree_model_closes_paragraphs_and_keeps_formatting_active() {
        let mut tree = TreeModel::default();
        for (start, name) in [(true, "p"), (true, "b"), (true, "i"), (false, "p")] {
            if start {
                tree.start(name.to_owned(), 3, false);
            } else {
                tree.end(name);
            }
        }
        // `</p>` closed the paragraph and both formatting elements, which stay active.
        assert!(tree.open.is_empty() && tree.root.is_empty());
        assert_eq!(tree.formatting(), 2);
        assert_eq!(tree.active_bytes(), 6);

        let mut tree = TreeModel::default();
        tree.start("div".to_owned(), 5, false);
        tree.start("p".to_owned(), 3, false);
        tree.start("b".to_owned(), 3, false);
        tree.end("b");
        assert_eq!(tree.formatting(), 0, "closed while last open");
        tree.end("div");
        assert!(
            tree.open.is_empty(),
            "a block closes with the paragraphs open in it"
        );
        let mut tree = TreeModel::default();
        tree.start("table".to_owned(), 7, false);
        tree.start("tr".to_owned(), 4, false);
        tree.start("td".to_owned(), 4, false);
        tree.end("table");
        assert!(
            tree.open.is_empty(),
            "a table closes with its rows and cells"
        );

        // Uncounted elements pile up without counting, and close only from the top.
        let mut tree = TreeModel::default();
        for _ in 0..1_000 {
            tree.start("td".to_owned(), 4, false);
        }
        tree.end("p");
        assert_eq!(tree.root.len(), 1_000);
        tree.end("td");
        assert_eq!(tree.root.len(), 999);
        assert!(tree.within_caps());
    }
}
