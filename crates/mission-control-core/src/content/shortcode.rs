//! Shortcodes: WordPress's `[tag attr="value"]content[/tag]` syntax inside post content.
//!
//! [`Shortcodes`] is a registry, like [`Hooks`]: methods take `&self`, the registry changes while
//! the site serves, and callbacks never run under the lock. The grammar is WordPress's
//! `get_shortcode_regex` as `do_shortcode` applies it, scanned by hand so a malformed `[` never
//! panics and hostile input stays close to linear.
//!
//! - `[tag]` and `[tag /]` are self-closing; `[tag]...[/tag]` encloses content (up to the first
//!   `[/tag]`; with no closing tag the content is `None`).
//! - `[[tag]]` is an escape: it prints `[tag]` literally, enclosed content and closing tag
//!   included. A half escape (`[[tag]` or `[tag]]`) keeps the stray bracket around the output.
//! - A tag that is not registered is left as text, untouched.
//! - A tag name ends at the first character that is not a word character or `-`, so `[gallery2]`
//!   is not `[gallery]`.
//! - Nested shortcodes in enclosed content are **not** processed automatically: as in
//!   WordPress, the callback decides, usually by calling [`Shortcodes::do_shortcodes`] on its
//!   content.
//! - A callback's `String` is inserted as is. Nothing is escaped, as in WordPress, so a callback
//!   must escape any attribute value it prints.
//!
//! ```
//! use mission_control_core::content::Shortcodes;
//! use mission_control_core::hooks::Owner;
//!
//! let shortcodes = Shortcodes::new();
//! shortcodes
//!     .add_shortcode("shout", Owner::Core, |atts, content| {
//!         let end = atts.get("end").unwrap_or("!");
//!         format!("{}{end}", content.unwrap_or("").to_uppercase())
//!     })
//!     .unwrap();
//! assert_eq!(shortcodes.do_shortcodes("[shout end='?']hi[/shout]"), "HI?");
//! assert_eq!(shortcodes.do_shortcodes("[[shout]]"), "[shout]");
//! ```
//!
//! [`Hooks`]: crate::hooks::Hooks

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::autop::{shortcode_unautop, wpautop};
use crate::hooks::{Hooks, Owner, THE_CONTENT};

/// The priority WordPress gives `do_shortcode` on `the_content`.
const THE_CONTENT_PRIORITY: i32 = 11;

/// The priority WordPress gives `wpautop` and `shortcode_unautop` on `the_content`.
const AUTOP_PRIORITY: i32 = 10;

/// The attributes of one shortcode call, after WordPress's `shortcode_parse_atts`: named
/// attributes (`name="v"`, `name='v'` or `name=v`, names lowercased) and positional values
/// (unnamed words or quoted strings), each kept in source order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    named: Vec<(String, String)>,
    positional: Vec<String>,
}

impl Attributes {
    /// Parses the text between the tag name and the closing `]` of a shortcode.
    pub fn parse(text: &str) -> Self {
        let text = text.replace(['\u{a0}', '\u{200b}'], " ");
        let b = text.as_bytes();
        let mut atts = Self::default();
        let mut i = 0;
        while i < b.len() {
            if is_space(b[i]) {
                i += 1;
                continue;
            }
            let (next, name, value) = parse_att(&text, i);
            match name {
                Some(name) => atts.set(name, value),
                // WordPress drops an empty quoted positional value (`""`, `''`).
                None if value.is_empty() => {}
                None => atts.positional.push(value),
            }
            i = next;
        }
        atts
    }

    /// Sets a named attribute; a later duplicate replaces the earlier, as in a PHP array.
    fn set(&mut self, name: String, value: String) {
        match self.named.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => self.named.push((name, value)),
        }
    }

    /// The named attribute `name` (give it lowercase).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.named
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The positional (unnamed) value at `index`.
    pub fn positional(&self, index: usize) -> Option<&str> {
        self.positional.get(index).map(String::as_str)
    }

    /// Every positional value, in source order.
    pub fn positionals(&self) -> &[String] {
        &self.positional
    }

    /// Every named attribute as `(name, value)`, in source order.
    pub fn named(&self) -> impl Iterator<Item = (&str, &str)> {
        self.named.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Whether there are no attributes at all.
    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.positional.is_empty()
    }
}

