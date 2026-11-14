//! Slugs: made from a title as WordPress's `sanitize_title` does, made unique as
//! `wp_unique_post_slug` does, freed when a post is trashed, and remembered when they change, so
//! an old link can still find its post.
//!
//! WordPress keeps non-ASCII letters in a slug, percent-encoded. Mission Control transliterates
//! them to ASCII instead, with the [`deunicode`] crate (BSD-3-Clause), so a slug is always plain
//! ASCII: `Ünïcode` gives `unicode`, `Привет мир` gives `privet-mir`, `日本` gives `ri-ben`.
//!
//! ```ignore
//! use mission_control_core::content::{make_slug, unique_slug};
//!
//! let slug = unique_slug(&db, "post", None, &make_slug("Hello world"), None).await?;
//! // "hello-world", or "hello-world-2" when a post already has it.
//! ```

use deunicode::deunicode_char;
use orbit_orm::{Database, Error, Model, Operator, Query, Value, raw};
use serde_json::json;

use super::meta::{add_post_meta_in, delete_post_meta_value_in, post_meta_has_value_in};
use super::post::{Post, PostStatus};
use super::taxonomy::Conn;

/// The longest slug the `slug` column holds, as WordPress's `post_name` (200 characters).
pub const MAX_SLUG_LENGTH: usize = 200;

/// The suffix a trashed post's slug gets, so the slug is free again, as in WordPress.
pub(super) const TRASHED_SUFFIX: &str = "__trashed";

/// The post meta that keeps a trashed post's slug, to give it back on untrash.
pub(super) const DESIRED_SLUG_META: &str = "_wp_desired_post_slug";

/// The post meta that keeps a published post's earlier slugs, one row each.
const OLD_SLUG_META: &str = "_wp_old_slug";

/// The feed names of WordPress's default rewrite rules (`$wp_rewrite->feeds`), and `embed`. A
/// post with one of them as its slug would hide the feed, so it gets a number.
const RESERVED: [&str; 6] = ["feed", "rdf", "rss", "rss2", "atom", "embed"];

/// The slug for `title`, as WordPress's `sanitize_title` makes it when a post is saved: tags
/// stripped, letters transliterated to ASCII, lower case, whitespace, dots, slashes and dashes
/// made single hyphens, other punctuation removed, no hyphen at either end. Underscores stay, as
/// in WordPress. The result has at most [`MAX_SLUG_LENGTH`] characters.
///
/// The rules of `sanitize_title_with_dashes` in its `save` context are kept: HTML entities are
/// removed (`&nbsp;`, `&ndash;` and `&mdash;` become hyphens), curly quotes, `©`, `®`, `™`, `°`
/// and `…` are dropped, `×` becomes `x`, and the Unicode spaces and dashes become hyphens. Where
/// WordPress percent-encodes the letters `remove_accents` leaves, the letters are transliterated
/// here with [`deunicode`].
///
/// When nothing is left but the title had characters that do not transliterate (such as private
/// use characters), the slug is made from their code points instead, `u` and lower-case hex,
/// joined by hyphens: `\u{E000}` gives `ue000`. So a title with letters always gives a non-empty
/// slug. An empty title, or one of only ASCII punctuation, gives an empty slug, as in WordPress;
/// `wp_insert_post` then uses the post id.
pub fn make_slug(title: &str) -> String {
    let stripped = strip_tags(title);
    let mut ascii = String::with_capacity(stripped.len());
    // The characters that gave no letter or digit, for the fallback.
    let mut untransliterated = Vec::new();
    for c in stripped.chars() {
        match c {
            // Spaces and dashes WordPress turns into hyphens.
            '/'
            | '\u{A0}'
            | '\u{2013}'
            | '\u{2014}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}' => ascii.push('-'),
            // Characters WordPress strips: soft hyphen, ¡ ¿, angle and curly quotes, bullet,
            // © ® ° … ™, acute and grave accents, macron, caron, and invisible characters.
            '\u{AD}'
            | '\u{A1}'
            | '\u{BF}'
            | '\u{AB}'
            | '\u{BB}'
            | '\u{2039}'
            | '\u{203A}'
            | '\u{2018}'..='\u{201F}'
            | '\u{2022}'
            | '\u{A9}'
            | '\u{AE}'
            | '\u{B0}'
            | '\u{2026}'
            | '\u{2122}'
            | '\u{B4}'
            | '\u{2CA}'
            | '\u{301}'
            | '\u{341}'
            | '\u{300}'
            | '\u{304}'
            | '\u{30C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{FEFF}'
            | '\u{FFFC}' => {}
            '\u{D7}' => ascii.push('x'),
            c if c.is_ascii() => ascii.push(c),
            c if c.is_whitespace() => ascii.push(' '),
            c => {
                // deunicode ends a CJK syllable with a space, so `日本` is `ri-ben`.
                let t = deunicode_char(c).unwrap_or("");
                if !t.bytes().any(|b| b.is_ascii_alphanumeric()) {
                    untransliterated.push(c);
                }
                ascii.push_str(t);
            }
        }
    }
    let slug = sanitize_ascii(&ascii.to_ascii_lowercase());
    if slug.is_empty() {
        return truncate(&code_points(&untransliterated), MAX_SLUG_LENGTH);
    }
    truncate(&slug, MAX_SLUG_LENGTH)
}

