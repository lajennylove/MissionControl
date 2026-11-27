//! WordPress's `sanitize_option`: how each option is cleaned before it is stored, shared by the
//! REST settings route ([`crate::rest::settings`]) and the admin's settings screens
//! ([`crate::admin::settings`]).
//!
//! As WordPress, a value `sanitize_option` refuses is not stored: the caller keeps the stored
//! value, and the settings screen reports the [`OptionError`]'s message (WordPress's
//! `add_settings_error`).
//!
//! ```
//! use mission_control_core::content::sanitize_option;
//! use serde_json::json;
//!
//! assert_eq!(sanitize_option("blogname", json!("Tom & Jerry")).unwrap(), json!("Tom &amp; Jerry"));
//! assert_eq!(sanitize_option("posts_per_page", json!(0)).unwrap(), json!(1));
//! assert!(sanitize_option("permalink_structure", json!("/archives/")).is_err());
//! ```

use std::fmt;

use serde_json::Value;

use super::kses_data;
use super::permalink::PermalinkStructure;
use crate::rest::params::as_integer;

/// Why [`sanitize_option`] refused a value: the option and WordPress's message for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionError {
    /// The option, such as `admin_email`.
    pub option: String,
    /// WordPress's message, as its settings screen shows it.
    pub message: String,
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.option, self.message)
    }
}

impl std::error::Error for OptionError {}

/// The options [`sanitize_option`] stores as `absint` does: a whole number, made positive.
const ABSINT_OPTIONS: &[&str] = &[
    "thumbnail_size_w",
    "thumbnail_size_h",
    "medium_size_w",
    "medium_size_h",
    "medium_large_size_w",
    "medium_large_size_h",
    "large_size_w",
    "large_size_h",
    "mailserver_port",
    "comment_max_links",
    "page_on_front",
    "page_for_posts",
    "rss_excerpt_length",
    "default_category",
    "default_email_category",
    "default_link_category",
    "close_comments_days_old",
    "comments_per_page",
    "thread_comments_depth",
    "users_can_register",
    "start_of_week",
    "site_icon",
    "fileupload_maxk",
];

