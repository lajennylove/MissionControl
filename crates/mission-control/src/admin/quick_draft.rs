//! The dashboard's Quick Draft box (`wp_dashboard_quick_press`), around Ignite 0.6.7's form
//! widget (see `mission_control_core::admin::dashboard::quick_draft_widget`), which posts to the
//! Posts screen's create, the normal post save path, and comes back to the dashboard. Ignite's
//! widget shows neither a refusal nor what was saved: a refused draft (an empty title) showed
//! the whole New Post page, and a saved one only Ignite's "Record created." above the
//! dashboard. [`QuickDraft`] keeps both in the box, as WordPress's Quick Draft does:
//!
//! - **A refused draft** comes back to the dashboard (303), with the refusal ("Title is
//!   required.") in the box and the title and content typed, as Laravel's
//!   `back()->withErrors()->withInput()` flashes them, in place of Ignite's dashboard shown again
//!   with the messages under the widget's fields (422).
//! - **A saved draft** comes back with "Draft saved." in the box, in place of Ignite's "Record
//!   created.".
//! - **Your Recent Drafts** is listed under the form (`wp_dashboard_recent_drafts`, see
//!   `mission_control_core::admin::dashboard::recent_drafts`): the user's own newest drafts,
//!   each its title linked to its edit form and its date, with "View all drafts" when there are
//!   more. A theme view that lists the drafts itself (`.drafts`) is left as it is.
//!
//! WordPress's Activity box lists published posts only ("Recently Published"), so a draft is not
//! there, as in WordPress.
//!
//! The flashed values are kept in memory by sign-in session until the dashboard shows them (at
//! most [`super::profile::NOTICE_TTL`]), as the profile page's are.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use ignite_panel::AdminUser;
use mission_control_core::admin::dashboard::{
    DRAFT_SAVED, QUICK_DRAFT, RECENT_DRAFTS, RecentDraft, YOUR_RECENT_DRAFTS, recent_drafts,
};
use orbit_orm::Database;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Header, Method, Status};
use rocket::request::Outcome;
use rocket::{Data, Request, Response};

use super::forms::form_value;
use super::profile::{NOTICE_TTL, session_key};
use super::{ADMIN_PATH, ADMIN_SEGMENT};

/// The most bytes of a Quick Draft read to find its fields: the token, the way back, the status
/// and the title come first, so they are found in a longer draft too; its content is kept for a
/// refused draft when it fits.
const PEEK: usize = 64 * 1024;

/// Ignite's flash message cookie, dropped in place of "Record created.".
const FLASH_COOKIE: &str = "ignite_flash";

/// What a Quick Draft sent, read before Ignite's create answers it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Sent {
    title: String,
    /// `None` when the content did not fit in [`PEEK`].
    content: Option<String>,
}

/// The Quick Draft of the request, when it is one.
#[derive(Default)]
struct PendingDraft(OnceLock<Sent>);

/// What the box shows once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Notice {
    /// "Draft saved.", or the refusal's messages (already escaped, as Ignite rendered them).
    saved: bool,
    errors: Vec<String>,
    /// The title and content typed, for a refused draft.
    sent: Option<Sent>,
}

/// The notices waiting for the dashboard, by sign-in session.
#[derive(Default)]
pub(crate) struct QuickDraftNotices(Mutex<HashMap<String, (Instant, Notice)>>);

impl QuickDraftNotices {
    fn put(&self, key: String, notice: Notice) {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, (at, _)| at.elapsed() < NOTICE_TTL);
        map.insert(key, (Instant::now(), notice));
    }

    fn take(&self, key: &str) -> Notice {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match map.remove(key) {
            Some((at, notice)) if at.elapsed() < NOTICE_TTL => notice,
            _ => Notice::default(),
        }
    }
}

/// Keeps Quick Draft's answer in its box; see the [module](self).
pub(crate) struct QuickDraft;

/// Whether `path` is the dashboard's (`/mc-admin`).
fn is_dashboard(path: &str) -> bool {
    path.trim_end_matches('/') == ADMIN_PATH
}

/// Whether `path` is the Posts screen's create (`/mc-admin/post`), where Quick Draft posts.
fn is_posts_create(path: &str) -> bool {
    let mut segments = path.trim_start_matches('/').split('/');
    segments.next() == Some(ADMIN_SEGMENT)
        && segments.next() == Some("post")
        && segments.next().is_none()
}