/// Whether `post_type` is hierarchical, so its slugs are unique among siblings and its links are
/// paths. Only `page` is, until post types are registered with their own settings.
pub fn is_hierarchical(post_type: &str) -> bool {
    post_type == "page"
}

/// `base`, or a free slug made from it with `-2`, `-3`, ..., for a post of `post_type` under
/// `parent_id`, as WordPress's `wp_unique_post_slug` does. `exclude_id` is the post being saved,
/// whose own slug does not count. [`is_hierarchical`] decides whether the slug is unique among
/// siblings; [`unique_slug_for`] takes that as an argument, for hierarchical custom types.
///
/// See [`unique_slug_for`] for the rules.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn unique_slug(
    db: &Database,
    post_type: &str,
    parent_id: Option<i64>,
    base: &str,
    exclude_id: Option<i64>,
) -> Result<String, Error> {
    unique_slug_on(
        &mut &*db,
        post_type,
        is_hierarchical(post_type),
        parent_id,
        base,
        exclude_id,
    )
    .await
}

/// [`unique_slug`] for a post type whose hierarchy the caller states.
///
/// The rules of `wp_unique_post_slug`:
/// - A slug is taken by a post of the same type; for a `hierarchical` type, only by a sibling
///   (same parent) of the same type or an attachment; for an attachment, by a post of any type.
///   Revisions, auto-drafts and trashed posts never take a slug.
/// - `feed`, `rdf`, `rss`, `rss2`, `atom` and `embed` are refused, and for a hierarchical type,
///   a slug that reads as a page number (`2`, `page2`), so the slug does not hide a feed or a
///   page of an archive.
/// - A refused or taken slug gets `-2`, then `-3`, and so on, until one is free. The base is cut
///   so the slug fits [`MAX_SLUG_LENGTH`].
///
/// An empty `base` gives an empty slug: a draft may have none. A revision's slug is returned as
/// it is. The caller decides when to call it: WordPress leaves the slugs of drafts and pending
/// posts as they are, and makes them unique when they are published.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn unique_slug_for(
    db: &Database,
    post_type: &str,
    hierarchical: bool,
    parent_id: Option<i64>,
    base: &str,
    exclude_id: Option<i64>,
) -> Result<String, Error> {
    unique_slug_on(
        &mut &*db,
        post_type,
        hierarchical,
        parent_id,
        base,
        exclude_id,
    )
    .await
}

