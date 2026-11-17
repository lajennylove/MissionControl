//! WordPress's `wpautop` and `shortcode_unautop` (`wp-includes/formatting.php`), the
//! `the_content` filters at priority 10 that turn blank lines into paragraphs and keep a
//! shortcode that stands alone out of a paragraph; and `wp_html_split`, which both use.
//!
//! The port follows WordPress 7.1's code step by step. WordPress writes the steps as regular
//! expressions; here each is scanned by hand, as the shortcode parser is, and the searches for
//! the end of a match (the next `>`, `</p>`, `[/tag]`, ...) read tables built once per step
//! ([`Next`]), so the work stays linear in the text. Each step names the expression it
//! replaces.
//!
//! ```
//! use mission_control_core::content::wpautop;
//!
//! assert_eq!(wpautop("One\n\nTwo\nlines"), "<p>One</p>\n<p>Two<br />\nlines</p>\n");
//! ```

use std::cell::RefCell;
use std::collections::HashMap;

/// The block-level elements of `wpautop`'s `$allblocks`, in its order. `h[1-6]` is written out.
const ALL_BLOCKS: &[&str] = &[
    "table",
    "thead",
    "tfoot",
    "caption",
    "col",
    "colgroup",
    "tbody",
    "tr",
    "td",
    "th",
    "div",
    "dl",
    "dd",
    "dt",
    "ul",
    "ol",
    "li",
    "pre",
    "form",
    "map",
    "area",
    "blockquote",
    "address",
    "style",
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "fieldset",
    "legend",
    "section",
    "article",
    "aside",
    "hgroup",
    "header",
    "footer",
    "nav",
    "figure",
    "figcaption",
    "details",
    "menu",
    "summary",
];

/// The block names.
fn all_blocks() -> &'static [&'static str] {
    ALL_BLOCKS
}

/// The tags before which a `<br />` is removed (`(?:p|li|div|dl|dd|dt|th|pre|td|ul|ol)`).
const BR_BEFORE: [&str; 11] = [
    "p", "li", "div", "dl", "dd", "dt", "th", "pre", "td", "ul", "ol",
];

/// For each position of a text, where the next match of a byte, a set of bytes or a sequence
/// starts (at or after it). Built once per pass in O(n), so a pass that looks for the end of
/// something from many starting points stays linear, where a search from each start would be
/// quadratic on text such as many `<div` without a `>`.
struct Next(Vec<u32>);

impl Next {
    /// The next byte for which `is` holds.
    fn matching(b: &[u8], is: impl Fn(u8) -> bool) -> Self {
        let mut table = vec![u32::MAX; b.len() + 1];
        for i in (0..b.len()).rev() {
            table[i] = if is(b[i]) { i as u32 } else { table[i + 1] };
        }
        Self(table)
    }

    /// The next `c`.
    fn byte(b: &[u8], c: u8) -> Self {
        Self::matching(b, |x| x == c)
    }

    /// The next start of `needle`.
    fn seq(b: &[u8], needle: &[u8]) -> Self {
        let mut table = vec![u32::MAX; b.len() + 1];
        for i in (0..b.len()).rev() {
            table[i] = if b[i..].starts_with(needle) {
                i as u32
            } else {
                table[i + 1]
            };
        }
        Self(table)
    }

    /// The next match at or after `i`.
    fn from(&self, i: usize) -> Option<usize> {
        match self.0.get(i).copied() {
            Some(u32::MAX) | None => None,
            Some(at) => Some(at as usize),
        }
    }
}

/// Where the next `>` is from `i`: the table when there is one for this text, else a search.
fn next_gt(b: &[u8], i: usize, gt: Option<&Next>) -> Option<usize> {
    match gt {
        Some(gt) => gt.from(i),
        None => b[i..].iter().position(|&c| c == b'>').map(|k| i + k),
    }
}

/// PCRE's `\s` without the `u` flag: space, tab, line feed, vertical tab, form feed and
/// carriage return.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The end of the run of `\s` starting at `i`.
fn skip_spaces(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    i
}

/// The start of the run of `\s` ending at `end`.
fn back_spaces(b: &[u8], mut end: usize) -> usize {
    while end > 0 && is_space(b[end - 1]) {
        end -= 1;
    }
    end
}