/// PCRE's `\s`.
fn is_space(b: u8) -> bool {
    b.is_ascii_whitespace() || b == 0x0b
}

/// Matches `shortcode_parse_atts`'s alternation at `i` (a non-space byte). Returns the offset
/// after the match, the name (lowercased) if named, and the value.
fn parse_att(text: &str, i: usize) -> (usize, Option<String>, String) {
    let b = text.as_bytes();
    // `(?:\s|$)` after a value: the match also swallows one whitespace byte.
    let after = |end: usize| -> Option<usize> {
        match b.get(end) {
            None => Some(end),
            Some(&c) if is_space(c) => Some(end + 1),
            _ => None,
        }
    };
    // A quoted string starting at `at`, with the whitespace-or-end rule: (value, next offset).
    let quoted = |at: usize| -> Option<(&str, usize)> {
        let q = *b.get(at)?;
        if q != b'"' && q != b'\'' {
            return None;
        }
        let close = text[at + 1..].find(q as char)? + at + 1;
        Some((&text[at + 1..close], after(close + 1)?))
    };

    // `(\w+)\s*=\s*` then a quoted or bare value.
    let mut j = i;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-') {
        j += 1;
    }
    if j > i {
        let name_end = j;
        while j < b.len() && is_space(b[j]) {
            j += 1;
        }
        if b.get(j) == Some(&b'=') {
            j += 1;
            while j < b.len() && is_space(b[j]) {
                j += 1;
            }
            let name = text[i..name_end].to_ascii_lowercase();
            if let Some((value, next)) = quoted(j) {
                return (next, Some(name), value.to_string());
            }
            let mut k = j;
            while k < b.len() && !is_space(b[k]) && b[k] != b'"' && b[k] != b'\'' {
                k += 1;
            }
            if k > j
                && let Some(next) = after(k)
            {
                return (next, Some(name), text[j..k].to_string());
            }
        }
    }
    // A quoted positional value.
    if let Some((value, next)) = quoted(i) {
        return (next, None, value.to_string());
    }
    // Any other run of non-space bytes.
    let mut k = i;
    while k < b.len() && !is_space(b[k]) {
        k += 1;
    }
    (after(k).unwrap_or(k), None, text[i..k].to_string())
}

/// A shortcode callback: the call's attributes and enclosed content (`None` for a
/// self-closing call or one with no closing tag), returning the replacement text.
type Callback = Arc<dyn Fn(&Attributes, Option<&str>) -> String + Send + Sync>;

struct Entry {
    tag: String,
    owner: Owner,
    callback: Callback,
}

/// A shortcode tag WordPress would refuse: empty, or containing whitespace, a control
/// character, or any of `& / < > [ ] =`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidShortcodeTag(pub String);

impl fmt::Display for InvalidShortcodeTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid shortcode tag {:?}", self.0)
    }
}

impl std::error::Error for InvalidShortcodeTag {}

/// The shortcode registry. Methods take `&self`; the registry may change while the site serves,
/// and callbacks never run under the lock, so a callback may itself call
/// [`do_shortcodes`](Self::do_shortcodes) or register shortcodes.
#[derive(Default)]
pub struct Shortcodes {
    /// In registration order, as WordPress builds its tag alternation.
    inner: RwLock<Vec<Entry>>,
}

impl fmt::Debug for Shortcodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shortcodes").finish_non_exhaustive()
    }
}

