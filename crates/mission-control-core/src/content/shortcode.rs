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

use crate::hooks::{Hooks, Owner, THE_CONTENT};

/// The priority WordPress gives `do_shortcode` on `the_content`.
const THE_CONTENT_PRIORITY: i32 = 11;

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

    /// Adds [`do_shortcodes`](Self::do_shortcodes) to the `the_content` filter on `hooks`, at
    /// WordPress's priority 11 and owned by [`Owner::Core`].
    pub fn register_the_content(self: &Arc<Self>, hooks: &Hooks) {
        let this = Arc::clone(self);
        hooks.add_filter(THE_CONTENT, THE_CONTENT_PRIORITY, Owner::Core, move |c| {
            this.do_shortcodes(&c)
        });
    }

    /// Replaces every registered shortcode in `content` with its callback's output, like
    /// WordPress's `do_shortcode`. Unknown tags and malformed brackets stay as text.
    pub fn do_shortcodes(&self, content: &str) -> String {
        if !content.contains('[') {
            return content.to_string();
        }
        let tags: Vec<(String, Callback)> = self
            .read()
            .iter()
            .map(|e| (e.tag.clone(), Arc::clone(&e.callback)))
            .collect();
        if tags.is_empty() {
            return content.to_string();
        }

        let mut out = String::with_capacity(content.len());
        // Tags with no `[/tag]` anywhere after a point: no later search can find one either.
        let mut no_closer: HashSet<&str> = HashSet::new();
        let mut copied = 0;
        let mut at = 0;
        while let Some(off) = content[at..].find('[') {
            let start = at + off;
            match scan(content, start, &tags, &mut no_closer) {
                Scan::Found(m) => {
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
        out
    }
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
