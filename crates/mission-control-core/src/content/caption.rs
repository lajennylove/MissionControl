//! WordPress's `[caption]` shortcode (`img_caption_shortcode`, Ruling R134), registered by default
//! as `caption` and `wp_caption` (see [`Shortcodes::register_core_shortcodes`]).
//!
//! The editor's Add Media inserts it for an image with a caption (`image_add_caption`):
//!
//! ```text
//! [caption id="attachment_7" align="aligncenter" width="300"]<img ... /> A caption[/caption]
//! ```
//!
//! It renders WordPress 6's HTML5 output (Mission Control's themes are HTML5; HTML5 captions
//! never added WordPress's extra 10 pixels to the width):
//!
//! ```text
//! <figure id="attachment_7" aria-describedby="caption-attachment-7" style="width: 300px"
//!     class="wp-caption aligncenter"><img ... /><figcaption id="caption-attachment-7"
//!     class="wp-caption-text">A caption</figcaption></figure>
//! ```
//!
//! As WordPress:
//!
//! - The caption is the `caption` attribute, or, in the new style, the text after the image (and
//!   the link around it) in the content, trimmed; anything before the image is dropped.
//! - With no width (not a whole number of at least 1) or no caption, the content is returned
//!   without the wrapper.
//! - `id` and `caption_id` are cleaned as `sanitize_html_class` cleans them; `caption_id`
//!   defaults to `caption-` and the id with `_` written `-`. Every attribute printed is escaped
//!   (`esc_attr`).
//! - The content runs through the shortcodes again (`do_shortcode`). The caption is printed as
//!   it is stored, HTML included: post content is filtered when it is saved (kses for users
//!   without `unfiltered_html`), not when it is shown. WordPress runs `wp_kses` on a `caption`
//!   attribute that holds `<`; Mission Control prints that attribute with its tags stripped
//!   instead, since a kses pass at render time is not available here.

use std::sync::{Arc, Weak};

use super::shortcode::{Attributes, Shortcodes};
use crate::text::{escape_html, html_class, strip_all_tags};

/// `[caption]`'s output for `atts` and `content`; `shortcodes` runs the shortcodes of the
/// content, as `do_shortcode( $content )` does (none when it is gone).
pub(crate) fn img_caption_shortcode(
    atts: &Attributes,
    content: Option<&str>,
    shortcodes: &Weak<Shortcodes>,
) -> String {
    let mut content = content.unwrap_or_default().to_owned();
    let caption = match atts.get("caption") {
        Some(caption) if caption.contains('<') => strip_all_tags(caption),
        Some(caption) => caption.to_owned(),
        None => match split_image(&content) {
            Some((image, rest)) => {
                let caption = rest.trim().to_owned();
                content = image.to_owned();
                caption
            }
            None => String::new(),
        },
    };
    let width = php_int(atts.get("width").unwrap_or_default());
    if width < 1 || caption.is_empty() {
        return content;
    }
    let id = html_class(atts.get("id").unwrap_or_default());
    let caption_id = match atts.get("caption_id").map(html_class) {
        Some(caption_id) if !caption_id.is_empty() => caption_id,
        _ if !id.is_empty() => format!("caption-{}", id.replace('_', "-")),
        _ => String::new(),
    };
    let align = atts.get("align").unwrap_or("alignnone");
    let class = format!(
        "wp-caption {align} {}",
        atts.get("class").unwrap_or_default()
    );
    let class = class.trim();
    let id_attr = if id.is_empty() {
        String::new()
    } else {
        format!("id=\"{}\" ", escape_html(&id))
    };
    let (caption_id_attr, describedby) = if caption_id.is_empty() {
        (String::new(), String::new())
    } else {
        let escaped = escape_html(&caption_id);
        (
            format!("id=\"{escaped}\" "),
            format!("aria-describedby=\"{escaped}\" "),
        )
    };
    let inner = match shortcodes.upgrade() {
        Some(shortcodes) => shortcodes.do_shortcodes(&content),
        None => content,
    };
    format!(
        "<figure {id_attr}{describedby}style=\"width: {width}px\" class=\"{}\">{inner}\
         <figcaption {caption_id_attr}class=\"wp-caption-text\">{caption}</figcaption></figure>",
        escape_html(class)
    )
}

/// PHP's `(int)` of a string: optional leading whitespace and sign, then the leading digits; 0
/// when there are none (or on overflow, which no width reaches).
fn php_int(text: &str) -> i64 {
    let text = text.trim_start();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |n| sign * n)
}

/// WordPress's `#((?:<a [^>]+>\s*)?<img [^>]+>(?:\s*</a>)?)(.*)#is`: the first image (with the
/// link around it) and everything after it, or `None` when the content has no `<img `.
fn split_image(content: &str) -> Option<(&str, &str)> {
    let lower = content.to_ascii_lowercase();
    let b = lower.as_bytes();
    // `<tag ` then one or more bytes that are not `>`, then `>`: the offset after it.
    let tag = |at: usize, name: &str| -> Option<usize> {
        let open = format!("<{name} ");
        if !lower[at..].starts_with(&open) {
            return None;
        }
        let from = at + open.len();
        let close = lower[from..].find('>')? + from;
        (close > from).then_some(close + 1)
    };
    let spaces = |mut at: usize| {
        while b.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at
    };
    let mut at = 0;
    while let Some(off) = lower[at..].find('<') {
        let start = at + off;
        let linked = tag(start, "a").and_then(|after_a| tag(spaces(after_a), "img"));
        let image_end = linked.or_else(|| tag(start, "img"));
        if let Some(mut end) = image_end {
            if linked.is_some() {
                let close = spaces(end);
                if lower[close..].starts_with("</a>") {
                    end = close + "</a>".len();
                }
            }
            return Some((&content[start..end], &content[end..]));
        }
        at = start + 1;
    }
    None
}

impl Shortcodes {
    /// Registers the shortcodes WordPress registers by default and Mission Control has: the
    /// `caption` and `wp_caption` shortcodes ([`img_caption_shortcode`]), owned by
    /// [`Owner::Core`](crate::hooks::Owner::Core). The app calls it once at start, beside
    /// [`register_the_content`](Self::register_the_content). A plugin may register the tag again
    /// to replace it.
    pub fn register_core_shortcodes(self: &Arc<Self>) {
        for tag in ["caption", "wp_caption"] {
            let weak = Arc::downgrade(self);
            self.add_shortcode(tag, crate::hooks::Owner::Core, move |atts, content| {
                img_caption_shortcode(atts, content, &weak)
            })
            .expect("caption is a valid shortcode tag");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_int_reads_leading_digits() {
        assert_eq!(php_int("300"), 300);
        assert_eq!(php_int(" 300px"), 300);
        assert_eq!(php_int("-5"), -5);
        assert_eq!(php_int("abc"), 0);
        assert_eq!(php_int(""), 0);
    }

    #[test]
    fn the_image_is_split_from_its_caption() {
        assert_eq!(
            split_image("x<IMG src=a> Cap"),
            Some(("<IMG src=a>", " Cap"))
        );
        assert_eq!(
            split_image("<a href=b> <img src=a> </a> Cap"),
            Some(("<a href=b> <img src=a> </a>", " Cap"))
        );
        assert_eq!(
            split_image("<a href=b>text</a><img src=a>Cap"),
            Some(("<img src=a>", "Cap"))
        );
        assert_eq!(split_image("no image"), None);
        assert_eq!(split_image("<img >"), None);
    }
}
