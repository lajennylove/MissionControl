//! WordPress's dropdowns on Ignite's forms, and the lost password form's checks, where Ignite
//! 0.6.6 renders or answers otherwise. Ignite's select always starts with a "Select an option"
//! placeholder, its create form has no default values, and a resource's choices do not know the
//! record being edited; WordPress's dropdowns differ:
//!
//! - A post's Status (`post_submit_meta_box`) has no blank choice, and a new post's is Draft.
//!   The placeholder is taken out and Draft is chosen when nothing is (the save also stores a
//!   blank status as a draft).
//! - Parent Category (`term.php` and `edit-tags.php`, `wp_dropdown_categories` with
//!   `show_option_none` "None") has one empty choice, "None": the placeholder is taken out.
//! - A category's or a page's parent dropdown leaves out the item itself and its descendants
//!   (`wp_dropdown_categories` and `wp_dropdown_pages` with `exclude_tree`); the save refuses
//!   the item as its own parent on the field (see `mission_control_core::admin::terms` and
//!   `mission_control_core::types`).
//! - The lost password form (`POST /mc-admin/password/forgot`, Ignite's): an empty email is
//!   answered "Please enter an email address." and an address that is not one "Please enter a
//!   valid email address." (WordPress's `edit_user` messages), on the form again (422) under the
//!   email field, before Ignite says a link was sent. Ignite's own answer stays for a valid
//!   address, whether or not it has an account.
//!
//! - A post's Status on the New screen has no Trash: a new post is not created in the trash, as
//!   WordPress's New screen offers no Trash (the save refuses it on the field, see
//!   `mission_control_core::types`). On the edit form Trash stays; a save that moves the post to
//!   the trash goes to the list with WordPress's notice, "1 post moved to the Trash."
//!   (`edit.php?trashed=1&ids=…`, `$bulk_messages['post']['trashed']`), in place of Ignite's
//!   "Changes saved.", as WordPress's Move to Trash goes to the list.
//! - Any other save of a post's edit form stays on the form, with WordPress's "Post updated."
//!   ("Page updated." on a page): `post.php` answers a save with `post.php?action=edit&message=1`,
//!   where Ignite's update goes to the list with "Changes saved.".
//! - A date and time field (`datetime-local`) takes any minute: it gets `step="1"`, as the
//!   browser's default step counts whole minutes from the stored value's seconds and refused a
//!   whole minute for a date stored with seconds. A date left as it is keeps its seconds.
//! - A settings screen's heading and title are its own name ("General Settings", as
//!   `options-general.php` shows it), not Ignite's "Edit General Settings #1": the screen has one
//!   record, which is not named. The record screens keep the number.
//! - A list asked for a page beyond its last is sent to its last page, as `WP_List_Table`'s
//!   `set_pagination_args` redirects to `paged` = the last page.
//!
//! The rewrites are on the HTML Ignite answers, as the media library's and the menu builder's
//! views are; a form without the field is left as it is.

use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;

use mission_control_core::admin::{CATEGORY_SCREEN, SettingsScreen};
use mission_control_core::content::{Post, PostStatus, Taxonomy, Term, is_email};
use mission_control_core::types::PostTypeResources;
use orbit_orm::{Database, Error, Model};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Header, Method, RawStr, Status};
use rocket::{Data, Request, Response};

use super::ADMIN_SEGMENT;
use super::media::escape;

/// The status a new post's dropdown shows, as WordPress's New screen.
const DRAFT: &str = "draft";

/// The trash status, which a new post's dropdown does not offer.
const TRASH: &str = "trash";

/// The name of Ignite's flash message cookie, which a save that trashes a post drops.
const FLASH_COOKIE: &str = "ignite_flash";

/// The message of an empty lost password form.
pub(crate) const EMAIL_REQUIRED: &str = "Please enter an email address.";