impl Shortcodes {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> RwLockReadGuard<'_, Vec<Entry>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Vec<Entry>> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers `f` for `[tag]`, like WordPress's `add_shortcode`. Registering a tag again
    /// replaces its callback and owner. Returns an error for a tag WordPress refuses.
    pub fn add_shortcode(
        &self,
        tag: &str,
        owner: Owner,
        f: impl Fn(&Attributes, Option<&str>) -> String + Send + Sync + 'static,
    ) -> Result<(), InvalidShortcodeTag> {
        if tag.is_empty()
            || tag
                .chars()
                .any(|c| c <= ' ' || matches!(c, '&' | '/' | '<' | '>' | '[' | ']' | '='))
        {
            return Err(InvalidShortcodeTag(tag.to_string()));
        }
        let entry = Entry {
            tag: tag.to_string(),
            owner,
            callback: Arc::new(f),
        };
        // The replaced entry is dropped after the lock is released.
        let _replaced = {
            let mut list = self.write();
            match list.iter_mut().find(|e| e.tag == tag) {
                Some(slot) => Some(std::mem::replace(slot, entry)),
                None => {
                    list.push(entry);
                    None
                }
            }
        };
        Ok(())
    }

    /// Removes `tag`. Returns whether it was registered.
    pub fn remove_shortcode(&self, tag: &str) -> bool {
        let removed = {
            let mut list = self.write();
            let (gone, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut *list)
                .into_iter()
                .partition(|e| e.tag == tag);
            *list = kept;
            gone
        };
        // Dropped here, after the guard.
        !removed.is_empty()
    }

    /// Whether `tag` is registered.
    pub fn shortcode_exists(&self, tag: &str) -> bool {
        self.read().iter().any(|e| e.tag == tag)
    }

    /// Removes every shortcode registered by `owner` (a plugin or theme deactivating).
    pub fn remove_owner(&self, owner: &Owner) {
        let removed = {
            let mut list = self.write();
            let (gone, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut *list)
                .into_iter()
                .partition(|e| &e.owner == owner);
            *list = kept;
            gone
        };
        // Dropped here, after the guard.
        drop(removed);
    }

    /// Adds WordPress's default `the_content` filters on `hooks`, owned by [`Owner::Core`]:
    /// [`wpautop`] and then [`shortcode_unautop`] at priority 10, and
    /// [`do_shortcodes`](Self::do_shortcodes) at 11 (Task A5). As WordPress's `do_blocks` does,
    /// `wpautop` leaves content with block markup (`<!-- wp:`) alone.
    pub fn register_the_content(self: &Arc<Self>, hooks: &Hooks) {
        hooks.add_filter(THE_CONTENT, AUTOP_PRIORITY, Owner::Core, |content| {
            if content.contains("<!-- wp:") {
                content
            } else {
                wpautop(&content)
            }
        });
        let this = Arc::clone(self);
        hooks.add_filter(THE_CONTENT, AUTOP_PRIORITY, Owner::Core, move |content| {
            let tags: Vec<String> = this.read().iter().map(|e| e.tag.clone()).collect();
            shortcode_unautop(&content, &tags)
        });
        let this = Arc::clone(self);
        hooks.add_filter(THE_CONTENT, THE_CONTENT_PRIORITY, Owner::Core, move |c| {
            this.do_shortcodes(&c)
        });
    }

    /// Replaces every registered shortcode in `content` with its callback's output, like
    /// WordPress's `do_shortcode`. Unknown tags and malformed brackets stay as text.
    ///
    /// As WordPress does, only the registered tags written in `content` (`[tag` followed by a
    /// character that cannot be in a tag) are looked for, and shortcodes inside HTML tags are
    /// handled first ([`do_shortcodes_in_html_tags`]): one in a quoted attribute value runs, and
    /// its output is kept only when kses allows the attribute; every other bracket inside a tag
    /// is left as written, never read as a shortcode.
    pub fn do_shortcodes(&self, content: &str) -> String {
        if !content.contains('[') {
            return content.to_string();
        }
        let present = tag_names_in(content);
        let tags: Vec<(String, Callback)> = self
            .read()
            .iter()
            .filter(|e| present.contains(e.tag.as_str()))
            .map(|e| (e.tag.clone(), Arc::clone(&e.callback)))
            .collect();
        if tags.is_empty() {
            return content.to_string();
        }
        let content = do_shortcodes_in_html_tags(content, &tags);
        let (content, _) = replace_shortcodes(&content, &tags);
        unescape_invalid_shortcodes(&content)
    }
}