/// `value` cleaned as WordPress's `sanitize_option( $option, $value )` cleans it, or the error
/// it reports (the caller then keeps the stored value):
///
/// - `blogname`, `blogdescription`: `esc_html`.
/// - `siteurl`, `home`: cleaned with `esc_url_raw`, then an `http://` or `https://` address.
/// - `admin_email`: trimmed, and an address `is_email` accepts.
/// - `timezone_string`: an IANA name (legacy names such as `US/Eastern` and `UTC` included),
///   or empty.
/// - `date_format`, `time_format`: `strip_tags`, then `wp_kses_data`.
/// - `posts_per_page`, `posts_per_rss`: a whole number; 0 becomes 1 and below -1 is made
///   positive.
/// - the ids, sizes and counts WordPress runs `absint` on (`page_on_front`, `default_category`,
///   `thumbnail_size_w`, `comments_per_page`, `start_of_week`, ...).
/// - `default_ping_status`, `default_comment_status`: `0` or empty becomes `closed`.
/// - `blog_public`: a whole number, 1 when unset.
/// - `gmt_offset`: only digits, `:`, `.` and `-` are kept, as text.
/// - `permalink_structure`, `category_base`, `tag_base`: `esc_url_raw`, with `http://` taken
///   out; a structure that is not empty needs a `%tag%`. Mission Control also refuses a
///   structure its permalinks cannot read (an unknown tag, a `%` not closed), which WordPress
///   would store.
/// - `moderation_keys`, `disallowed_keys`: one word per line, trimmed, without blank lines or
///   repeats.
///
/// Any other option is returned unchanged. A value that is not text where text is needed is
/// refused.
///
/// # Errors
///
/// [`OptionError`] with WordPress's message, as `add_settings_error` reports it.
pub fn sanitize_option(option: &str, value: Value) -> Result<Value, OptionError> {
    let refuse = |message: &str| OptionError {
        option: option.to_owned(),
        message: message.to_owned(),
    };
    let text = || -> Result<String, OptionError> {
        match &value {
            Value::String(text) => Ok(text.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Null => Ok(String::new()),
            _ => Err(refuse("The value is not text.")),
        }
    };
    Ok(match option {
        "blogname" | "blogdescription" => match &value {
            Value::String(text) => Value::String(esc_html(text)),
            _ => return Err(refuse("The value is not text.")),
        },
        "siteurl" | "home" => {
            // As WordPress: cleaned with `esc_url_raw` first, then checked.
            let url = esc_url_raw(&text()?);
            if !is_http_url(&url) {
                return Err(refuse(if option == "siteurl" {
                    "The Mission Control address you entered did not appear to be a valid URL. \
                     Please enter a valid URL."
                } else {
                    "The Site address you entered did not appear to be a valid URL. Please \
                     enter a valid URL."
                }));
            }
            Value::String(url)
        }
        "admin_email" | "new_admin_email" => {
            let email = text()?;
            let email = email.trim();
            if !is_email(email) {
                return Err(refuse(
                    "The email address entered did not appear to be a valid email address. \
                     Please enter a valid email address.",
                ));
            }
            Value::String(email.to_owned())
        }
        "timezone_string" => {
            let zone = text()?;
            if !zone.is_empty() && !is_timezone(&zone) {
                return Err(refuse(
                    "The timezone you have entered is not valid. Please select a valid timezone.",
                ));
            }
            Value::String(zone)
        }
        "date_format" | "time_format" => Value::String(kses_data(&strip_tags(&text()?))),
        "posts_per_page" | "posts_per_rss" => Value::from(match int(&value) {
            0 => 1,
            n if n < -1 => n.saturating_abs(),
            n => n,
        }),
        option if ABSINT_OPTIONS.contains(&option) => Value::from(int(&value).unsigned_abs()),
        "default_ping_status" | "default_comment_status" => match &value {
            Value::Null => Value::from("closed"),
            Value::String(s) if s.is_empty() || s == "0" => Value::from("closed"),
            Value::Number(n) if n.as_i64() == Some(0) => Value::from("closed"),
            _ => Value::String(text()?),
        },
        "blog_public" => match &value {
            Value::Null => Value::from(1),
            _ => Value::from(int(&value)),
        },
        "gmt_offset" => Value::String(
            text()?
                .chars()
                .filter(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '-'))
                .collect(),
        ),
        "permalink_structure" | "category_base" | "tag_base" => {
            let cleaned = esc_url_raw(&text()?).replace("http://", "");
            if option == "permalink_structure" && !cleaned.is_empty() {
                if !has_structure_tag(&cleaned) {
                    return Err(refuse(
                        "A structure tag is required when using custom permalinks.",
                    ));
                }
                if let Err(e) = cleaned.parse::<PermalinkStructure>() {
                    return Err(OptionError {
                        option: option.to_owned(),
                        message: format!("The permalink structure is not valid: {e}."),
                    });
                }
            }
            Value::String(cleaned)
        }
        "moderation_keys" | "disallowed_keys" => {
            let mut seen = Vec::<String>::new();
            for line in text()?.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if !seen.iter().any(|s| s == line) {
                    seen.push(line.to_owned());
                }
            }
            Value::String(seen.join("\n"))
        }
        _ => value,
    })
}

/// PHP's `(int)` on an option value: a whole number, or 0.
fn int(value: &Value) -> i64 {
    match value {
        Value::Bool(true) => 1,
        Value::String(s) => as_integer(value).unwrap_or_else(|| leading_int(s)),
        _ => as_integer(value).unwrap_or(0),
    }
}

/// The whole number at the start of `text`, as PHP reads `"12px"` as 12; 0 when there is none.
fn leading_int(text: &str) -> i64 {
    let text = text.trim_start();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits: String = digits
        .chars()
        .take_while(char::is_ascii_digit)
        .take(18)
        .collect();
    digits.parse::<i64>().map_or(0, |n| sign * n)
}

/// Whether `structure` holds a tag, as WordPress's `/%[^\/%]+%/` finds one.
fn has_structure_tag(structure: &str) -> bool {
    let mut rest = structure;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let Some(end) = after.find(['%', '/']) else {
            return false;
        };
        if end > 0 && after[end..].starts_with('%') {
            return true;
        }
        rest = &after[end..];
    }
    false
}