#[rocket::async_trait]
impl Fairing for QuickDraft {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control Quick Draft",
            kind: Kind::Request | Kind::Response,
        }
    }

    async fn on_request(&self, request: &mut Request<'_>, data: &mut Data<'_>) {
        if request.method() != Method::Post || !is_posts_create(request.uri().path().as_str()) {
            return;
        }
        let body = data.peek(PEEK).await.to_vec();
        let complete = data.peek_complete();
        // Ignite's form widget sends the way back to the dashboard (`_back`); the New screen
        // does not.
        if form_value(&body, "_back").is_none_or(|back| !is_dashboard(&back)) {
            return;
        }
        let sent = Sent {
            title: form_value(&body, "title").unwrap_or_default(),
            content: complete.then(|| form_value(&body, "content").unwrap_or_default()),
        };
        let _ = request.local_cache(PendingDraft::default).0.set(sent);
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        match request.method() {
            Method::Post => after_quick_draft(request, response).await,
            Method::Get if is_dashboard(request.uri().path().as_str()) => {
                dashboard(request, response).await;
            }
            _ => {}
        }
    }
}

/// After Ignite's create answered a Quick Draft: back to the dashboard, with "Draft saved." or
/// the refusal and the values typed kept for the box.
async fn after_quick_draft<'r>(request: &'r Request<'_>, response: &mut Response<'r>) {
    let Some(sent) = request.local_cache(PendingDraft::default).0.get() else {
        return;
    };
    let status = response.status();
    if status != Status::SeeOther && status != Status::UnprocessableEntity {
        return;
    }
    let Outcome::Success(user) = request.guard::<AdminUser>().await else {
        return;
    };
    let Some(notices) = request.rocket().state::<QuickDraftNotices>() else {
        return;
    };
    let notice = if status == Status::SeeOther {
        // Ignite's "Record created." goes: the box says "Draft saved.".
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
        Notice {
            saved: true,
            ..Notice::default()
        }
    } else {
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        let mut errors = field_errors(&html);
        if errors.is_empty() {
            errors.push("The draft could not be saved.".to_owned());
        }
        response.set_status(Status::SeeOther);
        response.remove_header("Content-Type");
        response.set_sized_body(0, Cursor::new(String::new()));
        Notice {
            saved: false,
            errors,
            sent: Some(sent.clone()),
        }
    };
    response.set_raw_header("Location", ADMIN_PATH);
    notices.put(session_key(request.cookies(), &user), notice);
}

/// The field errors of Ignite's form page `html`, as it rendered them (escaped), in order and
/// once each.
fn field_errors(html: &str) -> Vec<String> {
    const MARKER: &str = "text-error-emphasis\">";
    let mut errors: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(MARKER) {
        rest = &rest[at + MARKER.len()..];
        let Some(end) = rest.find("</p>") else {
            break;
        };
        let text = strip_tags(&rest[..end]).trim().to_owned();
        if !text.is_empty() && !errors.contains(&text) {
            errors.push(text);
        }
        rest = &rest[end..];
    }
    errors
}

/// `html` without its tags.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The dashboard: Quick Draft's notice, the values a refused draft held, and Your Recent Drafts
/// in its box.
async fn dashboard<'r>(request: &'r Request<'_>, response: &mut Response<'r>) {
    if response.status() != Status::Ok || response.content_type() != Some(ContentType::HTML) {
        return;
    }
    let Outcome::Success(user) = request.guard::<AdminUser>().await else {
        return;
    };
    let rocket = request.rocket();
    let (Some(db), Some(notices)) = (
        rocket.state::<Arc<Database>>(),
        rocket.state::<QuickDraftNotices>(),
    ) else {
        return;
    };
    let Ok(html) = response.body_mut().to_string().await else {
        return;
    };
    let notice = notices.take(&session_key(request.cookies(), &user));
    let drafts = match recent_drafts(db, user.user_id()).await {
        Ok(drafts) => drafts,
        Err(e) => {
            log::error!(
                "Could not read the recent drafts of user {}: {e}",
                user.user_id()
            );
            Vec::new()
        }
    };
    let html = with_quick_draft(&html, &notice, &drafts).unwrap_or(html);
    response.set_sized_body(html.len(), Cursor::new(html));
}