/// The names written after a `[` in `content`, as WordPress's
/// `preg_match_all( '@\[([^<>&/\[\]\x00-\x20=]++)@' )` finds them: each the whole run of
/// characters that can be in a tag.
fn tag_names_in(content: &str) -> HashSet<&str> {
    let b = content.as_bytes();
    let mut names = HashSet::new();
    let mut i = 0;
    while let Some(off) = content[i..].find('[') {
        let start = i + off + 1;
        let mut end = start;
        while end < b.len()
            && !matches!(
                b[end],
                b'<' | b'>' | b'&' | b'/' | b'[' | b']' | b'=' | 0..=0x20
            )
        {
            end += 1;
        }
        if end > start {
            names.insert(&content[start..end]);
        }
        i = start;
    }
    names
}

/// WordPress's `do_shortcode_tag` pass over `content` with `tags`: every shortcode replaced with
/// its callback's output. Returns the text and how many shortcodes were found.
fn replace_shortcodes(content: &str, tags: &[(String, Callback)]) -> (String, usize) {
    let mut out = String::with_capacity(content.len());
    let mut found = 0;
    // Tags with no `[/tag]` anywhere after a point: no later search can find one either.
    let mut no_closer: HashSet<&str> = HashSet::new();
    let mut copied = 0;
    let mut at = 0;
    while let Some(off) = content[at..].find('[') {
        let start = at + off;
        match scan(content, start, tags, &mut no_closer) {
            Scan::Found(m) => {
                found += 1;
                out.push_str(&content[copied..start]);
                let (tag_cb, whole_end) = (m.callback, m.end);
                if m.lead && m.trail {
                    // `[[tag]]`: the escape prints the match without its outer brackets.
                    out.push_str(&content[start + 1..whole_end - 1]);
                } else {
                    if m.lead {
                        out.push('[');
                    }
                    out.push_str(&tag_cb(&Attributes::parse(m.atts), m.content));
                    if m.trail {
                        out.push(']');
                    }
                }
                copied = whole_end;
                at = whole_end;
            }
            Scan::NoMatch => at = start + 1,
            // No `]` anywhere after this point, so no later `[` can match.
            Scan::NoTerminator => break,
        }
    }
    out.push_str(&content[copied..]);
    (out, found)
}

/// `[` and `]` written as the entities `do_shortcodes_in_html_tags` uses, so the main pass
/// does not read them.
fn encode_brackets(text: &str) -> String {
    text.replace('[', "&#91;").replace(']', "&#93;")
}

/// WordPress's `unescape_invalid_shortcodes`: the brackets [`encode_brackets`] wrote come back.
fn unescape_invalid_shortcodes(content: &str) -> String {
    content.replace("&#91;", "[").replace("&#93;", "]")
}