/// Splits `input` into text and HTML elements, as WordPress's `wp_html_split`: the text before
/// the first element, then each element followed by the text after it, so the elements are at
/// the odd indexes. An element is a comment (`<!--` to `-->`), a CDATA section (`<![CDATA[` to
/// `]]>`), or `<` up to the next `>`; one without its end runs to the end of the input.
pub fn html_split(input: &str) -> Vec<&str> {
    let b = input.as_bytes();
    let mut parts = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while let Some(off) = input[i..].find('<') {
        let start = i + off;
        let rest = &input[start + 1..];
        let end = if rest.starts_with("!--") {
            // The comment ends at the first `-->` after `<!`, which may share the opening's
            // dashes (`<!-->` is a whole comment).
            match input[start + 2..].find("-->") {
                Some(k) => start + 2 + k + 3,
                None => b.len(),
            }
        } else if rest.starts_with("![CDATA[") {
            match input[start + 9..].find("]]>") {
                Some(k) => start + 9 + k + 3,
                None => b.len(),
            }
        } else {
            match input[start + 1..].find('>') {
                Some(k) => start + 1 + k + 1,
                None => b.len(),
            }
        };
        parts.push(&input[text_start..start]);
        parts.push(&input[start..end]);
        text_start = end;
        i = end;
        if i >= b.len() {
            break;
        }
    }
    parts.push(&input[text_start..]);
    parts
}

/// `haystack` with `needle` replaced by `replace` inside its HTML elements only, as
/// WordPress's `wp_replace_in_html_tags` with one pair.
pub fn replace_in_html_tags(haystack: &str, needle: &str, replace: &str) -> String {
    let parts = html_split(haystack);
    let mut out = String::with_capacity(haystack.len());
    for (index, part) in parts.iter().enumerate() {
        if index % 2 == 1 {
            out.push_str(&part.replace(needle, replace));
        } else {
            out.push_str(part);
        }
    }
    out
}

/// The length of the tag name at `i` when it starts with one of `names` (a prefix match, as a
/// regular expression alternation followed by `[^>]*` matches). `h1` to `h6` are in the names.
fn starts_with_any(b: &[u8], i: usize, names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| !name.is_empty() && b[i..].starts_with(name.as_bytes()))
}

/// Matches `</?(?:names)[^>]*>` at `i`: returns the end, after the `>`.
fn block_tag_at(b: &[u8], i: usize, names: &[&str], gt: Option<&Next>) -> Option<usize> {
    if b.get(i) != Some(&b'<') {
        return None;
    }
    let mut j = i + 1;
    if b.get(j) == Some(&b'/') {
        j += 1;
    }
    if j >= b.len() || !starts_with_any(b, j, names) {
        return None;
    }
    Some(next_gt(b, j, gt)? + 1)
}

/// Replaces every non-overlapping match found by `find` (left to right, from a position) with
/// what `replace` returns for it. `find(b, from)` returns the next match `(start, end)` at or
/// after `from`.
fn replace_matches(
    text: &str,
    mut find: impl FnMut(&[u8], usize) -> Option<(usize, usize)>,
    mut replace: impl FnMut(&str) -> String,
) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while at <= b.len() {
        let Some((start, end)) = find(b, at) else {
            break;
        };
        out.push_str(&text[copied..start]);
        out.push_str(&replace(&text[start..end]));
        copied = end;
        // An empty match moves on by one, as PCRE does.
        at = if end == start { end + 1 } else { end };
    }
    out.push_str(&text[copied..]);
    out
}

/// The next match at or after `from` of a pattern that can only start at a byte `first`, tried
/// with `at` at each such byte: `at(b, i)` returns the match's end.
fn next_at(
    b: &[u8],
    from: usize,
    first: u8,
    mut at: impl FnMut(&[u8], usize) -> Option<usize>,
) -> Option<(usize, usize)> {
    let mut i = from;
    while i < b.len() {
        let off = b[i..].iter().position(|&c| c == first)?;
        let start = i + off;
        if let Some(end) = at(b, start) {
            return Some((start, end));
        }
        i = start + 1;
    }
    None
}