/// Text escaped as the admin's views escape it.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// The dashboard `html` with `notice` and `drafts` in the Quick Draft box, or `None` when the
/// page has no Quick Draft box (a user who may not create posts).
fn with_quick_draft(html: &str, notice: &Notice, drafts: &[RecentDraft]) -> Option<String> {
    let heading = html.find(&format!(">{QUICK_DRAFT}</h2>"))?;
    let form_start = heading + html[heading..].find("<form")?;
    let form_end = form_start + html[form_start..].find("</form>")? + "</form>".len();
    let wordpress = html.contains("mc-dashboard-form");
    let mut form = html[form_start..form_end].to_owned();
    if let Some(sent) = &notice.sent {
        form = with_input_value(&form, "title", &sent.title);
        if let Some(content) = &sent.content {
            form = with_textarea_value(&form, "content", content);
        }
    }
    let message = notice_html(notice, wordpress);
    // A theme view that lists the drafts itself keeps its list.
    let list = if html.contains("<div class=\"drafts\">") {
        String::new()
    } else {
        drafts_html(drafts, wordpress)
    };
    Some(format!(
        "{}{message}{form}{list}{}",
        &html[..form_start],
        &html[form_end..]
    ))
}

/// The box's message: "Draft saved." or the refusal, in the theme's notice.
fn notice_html(notice: &Notice, wordpress: bool) -> String {
    let (lines, error) = if notice.saved {
        (vec![escape(DRAFT_SAVED)], false)
    } else if notice.errors.is_empty() {
        return String::new();
    } else {
        // Ignite rendered the messages escaped.
        (notice.errors.clone(), true)
    };
    let text = lines.join(" ");
    match (wordpress, error) {
        (true, false) => {
            format!(
                "<div class=\"notice notice-success inline\" role=\"status\"><p>{text}</p></div>"
            )
        }
        (true, true) => {
            format!("<div class=\"notice notice-error inline\" role=\"alert\"><p>{text}</p></div>")
        }
        (false, false) => format!(
            "<p class=\"mb-5 rounded-md border border-primary/30 bg-lightprimary px-4 py-3 \
             text-heading\" role=\"status\">{text}</p>"
        ),
        (false, true) => format!(
            "<p class=\"mb-5 rounded-md border border-error bg-lighterror px-4 py-3 text-sm \
             text-error-emphasis\" role=\"alert\">{text}</p>"
        ),
    }
}

/// "Your Recent Drafts" (`wp_dashboard_recent_drafts`): "View all drafts" when there are more
/// than [`RECENT_DRAFTS`], then each draft's title, linked to its edit form, its date and the
/// start of its content. Nothing without drafts.
fn drafts_html(drafts: &[RecentDraft], wordpress: bool) -> String {
    if drafts.is_empty() {
        return String::new();
    }
    let view_all = if drafts.len() > RECENT_DRAFTS {
        format!(
            "<p class=\"view-all\"><a href=\"{ADMIN_PATH}/post?tab=draft\">View all drafts</a></p>"
        )
    } else {
        String::new()
    };
    let items: String = drafts
        .iter()
        .take(RECENT_DRAFTS)
        .map(|draft| {
            let title = escape(&draft.title);
            let excerpt = if draft.excerpt.is_empty() {
                String::new()
            } else {
                format!("<p>{}</p>", escape(&draft.excerpt))
            };
            format!(
                "<li><div class=\"draft-title\"><a href=\"{ADMIN_PATH}/post/{id}/edit\" \
                 aria-label=\"Edit \u{201c}{title}\u{201d}\">{title}</a> <span><time \
                 datetime=\"{datetime}\">{date}</time></span></div>{excerpt}</li>",
                id = draft.id,
                datetime = escape(&draft.datetime),
                date = escape(&draft.date),
            )
        })
        .collect();
    if wordpress {
        format!(
            "<div class=\"drafts\">{view_all}<h2>{YOUR_RECENT_DRAFTS}</h2><ul>{items}</ul></div>"
        )
    } else {
        format!(
            "<div class=\"drafts mt-5 grid gap-2 border-t border-border pt-4\">{view_all}<h3 \
             class=\"text-sm font-semibold text-heading\">{YOUR_RECENT_DRAFTS}</h3><ul \
             class=\"grid gap-2\">{items}</ul></div>"
        )
    }
}