/// WordPress's `do_shortcodes_in_html_tags`: inside each HTML element of `content`, a shortcode
/// in a quoted attribute value runs, kept only when kses allows the attribute it makes
/// ([`kses_one_attr`]); one written unquoted runs as is (only an `unfiltered_html` user can save
/// it); every other `[` and `]` in an element is encoded, so the main pass leaves it.
fn do_shortcodes_in_html_tags(content: &str, tags: &[(String, Callback)]) -> String {
    // Entities already in the text are normalized first, so they are not decoded at the end.
    let content = content
        .replace("&#91;", "&#091;")
        .replace("&#93;", "&#093;");
    let parts = crate::content::autop::html_split(&content);
    let mut out = String::with_capacity(content.len());
    for part in parts {
        if !part.starts_with('<') {
            out.push_str(part);
            continue;
        }
        let noopen = !part.contains('[');
        let noclose = !part.contains(']');
        if noopen || noclose {
            // No shortcode here; a stray bracket is encoded.
            if noopen != noclose {
                out.push_str(&encode_brackets(part));
            } else {
                out.push_str(part);
            }
            continue;
        }
        if part.starts_with("<!--") || part.starts_with("<![CDATA[") {
            out.push_str(&encode_brackets(part));
            continue;
        }
        let Some(attributes) = kses_attr_parse(part) else {
            // Some plugins write things like `[name] <[email]>`.
            let element = if starts_with_bracketed_name(part) {
                replace_shortcodes(part, tags).0
            } else {
                part.to_owned()
            };
            out.push_str(&encode_brackets(&element));
            continue;
        };
        let (front, rest) = attributes.split_first().expect("a front");
        let (back, attrs) = rest.split_last().expect("a back");
        let elname: String = front
            .chars()
            .skip_while(|c| !c.is_ascii_alphanumeric())
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let mut element = front.clone();
        for attr in attrs {
            let (Some(open), Some(_)) = (attr.find('['), attr.find(']')) else {
                element.push_str(attr);
                continue;
            };
            let double = attr.find('"');
            let single = attr.find('\'');
            if single.is_none_or(|q| open < q) && double.is_none_or(|q| open < q) {
                // `[shortcode]` or `name = [shortcode]`: written by a user with
                // `unfiltered_html`, so the output is kept as is.
                element.push_str(&replace_shortcodes(attr, tags).0);
            } else {
                // `name = "[shortcode]"`: the output goes through kses.
                let (new_attr, count) = replace_shortcodes(attr, tags);
                let kept = if count > 0 {
                    let filtered = kses_one_attr(&new_attr, &elname);
                    if filtered.trim().is_empty() {
                        None
                    } else {
                        Some(filtered)
                    }
                } else {
                    None
                };
                element.push_str(kept.as_deref().unwrap_or(attr));
            }
        }
        element.push_str(back);
        out.push_str(&encode_brackets(&element));
    }
    out
}

/// `%^<\s*\[\[?[^\[\]]+\]%`: an element that starts with a bracketed name.
fn starts_with_bracketed_name(element: &str) -> bool {
    let b = element.as_bytes();
    let mut i = 1;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    if b.get(i) != Some(&b'[') {
        return false;
    }
    i += 1;
    if b.get(i) == Some(&b'[') {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i] != b'[' && b[i] != b']' {
        i += 1;
    }
    i > start && b.get(i) == Some(&b']')
}

/// WordPress's `wp_kses_attr_parse`: an opening element split into its start (`<name`), each
/// attribute with the whitespace after it, and its end (`>`, or `/>`); `None` for a closing
/// element or one whose attributes cannot be read.
fn kses_attr_parse(element: &str) -> Option<Vec<String>> {
    // `%^(<\s*)(/\s*)?([a-zA-Z0-9]+\s*)([^>]*)(>?)$%`.
    let b = element.as_bytes();
    if b.first() != Some(&b'<') {
        return None;
    }
    let mut i = 1;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    if b.get(i) == Some(&b'/') {
        // Closing elements are not parsed.
        return None;
    }
    let name_start = i;
    while i < b.len() && b[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let front = &element[..i];
    let attr_start = i;
    while i < b.len() && b[i] != b'>' {
        i += 1;
    }
    let mut attr = &element[attr_start..i];
    let end = &element[i..];
    // Only one `>`, at the very end (or before a final line break, as `$` allows).
    if !(end.is_empty() || end == ">" || end == ">\n") {
        return None;
    }
    let end = if end == ">\n" { ">" } else { end };
    // `%\s*/\s*$%`: an XHTML slash before the end.
    let mut xhtml_slash = "";
    let trimmed = attr.trim_end_matches(|c: char| c.is_ascii() && is_space(c as u8));
    if let Some(before) = trimmed.strip_suffix('/') {
        let slash_start = before
            .trim_end_matches(|c: char| c.is_ascii() && is_space(c as u8))
            .len();
        xhtml_slash = &attr[slash_start..];
        attr = &attr[..slash_start];
    }
    let mut parts = vec![front.to_owned()];
    parts.extend(kses_hair_parse(attr)?);
    parts.push(format!("{xhtml_slash}{end}"));
    Some(parts)
}

/// WordPress's `wp_kses_hair_parse`: the attributes of `attr`, each with the whitespace after
/// it, or `None` when the whole text is not a run of attributes.
fn kses_hair_parse(attr: &str) -> Option<Vec<String>> {
    let b = attr.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let end = hair_attribute(b, i)?;
        out.push(attr[i..end].to_owned());
        i = end;
    }
    Some(out)
}