/// `esc_html`: `&`, `<`, `>`, `"` and `'` escaped, without encoding an entity again (`&amp;`
/// stays `&amp;`), so a value read and saved back does not change.
fn esc_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        match c {
            '&' if starts_entity(&text[i..]) => out.push('&'),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#039;"),
            _ => out.push(c),
        }
    }
    out
}

/// Whether `text` starts with an entity: `&name;`, `&#123;` or `&#x1F;`.
fn starts_entity(text: &str) -> bool {
    let Some(end) = text.get(1..).and_then(|rest| rest.find(';')) else {
        return false;
    };
    let body = &text[1..=end];
    if let Some(number) = body.strip_prefix('#') {
        match number.strip_prefix(['x', 'X']) {
            Some(hex) => {
                !hex.is_empty() && hex.len() <= 6 && hex.chars().all(|c| c.is_ascii_hexdigit())
            }
            None => {
                !number.is_empty()
                    && number.len() <= 7
                    && number.chars().all(|c| c.is_ascii_digit())
            }
        }
    } else {
        body.len() <= 32
            && body.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && body.chars().all(|c| c.is_ascii_alphanumeric())
    }
}

/// PHP's `strip_tags`, as `sanitize_option` runs it on the date and time formats.
fn strip_tags(text: &str) -> String {
    crate::text::strip_all_tags(text)
}

/// The protocols `esc_url` keeps, as `wp_allowed_protocols` lists them.
const ALLOWED_PROTOCOLS: &[&str] = &[
    "http", "https", "ftp", "ftps", "mailto", "news", "irc", "irc6", "ircs", "gopher", "nntp",
    "feed", "telnet", "mms", "rtsp", "sms", "svn", "tel", "fax", "xmpp", "webcal", "urn",
];

/// `esc_url_raw`: spaces encoded, characters a URL cannot hold removed, `http://` added to an
/// address with no scheme, and empty for a protocol that is not allowed.
pub(crate) fn esc_url_raw(url: &str) -> String {
    let url = url.trim().replace(' ', "%20");
    let mut url: String = url
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || !c.is_ascii() || "-~+_.?#=!&;,/:%@$|*'()[]".contains(*c)
        })
        .collect();
    for encoded in ["%0d", "%0a", "%0D", "%0A"] {
        while url.contains(encoded) {
            url = url.replace(encoded, "");
        }
    }
    if url.is_empty() {
        return url;
    }
    let looks_like_php = {
        let name: String = url
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        !name.is_empty() && url[name.len()..].to_ascii_lowercase().starts_with(".php")
    };
    if !url.contains(':') && !url.starts_with(['/', '#', '?']) && !looks_like_php {
        url = format!("http://{url}");
    }
    if let Some((scheme, _)) = url.split_once(':') {
        let scheme_like = !scheme.is_empty()
            && !scheme.contains(['/', '?', '#'])
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        if scheme_like && !ALLOWED_PROTOCOLS.contains(&scheme.to_ascii_lowercase().as_str()) {
            return String::new();
        }
    }
    url
}

/// Whether `url` is a link a custom menu item can hold: not empty, no spaces or control
/// characters (the menu editor's URL field is an `<input type="url">`), matching the menu
/// editor's check ([`matches_menu_url_pattern`], `nav-menu.js`' `urlRegex`), and a URL
/// `esc_url_raw` keeps: a path, query or fragment (`/about`, `?p=2`, `#top`), or an allowed
/// protocol (`https://example.com`, `mailto:...`). A bare word or host without a protocol
/// (`notaurl`, `example.com`) is no link, as WordPress's menu editor refuses it, and neither is
/// `https://` with nothing after it.
pub fn is_valid_link_url(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if !matches_menu_url_pattern(url) {
        return false;
    }
    if url.starts_with(['/', '#', '?']) {
        return true;
    }
    let escaped = esc_url_raw(url);
    if escaped.is_empty() || escaped != url && escaped != format!("http://{url}") {
        return false;
    }
    match escaped.split_once("://") {
        Some((_, rest)) => rest
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|host| !host.is_empty() && !host.starts_with(['.', '-'])),
        // `mailto:`, `tel:` and the other protocols without an authority need something after
        // the colon.
        None => escaped
            .split_once(':')
            .is_some_and(|(_, rest)| !rest.is_empty()),
    }
}

