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
//! Input nested deeper than [`MAX_NESTING`] elements is not parsed (the parser's time grows
//! with length times depth); it is stored as escaped text. The input's size is bounded by the
//! layer that receives it (the routes' request body limits), not by kses.

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
/// typed.
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
/// Input nested deeper than [`MAX_NESTING`] is stored as escaped text, unparsed.
pub fn kses_post(html: &str) -> String {
    if !nesting_within_cap(html) {
        return escape_text(html);
    }
    let (protected, token, markers) = protect_markers(html);
    let cleaned = POST_SANITIZER.clean(&protected).to_string();
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
    if !nesting_within_cap(html) {
        return escape_text(html);
    }
    DATA_SANITIZER.clean(html).to_string()
}

/// `title` as plain text, for a user without `unfiltered_html` (Ruling R39: a title is text,
/// and themes escape it when they print it): every tag is stripped (`<script>` and `<style>`
/// with their content, and comments), and entities are decoded once, so `A & <em>B</em>` and
/// `A &amp; B` are both stored as `A & B`, and `&amp;lt;` as `&lt;`.
///
/// WordPress instead keeps the `$allowedtags` markup in titles ([`kses_data`]) and prints them
/// raw; Ruling R39 departs from it.
pub fn kses_title(title: &str) -> String {
    if !nesting_within_cap(title) {
        // Not parsed: kept as typed, which is text, as a title is.
        return title.to_owned();
    }
    let text = TEXT_SANITIZER.clean(title).to_string();
    decode_serialized_text(&text)
}

/// `html` as text, escaped as the sanitiser escapes text (`&`, `<`, `>` and the no-break
/// space), so that it reads back as the same text and contains no markup. What kses stores for
/// input nested past [`MAX_NESTING`].
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

/// The elements [`nesting_within_cap`] does not count (outside SVG and MathML): the parser
/// closes them itself when a sibling starts or their parent ends (`p`; table sections, rows and
/// cells, which only open inside a counted `<table>` or `<template>`), or merges them into the
/// document's own (`html`, `head`, `body`). None of them can nest inside itself without a
/// counted element in between, so leaving them out at most doubles the depth the cap allows,
/// and keeps unclosed `<p>` and table cells from adding up. (`li`, `dd`, `dt`, `option` and
/// ruby text are counted: they close a sibling only in some contexts.)
const UNCOUNTED_ELEMENTS: &[&str] = &[
    "html", "head", "body", "p", "tr", "td", "th", "tbody", "thead", "tfoot", "caption", "colgroup",
];

/// The elements whose content the tokenizer reads as text up to their end tag (raw text and
/// escapable raw text, with scripting on, as html5ever parses for ammonia).
const RAW_TEXT_ELEMENTS: &[&str] = &[
    "script", "style", "xmp", "iframe", "noembed", "noframes", "noscript", "textarea", "title",
];

