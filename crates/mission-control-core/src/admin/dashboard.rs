//! The admin dashboard's widgets, as WordPress's dashboard (`wp-admin/includes/dashboard.php`),
//! given to Ignite's panel: stat cards (`Ignite::widget`, Filament's stats overview widget), list
//! widgets (`Ignite::list_widget`) and a form widget (`Ignite::form_widget`), Filament's custom
//! widgets. Every admin theme shows them.
//!
//! - [`AtAGlance`], the stat cards: one card per count, titled with its noun: the published
//!   Posts and Pages, the post Drafts, the Categories and Tags, the Media items (attachments not
//!   in the trash) and the Users.
//! - [`at_a_glance_widget`], WordPress's "At a Glance" box (`wp_dashboard_right_now`): one row
//!   per count in WordPress's words ("6 Posts", "1 Page", `_n( '%s Post', '%s Posts' )`), each
//!   linked to its screen when the user may open it, as WordPress links a count only for a user
//!   with the type's `edit_posts`: Posts and Pages, then the owner's Categories, Tags, Media
//!   items and Users.
//! - [`activity_widget`], WordPress's "Activity" box (`wp_dashboard_recent_posts`, "Recently
//!   Published"): the five newest published posts, each its title, linked to its edit form when
//!   the user may edit it (`edit_post`), with its date as WordPress words it ("Today, 3:04 pm",
//!   "Nov 27th, 3:04 pm", "Nov 27th 2025, 3:04 pm"), in the site's time zone. Without a
//!   published post it says "No activity yet!", as WordPress does.
//! - [`quick_draft_widget`], WordPress's "Quick Draft" box (`wp_dashboard_quick_press`): a title
//!   and the content, saved as a draft by "Save Draft" through the Posts screen's create, the
//!   normal post save path. Ignite shows it only to a user who may create posts (`edit_posts`).
//!
//! The time is written in WordPress's default time format (`g:i a`), not the site's
//! `time_format` option.

use chrono::{Datelike, Duration, NaiveDateTime, Timelike, Utc};
use ignite_core::{
    ActingUser, FormWidget, ListWidget, ListWidgetRow, RuntimeFuture, Stat, StatsOverviewWidget,
};
use orbit_orm::{Database, Error, Value as Sql, raw};

use crate::content::{PostStatus, gmt_offset};

/// How many posts the Activity box lists (`wp_dashboard_site_activity`'s `max`).
pub const RECENT_POSTS: i64 = 5;

/// The title of WordPress's "At a Glance" box.
pub const AT_A_GLANCE: &str = "At a Glance";

/// The title of WordPress's "Activity" box.
pub const ACTIVITY: &str = "Activity";

/// What the Activity box says without a published post.
pub const NO_ACTIVITY: &str = "No activity yet!";

/// The title of WordPress's "Quick Draft" box.
pub const QUICK_DRAFT: &str = "Quick Draft";

/// The Quick Draft button's text.
pub const SAVE_DRAFT: &str = "Save Draft";

/// WordPress's "At a Glance" box, as stat cards. See the [module](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct AtAGlance;

/// The number of posts of `post_type` with `status`.
async fn count_posts(db: &Database, post_type: &str, status: PostStatus) -> Result<i64, Error> {
    raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM posts WHERE post_type = ? AND status = ?",
        &[post_type.into(), status.into()],
    )
    .await
}

/// The number of terms of `taxonomy`.
async fn count_terms(db: &Database, taxonomy: &str) -> Result<i64, Error> {
    raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM term_taxonomy WHERE taxonomy = ?",
        &[taxonomy.into()],
    )
    .await
}

impl StatsOverviewWidget for AtAGlance {
    fn stats<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
    ) -> RuntimeFuture<'a, Vec<Stat>> {
        Box::pin(async move {
            let media: i64 = raw::fetch_scalar(
                db,
                "SELECT COUNT(*) FROM posts WHERE post_type = ? AND status <> ?",
                &[Sql::from("attachment"), PostStatus::Trash.into()],
            )
            .await?;
            let users: i64 = raw::fetch_scalar(db, "SELECT COUNT(*) FROM users", &[]).await?;
            Ok(vec![
                Stat::make("Posts", count_posts(db, "post", PostStatus::Publish).await?),
                Stat::make("Pages", count_posts(db, "page", PostStatus::Publish).await?),
                Stat::make("Drafts", count_posts(db, "post", PostStatus::Draft).await?),
                Stat::make("Categories", count_terms(db, "category").await?),
                Stat::make("Tags", count_terms(db, "post_tag").await?),
                Stat::make("Media", media),
                Stat::make("Users", users),
            ])
        })
    }
}