/// Whether `url` matches the regular expression WordPress's menu editor checks a custom link's
/// URL with (`nav-menu.js`, `addCustomLink` and the Add to Menu button):
/// `^((\w+:)?\/\/\w.*|\w+:(?!\/\/$)|\/|\?|#)`: a protocol followed by anything but a
/// lone `//`, or a URL that starts with `/`, `?` or `#`.
fn matches_menu_url_pattern(url: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    // `\/` matches `//...` too.
    if url.starts_with(['/', '?', '#']) {
        return true;
    }
    let scheme_end = url.find(|c: char| !word(c)).unwrap_or(url.len());
    if scheme_end == 0 {
        return false;
    }
    // `\w+:(?!\/\/$)`, or `\w+:\/\/\w.*`: both hold unless only `//` follows the colon.
    url[scheme_end..]
        .strip_prefix(':')
        .is_some_and(|rest| rest != "//")
}

/// Whether `url` holds `http://` or `https://` and an address after it, as `sanitize_option`
/// checks `siteurl` and `home`.
fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ["http://", "https://"].iter().any(|scheme| {
        lower
            .find(scheme)
            .is_some_and(|at| at + scheme.len() < lower.len())
    })
}

/// WordPress's `is_email`: a local part of the allowed characters, an `@`, and a domain of at
/// least two dot-separated labels of letters, digits and inner hyphens.
pub fn is_email(email: &str) -> bool {
    if email.len() < 6 {
        return false;
    }
    let Some(at) = email.find('@').filter(|at| *at > 0) else {
        return false;
    };
    let (local, domain) = (&email[..at], &email[at + 1..]);
    let local_ok = local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c));
    if !local_ok || domain.contains("..") {
        return false;
    }
    let domain = domain.trim_matches(|c: char| c == '.' || c.is_whitespace() || c == '\0');
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            let label = label.trim_matches(|c: char| c == '-' || c.is_whitespace());
            !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Whether `zone` is a timezone identifier PHP's `timezone_identifiers_list( ALL_WITH_BC )`
/// lists: an IANA name, such as `America/Toronto`, `UTC` or a legacy `US/Eastern`.
pub(crate) fn is_timezone(zone: &str) -> bool {
    zone.parse::<chrono_tz::Tz>().is_ok()
}

/// The offsets WordPress's timezone menu offers as `UTC+N` and `UTC-N`, in hours.
fn is_menu_offset(hours: f64) -> bool {
    (-12.0..=14.0).contains(&hours)
        && ((hours * 2.0).fract() == 0.0 || [5.75, 8.75, 12.75, 13.75].contains(&hours))
}

