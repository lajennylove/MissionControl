//! Text cleaning shared by the content, media and user code, ported from WordPress's
//! formatting functions.

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
fn strip_all_tags(text: &str) -> String {
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