/// The message of a lost password form whose email is not an address.
pub(crate) const EMAIL_INVALID: &str = "Please enter a valid email address.";

/// The most bytes of a lost password request read to check its email: the form holds a token
/// and one address. A longer body goes on to Ignite unchecked.
const FORGOT_PEEK: usize = 512;

/// Rewrites the dropdowns of the post type and category forms, and checks the lost password
/// form; see the [module](self).
pub(crate) struct FormViews;

/// The lost password form's refusal, kept for the answer.
#[derive(Default)]
struct ForgotRefusal(std::sync::OnceLock<(String, &'static str)>);

/// The form page a request answers, by its path after `/mc-admin`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FormPage {
    /// A post type's New screen or its create (`<slug>/new`, `POST <slug>`).
    New(String),
    /// A record's edit form or its save (`<slug>/<id>/edit`, `POST <slug>/<id>`).
    Edit(String, i64),
}

/// The form page of `method` on `path`, answered with `status`.
fn form_page(method: Method, status: Status, path: &str) -> Option<FormPage> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != ADMIN_SEGMENT {
        return None;
    }
    let slug = segments.next()?.to_owned();
    let rest: Vec<&str> = segments.collect();
    let id = |text: &str| text.parse::<i64>().ok().filter(|id| *id > 0);
    match (method, status, rest.as_slice()) {
        (Method::Get, Status::Ok, ["new"]) => Some(FormPage::New(slug)),
        (Method::Post, Status::UnprocessableEntity, []) => Some(FormPage::New(slug)),
        (Method::Get, Status::Ok, [record, "edit"]) => {
            id(record).map(|id| FormPage::Edit(slug, id))
        }
        (Method::Post, Status::UnprocessableEntity, [record]) => {
            id(record).map(|id| FormPage::Edit(slug, id))
        }
        _ => None,
    }
}