/// The `gmt_offset` a `UTC+N` or `UTC-N` timezone choice sets, as WordPress's settings screen
/// (`options.php` for `options-general.php`) turns it into one: `UTC+5.5` is `5.5`, `UTC-3` is
/// `-3`. `None` for any other value, and for an offset the menu does not offer.
pub(crate) fn utc_offset(zone: &str) -> Option<String> {
    let rest = zone.strip_prefix("UTC")?;
    let (negative, number) = match rest.as_bytes().first()? {
        b'+' => (false, &rest[1..]),
        b'-' => (true, &rest[1..]),
        _ => return None,
    };
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let hours: f64 = number.parse().ok()?;
    let hours = if negative { -hours } else { hours };
    is_menu_offset(hours).then(|| format!("{}{number}", if negative { "-" } else { "" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_menu_link_is_a_url_esc_url_keeps() {
        for valid in [
            "https://example.com",
            "http://example.com/a?b=c#d",
            "//example.com/a",
            "/about/",
            "#top",
            "?p=2",
            "mailto:owner@example.com",
            "tel:+15551234567",
        ] {
            assert!(is_valid_link_url(valid), "{valid}");
        }
        for invalid in [
            "",
            "   ",
            "not a url",
            "notaurl",
            "example.com",
            "https://",
            "http://",
            "javascript:alert(1)",
            "https://exa mple.com",
            "mailto:",
        ] {
            assert!(!is_valid_link_url(invalid), "{invalid}");
        }
    }

    #[test]
    fn titles_are_escaped_once() {
        assert_eq!(esc_html("Tom & Jerry <b>"), "Tom &amp; Jerry &lt;b&gt;");
        assert_eq!(
            esc_html("Tom &amp; Jerry &#8217;"),
            "Tom &amp; Jerry &#8217;"
        );
        assert_eq!(esc_html("a &b c"), "a &amp;b c");
    }

    #[test]
    fn urls_are_cleaned_as_esc_url_raw_cleans_them() {
        assert_eq!(esc_url_raw(" example.com/a b "), "http://example.com/a%20b");
        assert_eq!(esc_url_raw("https://x.test/<p>"), "https://x.test/p");
        assert_eq!(esc_url_raw("javascript:alert(1)"), "");
        assert!(is_http_url("https://x.test"));
        assert!(!is_http_url("ftp://x.test"));
    }

    #[test]
    fn emails_and_timezones_are_checked() {
        assert!(is_email("admin@example.com"));
        assert!(!is_email("admin@localhost"));
        assert!(!is_email("@example.com"));
        assert!(is_timezone("America/Toronto"));
        assert!(is_timezone("America/Argentina/Buenos_Aires"));
        assert!(is_timezone("UTC"));
        assert!(is_timezone("US/Eastern"));
        assert!(!is_timezone("Mars/Olympus"));
        assert!(!is_timezone("Europe/Narnia"));
        assert!(!is_timezone("UTC+5"));
        assert_eq!(utc_offset("UTC+5.5").as_deref(), Some("5.5"));
        assert_eq!(utc_offset("UTC-3").as_deref(), Some("-3"));
        assert_eq!(utc_offset("UTC+13.75").as_deref(), Some("13.75"));
        assert_eq!(utc_offset("UTC+0").as_deref(), Some("0"));
        assert_eq!(utc_offset("UTC+5.3"), None);
        assert_eq!(utc_offset("UTC+15"), None);
        assert_eq!(utc_offset("UTC5"), None);
    }

    #[test]
    fn options_are_sanitized_as_wordpress_sanitizes_them() {
        let ok = |option, value| sanitize_option(option, value).unwrap();
        assert_eq!(ok("posts_per_page", json!(-5)), json!(5));
        assert_eq!(ok("posts_per_rss", json!("abc")), json!(1));
        assert_eq!(ok("thumbnail_size_w", json!("-150")), json!(150));
        assert_eq!(ok("comment_max_links", json!("2px")), json!(2));
        assert_eq!(ok("default_ping_status", json!("")), json!("closed"));
        assert_eq!(ok("default_comment_status", json!("open")), json!("open"));
        assert_eq!(ok("blog_public", Value::Null), json!(1));
        assert_eq!(ok("blog_public", json!("0")), json!(0));
        assert_eq!(ok("gmt_offset", json!("+5:30h")), json!("5:30"));
        assert_eq!(ok("date_format", json!("<b>j F</b> Y")), json!("j F Y"));
        assert_eq!(
            ok("moderation_keys", json!(" spam \n\nviagra\nspam\n")),
            json!("spam\nviagra")
        );
        assert_eq!(
            ok("permalink_structure", json!("/%postname%/")),
            json!("/%postname%/")
        );
        assert_eq!(
            ok("permalink_structure", json!("archives/%post_id%")),
            json!("archives/%post_id%")
        );
        assert_eq!(ok("permalink_structure", json!("")), json!(""));
        assert_eq!(ok("category_base", json!("topics")), json!("topics"));
        assert_eq!(ok("acme_unknown", json!([1])), json!([1]));
        // As WordPress, the address is cleaned first: `esc_url_raw` adds `http://`.
        assert_eq!(
            ok("home", json!("example.com")),
            json!("http://example.com")
        );
        for (option, value) in [
            ("permalink_structure", json!("/archives/")),
            ("permalink_structure", json!("/%nope%/")),
            ("permalink_structure", json!("/%postname/")),
            ("admin_email", json!("nope")),
            ("siteurl", json!("ftp://example.com")),
            ("home", json!("javascript:alert(1)")),
            ("timezone_string", json!("UTC+5")),
            ("blogname", json!(5)),
        ] {
            let error = sanitize_option(option, value.clone()).unwrap_err();
            assert_eq!(error.option, option, "{value}");
            assert!(!error.message.is_empty());
        }
        assert!(has_structure_tag("/a/%x%/"));
        assert!(!has_structure_tag("/%a/b%/"));
        assert!(!has_structure_tag("%%"));
    }
}
