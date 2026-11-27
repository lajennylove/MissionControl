//! The admin dashboard's widgets, as WordPress's dashboard (`wp-admin/includes/dashboard.php`),
//! given to Ignite's panel with `Ignite::widget` (Filament's stats overview widgets).
//!
//! - [`AtAGlance`], WordPress's "At a Glance" (`wp_dashboard_right_now`): one card per count,
//!   titled with its noun: the published Posts and Pages, the post Drafts, the Categories and
//!   Tags, the Media items (attachments not in the trash) and the Users.
//! - [`Activity`], the "Recently Published" list of WordPress's "Activity" box
//!   (`wp_dashboard_recent_posts`): the five newest published posts, one card each, titled
//!   "Recently Published", with the post's title and, under it, its date as WordPress words it
//!   ("Today, 3:04 pm", "Nov 27th, 3:04 pm", "Nov 27th 2025, 3:04 pm"), in the site's time zone.
//!   Without a published post it says "No activity yet!", as WordPress does.
//!
//! WordPress's "Quick Draft" box is a form, which a card cannot hold: the WordPress admin theme
//! shows it on its dashboard, posting to the Posts screen's create (the normal post save path).
//!
//! The time is written in WordPress's default time format (`g:i a`), not the site's
//! `time_format` option.

use chrono::{Datelike, Duration, NaiveDateTime, Timelike, Utc};
use ignite_core::{ActingUser, RuntimeFuture, Stat, StatsOverviewWidget};
use orbit_orm::{Database, Error, Value as Sql, raw};

use crate::content::{PostStatus, gmt_offset};

/// How many posts the Activity box lists (`wp_dashboard_site_activity`'s `max`).
pub const RECENT_POSTS: i64 = 5;

/// The title of each card of the Activity box: WordPress's heading of its list.
pub const RECENTLY_PUBLISHED: &str = "Recently Published";

/// WordPress's "At a Glance" box, as stat cards. See the [module](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct AtAGlance;

/// The "Recently Published" list of WordPress's "Activity" box, as stat cards. See the
/// [module](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct Activity;

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

impl StatsOverviewWidget for Activity {
    fn stats<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
    ) -> RuntimeFuture<'a, Vec<Stat>> {
        Box::pin(async move {
            let posts: Vec<(String, NaiveDateTime)> = raw::fetch_all(
                db,
                "SELECT title, published_at FROM posts WHERE post_type = ? AND status = ? \
                 AND published_at IS NOT NULL ORDER BY published_at DESC, id DESC LIMIT ?",
                &[
                    Sql::from("post"),
                    PostStatus::Publish.into(),
                    RECENT_POSTS.into(),
                ],
            )
            .await?;
            if posts.is_empty() {
                return Ok(vec![Stat::make("Activity", "No activity yet!")]);
            }
            let offset = gmt_offset(db).await?;
            let now =
                Utc::now().naive_utc() + Duration::seconds(i64::from(offset.local_minus_utc()));
            Ok(posts
                .into_iter()
                .map(|(title, published)| {
                    let local = published + Duration::seconds(i64::from(offset.local_minus_utc()));
                    let title = if title.trim().is_empty() {
                        "(no title)".to_owned()
                    } else {
                        title
                    };
                    Stat::make(RECENTLY_PUBLISHED, title).description(recent_date(local, now))
                })
                .collect())
        })
    }
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