/// [`unique_slug_for`] on a connection that may be a transaction.
pub(super) async fn unique_slug_on<C: Conn>(
    c: &mut C,
    post_type: &str,
    hierarchical: bool,
    parent_id: Option<i64>,
    base: &str,
    exclude_id: Option<i64>,
) -> Result<String, Error> {
    let slug = truncate(base, MAX_SLUG_LENGTH);
    if slug.is_empty() || post_type == "revision" {
        return Ok(slug);
    }
    let taken = async |c: &mut C, candidate: &str| {
        let mut query = Post::query()
            .where_eq("slug", candidate)
            .where_op("post_type", Operator::Ne, "revision")
            .where_op("status", Operator::Ne, PostStatus::Trash)
            .where_op("status", Operator::Ne, PostStatus::AutoDraft);
        if let Some(id) = exclude_id {
            query = query.where_op(Post::PRIMARY_KEY, Operator::Ne, id);
        }
        query = scope(query, post_type, hierarchical, parent_id);
        query.exists(c.exec()).await
    };
    if !reserved(&slug, hierarchical) && !taken(c, &slug).await? {
        return Ok(slug);
    }
    let mut n: u64 = 2;
    loop {
        let suffix = format!("-{n}");
        let candidate = format!(
            "{}{suffix}",
            truncate(&slug, MAX_SLUG_LENGTH - suffix.len())
        );
        if !taken(c, &candidate).await? {
            return Ok(candidate);
        }
        n += 1;
    }
}

/// Narrows `query` to the posts whose slug would clash with one of `post_type` under
/// `parent_id`.
fn scope(
    query: Query<Post>,
    post_type: &str,
    hierarchical: bool,
    parent_id: Option<i64>,
) -> Query<Post> {
    if post_type == "attachment" {
        query
    } else if hierarchical {
        query
            .where_in("post_type", [post_type, "attachment"])
            .where_eq("parent_id", parent_id)
    } else {
        query.where_eq("post_type", post_type)
    }
}

/// Whether WordPress refuses `slug` because it would clash with a feed, an embed or, for a
/// hierarchical type, a page number (`^(page)?\d+$`).
fn reserved(slug: &str, hierarchical: bool) -> bool {
    if RESERVED.contains(&slug) {
        return true;
    }
    let digits = slug.strip_prefix("page").unwrap_or(slug);
    hierarchical && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// The post of `post_type` that once had `slug`, as WordPress's `_find_post_by_old_slug` finds
/// it for `wp_old_slug_redirect`: the post whose `_wp_old_slug` meta holds the slug. When
/// several posts once had it, the one that gave it up last. `None` when no post had it.
///
/// WordPress narrows the search by the year, month and day of the requested URL when it has
/// them; that is left to the caller, which can compare the found post's date.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn find_by_old_slug(
    db: &Database,
    post_type: &str,
    slug: &str,
) -> Result<Option<Post>, Error> {
    let id: Option<(i64,)> = raw::fetch_optional(
        db,
        "SELECT post_meta.post_id FROM post_meta \
         INNER JOIN posts ON posts.id = post_meta.post_id \
         WHERE posts.post_type = ? AND post_meta.meta_key = ? AND post_meta.meta_value = ? \
         ORDER BY post_meta.id DESC LIMIT 1",
        &[
            post_type.into(),
            OLD_SLUG_META.into(),
            Value::from(json!(slug).to_string()),
        ],
    )
    .await?;
    match id {
        Some((id,)) => Post::query().find(db, id).await,
        None => Ok(None),
    }
}