/// One attribute of `wp_kses_hair_parse`'s pattern at `i`: its end, or `None`.
fn hair_attribute(b: &[u8], i: usize) -> Option<usize> {
    let at_end = |j: usize| j == b.len() || (j + 1 == b.len() && b[j] == b'\n');
    // The name: `[_a-zA-Z][-_a-zA-Z0-9:.]*`, or a shortcode `\[\[?[^\[\]]+\]\]?`.
    let mut j = i;
    match b.get(j)? {
        c if c.is_ascii_alphabetic() || *c == b'_' => {
            j += 1;
            while j < b.len()
                && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b'_' | b':' | b'.'))
            {
                j += 1;
            }
        }
        b'[' => {
            j += 1;
            if b.get(j) == Some(&b'[') {
                j += 1;
            }
            let start = j;
            while j < b.len() && b[j] != b'[' && b[j] != b']' {
                j += 1;
            }
            if j == start || b.get(j) != Some(&b']') {
                return None;
            }
            j += 1;
            if b.get(j) == Some(&b']') {
                j += 1;
            }
        }
        _ => return None,
    }
    let skip = |mut k: usize| {
        while k < b.len() && is_space(b[k]) {
            k += 1;
        }
        k
    };
    // `\s*=\s*` and a value.
    let eq = skip(j);
    if b.get(eq) == Some(&b'=') {
        let v = skip(eq + 1);
        let value_end = match b.get(v) {
            Some(q @ (b'"' | b'\'')) => b[v + 1..]
                .iter()
                .position(|c| c == q)
                .map(|k| v + 1 + k + 1),
            Some(c) if !is_space(*c) => {
                let mut k = v;
                while k < b.len() && !is_space(b[k]) && b[k] != b'"' && b[k] != b'\'' {
                    k += 1;
                }
                // `[^\s"']+(?:\s|$)`.
                if k < b.len() && is_space(b[k]) {
                    Some(k + 1)
                } else if at_end(k) {
                    Some(k)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(end) = value_end {
            return Some(skip(end));
        }
    }
    // `(?:\s|$)`.
    if j < b.len() && is_space(b[j]) {
        Some(skip(j + 1))
    } else if at_end(j) {
        Some(skip(j))
    } else {
        None
    }
}

/// WordPress's `wp_kses_one_attr` for the attribute `attr` of an `element`: the attribute as
/// kses keeps it (`name="value"`, with the whitespace around it), or empty when kses drops it.
/// Mission Control's kses ([`kses_post`](crate::content::kses_post)) runs on the element with
/// this one attribute, and the attribute is read back from what it keeps (Ruling R144).
fn kses_one_attr(attr: &str, element: &str) -> String {
    let element = element.to_ascii_lowercase();
    let trimmed = attr.trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
    if trimmed.is_empty() || element.is_empty() {
        return String::new();
    }
    let lead = &attr[..attr.len()
        - attr
            .trim_start_matches(|c: char| c.is_ascii() && is_space(c as u8))
            .len()];
    let trail = &attr[attr
        .trim_end_matches(|c: char| c.is_ascii() && is_space(c as u8))
        .len()..];
    let name: String = trimmed
        .split(|c: char| c == '=' || (c.is_ascii() && is_space(c as u8)))
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.is_empty() {
        return String::new();
    }
    // Table parts are kept by kses only inside a table, as an HTML parser reads them.
    let one = format!("<{element} {trimmed}></{element}>");
    let html = crate::content::kses_post(&match element.as_str() {
        "td" | "th" => format!("<table><tr>{one}</tr></table>"),
        "tr" | "tbody" | "thead" | "tfoot" | "caption" | "colgroup" => {
            format!("<table>{one}</table>")
        }
        "col" => format!("<table><colgroup>{one}</colgroup></table>"),
        _ => one,
    });
    let open = format!("<{element}");
    // The element's own tag: `<col` is not `<colgroup`.
    let Some(start) = html.match_indices(&open).map(|(at, _)| at).find(|at| {
        matches!(
            html.as_bytes().get(at + open.len()),
            Some(b' ' | b'>' | b'/' | b'\t' | b'\n')
        )
    }) else {
        return String::new();
    };
    let Some(end) = html[start..].find('>') else {
        return String::new();
    };
    let tag = &html[start + open.len()..start + end];
    let needle = format!(" {name}=\"");
    let Some(at) = tag.find(&needle) else {
        return String::new();
    };
    let value_start = at + needle.len();
    let Some(len) = tag[value_start..].find('"') else {
        return String::new();
    };
    format!(
        "{lead}{name}=\"{}\"{trail}",
        &tag[value_start..value_start + len]
    )
}

/// One shortcode found in the text.
struct Found<'a> {
    callback: Callback,
    /// A second `[` followed the first.
    lead: bool,
    /// A `]` followed the match.
    trail: bool,
    atts: &'a str,
    content: Option<&'a str>,
    /// The offset after the whole match, trailing `]` included.
    end: usize,
}

enum Scan<'a> {
    Found(Found<'a>),
    NoMatch,
    NoTerminator,
}

/// Tries to match a shortcode starting at the `[` at `start`.
fn scan<'a>(
    content: &'a str,
    start: usize,
    tags: &[(String, Callback)],
    no_closer: &mut HashSet<&'a str>,
) -> Scan<'a> {
    let b = content.as_bytes();
    let lead = b.get(start + 1) == Some(&b'[');
    let name_at = start + 1 + usize::from(lead);
    let rest = &content[name_at..];

    // The first registered tag, in registration order, that is a prefix and ends at a boundary.
    let Some((tag, callback)) = tags.iter().find(|(t, _)| {
        rest.starts_with(t.as_str())
            && !rest
                .as_bytes()
                .get(t.len())
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
    }) else {
        return Scan::NoMatch;
    };

    // The attribute text runs to the first `]`, where a `/` just before it marks self-closing.
    let atts_start = name_at + tag.len();
    let mut j = atts_start;
    let (atts_end, self_closing) = loop {
        match b.get(j) {
            None => return Scan::NoTerminator,
            Some(b']') => break (j, false),
            Some(b'/') if b.get(j + 1) == Some(&b']') => break (j, true),
            _ => j += 1,
        }
    };
    let atts = &content[atts_start..atts_end];

    let mut end;
    let mut inner = None;
    if self_closing {
        end = atts_end + 2;
    } else {
        end = atts_end + 1;
        let closer = format!("[/{tag}]");
        let known_missing = no_closer.contains(tag.as_str());
        if !known_missing {
            match content[end..].find(&closer) {
                Some(k) => {
                    inner = Some(&content[end..end + k]);
                    end += k + closer.len();
                }
                None => {
                    // `tag` outlives the loop, and so does `content`; key on the slice in it.
                    no_closer.insert(&content[name_at..name_at + tag.len()]);
                }
            }
        }
    }
    let trail = b.get(end) == Some(&b']');
    if trail {
        end += 1;
    }
    Scan::Found(Found {
        callback: Arc::clone(callback),
        lead,
        trail,
        atts,
        content: inner,
        end,
    })
}