#[rocket::async_trait]
impl Fairing for FormViews {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control form dropdowns",
            kind: Kind::Request | Kind::Response,
        }
    }

    async fn on_request(&self, request: &mut Request<'_>, data: &mut Data<'_>) {
        if request.method() != Method::Post || request.uri().path().as_str() != forgot_path() {
            return;
        }
        let body = data.peek(FORGOT_PEEK).await.to_vec();
        if !data.peek_complete() {
            return;
        }
        let email = form_value(&body, "email").unwrap_or_default();
        let email = email.trim();
        let message = if email.is_empty() {
            EMAIL_REQUIRED
        } else if !is_email(email) {
            EMAIL_INVALID
        } else {
            return;
        };
        let _ = request
            .local_cache(ForgotRefusal::default)
            .0
            .set((email.to_owned(), message));
        // Ignite's form answers the request again, with the message (see `on_response`); no
        // link is asked for.
        request.set_method(Method::Get);
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if request.method() == Method::Post && response.status() == Status::SeeOther {
            after_save(request, response).await;
            return;
        }
        if response.content_type() != Some(ContentType::HTML) {
            return;
        }
        if request.method() == Method::Get
            && response.status() == Status::Ok
            && let Some(slug) = list_slug(request.uri().path().as_str())
        {
            list_page(request, response, &slug).await;
            return;
        }
        if let Some((email, message)) = request.local_cache(ForgotRefusal::default).0.get() {
            if response.status() != Status::Ok {
                return;
            }
            let Ok(html) = response.body_mut().to_string().await else {
                return;
            };
            let html = with_email_error(&html, email, message);
            response.set_status(Status::UnprocessableEntity);
            response.set_sized_body(html.len(), Cursor::new(html));
            return;
        }
        let Some(page) = form_page(
            request.method(),
            response.status(),
            request.uri().path().as_str(),
        ) else {
            return;
        };
        let rocket = request.rocket();
        let Some(db) = rocket.state::<Arc<Database>>() else {
            return;
        };
        let slug = match &page {
            FormPage::New(slug) | FormPage::Edit(slug, _) => slug.as_str(),
        };
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        // Every form's date and time takes any minute (see `with_second_steps`).
        let mut html = with_second_steps(&html);
        if let (FormPage::Edit(_, id), Some(screen)) = (&page, SettingsScreen::from_slug(slug)) {
            let html = with_screen_title(&html, screen, *id);
            response.set_sized_body(html.len(), Cursor::new(html));
            return;
        }
        let post_type = rocket
            .state::<Arc<PostTypeResources>>()
            .and_then(|resources| resources.post_type(slug));
        let category = slug == CATEGORY_SCREEN;
        if post_type.is_none() && !category {
            response.set_sized_body(html.len(), Cursor::new(html));
            return;
        }
        // A save of the edit form comes back here with WordPress's message (`post.php`'s
        // `message=1`, see `after_save`).
        if let FormPage::Edit(..) = page
            && post_type.is_some()
            && request.query_value::<i64>("message").and_then(Result::ok) == Some(1)
        {
            html = with_notice(&html, updated_message(slug));
        }
        let excluded = match &page {
            FormPage::Edit(slug, id) => {
                let tree = if category {
                    category_tree(db, *id).await
                } else {
                    post_tree(db, slug, *id).await
                };
                match tree {
                    Ok(tree) => tree,
                    Err(e) => {
                        log::error!("Could not read the descendants of {slug} {id}: {e}");
                        HashSet::new()
                    }
                }
            }
            FormPage::New(_) => HashSet::new(),
        };
        let new = matches!(page, FormPage::New(_));
        if post_type.is_some() {
            html = edit_select(&html, "status", |options| {
                // WordPress's New screen has no Trash: a post goes there once it exists.
                options.retain(|o| !o.value.is_empty() && !(new && o.value == TRASH));
                if !options.iter().any(|o| o.selected) {
                    for option in options.iter_mut() {
                        option.selected = option.value == DRAFT;
                    }
                }
            });
        }
        html = edit_select(&html, "parent", |options| {
            if category {
                // One empty choice, "None": the placeholder goes.
                let mut seen_empty = false;
                options.retain(|o| {
                    if !o.value.is_empty() {
                        return true;
                    }
                    let keep = !seen_empty && o.label != "Select an option";
                    seen_empty |= keep;
                    keep
                });
            }
            options.retain(|o| {
                o.value
                    .parse::<i64>()
                    .map_or(true, |id| !excluded.contains(&id))
            });
        });
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}

/// `html`, a settings screen's form, with its own name as its heading and title: Ignite's
/// "Edit General Settings #1" is "General Settings", as WordPress's settings screens are named.
fn with_screen_title(html: &str, screen: SettingsScreen, id: i64) -> String {
    html.replace(&format!("Edit {} #{id}", screen.title()), screen.title())
}

/// The slug of the list a path shows (`/mc-admin/<slug>`), if it is one.
fn list_slug(path: &str) -> Option<String> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != ADMIN_SEGMENT {
        return None;
    }
    let slug = segments.next().filter(|slug| !slug.is_empty())?;
    segments.next().is_none().then(|| slug.to_owned())
}