/// The next match at or after `from` of `\s*X`, where `X` starts with `first` and `at` matches
/// it: the match takes the whitespace before `X` (the leftmost start, as PCRE finds it).
fn next_with_spaces_before(
    b: &[u8],
    from: usize,
    first: u8,
    at: impl Fn(&[u8], usize) -> Option<usize>,
) -> Option<(usize, usize)> {
    let (start, end) = next_at(b, from, first, at)?;
    Some((back_spaces(b, start).max(from), end))
}

/// Matches `<br\s*/?>` at `i`; returns its end.
fn br_at(b: &[u8], i: usize) -> Option<usize> {
    if !b[i..].starts_with(b"<br") {
        return None;
    }
    let mut j = skip_spaces(b, i + 3);
    if b.get(j) == Some(&b'/') {
        j += 1;
    }
    (b.get(j) == Some(&b'>')).then_some(j + 1)
}

/// Matches `<name[^>]*>` at `i` for a literal `prefix` such as `<object`; returns its end.
fn open_tag_at(b: &[u8], i: usize, prefix: &[u8], gt: Option<&Next>) -> Option<usize> {
    if !b[i..].starts_with(prefix) {
        return None;
    }
    Some(next_gt(b, i + prefix.len(), gt)? + 1)
}

/// Turns double line breaks into paragraphs and single ones into `<br />`, as WordPress's
/// `wpautop( $text, true )`.
pub fn wpautop(text: &str) -> String {
    // PHP's `trim`: space, tab, line feed, carriage return, NUL and vertical tab.
    if text
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
        .is_empty()
    {
        return String::new();
    }
    // Just to make things a little easier, pad the end.
    let mut text = format!("{text}\n");

    // Pre tags are not touched: each is replaced with a placeholder, put back at the end.
    let mut pre_tags: Vec<(String, String)> = Vec::new();
    if text.contains("<pre") {
        let mut parts: Vec<&str> = text.split("</pre>").collect();
        let last = parts.pop().unwrap_or_default();
        let mut rebuilt = String::with_capacity(text.len());
        for part in parts {
            match part.find("<pre") {
                // Malformed HTML: the part is kept, and its `</pre>` is dropped, as WordPress
                // drops it.
                None => rebuilt.push_str(part),
                Some(start) => {
                    let name = format!("<pre wp-pre-tag-{}></pre>", pre_tags.len());
                    pre_tags.push((name.clone(), format!("{}</pre>", &part[start..])));
                    rebuilt.push_str(&part[..start]);
                    rebuilt.push_str(&name);
                }
            }
        }
        rebuilt.push_str(last);
        text = rebuilt;
    }

    // `|<br\s*/?>\s*<br\s*/?>|` => "\n\n".
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                let first = br_at(b, i)?;
                br_at(b, skip_spaces(b, first))
            })
        },
        |_| "\n\n".to_owned(),
    );

    // `!(<(?:blocks)[\s/>])!` => "\n\n$1": a double line break above an opening block tag.
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                all_blocks().iter().find_map(|name| {
                    let j = i + 1;
                    if !b[j..].starts_with(name.as_bytes()) {
                        return None;
                    }
                    let k = j + name.len();
                    b.get(k)
                        .filter(|&&c| is_space(c) || c == b'/' || c == b'>')
                        .map(|_| k + 1)
                })
            })
        },
        |m| format!("\n\n{m}"),
    );

    // `!(</(?:blocks)>)!` => "$1\n\n": a double line break below a closing block tag.
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if b.get(i + 1) != Some(&b'/') {
                    return None;
                }
                all_blocks().iter().find_map(|name| {
                    let j = i + 2;
                    let k = j + name.len();
                    (b[j..].starts_with(name.as_bytes()) && b.get(k) == Some(&b'>'))
                        .then_some(k + 1)
                })
            })
        },
        |m| format!("{m}\n\n"),
    );

    // `!(<hr\s*?/?>)!` => "$1\n\n".
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..].starts_with(b"<hr") {
                    return None;
                }
                let mut j = skip_spaces(b, i + 3);
                if b.get(j) == Some(&b'/') {
                    j += 1;
                }
                (b.get(j) == Some(&b'>')).then_some(j + 1)
            })
        },
        |m| format!("{m}\n\n"),
    );

    // Standardize newline characters to "\n".
    text = text.replace("\r\n", "\n").replace('\r', "\n");

    // Newlines inside elements get placeholders.
    text = replace_in_html_tags(&text, "\n", " <!-- wpnl --> ");

    // Line breaks before and after <option> elements are collapsed.
    if text.contains("<option") {
        // `|\s*<option|` => "<option".
        text = replace_matches(
            &text,
            |b, from| {
                next_with_spaces_before(b, from, b'<', |b, i| {
                    b[i..].starts_with(b"<option").then_some(i + 7)
                })
            },
            |_| "<option".to_owned(),
        );
        // `|</option>\s*|` => "</option>".
        text = replace_matches(
            &text,
            |b, from| {
                next_at(b, from, b'<', |b, i| {
                    b[i..]
                        .starts_with(b"</option>")
                        .then(|| skip_spaces(b, i + 9))
                })
            },
            |_| "</option>".to_owned(),
        );
    }

    // Line breaks inside <object> elements, before <param> and <embed>, are collapsed.
    if text.contains("</object>") {
        // `|(<object[^>]*>)\s*|` => "$1".
        let gt = Next::byte(text.as_bytes(), b'>');
        text = replace_keeping(&text, b'<', |b, i| {
            let end = open_tag_at(b, i, b"<object", Some(&gt))?;
            Some((i, end, skip_spaces(b, end)))
        });
        // `|\s*</object>|` => "</object>".
        text = replace_matches(
            &text,
            |b, from| {
                next_with_spaces_before(b, from, b'<', |b, i| {
                    b[i..].starts_with(b"</object>").then_some(i + 9)
                })
            },
            |_| "</object>".to_owned(),
        );
        // `%\s*(</?(?:param|embed)[^>]*>)\s*%` => "$1".
        let gt = Next::byte(text.as_bytes(), b'>');
        text = replace_spaced_tag(&text, |b, i| {
            block_tag_at(b, i, &["param", "embed"], Some(&gt))
        });
    }

    // Line breaks inside <audio> and <video>, before and after <source> and <track>, are
    // collapsed.
    if text.contains("<source") || text.contains("<track") {
        // `%([<\[](?:audio|video)[^>\]]*[>\]])\s*%` => "$1".
        let close = Next::matching(text.as_bytes(), |c| c == b'>' || c == b']');
        text = replace_keeping_any(&text, |b, i| {
            if !matches!(b[i], b'<' | b'[') {
                return None;
            }
            let j = i + 1;
            if !(b[j..].starts_with(b"audio") || b[j..].starts_with(b"video")) {
                return None;
            }
            let end = close.from(j + 5)? + 1;
            Some((i, end, skip_spaces(b, end)))
        });
        // `%\s*([<\[]/(?:audio|video)[>\]])%` => "$1".
        text = replace_spaced_before_any(&text, |b, i| {
            if !matches!(b[i], b'<' | b'[') || b.get(i + 1) != Some(&b'/') {
                return None;
            }
            let j = i + 2;
            if !(b[j..].starts_with(b"audio") || b[j..].starts_with(b"video")) {
                return None;
            }
            matches!(b.get(j + 5), Some(b'>' | b']')).then_some(j + 6)
        });
        // `%\s*(<(?:source|track)[^>]*>)\s*%` => "$1".
        let gt = Next::byte(text.as_bytes(), b'>');
        text = replace_spaced_tag(&text, |b, i| {
            open_tag_at(b, i, b"<source", Some(&gt))
                .or_else(|| open_tag_at(b, i, b"<track", Some(&gt)))
        });
    }

    // Line breaks before and after <figcaption> elements are collapsed.
    if text.contains("<figcaption") {
        // `|\s*(<figcaption[^>]*>)|` => "$1".
        let gt = Next::byte(text.as_bytes(), b'>');
        text =
            replace_spaced_before_any(&text, |b, i| open_tag_at(b, i, b"<figcaption", Some(&gt)));
        // `|</figcaption>\s*|` => "</figcaption>".
        text = replace_matches(
            &text,
            |b, from| {
                next_at(b, from, b'<', |b, i| {
                    b[i..]
                        .starts_with(b"</figcaption>")
                        .then(|| skip_spaces(b, i + 13))
                })
            },
            |_| "</figcaption>".to_owned(),
        );
    }

    // `/\n\n+/` => "\n\n".
    text = collapse_newlines(&text);

    // Split into paragraphs at double line breaks (`/\n\s*\n/`, no empty pieces), each wrapped
    // in a <p>.
    let mut rebuilt = String::with_capacity(text.len() + 64);
    for paragraph in split_paragraphs(&text) {
        rebuilt.push_str("<p>");
        rebuilt.push_str(paragraph.trim_matches('\n'));
        rebuilt.push_str("</p>\n");
    }
    text = rebuilt;

    // `|<p>\s*</p>|` => "": a paragraph of only whitespace.
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..].starts_with(b"<p>") {
                    return None;
                }
                let j = skip_spaces(b, i + 3);
                b[j..].starts_with(b"</p>").then_some(j + 4)
            })
        },
        |_| String::new(),
    );

    // `!<p>([^<]+)</(div|address|form)>!` => "<p>$1</p></$2>".
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..].starts_with(b"<p>") {
                    return None;
                }
                let j = i + 3;
                let k = j + b[j..].iter().position(|&c| c == b'<')?;
                if k == j {
                    return None;
                }
                ["</div>", "</address>", "</form>"]
                    .iter()
                    .find(|close| b[k..].starts_with(close.as_bytes()))
                    .map(|close| k + close.len())
            })
        },
        |m| {
            let close = m.rfind("</").unwrap_or(m.len());
            format!("{}</p>{}", &m[..close], &m[close..])
        },
    );

    // `!<p>\s*(</?(?:blocks)[^>]*>)\s*</p>!` => "$1": a block tag alone in a paragraph.
    let gt = Next::byte(text.as_bytes(), b'>');
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..].starts_with(b"<p>") {
                    return None;
                }
                let tag = skip_spaces(b, i + 3);
                let tag_end = block_tag_at(b, tag, all_blocks(), Some(&gt))?;
                let j = skip_spaces(b, tag_end);
                b[j..].starts_with(b"</p>").then_some(j + 4)
            })
        },
        |m| {
            let b = m.as_bytes();
            let tag = skip_spaces(b, 3);
            let end = block_tag_at(b, tag, all_blocks(), None).unwrap_or(m.len());
            m[tag..end].to_owned()
        },
    );

    // `|<p>(<li.+?)</p>|` => "$1": a <li> wrapped in a <p> (`.` stops at a line break).
    let newline = Next::byte(text.as_bytes(), b'\n');
    let close_p = Next::seq(text.as_bytes(), b"</p>");
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..].starts_with(b"<p><li") {
                    return None;
                }
                let j = i + 6;
                // At least one character after `<li`, then the first `</p>` on the line.
                if j >= b.len() || b[j] == b'\n' {
                    return None;
                }
                let close = close_p.from(j + 1)?;
                match newline.from(j + 1) {
                    Some(line_end) if line_end < close => None,
                    _ => Some(close + 4),
                }
            })
        },
        |m| m[3..m.len() - 4].to_owned(),
    );

    // `!<p><blockquote((?:[^>"']|"[^"]*"|'[^']*')*)>!i` => "<blockquote$1><p>".
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..]
                    .get(..3)
                    .is_some_and(|s| s.eq_ignore_ascii_case(b"<p>"))
                    || !b[i + 3..]
                        .get(..11)
                        .is_some_and(|s| s.eq_ignore_ascii_case(b"<blockquote"))
                {
                    return None;
                }
                let mut j = i + 14;
                loop {
                    match b.get(j)? {
                        b'>' => return Some(j + 1),
                        q @ (b'"' | b'\'') => {
                            let close = b[j + 1..].iter().position(|c| c == q)?;
                            j += close + 2;
                        }
                        _ => j += 1,
                    }
                }
            })
        },
        |m| {
            let inner = &m[3..m.len() - 1];
            format!("<blockquote{}><p>", &inner[11..])
        },
    );
    text = text.replace("</blockquote></p>", "</p></blockquote>");

    // `!<p>\s*(</?(?:blocks)[^>]*>)!` => "$1": an opening <p> before a block tag.
    let gt = Next::byte(text.as_bytes(), b'>');
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..].starts_with(b"<p>") {
                    return None;
                }
                block_tag_at(b, skip_spaces(b, i + 3), all_blocks(), Some(&gt))
            })
        },
        |m| m[skip_spaces(m.as_bytes(), 3)..].to_owned(),
    );

    // `!(</?(?:blocks)[^>]*>)\s*</p>!` => "$1": a closing </p> after a block tag.
    let gt = Next::byte(text.as_bytes(), b'>');
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                let end = block_tag_at(b, i, all_blocks(), Some(&gt))?;
                let j = skip_spaces(b, end);
                b[j..].starts_with(b"</p>").then_some(j + 4)
            })
        },
        |m| {
            let end = block_tag_at(m.as_bytes(), 0, all_blocks(), None).unwrap_or(m.len());
            m[..end].to_owned()
        },
    );

    // Line breaks become <br />.
    // Newlines in <script>, <style>, <svg> and <math> are kept
    // (`/<(script|style|svg|math).*?<\/\1>/s`).
    text = preserve_newlines(&text);
    // Normalize <br>.
    text = text.replace("<br>", "<br />").replace("<br/>", "<br />");
    // `|(?<!<br />)\s*\n|` => "<br />\n".
    text = newlines_to_br(&text);
    text = text.replace("<WPPreserveNewline />", "\n");

    // `!(</?(?:blocks)[^>]*>)\s*<br />!` => "$1": a <br /> after a block tag.
    let gt = Next::byte(text.as_bytes(), b'>');
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                let end = block_tag_at(b, i, all_blocks(), Some(&gt))?;
                let j = skip_spaces(b, end);
                b[j..].starts_with(b"<br />").then_some(j + 6)
            })
        },
        |m| {
            let end = block_tag_at(m.as_bytes(), 0, all_blocks(), None).unwrap_or(m.len());
            m[..end].to_owned()
        },
    );

    // `!<br />(\s*</?(?:p|li|div|dl|dd|dt|th|pre|td|ul|ol)[^>]*>)!` => "$1".
    let gt = Next::byte(text.as_bytes(), b'>');
    text = replace_matches(
        &text,
        |b, from| {
            next_at(b, from, b'<', |b, i| {
                if !b[i..].starts_with(b"<br />") {
                    return None;
                }
                block_tag_at(b, skip_spaces(b, i + 6), &BR_BEFORE, Some(&gt))
            })
        },
        |m| m[6..].to_owned(),
    );

    // `|\n</p>$|` => "</p>" (`$` is the end, or before a final line break).
    if let Some(stripped) = text.strip_suffix("\n</p>\n") {
        text = format!("{stripped}</p>\n");
    } else if let Some(stripped) = text.strip_suffix("\n</p>") {
        text = format!("{stripped}</p>");
    }

    // The <pre> placeholders get their content back.
    for (name, pre) in &pre_tags {
        text = text.replace(name, pre);
    }

    // The newlines in elements come back.
    if text.contains("<!-- wpnl -->") {
        text = text
            .replace(" <!-- wpnl --> ", "\n")
            .replace("<!-- wpnl -->", "\n");
    }
    text
}

