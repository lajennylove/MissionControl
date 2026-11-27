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
//! The rewrites are on the HTML Ignite answers, as the media library's and the menu builder's
//! views are; a form without the field is left as it is.

use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;

use mission_control_core::admin::CATEGORY_SCREEN;
use mission_control_core::content::{Post, Taxonomy, Term, is_email};
use mission_control_core::types::PostTypeResources;
use orbit_orm::{Database, Error, Model};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Method, RawStr, Status};
use rocket::{Data, Request, Response};

use super::ADMIN_SEGMENT;
use super::media::escape;

/// The status a new post's dropdown shows, as WordPress's New screen.
const DRAFT: &str = "draft";

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
        if response.content_type() != Some(ContentType::HTML) {
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
        let post_type = rocket
            .state::<Arc<PostTypeResources>>()
            .and_then(|resources| resources.post_type(slug));
        let category = slug == CATEGORY_SCREEN;
        if post_type.is_none() && !category {
            return;
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
        let Ok(mut html) = response.body_mut().to_string().await else {
            return;
        };
        if post_type.is_some() {
            html = edit_select(&html, "status", |options| {
                options.retain(|o| !o.value.is_empty());
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

/// The lost password form's address, `/mc-admin/password/forgot`.
fn forgot_path() -> String {
    format!("{}/password/forgot", super::ADMIN_PATH)
}

/// The value of `name` in the URL-encoded form `body`, if it has one.
fn form_value(body: &[u8], name: &str) -> Option<String> {
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
struct SelectOption {
    value: String,
    label: String,
    selected: bool,
}

/// `html` with the options of its select named `name` changed by `change`; `html` as it is
/// when it has no such select.
fn edit_select(html: &str, name: &str, change: impl FnOnce(&mut Vec<SelectOption>)) -> String {
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