/// The counts of At a Glance: (singular, plural, count, the screen's slug, the capability that
/// links it).
async fn glance_counts(db: &Database) -> Result<Vec<GlanceCount>, Error> {
    let media: i64 = raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM posts WHERE post_type = ? AND status <> ?",
        &[Sql::from("attachment"), PostStatus::Trash.into()],
    )
    .await?;
    let users: i64 = raw::fetch_scalar(db, "SELECT COUNT(*) FROM users", &[]).await?;
    Ok(vec![
        GlanceCount {
            one: "Post",
            many: "Posts",
            count: count_posts(db, "post", PostStatus::Publish).await?,
            slug: "post",
            cap: "edit_posts",
        },
        GlanceCount {
            one: "Page",
            many: "Pages",
            count: count_posts(db, "page", PostStatus::Publish).await?,
            slug: "page",
            cap: "edit_pages",
        },
        GlanceCount {
            one: "Category",
            many: "Categories",
            count: count_terms(db, "category").await?,
            slug: "category",
            cap: "manage_categories",
        },
        GlanceCount {
            one: "Tag",
            many: "Tags",
            count: count_terms(db, "post_tag").await?,
            slug: "post_tag",
            cap: "manage_categories",
        },
        GlanceCount {
            one: "Media item",
            many: "Media items",
            count: media,
            slug: "attachment",
            cap: "upload_files",
        },
        GlanceCount {
            one: "User",
            many: "Users",
            count: users,
            slug: "users",
            cap: "list_users",
        },
    ])
}

/// One count of At a Glance.
struct GlanceCount {
    one: &'static str,
    many: &'static str,
    count: i64,
    slug: &'static str,
    cap: &'static str,
}