/// Replaces each match of `(X)\s*` with `X`, where `at(b, i)` matches at a byte `first`:
/// `(start, end of X, end of the match)`.
fn replace_keeping(
    text: &str,
    first: u8,
    at: impl Fn(&[u8], usize) -> Option<(usize, usize, usize)>,
) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        let Some(off) = b[i..].iter().position(|&c| c == first) else {
            break;
        };
        let start = i + off;
        match at(b, start) {
            Some((s, keep_end, end)) => {
                out.push_str(&text[copied..s]);
                out.push_str(&text[s..keep_end]);
                copied = end;
                i = end.max(start + 1);
            }
            None => i = start + 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// [`replace_keeping`] for a pattern that may start at any byte.
fn replace_keeping_any(
    text: &str,
    at: impl Fn(&[u8], usize) -> Option<(usize, usize, usize)>,
) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        match at(b, i) {
            Some((s, keep_end, end)) => {
                out.push_str(&text[copied..s]);
                out.push_str(&text[s..keep_end]);
                copied = end;
                i = end.max(i + 1);
            }
            None => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Replaces each match of `\s*(X)` with `X`, where `at(b, i)` returns the end of `X` at `i`.
/// The whitespace taken is the run just before `X`, from the end of the previous match on.
fn replace_spaced_before_any(text: &str, at: impl Fn(&[u8], usize) -> Option<usize>) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        match at(b, i) {
            Some(end) => {
                let start = back_spaces(b, i).max(copied);
                out.push_str(&text[copied..start]);
                out.push_str(&text[i..end]);
                copied = end;
                i = end.max(i + 1);
            }
            None => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Replaces each match of `\s*(X)\s*` with `X`, where `at(b, i)` returns the end of `X` at `i`.
fn replace_spaced_tag(text: &str, at: impl Fn(&[u8], usize) -> Option<usize>) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        match at(b, i) {
            Some(end) => {
                let start = back_spaces(b, i).max(copied);
                out.push_str(&text[copied..start]);
                out.push_str(&text[i..end]);
                copied = skip_spaces(b, end);
                i = copied.max(i + 1);
            }
            None => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `/\n\n+/` => "\n\n".
fn collapse_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0;
    for c in text.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push(c);
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

/// `preg_split( '/\n\s*\n/', $text, -1, PREG_SPLIT_NO_EMPTY )`: the pieces between the matches,
/// without the empty ones. A match is a line feed, then the longest run of `\s` that still
/// ends with a line feed.
fn split_paragraphs(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\n' {
            let run_end = skip_spaces(b, i + 1);
            if let Some(last) = b[i + 1..run_end].iter().rposition(|&c| c == b'\n') {
                let end = i + 1 + last + 1;
                if start < i {
                    pieces.push(&text[start..i]);
                }
                start = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if start < b.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// `/<(script|style|svg|math).*?<\/\1>/s`: the newlines in each such element become
/// `<WPPreserveNewline />`.
fn preserve_newlines(text: &str) -> String {
    let b = text.as_bytes();
    // Where each element's next closing tag is, built when the text has one.
    let closers: Vec<(&str, Option<Next>)> = ["script", "style", "svg", "math"]
        .into_iter()
        .map(|name| {
            let close = format!("</{name}>");
            let table = text
                .contains(close.as_str())
                .then(|| Next::seq(b, close.as_bytes()));
            (name, table)
        })
        .collect();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(off) = text[i..].find('<') {
        let start = i + off;
        let found = closers
            .iter()
            .find(|(name, _)| b[start + 1..].starts_with(name.as_bytes()));
        let Some((name, table)) = found else {
            i = start + 1;
            continue;
        };
        let Some(k) = table.as_ref().and_then(|t| t.from(start + 1 + name.len())) else {
            i = start + 1;
            continue;
        };
        let end = k + name.len() + 3;
        out.push_str(&text[copied..start]);
        out.push_str(&text[start..end].replace('\n', "<WPPreserveNewline />"));
        copied = end;
        i = end;
    }
    out.push_str(&text[copied..]);
    out
}

/// `|(?<!<br />)\s*\n|` => "<br />\n": each line break, with the whitespace before it, becomes
/// `<br />` and a line break, unless it already follows a `<br />`. As PCRE scans it: from each
/// position not just after `<br />`, the longest run of `\s` that ends with a line feed.
fn newlines_to_br(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 64);
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        if !is_space(b[i]) || b[..i].ends_with(b"<br />") {
            i += 1;
            continue;
        }
        let run_end = skip_spaces(b, i);
        match b[i..run_end].iter().rposition(|&c| c == b'\n') {
            Some(last) => {
                out.push_str(&text[copied..i]);
                out.push_str("<br />\n");
                copied = i + last + 1;
                i = copied;
            }
            None => i = run_end,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// WordPress's `wp_spaces_regexp` default: `[\r\n\t ]|\xC2\xA0|&nbsp;`. Returns the length of
/// the space at `i`, or 0.
fn wp_space_at(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        Some(b'\r' | b'\n' | b'\t' | b' ') => 1,
        Some(0xC2) if b.get(i + 1) == Some(&0xA0) => 2,
        Some(b'&') if b[i..].starts_with(b"&nbsp;") => 6,
        _ => 0,
    }
}

/// Skips WordPress's spaces from `i`.
fn skip_wp_spaces(b: &[u8], mut i: usize) -> usize {
    loop {
        let n = wp_space_at(b, i);
        if n == 0 {
            return i;
        }
        i += n;
    }
}

/// Keeps a shortcode that stands alone in a paragraph out of it, as WordPress's
/// `shortcode_unautop`: `<p>[tag ...]</p>` (or with its content and `[/tag]`, and spaces around
/// it) becomes the shortcode alone. `tags` are the registered shortcode tags, in registration
/// order; with none, the text is returned unchanged.
pub fn shortcode_unautop(text: &str, tags: &[String]) -> String {
    if tags.is_empty() {
        return text.to_owned();
    }
    // The searches from each `<p>[tag` use tables of this text, so the pass stays linear.
    let tables = RefCell::new(UnautopTables {
        bracket: Next::byte(text.as_bytes(), b']'),
        closers: HashMap::new(),
    });
    replace_matches(
        text,
        |b, from| {
            let mut tables = tables.borrow_mut();
            next_at(b, from, b'<', |b, i| {
                unautop_at(b, i, tags, Some(&mut tables)).map(|m| m.1)
            })
        },
        |m| {
            let (start, _, end) =
                unautop_at(m.as_bytes(), 0, tags, None).expect("the match matches again");
            m[start..end].to_owned()
        },
    )
}

/// The tables of [`shortcode_unautop`]'s text: the next `]`, and each tag's next `[/tag]`, built
/// the first time the tag is met.
struct UnautopTables {
    bracket: Next,
    closers: HashMap<String, Next>,
}

/// Matches `shortcode_unautop`'s pattern at `i`: the shortcode's start, the match's end, and
/// the shortcode's end.
fn unautop_at(
    b: &[u8],
    i: usize,
    tags: &[String],
    mut tables: Option<&mut UnautopTables>,
) -> Option<(usize, usize, usize)> {
    if !b[i..].starts_with(b"<p>") {
        return None;
    }
    let open = skip_wp_spaces(b, i + 3);
    if b.get(open) != Some(&b'[') {
        return None;
    }
    let name_at = open + 1;
    for tag in tags {
        let t = tag.as_bytes();
        if !b[name_at..].starts_with(t) {
            continue;
        }
        let after = name_at + t.len();
        // `(?![\w-])`.
        if b.get(after)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
        {
            continue;
        }
        // Inside the opening tag: up to the first `]`, a `/` just before it closing the tag.
        let next_bracket = match tables.as_deref() {
            Some(tables) => tables.bracket.from(after),
            None => b[after..]
                .iter()
                .position(|&c| c == b']')
                .map(|k| after + k),
        };
        let Some(bracket) = next_bracket else {
            continue;
        };
        let self_closing = bracket > after && b[bracket - 1] == b'/';
        let tag_end = bracket + 1;
        let closes = |end: usize| -> Option<usize> {
            let j = skip_wp_spaces(b, end);
            b[j..].starts_with(b"</p>").then_some(j + 4)
        };
        if !self_closing {
            // The enclosed content up to `[/tag]`, when the paragraph closes after it.
            let closer = format!("[/{tag}]");
            let found = match tables.as_deref_mut() {
                Some(tables) => tables
                    .closers
                    .entry(tag.clone())
                    .or_insert_with(|| Next::seq(b, closer.as_bytes()))
                    .from(tag_end),
                None => find_bytes(&b[tag_end..], closer.as_bytes()).map(|k| tag_end + k),
            };
            if let Some(k) = found {
                let end = k + closer.len();
                if let Some(m) = closes(end) {
                    return Some((open, m, end));
                }
            }
        }
        if let Some(m) = closes(tag_end) {
            return Some((open, m, tag_end));
        }
    }
    None
}

/// The first index of `needle` in `hay`.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