/// After a save of a post (`POST <slug>/<id>`), in place of Ignite's redirect to the list with
/// "Changes saved.", as `post.php` answers a save: a save that moved the post to the trash goes
/// to the list with WordPress's notice (`edit.php?trashed=1&ids=<id>`); any other goes back to
/// the post's edit form with WordPress's "Post updated." (`post.php?action=edit&message=1`),
/// shown by the form ([`updated_message`]). Ignite's flash is dropped either way. Any other
/// answer is left alone.
async fn after_save<'r>(request: &'r Request<'_>, response: &mut Response<'r>) {
    let mut segments = request.uri().path().segments();
    let (Some(ADMIN_SEGMENT), Some(slug), Some(record), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return;
    };
    let Some(id) = record.parse::<i64>().ok().filter(|id| *id > 0) else {
        return;
    };
    let rocket = request.rocket();
    let is_post_type = rocket
        .state::<Arc<PostTypeResources>>()
        .is_some_and(|resources| resources.post_type(slug).is_some());
    let Some(db) = rocket.state::<Arc<Database>>() else {
        return;
    };
    if !is_post_type {
        return;
    }
    let trashed = match Post::query().find(db.as_ref(), id).await {
        Ok(post) => post.is_some_and(|p| p.post_type == slug && p.status == PostStatus::Trash),
        Err(e) => {
            log::error!("Could not read post {id} after its save: {e}");
            false
        }
    };
    let kept: Vec<String> = response
        .headers()
        .get("Set-Cookie")
        .filter(|cookie| !cookie.starts_with(&format!("{FLASH_COOKIE}=")))
        .map(str::to_owned)
        .collect();
    response.remove_header("Set-Cookie");
    for cookie in kept {
        response.adjoin_header(Header::new("Set-Cookie", cookie));
    }
    let location = if trashed {
        format!("{}/{slug}?trashed=1&ids={id}", super::ADMIN_PATH)
    } else {
        format!("{}/{slug}/{id}/edit?message=1", super::ADMIN_PATH)
    };
    response.set_raw_header("Location", location);
}

/// WordPress's message after a post is saved (`$messages['post'][1]`, `$messages['page'][1]`):
/// "Page updated." on a page, "Post updated." on the other post types, as `post_updated_messages`
/// gives a custom post type the post's messages.
fn updated_message(slug: &str) -> &'static str {
    if slug == "page" {
        "Page updated."
    } else {
        "Post updated."
    }
}