/// `form` with `value` in its input named `name`.
fn with_input_value(form: &str, name: &str, value: &str) -> String {
    let marker = format!("name=\"{name}\"");
    let Some(at) = form.find(&marker) else {
        return form.to_owned();
    };
    let Some(start) = form[..at].rfind("<input") else {
        return form.to_owned();
    };
    let Some(end) = form[at..].find('>').map(|end| at + end) else {
        return form.to_owned();
    };
    let tag = &form[start..end];
    let tag = match tag.find(" value=\"") {
        Some(v) => {
            let open = v + " value=\"".len();
            let close = tag[open..].find('"').map_or(tag.len(), |c| open + c);
            format!("{}{}{}", &tag[..open], escape(value), &tag[close..])
        }
        None => format!("{tag} value=\"{}\"", escape(value)),
    };
    format!("{}{tag}{}", &form[..start], &form[end..])
}

/// `form` with `value` in its textarea named `name`.
fn with_textarea_value(form: &str, name: &str, value: &str) -> String {
    let marker = format!("name=\"{name}\"");
    let Some(at) = form.find(&marker) else {
        return form.to_owned();
    };
    if form[..at]
        .rfind("<textarea")
        .is_none_or(|t| form[t..at].contains('>'))
    {
        return form.to_owned();
    }
    let Some(open) = form[at..].find('>').map(|end| at + end + 1) else {
        return form.to_owned();
    };
    let Some(close) = form[open..].find("</textarea>").map(|end| open + end) else {
        return form.to_owned();
    };
    format!("{}{}{}", &form[..open], escape(value), &form[close..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const IGNITE: &str = "<main><section class=\"ig-card\"><h2 class=\"mb-5\" id=\"w2\">Quick \
        Draft</h2>\n  <form class=\"grid gap-5\" method=\"post\" action=\"/mc-admin/post\">\
        <input class=\"ig-input\" id=\"title\" name=\"title\" type=\"text\" value=\"\">\
        <textarea class=\"ig-input\" id=\"content\" name=\"content\"></textarea></form>\
        </section></main>";

    fn draft(id: i64, title: &str) -> RecentDraft {
        RecentDraft {
            id,
            title: title.into(),
            date: "November 27, 2026".into(),
            datetime: "2026-11-27T10:00:00+00:00".into(),
            excerpt: String::new(),
        }
    }

    #[test]
    fn a_refusal_shows_in_the_box_with_the_values_typed() {
        let html = IGNITE;
        let notice = Notice {
            saved: false,
            errors: vec!["Title is required.".into()],
            sent: Some(Sent {
                title: "a \"b\"".into(),
                content: Some("<x>".into()),
            }),
        };
        let out = with_quick_draft(html, &notice, &[]).expect("a box");
        assert!(
            out.contains("role=\"alert\">Title is required.</p><form"),
            "{out}"
        );
        assert!(out.contains("value=\"a &quot;b&quot;\""), "{out}");
        assert!(
            out.contains("name=\"content\">&lt;x&gt;</textarea>"),
            "{out}"
        );
    }

    #[test]
    fn a_saved_draft_and_the_recent_drafts_show_in_the_box() {
        let html = IGNITE;
        let notice = Notice {
            saved: true,
            ..Notice::default()
        };
        let drafts = [draft(4, "D"), draft(3, "C"), draft(2, "B"), draft(1, "A")];
        let out = with_quick_draft(html, &notice, &drafts).expect("a box");
        assert!(out.contains(">Draft saved.</p><form"), "{out}");
        assert!(out.contains(">Your Recent Drafts</h3>"), "{out}");
        assert!(out.contains("<a href=\"/mc-admin/post/4/edit\""), "{out}");
        assert!(!out.contains("/mc-admin/post/1/edit"), "{out}");
        assert!(out.contains(">View all drafts</a>"), "{out}");
        assert!(out.contains("November 27, 2026"), "{out}");
        // No box, no change.
        assert_eq!(with_quick_draft("<main></main>", &notice, &drafts), None);
    }

    #[test]
    fn ignites_field_errors_are_read() {
        let html = "<p class=\"mt-1.5 mb-0 text-sm text-error-emphasis\">Title is required.</p>\
                    <p class=\"text-error-emphasis\">Title is required.</p>";
        assert_eq!(field_errors(html), ["Title is required."]);
        assert!(is_posts_create("/mc-admin/post"));
        assert!(!is_posts_create("/mc-admin/post/3"));
        assert!(is_dashboard("/mc-admin/") && is_dashboard("/mc-admin"));
    }
}
