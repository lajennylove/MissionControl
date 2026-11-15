//! kses, WordPress's HTML filter on save: the markup a user without the `unfiltered_html`
//! capability may store in a post.
//!
//! WordPress runs it from `kses_init_filters` when the acting user lacks `unfiltered_html`: the
//! content and the excerpt go through `wp_filter_post_kses` (the `$allowedposttags` list), and
//! the title through `wp_filter_kses` (the smaller `$allowedtags` list). Administrators and
//! editors hold `unfiltered_html` on a single site, so what they send is stored as sent.
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
//! - the tags and attributes of `$allowedtags`, without global attributes, for titles;
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

/// The sanitiser for post content and excerpts.
static POST_SANITIZER: LazyLock<Builder<'static>> =
    LazyLock::new(|| sanitizer(ALLOWED_POST_TAGS, GLOBAL_ATTRIBUTES, true));

/// The sanitiser for titles.
static DATA_SANITIZER: LazyLock<Builder<'static>> =
    LazyLock::new(|| sanitizer(ALLOWED_TAGS, &[], false));

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
/// comments go. See the [module documentation](self) for how it differs from WordPress.
pub fn kses_post(html: &str) -> String {
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

/// `html` filtered for a title, as WordPress's `wp_filter_kses`: only the tags and attributes
/// of `$allowedtags` (`a` with `href` and `title`, `abbr`, `acronym`, `b`, `blockquote`,
/// `cite`, `code`, `del`, `em`, `i`, `q`, `s`, `strike`, `strong`), with URLs checked as in
/// [`kses_post`]. Comments are removed.
pub fn kses_data(html: &str) -> String {
    DATA_SANITIZER.clean(html).to_string()
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
/// [`kses_post`] and the title through [`kses_data`]; otherwise (administrators and editors on
/// a single site) they are returned as sent.
///
/// Every path that saves a post's fields for a user calls this first; see the
/// [module documentation](self).
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
        title: kses_data(&fields.title),
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
/// placeholder is not the start of marker 10's.
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