/// `html` with `step="1"` on each date and time input (`type="datetime-local"`) that has no
/// step, so any minute is accepted. The browser's default step is a minute counted from the
/// value's seconds, so a stored date with seconds (`02:43:22`) refused a whole minute
/// (`10:00`). WordPress's date fields take any minute; a date left as it is keeps its seconds.
fn with_second_steps(html: &str) -> String {
    const INPUT: &str = "type=\"datetime-local\"";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(INPUT) {
        let end = at + INPUT.len();
        let tag_start = rest[..at].rfind('<').unwrap_or(0);
        let tag_end = rest[at..].find('>').map_or(rest.len(), |close| at + close);
        out.push_str(&rest[..end]);
        if !rest[tag_start..tag_end].contains(" step=") {
            out.push_str(" step=\"1\"");
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// WordPress's notice of posts moved to the trash (`$bulk_messages['post']['trashed']`): "1 post
/// moved to the Trash.", with "page" on the Pages list.
fn trashed_message(slug: &str, count: i64) -> String {
    let noun = if slug == "page" { "page" } else { "post" };
    if count == 1 {
        format!("1 {noun} moved to the Trash.")
    } else {
        format!("{count} {noun}s moved to the Trash.")
    }
}

/// A list page: sent to its last page when it asks for one beyond it (`set_pagination_args`),
/// and on a post type's list, the trash notice of `?trashed=<n>`.
async fn list_page<'r>(request: &'r Request<'_>, response: &mut Response<'r>, slug: &str) {
    let Ok(html) = response.body_mut().to_string().await else {
        return;
    };
    if let Some(last) = beyond_last_page(&html) {
        let query: Vec<String> = request
            .uri()
            .query()
            .map(|query| {
                query
                    .segments()
                    .filter(|(key, _)| *key != "page")
                    .map(|(key, value)| {
                        format!(
                            "{}={}",
                            RawStr::new(key).percent_encode(),
                            RawStr::new(value).percent_encode()
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut query = query;
        if last > 1 {
            query.push(format!("page={last}"));
        }
        let location = if query.is_empty() {
            request.uri().path().to_string()
        } else {
            format!("{}?{}", request.uri().path(), query.join("&"))
        };
        response.set_status(Status::SeeOther);
        response.set_raw_header("Location", location);
        response.set_sized_body(0, Cursor::new(String::new()));
        return;
    }
    let count = request
        .query_value::<i64>("trashed")
        .and_then(Result::ok)
        .filter(|n| *n > 0);
    let is_post_type = request
        .rocket()
        .state::<Arc<PostTypeResources>>()
        .is_some_and(|resources| resources.post_type(slug).is_some());
    let html = match count {
        Some(count) if is_post_type => with_notice(&html, &trashed_message(slug, count)),
        _ => html,
    };
    response.set_sized_body(html.len(), Cursor::new(html));
}

/// The last page of a list page that shows a page beyond it (Ignite's "Page 4 of 3"), if it does.
fn beyond_last_page(html: &str) -> Option<i64> {
    let marker = "<span>Page ";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let (page, rest) = rest.split_once(" of ")?;
    let last: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let page: i64 = page.trim().parse().ok()?;
    let last: i64 = last.parse().ok()?;
    (page > last).then_some(last)
}

/// `html` with `message` as a success notice above the content, in the markup of the theme's
/// flash message: WordPress's admin notice in the WordPress admin theme (its `.wrap`), Ignite's
/// otherwise (the start of `<main>`).
fn with_notice(html: &str, message: &str) -> String {
    let message = escape(message);
    let wordpress = "<div class=\"wrap\">";
    if let Some(at) = html.find(wordpress) {
        let at = at + wordpress.len();
        return format!(
            "{}<div class=\"notice notice-success\" role=\"status\"><p>{message}</p></div>{}",
            &html[..at],
            &html[at..]
        );
    }
    let Some(main) = html.find("<main") else {
        return html.to_owned();
    };
    let Some(at) = html[main..].find('>').map(|end| main + end + 1) else {
        return html.to_owned();
    };
    format!(
        "{}<p class=\"mb-5 rounded-md border border-primary/30 bg-lightprimary px-4 py-3 \
         text-heading\" role=\"status\">{message}</p>{}",
        &html[..at],
        &html[at..]
    )
}

/// The lost password form's address, `/mc-admin/password/forgot`.
fn forgot_path() -> String {
    format!("{}/password/forgot", super::ADMIN_PATH)
}

/// The value of `name` in the URL-encoded form `body`, if it has one.
pub(crate) fn form_value(body: &[u8], name: &str) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    body.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |text: &str| {
            RawStr::new(&text.replace('+', " "))
                .url_decode_lossy()
                .into_owned()
        };
        (decode(key) == name).then(|| decode(value))
    })
}

/// `html` with `message` under its email field (`name="email"`), which shows `email` again and
/// is marked invalid.
fn with_email_error(html: &str, email: &str, message: &str) -> String {
    let Some(at) = html.find("name=\"email\"") else {
        return html.to_owned();
    };
    let Some(start) = html[..at].rfind("<input") else {
        return html.to_owned();
    };
    let Some(end) = html[at..].find('>').map(|end| at + end + 1) else {
        return html.to_owned();
    };
    let tag = &html[start..end];
    let id = attribute(tag, "id").unwrap_or_else(|| "email".to_owned());
    let error_id = format!("{id}-error");
    let mut input = match attribute(tag, "value") {
        Some(old) => tag.replacen(
            &format!("value=\"{old}\""),
            &format!("value=\"{}\"", escape(email)),
            1,
        ),
        None => tag.replacen("<input", &format!("<input value=\"{}\"", escape(email)), 1),
    };
    input = input.replacen(
        "<input",
        &format!("<input aria-invalid=\"true\" aria-describedby=\"{error_id}\""),
        1,
    );
    format!(
        "{}{input}<p class=\"ig-error mc-field-error\" id=\"{error_id}\" role=\"alert\">{}</p>{}",
        &html[..start],
        escape(message),
        &html[end..]
    )
}

/// The value of the attribute `name` in the tag `tag`, as written.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let marker = format!(" {name}=\"");
    let start = tag.find(&marker)? + marker.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_owned())
}

/// One `<option>` of a select, as Ignite renders it: its value and label as written (escaped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectOption {
    pub(crate) value: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
}

/// `html` with the options of its select named `name` changed by `change`; `html` as it is
/// when it has no such select.
pub(crate) fn edit_select(
    html: &str,
    name: &str,
    change: impl FnOnce(&mut Vec<SelectOption>),
) -> String {
    let marker = format!("name=\"{name}\"");
    let Some(open) = html.match_indices("<select").map(|(at, _)| at).find(|at| {
        html[*at..]
            .find('>')
            .is_some_and(|end| html[*at..*at + end].contains(&marker))
    }) else {
        return html.to_owned();
    };
    let Some(body_start) = html[open..].find('>').map(|end| open + end + 1) else {
        return html.to_owned();
    };
    let Some(body_end) = html[body_start..]
        .find("</select>")
        .map(|end| body_start + end)
    else {
        return html.to_owned();
    };
    let body = &html[body_start..body_end];
    let mut options = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<option") {
        let Some(tag_end) = rest[start..].find('>').map(|end| start + end) else {
            return html.to_owned();
        };
        let Some(close) = rest[tag_end..].find("</option>").map(|end| tag_end + end) else {
            return html.to_owned();
        };
        let tag = &rest[start..tag_end];
        options.push(SelectOption {
            value: attribute(tag, "value").unwrap_or_default(),
            label: rest[tag_end + 1..close].to_owned(),
            selected: tag
                .split(|c: char| c.is_whitespace() || c == '"')
                .any(|token| token == "selected"),
        });
        rest = &rest[close + "</option>".len()..];
    }
    change(&mut options);
    let rendered: String = options
        .iter()
        .map(|o| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                o.value,
                if o.selected { " selected" } else { "" },
                o.label
            )
        })
        .collect();
    format!("{}{rendered}{}", &html[..body_start], &html[body_end..])
}