/// Whether `html` nests elements no deeper than [`MAX_NESTING`], found in one linear pass
/// before html5ever parses it.
///
/// html5ever (as browsers' parsers) scans its stack of open elements for many tags, so its time
/// grows with the input's length times its nesting: 100,000 nested `<div>` take seconds.
/// Capping the nesting keeps kses linear in the input's length. **This guard is the only bound
/// kses enforces; the size of the input is bounded by the layer that receives it** (the request
/// body limit of the REST and admin routes, Rocket's data limits), not here.
///
/// The pass tokenizes as the HTML standard does (tags, quoted and unquoted attribute values,
/// comments with all their endings, bogus comments, `DOCTYPE`, raw text elements, CDATA), and
/// models the tree conservatively, so that it can count more open elements than the parser
/// keeps, never fewer:
///
/// - every start tag of an element that is not void or [uncounted](UNCOUNTED_ELEMENTS) opens
///   one (every start tag at all when the input has an `<svg` or `<math` tag, since in SVG and
///   MathML those names are ordinary elements), `<li>` closing an open `<li>`, and `<dd>` or `<dt>` an open `<dd>` or `<dt>`, as the
///   parser does;
/// - an end tag closes only the element it names when that element is the last one open (and
///   `</ul>`, `</ol>`, `</menu>` and `</dl>` their open items first); an end tag the parser
///   would ignore, or one inside a comment, an attribute value, raw text or CDATA, closes
///   nothing;
/// - text that may be markup for the parser counts each `<` followed by a letter as an element
///   that never closes: CDATA (a bogus comment outside SVG and MathML, which ends sooner), and,
///   when the input has an `<svg` or `<math` tag anywhere, raw text and the rest of the input
///   after `<plaintext>` (in SVG and MathML, `<style>`, `<title>` or `<plaintext>` hold
///   markup); and after a
///   `<script>` whose text holds `<!--` (where the parser's end of the script is hard to tell),
///   no end tag closes anything.
///
/// Past the cap, the input is not parsed: [`kses_post`] and [`kses_data`] store it as escaped
/// text (every `<` as `&lt;`), so nothing runs and nothing typed is lost, and [`kses_title`]
/// keeps it as typed, as the text a title is.
fn nesting_within_cap(html: &str) -> bool {
    let bytes = html.as_bytes();
    let length = bytes.len();
    // The counted elements open, by name, last opened last.
    let mut open: Vec<String> = Vec::new();
    // Elements counted in text that may be markup; they never close.
    let mut unclosable = 0usize;
    // Once set, no end tag closes anything.
    let mut no_closing = false;
    // Whether the parser may be in SVG or MathML anywhere, where raw text is markup.
    let foreign = contains_ignoring_case(bytes, b"<svg") || contains_ignoring_case(bytes, b"<math");
    let mut i = 0;
    while let Some(at) = bytes[i..].iter().position(|&b| b == b'<').map(|at| i + at) {
        if open.len() + unclosable > MAX_NESTING {
            return false;
        }
        i = at + 1;
        let Some(&next) = bytes.get(i) else { break };
        if next.is_ascii_alphabetic() {
            let (name_end, after) = scan_tag(bytes, i);
            let name = html[i..name_end].to_ascii_lowercase();
            i = after;
            // In SVG and MathML, these names are ordinary elements, which nest.
            if !foreign
                && (VOID_ELEMENTS.contains(&name.as_str())
                    || UNCOUNTED_ELEMENTS.contains(&name.as_str()))
            {
                continue;
            }
            if name == "plaintext" {
                // The rest is text for the parser, or markup in SVG and MathML.
                if foreign {
                    unclosable += 1 + count_tag_opens(&bytes[i..]);
                }
                break;
            }
            let top = open.last().map(String::as_str);
            if (name == "li" && top == Some("li"))
                || (matches!(name.as_str(), "dd" | "dt") && matches!(top, Some("dd" | "dt")))
            {
                open.pop();
            }
            if RAW_TEXT_ELEMENTS.contains(&name.as_str()) {
                let close = find_raw_text_end(bytes, i, &name).unwrap_or(length);
                let text = &bytes[i..close];
                if foreign {
                    unclosable += count_tag_opens(text);
                }
                if name == "script" && contains(text, b"<!--") {
                    no_closing = true;
                }
                // The end tag is read as a tag on the next turn.
                i = close;
            }
            open.push(name);
        } else if next == b'/' {
            match bytes.get(i + 1) {
                Some(c) if c.is_ascii_alphabetic() => {
                    let (name_end, after) = scan_tag(bytes, i + 1);
                    let name = html[i + 1..name_end].to_ascii_lowercase();
                    i = after;
                    if !no_closing {
                        close_element(&mut open, &name);
                    }
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
                unclosable += count_tag_opens(&bytes[i..end]);
                i = end;
            } else {
                // `DOCTYPE` or a bogus comment, up to the next `>`.
                i = skip_past(bytes, i, b">");
            }
        } else if next == b'?' {
            i = skip_past(bytes, i, b">");
        }
    }
    open.len() + unclosable <= MAX_NESTING
}

/// Closes `name` in the model of [`nesting_within_cap`]: only when it is the last element open,
/// after the open items of a list it ends.
fn close_element(open: &mut Vec<String>, name: &str) {
    let items: &[&str] = match name {
        "ul" | "ol" | "menu" => &["li"],
        "dl" => &["dd", "dt"],
        _ => &[],
    };
    let below_items = open
        .iter()
        .rposition(|element| !items.contains(&element.as_str()));
    if let Some(at) = below_items
        && open[at] == name
    {
        open.truncate(at);
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
/// No `rel` is added to links with a `target`: WordPress 5.1 to 6.6 added `rel="noopener"` on
/// save (`wp_targeted_link_rel`), but WordPress 6.7 deprecated it and no longer runs it.
///
/// Every path that saves a post's fields for a user calls this first (the REST API's and the
/// admin's create, update and autosave routes, and imports that act for a user):
/// [`Post::create`](super::Post::create), [`Post::save`](super::Post::save) and the revision
/// and attachment writers store what they are given.
///
/// ```ignore
/// let fields = kses_post_fields(&user, PostFields { title, content, excerpt });
/// let post = Post::create(&db, NewPost {
///     title: fields.title,
///     content: fields.content,
///     excerpt: fields.excerpt,
///     ..NewPost::new(user.id(), "post")
/// })
/// .await?;
/// ```
pub fn kses_post_fields(user: &UserCaps, fields: PostFields) -> PostFields {
    if has_cap(user, "unfiltered_html") {
        return fields;
    }
    PostFields {
        title: kses_title(&fields.title),
        content: kses_post(&fields.content),
        excerpt: kses_post(&fields.excerpt),
    }
}

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