/// Records `before`'s slug as an old slug of the post when a save changes it, as WordPress's
/// `wp_check_for_changed_slugs` does on `post_updated`: only for a published post (or an
/// `inherit` attachment) of a type that is not hierarchical. An old slug already recorded is not
/// added again, and the new slug, if it was an old one, is removed from the list, since it is
/// the slug again. Runs on the connection of the save's transaction.
pub(super) async fn record_old_slug<C: Conn>(
    c: &mut C,
    before: &Post,
    after: &Post,
) -> Result<(), Error> {
    if before.slug == after.slug {
        return Ok(());
    }
    let published = after.status == PostStatus::Publish
        || (after.post_type == "attachment" && after.status == PostStatus::Inherit);
    if !published || is_hierarchical(&after.post_type) {
        return Ok(());
    }
    let old = json!(before.slug);
    if !before.slug.is_empty() && !post_meta_has_value_in(c, after.id, OLD_SLUG_META, &old).await? {
        add_post_meta_in(c, after.id, OLD_SLUG_META, &old).await?;
    }
    delete_post_meta_value_in(c, after.id, OLD_SLUG_META, &json!(after.slug)).await?;
    Ok(())
}

/// `slug` with the [`TRASHED_SUFFIX`], as `wp_add_trashed_suffix_to_post_name_for_post` makes
/// it: cut to 191 characters first, so the result fits the column. A slug that already ends with
/// the suffix is kept.
pub(super) fn trashed_slug(slug: &str) -> String {
    if slug.ends_with(TRASHED_SUFFIX) {
        return slug.to_owned();
    }
    format!(
        "{}{TRASHED_SUFFIX}",
        truncate(slug, MAX_SLUG_LENGTH - 9 /* 191, as WordPress */)
    )
}

/// `slug` cut to at most `max` characters, with no hyphen left at the end, as WordPress's
/// `_truncate_post_slug`.
fn truncate(slug: &str, max: usize) -> String {
    match slug.char_indices().nth(max) {
        Some((end, _)) => slug[..end].trim_end_matches('-').to_owned(),
        None => slug.to_owned(),
    }
}

/// `text` without HTML tags, as PHP's `strip_tags`: from each `<` to the next `>`, or to the end
/// when the tag is not closed. The content between tags is kept.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match (in_tag, c) {
            (false, '<') => in_tag = true,
            (true, '>') => in_tag = false,
            (false, c) => out.push(c),
            (true, _) => {}
        }
    }
    out
}

/// The ASCII steps of `sanitize_title_with_dashes` on lower-case `text`: entities removed (the
/// space and dash entities made hyphens), dots made hyphens, anything but `a-z`, `0-9`, space,
/// `_` and `-` removed, whitespace made hyphens, repeated hyphens folded, hyphens trimmed.
fn sanitize_ascii(text: &str) -> String {
    let mut text = text.to_owned();
    for entity in [
        "&nbsp;", "&#160;", "&ndash;", "&#8211;", "&mdash;", "&#8212;",
    ] {
        text = text.replace(entity, "-");
    }
    let text = remove_entities(&text);
    let mut slug = String::with_capacity(text.len());
    for c in text.chars() {
        let c = match c {
            '.' => '-',
            c if c.is_ascii_whitespace() || c == '\u{B}' => '-',
            c => c,
        };
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !keep || (c == '-' && slug.ends_with('-')) {
            continue;
        }
        slug.push(c);
    }
    slug.trim_matches('-').to_owned()
}

/// `text` without the entities `&...;`, matched as PHP's `/&.+?;/`: from an `&` to the nearest
/// `;` after at least one character.
fn remove_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        // `.+?` needs one character, which may itself be `;`, before the closing `;`.
        let end = after.chars().next().and_then(|first| {
            let skip = first.len_utf8();
            after[skip..].find(';').map(|i| skip + i)
        });
        match end {
            Some(end) => rest = &after[end + 1..],
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The fallback slug when nothing transliterates: `u` and the lower-case hex code point of each
/// of `chars`, joined by hyphens.
fn code_points(chars: &[char]) -> String {
    chars
        .iter()
        .map(|&c| format!("u{:x}", u32::from(c)))
        .collect::<Vec<_>>()
        .join("-")
}