/// Category `id` and its descendants' term ids.
async fn category_tree(db: &Database, id: i64) -> Result<HashSet<i64>, Error> {
    let terms = Term::all(db, &Taxonomy::CATEGORY).await?;
    let mut tree = HashSet::from([id]);
    loop {
        let before = tree.len();
        for term in &terms {
            if term.parent_id.is_some_and(|parent| tree.contains(&parent)) {
                tree.insert(term.term_id);
            }
        }
        if tree.len() == before {
            return Ok(tree);
        }
    }
}

/// Post `id` of `post_type` and its descendants' ids.
async fn post_tree(db: &Database, post_type: &str, id: i64) -> Result<HashSet<i64>, Error> {
    let mut tree = HashSet::from([id]);
    let mut frontier = vec![id];
    // A loop in stored parents ends when no new post is found.
    while !frontier.is_empty() {
        let children = Post::query()
            .where_in("parent_id", frontier.clone())
            .where_eq("post_type", post_type)
            .get(db)
            .await?;
        frontier = children
            .into_iter()
            .map(|post| post.id)
            .filter(|child| tree.insert(*child))
            .collect();
    }
    Ok(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "<div><select class=\"ig-input\" id=\"status\" name=\"status\">\
        <option value=\"\" selected>Select an option</option>\
        <option value=\"draft\" >Draft</option><option value=\"publish\" >Published</option>\
        </select></div>";

    #[test]
    fn the_status_starts_at_draft_without_a_placeholder() {
        let html = edit_select(STATUS, "status", |options| {
            options.retain(|o| !o.value.is_empty());
            if !options.iter().any(|o| o.selected) {
                for option in options.iter_mut() {
                    option.selected = option.value == DRAFT;
                }
            }
        });
        assert_eq!(
            html,
            "<div><select class=\"ig-input\" id=\"status\" name=\"status\">\
             <option value=\"draft\" selected>Draft</option>\
             <option value=\"publish\">Published</option></select></div>"
        );
        // Another select is left alone.
        assert_eq!(edit_select(STATUS, "parent", |o| o.clear()), STATUS);
    }

    #[test]
    fn form_pages_are_read_from_the_path() {
        assert_eq!(
            form_page(Method::Get, Status::Ok, "/mc-admin/post/new"),
            Some(FormPage::New("post".into()))
        );
        assert_eq!(
            form_page(
                Method::Post,
                Status::UnprocessableEntity,
                "/mc-admin/page/7"
            ),
            Some(FormPage::Edit("page".into(), 7))
        );
        assert_eq!(
            form_page(Method::Get, Status::Ok, "/mc-admin/category/3/edit"),
            Some(FormPage::Edit("category".into(), 3))
        );
        assert_eq!(form_page(Method::Get, Status::Ok, "/mc-admin/post"), None);
        assert_eq!(
            form_page(Method::Post, Status::SeeOther, "/mc-admin/post"),
            None
        );
    }

    #[test]
    fn settings_screens_are_named_without_the_record() {
        let html = "<title>\n  Edit Reading Settings #1\n &lsaquo; Site</title>\
                    <h1 class=\"mb-6\">Edit Reading Settings #1</h1>";
        assert_eq!(
            with_screen_title(html, SettingsScreen::Reading, 1),
            "<title>\n  Reading Settings\n &lsaquo; Site</title>\
             <h1 class=\"mb-6\">Reading Settings</h1>"
        );
    }

    #[test]
    fn a_page_beyond_the_last_is_found() {
        assert_eq!(
            beyond_last_page("<span>Page 4 of 3 · 60 in total</span>"),
            Some(3)
        );
        assert_eq!(
            beyond_last_page("<span>Page 3 of 3 · 60 in total</span>"),
            None
        );
        assert_eq!(beyond_last_page("<p>No pager</p>"), None);
        assert_eq!(list_slug("/mc-admin/attachment"), Some("attachment".into()));
        assert_eq!(list_slug("/mc-admin/attachment/3/edit"), None);
        assert_eq!(list_slug("/mc-admin"), None);
    }

    #[test]
    fn the_trash_notice_is_wordpress_s() {
        assert_eq!(trashed_message("post", 1), "1 post moved to the Trash.");
        assert_eq!(trashed_message("page", 2), "2 pages moved to the Trash.");
        let html = "<div class=\"wrap\"><h1>Posts</h1></div>";
        assert_eq!(
            with_notice(html, "1 post moved to the Trash."),
            "<div class=\"wrap\"><div class=\"notice notice-success\" role=\"status\">\
             <p>1 post moved to the Trash.</p></div><h1>Posts</h1></div>"
        );
        let html = "<main class=\"mx-auto\"><h1>Posts</h1></main>";
        assert!(with_notice(html, "x").starts_with("<main class=\"mx-auto\"><p class="));
    }

    #[test]
    fn an_email_error_shows_under_the_field() {
        let page = "<form><input class=\"ig-input\" id=\"email\" name=\"email\" type=\"email\" \
                    value=\"\" autocomplete=\"username\"></form>";
        let html = with_email_error(page, "a<b", EMAIL_INVALID);
        assert!(html.contains("value=\"a&lt;b\""), "{html}");
        assert!(html.contains("aria-describedby=\"email-error\""), "{html}");
        assert!(
            html.contains(
                "<p class=\"ig-error mc-field-error\" id=\"email-error\" role=\"alert\">\
                 Please enter a valid email address.</p></form>"
            ),
            "{html}"
        );
        assert_eq!(
            form_value(b"_token=x&email=+a%40b.co+", "email").as_deref(),
            Some(" a@b.co ")
        );
        assert_eq!(form_value(b"_token=x", "email"), None);
    }
}
