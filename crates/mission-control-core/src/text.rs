//! Text cleaning shared by the content, media and user code, ported from WordPress's
//! formatting functions.

/// `text` with the entities `esc_html` writes turned back into characters: `&amp;`, `&lt;`,
/// `&gt;`, `&quot;`, `&#039;` (and `&#39;`, `&apos;`, `&nbsp;`), and numeric references such as
/// `&#8217;` or `&#x2019;`. Other text, an unknown name and a reference to no character are kept.
///
/// An option stored through `esc_html` (`blogname`, `blogdescription`) is decoded with it before
/// a template or a form escapes it again: WordPress prints it with `esc_html` and `esc_attr`,
/// which do not encode an entity twice.
pub(crate) fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest.get(1..).and_then(|after| {
            let end = after.find(';').filter(|end| *end <= 10)?;
            let name = &after[..end];
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => '\u{a0}',
                _ => {
                    let number = name.strip_prefix('#')?;
                    let code = match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => number.parse().ok()?,
                    };
                    char::from_u32(code).filter(|c| *c != '\0')?
                }
            };
            Some((c, end + 2))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Cleans one line of text as WordPress's `sanitize_text_field` does: a `<` that does not open a
/// tag becomes `&lt;` (`wp_pre_kses_less_than`), tags are stripped (with the content of
/// `script` and `style` elements, as `wp_strip_all_tags` does), line breaks, tabs and runs of
/// spaces become one space, percent-encoded octets (`%C3`) are removed, and the ends are
/// trimmed.
pub(crate) fn sanitize_text_field(text: &str) -> String {
    let stripped = strip_all_tags(text);
    let mut collapsed = String::with_capacity(stripped.len());
    for c in stripped.chars() {
        let c = if matches!(c, '\r' | '\n' | '\t') {
            ' '
        } else {
            c
        };
        if c == ' ' && collapsed.ends_with(' ') {
            continue;
        }
        if c.is_control() {
            continue;
        }
        collapsed.push(c);
    }
    // WordPress removes octets until none is left, so `%%41` cannot leave a new one.
    let mut out = collapsed;
    loop {
        let next = remove_octets(&out);
        if next == out {
            break;
        }
        out = next;
    }
    out.trim_matches(' ').to_owned()
}

/// `text` without percent-encoded octets (`%` and two hex digits).
fn remove_octets(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b'%'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            i += 3;
            continue;
        }
        let c = text[i..].chars().next().expect("i is on a char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// `text` with its tags stripped, after `wp_pre_kses_less_than` and `wp_strip_all_tags`: a tag
/// is `<` followed by a letter, `/`, `!` or `?`, up to the next `>`; any other `<`, or one with
/// no `>` after it, becomes `&lt;`. The content of `script` and `style` elements goes with them.
pub(crate) fn strip_all_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let opens_tag = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));
        match after.find('>') {
            Some(end) if opens_tag => {
                let name: String = after
                    .chars()
                    .take_while(char::is_ascii_alphanumeric)
                    .collect::<String>()
                    .to_ascii_lowercase();
                rest = &after[end + 1..];
                if name == "script" || name == "style" {
                    let close = format!("</{name}");
                    rest = match rest.to_ascii_lowercase().find(&close) {
                        Some(at) => match rest[at..].find('>') {
                            Some(gt) => &rest[at + gt + 1..],
                            None => "",
                        },
                        None => "",
                    };
                }
            }
            _ => {
                out.push_str("&lt;");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `text` with `&`, `<`, `>`, `"` and `'` escaped, as `esc_html`.
pub(crate) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
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

/// `sanitize_html_class`: percent-encoded octets are removed, then every character but ASCII
/// letters, digits, `_` and `-`.
pub(crate) fn html_class(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            i += 3;
            continue;
        }
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            out.push(b as char);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities_esc_html_writes_are_decoded() {
        assert_eq!(decode_entities("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(
            decode_entities("Jenny&#039;s &lt;Blog&gt; &#x2019;&quot;"),
            "Jenny's <Blog> \u{2019}\""
        );
        assert_eq!(
            decode_entities("a & b &unknown; &#0; &#xZZ; &"),
            "a & b &unknown; &#0; &#xZZ; &"
        );
    }
}