/// A count in WordPress's words: `_n( '%s Post', '%s Posts', $count )`.
pub fn glance_label(count: i64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// WordPress's "At a Glance" box as a list widget, its rows linking under `admin` (the panel's
/// mount, such as `/mc-admin`). See the [module](self).
pub fn at_a_glance_widget(admin: &str) -> ListWidget {
    let admin = admin.to_owned();
    ListWidget::make(AT_A_GLANCE).rows(move |db, user| {
        let admin = admin.clone();
        Box::pin(async move {
            Ok(glance_counts(db)
                .await?
                .into_iter()
                .map(|glance| {
                    let row =
                        ListWidgetRow::make(glance_label(glance.count, glance.one, glance.many));
                    if user.can(glance.cap) {
                        row.url(format!("{admin}/{}", glance.slug))
                    } else {
                        row
                    }
                })
                .collect())
        })
    })
}

/// WordPress's "Activity" box ("Recently Published") as a list widget, each post linking to its
/// edit form under `admin` when the user may edit it. See the [module](self).
pub fn activity_widget(admin: &str) -> ListWidget {
    let admin = admin.to_owned();
    ListWidget::make(ACTIVITY)
        .empty_state(NO_ACTIVITY)
        .rows(move |db, user| {
            let admin = admin.clone();
            Box::pin(async move {
                let posts: Vec<(i64, String, NaiveDateTime, i64)> = raw::fetch_all(
                    db,
                    "SELECT id, title, published_at, author_id FROM posts WHERE post_type = ? \
                     AND status = ? AND published_at IS NOT NULL \
                     ORDER BY published_at DESC, id DESC LIMIT ?",
                    &[
                        Sql::from("post"),
                        PostStatus::Publish.into(),
                        RECENT_POSTS.into(),
                    ],
                )
                .await?;
                if posts.is_empty() {
                    return Ok(Vec::new());
                }
                let offset = gmt_offset(db).await?;
                let shift = Duration::seconds(i64::from(offset.local_minus_utc()));
                let now = Utc::now().naive_utc() + shift;
                Ok(posts
                    .into_iter()
                    .map(|(id, title, published, author)| {
                        let title = if title.trim().is_empty() {
                            "(no title)".to_owned()
                        } else {
                            title
                        };
                        let row =
                            ListWidgetRow::make(title).value(recent_date(published + shift, now));
                        if may_edit_published(user, author) {
                            row.url(format!("{admin}/post/{id}/edit"))
                        } else {
                            row
                        }
                    })
                    .collect())
            })
        })
}

/// Whether `user` may edit a published post by `author`, as `map_meta_cap( 'edit_post' )` maps
/// it: `edit_published_posts`, and `edit_others_posts` for another's post.
fn may_edit_published(user: &ActingUser, author: i64) -> bool {
    user.can("edit_published_posts") && (author == user.id() || user.can("edit_others_posts"))
}

/// WordPress's "Quick Draft" box as a form widget on the Posts screen (`post`): the title and
/// the content, with the status `draft`, and "Save Draft". See the [module](self).
pub fn quick_draft_widget() -> FormWidget {
    FormWidget::make(QUICK_DRAFT, "post")
        .fields(["title", "content"])
        .hidden("status", PostStatus::Draft.as_str())
        .submit_label(SAVE_DRAFT)
}

/// The date of a post published at `local` (the site's time) as WordPress's Activity box writes
/// it, `now` being the site's time now: "Today", "Tomorrow" (a scheduled post), "Nov 27th" this
/// year, "Nov 27th 2025" another year, then ", " and the time ("3:04 pm").
pub fn recent_date(local: NaiveDateTime, now: NaiveDateTime) -> String {
    let date = if local.date() == now.date() {
        "Today".to_owned()
    } else if Some(local.date()) == now.date().succ_opt() {
        "Tomorrow".to_owned()
    } else if local.year() != now.year() {
        format!(
            "{} {}{} {}",
            local.format("%b"),
            local.day(),
            ordinal(local.day()),
            local.year()
        )
    } else {
        format!(
            "{} {}{}",
            local.format("%b"),
            local.day(),
            ordinal(local.day())
        )
    };
    let (pm, hour) = local.hour12();
    format!(
        "{date}, {hour}:{:02} {}",
        local.minute(),
        if pm { "pm" } else { "am" }
    )
}

/// The English ordinal suffix of `day`, as PHP's `S` format: "st", "nd", "rd" or "th".
fn ordinal(day: u32) -> &'static str {
    match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn counts_are_worded_as_wordpress_at_a_glance() {
        assert_eq!(glance_label(1, "Post", "Posts"), "1 Post");
        assert_eq!(glance_label(0, "Post", "Posts"), "0 Posts");
        assert_eq!(glance_label(6, "Category", "Categories"), "6 Categories");
    }

    #[test]
    fn quick_draft_saves_a_draft_of_a_post() {
        let widget = quick_draft_widget();
        assert_eq!(widget.get_title(), "Quick Draft");
        assert_eq!(widget.get_resource(), "post");
        assert_eq!(widget.get_fields(), ["title", "content"]);
        assert_eq!(
            widget.get_hidden(),
            [("status".to_owned(), "draft".to_owned())]
        );
        assert_eq!(widget.get_submit_label(), "Save Draft");
    }

    #[test]
    fn dates_are_worded_as_wordpress_activity_box() {
        let now = at("2026-11-27 18:00");
        assert_eq!(recent_date(at("2026-11-27 15:04"), now), "Today, 3:04 pm");
        assert_eq!(
            recent_date(at("2026-11-28 00:05"), now),
            "Tomorrow, 12:05 am"
        );
        assert_eq!(
            recent_date(at("2026-11-21 09:30"), now),
            "Nov 21st, 9:30 am"
        );
        assert_eq!(
            recent_date(at("2026-03-12 12:00"), now),
            "Mar 12th, 12:00 pm"
        );
        assert_eq!(
            recent_date(at("2025-11-02 23:59"), now),
            "Nov 2nd 2025, 11:59 pm"
        );
        assert_eq!(
            recent_date(at("2026-11-23 10:00"), now),
            "Nov 23rd, 10:00 am"
        );
    }
}
