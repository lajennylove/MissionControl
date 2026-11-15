//! The template hierarchy: the templates a request may render, most specific first.
//!
//! This follows `wp-includes/template-loader.php` and the `get_*_template` functions it calls.
//! The loader tries each kind of request in turn, and a kind with no template found on disk
//! falls through to the next, so the list for a request is the concatenation of every branch
//! that applies to it: a post is `single` and then `singular`, a category is `category` and then
//! `archive`, and every list ends with `index`.
//!
//! Left out, on purpose:
//!
//! - `author-{nicename}` (WordPress 4.7): [`Archive::Author`] carries the user id only. Until the
//!   caller can pass the nicename, `author-{id}`, `author` and `archive` are listed.
//! - The privacy policy page (`privacy-policy`): it needs the `wp_page_for_privacy_policy`
//!   option, which a [`Resolved`] does not carry. The page is listed by its slug, as any page.
//! - The deprecated `paged` template, and `embed` templates.
//! - Custom taxonomy archives (`taxonomy-{tax}-{term}`, `taxonomy-{tax}`, `taxonomy`):
//!   [`Archive`] has no variant for them yet.

use crate::content::Post;
use crate::front::{Archive, Resolved};

/// The template names a request may render, without a file extension, most specific first, as
/// WordPress's template loader tries them. `index` is last in every list, so a theme with only an
/// `index` template renders everything. A [`Resolved::Redirect`] has no templates: the caller
/// redirects.
///
/// `page_template` is the post's `_wp_page_template` meta, which the caller reads: the template
/// chosen in the editor for a page, or for any post of a type whose theme offers templates. It is
/// used for a page, a post, an attachment's single rules and a static front page. It is ignored
/// when it is empty, `default`, or not a safe path (`..`, `./`, a leading slash or backslash, a
/// drive letter), as WordPress's `validate_file` refuses it; it is listed as stored, with its
/// own extension, because it names a file of the theme.
///
/// A slug with letters outside ASCII is listed in both forms, decoded and percent-encoded
/// (`page-café`, `page-caf%c3%a9`), because WordPress stores such slugs encoded and themes name
/// their files either way. WordPress lists the decoded form only when the stored slug is
/// encoded; here the encoded form is also made from a slug stored as letters.
///
/// # Examples
///
/// ```ignore
/// let names = templates_for(&Resolved::NotFound, None);
/// assert_eq!(names, ["404", "index"]);
/// ```
pub fn templates_for(resolved: &Resolved, page_template: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    let custom = page_template.filter(|template| is_usable_template(template));
    match resolved {
        Resolved::Redirect { .. } => return names,
        Resolved::NotFound => names.push("404".to_owned()),
        Resolved::Search { .. } => names.push("search".to_owned()),
        Resolved::FrontPage { page, .. } => {
            names.push("front-page".to_owned());
            match page {
                Some(page) => {
                    page_rules(&mut names, page, custom);
                    names.push("singular".to_owned());
                }
                None => names.push("home".to_owned()),
            }
        }
        Resolved::Home { .. } => names.push("home".to_owned()),
        Resolved::Archive { archive, .. } => archive_rules(&mut names, archive),
        Resolved::Attachment(post) => {
            attachment_rules(&mut names, post);
            single_rules(&mut names, post, custom);
            names.push("singular".to_owned());
        }
        Resolved::Single(post) => {
            single_rules(&mut names, post, custom);
            names.push("singular".to_owned());
        }
        Resolved::Page(post) => {
            page_rules(&mut names, post, custom);
            names.push("singular".to_owned());
        }
    }
    names.push("index".to_owned());
    names
}

/// `get_post_type_archive_template`, the term templates, `get_author_template` and
/// `get_date_template`, then `get_archive_template`'s fallback.
fn archive_rules(names: &mut Vec<String>, archive: &Archive) {
    match archive {
        Archive::PostType(post_type) => names.push(format!("archive-{post_type}")),
        Archive::Category(term) => term_rules(names, "category", &term.slug, term.term_id),
        Archive::Tag(term) => term_rules(names, "tag", &term.slug, term.term_id),
        Archive::Author(id) => {
            names.push(format!("author-{id}"));
            names.push("author".to_owned());
        }
        Archive::Date { .. } => names.push("date".to_owned()),
    }
    names.push("archive".to_owned());
}

/// `get_category_template` and `get_tag_template`: `{prefix}-{slug}`, `{prefix}-{id}`, `{prefix}`.
fn term_rules(names: &mut Vec<String>, prefix: &str, slug: &str, id: i64) {
    for form in slug_forms(slug) {
        names.push(format!("{prefix}-{form}"));
    }
    names.push(format!("{prefix}-{id}"));
    names.push(prefix.to_owned());
}

/// `get_attachment_template`: the MIME type's forms, then `attachment`.
fn attachment_rules(names: &mut Vec<String>, post: &Post) {
    let (kind, subtype) = match post.mime_type.split_once('/') {
        Some((kind, subtype)) => (kind, subtype),
        None => (post.mime_type.as_str(), ""),
    };
    if !kind.is_empty() {
        if !subtype.is_empty() {
            names.push(format!("{kind}-{subtype}"));
            names.push(subtype.to_owned());
        }
        names.push(kind.to_owned());
    }
    names.push("attachment".to_owned());
}

/// `get_single_template`: the chosen template, `single-{type}-{slug}`, `single-{type}`, `single`.
fn single_rules(names: &mut Vec<String>, post: &Post, custom: Option<&str>) {
    if let Some(custom) = custom {
        names.push(custom.to_owned());
    }
    if !post.post_type.is_empty() {
        if !post.slug.is_empty() {
            for form in slug_forms(&post.slug) {
                names.push(format!("single-{}-{form}", post.post_type));
            }
        }
        names.push(format!("single-{}", post.post_type));
    }
    names.push("single".to_owned());
}

/// `get_page_template`: the chosen template, `page-{slug}`, `page-{id}`, `page`.
fn page_rules(names: &mut Vec<String>, post: &Post, custom: Option<&str>) {
    if let Some(custom) = custom {
        names.push(custom.to_owned());
    }
    if !post.slug.is_empty() {
        for form in slug_forms(&post.slug) {
            names.push(format!("page-{form}"));
        }
    }
    names.push(format!("page-{}", post.id));
    names.push("page".to_owned());
}

/// Whether a stored page template names a file the theme may be asked for: not empty, not
/// `default` (the "no template" choice), and passing `validate_file`'s path checks.
fn is_usable_template(template: &str) -> bool {
    let bytes = template.as_bytes();
    !template.is_empty()
        && template != "default"
        && !template.contains("..")
        && !template.contains("./")
        && !template.contains('\\')
        && !template.starts_with('/')
        && bytes.get(1) != Some(&b':')
}

/// The forms of a slug a theme may name a file by: decoded (`café`) first, then percent-encoded
/// (`caf%c3%a9`). One form when they are the same, as for an ASCII slug.
fn slug_forms(slug: &str) -> Vec<String> {
    let decoded = percent_decode(slug);
    let encoded = if slug.contains('%') {
        slug.to_owned()
    } else {
        percent_encode(slug)
    };
    if decoded == encoded {
        vec![decoded]
    } else {
        vec![decoded, encoded]
    }
}

/// `urldecode`: `%` and two hex digits become the byte. Kept as it is when the bytes are not
/// UTF-8.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// Percent-encodes the bytes outside ASCII, in lowercase hex as WordPress stores them.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii() {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02x}"));
        }
    }
    out
}
