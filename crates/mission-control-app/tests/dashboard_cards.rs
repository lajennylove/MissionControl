//! The admin dashboard, as WordPress's (`wp-admin/includes/dashboard.php`): the At a Glance
//! cards, then WordPress's "At a Glance" box (`wp_dashboard_right_now`: the counts in its words,
//! linked to their screens), its "Activity" box (`wp_dashboard_recent_posts`: the five newest
//! published posts with their dates) and its "Quick Draft" box (`wp_dashboard_quick_press`: a
//! title and the content saved as a draft through the Posts screen's create), as Ignite's list
//! and form widgets. The WordPress admin theme shows the same widgets in WordPress's dashboard
//! boxes, without the stat cards.
//!
//! The flows run on MySQL or MariaDB only (see `admin_support`).

mod admin_support;

use admin_support::{
    choose_theme, get, location, new_post, post, run, sign_in, sign_out, token_in, user,
};
use mission_control_core::admin::dashboard::recent_date;
use mission_control_core::content::{Post, PostStatus};
use orbit_orm::{Database, Direction, Model, Value as Sql, raw};
use rocket::http::Status;

/// The text between `open` and the next `</p>`.
fn text_after<'h>(html: &'h str, open: &str) -> Option<&'h str> {
    let start = html.find(open)? + open.len();
    let end = start + html[start..].find("</p>")?;
    Some(&html[start..end])
}

/// The dashboard's stat cards in the Ignite theme: (title, value).
fn ignite_cards(html: &str) -> Vec<(String, String)> {
    html.split("<div class=\"ig-card ig-tone-")
        .skip(1)
        .filter_map(|card| {
            let card = &card[..card.find("</div>").unwrap_or(card.len())];
            Some((
                text_after(card, "<p class=\"m-0 font-semibold\">")?.to_owned(),
                text_after(card, "<p class=\"m-0 text-2xl font-semibold\">")?.to_owned(),
            ))
        })
        .collect()
}

/// One row of a list widget: (label, link, value).
type WidgetRow = (String, Option<String>, Option<String>);

/// The widget titled `title` in the Ignite theme: its markup, from its heading to its end.
fn widget<'h>(html: &'h str, title: &str) -> Option<&'h str> {
    let heading = format!("\">{title}</h2>");
    let start = html.find(&heading)?;
    let end = start + html[start..].find("</section>")?;
    Some(&html[start..end])
}

/// The rows of the list widget titled `title`.
fn widget_rows(html: &str, title: &str) -> Vec<WidgetRow> {
    let Some(widget) = widget(html, title) else {
        return Vec::new();
    };
    widget
        .split("<li class=")
        .skip(1)
        .map(|row| {
            let row = &row[..row.find("</li>").unwrap_or(row.len())];
            let open = "<span class=\"min-w-0\">";
            let label_start = row.find(open).unwrap() + open.len();
            let label_end = label_start + row[label_start..].find("</span>").unwrap();
            let label = &row[label_start..label_end];
            let (label, link) = match label.strip_prefix("<a href=\"") {
                Some(rest) => {
                    let (href, rest) = rest.split_once("\">").unwrap();
                    (
                        rest.trim_end_matches("</a>").to_owned(),
                        Some(href.to_owned()),
                    )
                }
                None => (label.to_owned(), None),
            };
            let value = row
                .split_once("<span class=\"shrink-0 text-sm\">")
                .map(|(_, rest)| rest[..rest.find("</span>").unwrap()].to_owned());
            (label, link, value)
        })
        .collect()
}

/// The WordPress theme's dashboard box titled `title`: its markup, from its heading to the end
/// of its content.
fn wp_box<'h>(html: &'h str, title: &str) -> Option<&'h str> {
    let heading = format!("<h2 class=\"hndle\">{title}</h2>");
    let start = html.find(&heading)?;
    let end = start + html[start..].find("</div>\n</div>")?;
    Some(&html[start..end])
}

async fn count(db: &Database, sql: &str, binds: &[Sql]) -> i64 {
    raw::fetch_scalar(db, sql, binds).await.unwrap()
}

#[test]
fn the_dashboard_has_wordpresss_cards() {
    run("dashboard_cards", |client, db| async move {
        let (admin_id, admin) = user(&db, "ada", "administrator").await;
        let (_, author) = user(&db, "aubrey", "author").await;
        let (_, subscriber) = user(&db, "sue", "subscriber").await;

        sign_in(&client, &admin).await;
        // Without a published post, Activity says so, as WordPress does.
        let (_, dashboard) = get(&client, "/mc-admin").await;
        let activity = widget(&dashboard, "Activity").expect("the Activity box");
        assert!(activity.contains("No activity yet!"), "{activity}");
        for n in 1..=6 {
            new_post(
                &client,
                "post",
                &[("title", &format!("Story {n}")), ("status", "publish")],
            )
            .await;
        }
        new_post(&client, "post", &[("title", "Not yet")]).await;
        new_post(
            &client,
            "page",
            &[("title", "About"), ("status", "publish")],
        )
        .await;

        // The At a Glance cards stay, first.
        let (status, dashboard) = get(&client, "/mc-admin").await;
        assert_eq!(status, Status::Ok, "{dashboard}");
        let terms = "SELECT COUNT(*) FROM term_taxonomy WHERE taxonomy = ?";
        let categories = count(&db, terms, &["category".into()]).await;
        let tags = count(&db, terms, &["post_tag".into()]).await;
        let media = count(
            &db,
            "SELECT COUNT(*) FROM posts WHERE post_type = 'attachment' AND status <> 'trash'",
            &[],
        )
        .await;
        let users = count(&db, "SELECT COUNT(*) FROM users", &[]).await;
        let cards = ignite_cards(&dashboard);
        let expected: Vec<(String, String)> = [
            ("Posts", 6),
            ("Pages", 1),
            ("Drafts", 1),
            ("Categories", categories),
            ("Tags", tags),
            ("Media", media),
            ("Users", users),
        ]
        .into_iter()
        .map(|(label, n)| (label.to_owned(), n.to_string()))
        .collect();
        assert_eq!(cards, expected, "{dashboard}");

        // At a Glance, in WordPress's words, each count linked to its screen.
        let plural =
            |n: i64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let glance = widget_rows(&dashboard, "At a Glance");
        let linked = |label: String, slug: &str| (label, Some(format!("/mc-admin/{slug}")), None);
        assert_eq!(
            glance,
            vec![
                linked("6 Posts".to_owned(), "post"),
                linked("1 Page".to_owned(), "page"),
                linked(plural(categories, "Category", "Categories"), "category"),
                linked(plural(tags, "Tag", "Tags"), "post_tag"),
                linked(plural(media, "Media item", "Media items"), "attachment"),
                linked(plural(users, "User", "Users"), "users"),
            ],
            "{dashboard}"
        );

        // Activity: the five newest published posts, linked to their edit forms, with their
        // dates as WordPress words them.
        let newest: Vec<Post> = Post::query()
            .where_eq("post_type", "post")
            .where_eq("status", PostStatus::Publish.as_str())
            .order_by("published_at", Direction::Desc)
            .order_by("id", Direction::Desc)
            .get(&*db)
            .await
            .unwrap()
            .into_iter()
            .take(5)
            .collect();
        let now = chrono::Utc::now().naive_utc();
        let activity: Vec<WidgetRow> = newest
            .iter()
            .map(|post| {
                (
                    post.title.clone(),
                    Some(format!("/mc-admin/post/{}/edit", post.id)),
                    Some(recent_date(post.published_at.unwrap(), now)),
                )
            })
            .collect();
        assert_eq!(widget_rows(&dashboard, "Activity"), activity, "{dashboard}");
        assert_eq!(activity.len(), 5);
        assert_eq!(activity[0].0, "Story 6");
        assert!(activity[0].2.as_deref().unwrap().starts_with("Today, "));

        // Quick Draft: the title and the content, sent as a draft, back to the dashboard.
        let quick = widget(&dashboard, "Quick Draft").expect("the Quick Draft box");
        for expected in [
            "action=\"/mc-admin/post\"",
            "name=\"_back\" value=\"/mc-admin\"",
            "name=\"status\" value=\"draft\"",
            "name=\"title\"",
            "name=\"content\"",
            ">Save Draft</button>",
        ] {
            assert!(quick.contains(expected), "`{expected}` in {quick}");
        }
        let csrf = token_in(&dashboard);
        let response = post(
            &client,
            "/mc-admin/post",
            &[
                ("_token", &csrf),
                ("_back", "/mc-admin"),
                ("status", "draft"),
                ("title", "A quick idea"),
                ("content", "Written on the dashboard."),
            ],
        )
        .await;
        assert_eq!(response.status(), Status::SeeOther);
        assert_eq!(location(&response), "/mc-admin");
        drop(response);
        let draft = Post::query()
            .where_eq("title", "A quick idea")
            .first(&*db)
            .await
            .unwrap()
            .expect("the draft is saved");
        assert_eq!(draft.status, PostStatus::Draft);
        assert_eq!(draft.author_id, admin_id);
        assert!(draft.content.contains("Written on the dashboard."));
        let (_, dashboard) = get(&client, "/mc-admin").await;
        let drafts = ignite_cards(&dashboard)
            .into_iter()
            .find(|(label, _)| label == "Drafts")
            .unwrap();
        assert_eq!(drafts.1, "2", "{dashboard}");

        // The WordPress theme shows the same widgets in WordPress's dashboard boxes: At a Glance
        // and Activity in the first column, Quick Draft in the second, and no stat cards.
        choose_theme(&client, "wordpress").await;
        let (status, wordpress) = get(&client, "/mc-admin").await;
        assert_eq!(status, Status::Ok, "{wordpress}");
        let glance = wp_box(&wordpress, "At a Glance").expect("the At a Glance box");
        assert!(
            glance.contains("<li><a href=\"/mc-admin/post\">6 Posts</a></li>"),
            "{glance}"
        );
        assert!(
            glance.contains("<li><a href=\"/mc-admin/page\">1 Page</a></li>"),
            "{glance}"
        );
        assert!(
            glance.contains(&format!(
                "<li><a href=\"/mc-admin/users\">{}</a></li>",
                plural(users, "User", "Users")
            )),
            "{glance}"
        );
        let recent = wp_box(&wordpress, "Activity").expect("the Activity box");
        let (title, link, date) = &activity[0];
        assert!(
            recent.contains(&format!(
                "<li><span>{}</span> <a href=\"{}\">{title}</a></li>",
                date.as_deref().unwrap(),
                link.as_deref().unwrap()
            )),
            "{recent}"
        );
        assert_eq!(recent.matches("<li>").count(), 5, "{recent}");
        let quick = wp_box(&wordpress, "Quick Draft").expect("the Quick Draft box");
        for expected in [
            "action=\"/mc-admin/post\"",
            "name=\"_back\" value=\"/mc-admin\"",
            "name=\"status\" value=\"draft\"",
            "<input type=\"text\" name=\"title\" id=\"title\"",
            "<textarea name=\"content\" id=\"content\" placeholder=\"What&#8217;s on your mind?\"",
            "value=\"Save Draft\"",
        ] {
            assert!(quick.contains(expected), "`{expected}` in {quick}");
        }
        // Quick Draft in WordPress's postbox, with its header and inside; its recent drafts,
        // when the page has them, show under the form.
        assert!(
            quick.starts_with("<h2 class=\"hndle\">Quick Draft</h2></div>\n<div class=\"inside\">"),
            "{quick}"
        );
        if let Some(drafts) = quick.find("class=\"drafts\"") {
            assert!(quick.find("</form>").unwrap() < drafts, "{quick}");
        }
        // At a Glance's rows have WordPress's Dashicons, chosen by the screen each one links to.
        for expected in [
            "#dashboard-widgets .main li:not(:has(> span))::before { color: #646970; content: \"\\f159\"; display: inline-block; font: normal 20px/1 dashicons;",
            "#dashboard-widgets .main li:has(> a[href$=\"/post\"])::before { content: \"\\f109\"; }",
            "#dashboard-widgets .main li:has(> a[href$=\"/page\"])::before { content: \"\\f105\"; }",
            "#dashboard-widgets .main li:has(> a[href$=\"/users\"])::before { content: \"\\f110\"; }",
        ] {
            assert!(wordpress.contains(expected), "`{expected}` in {wordpress}");
        }
        let first = wordpress.find("id=\"postbox-container-1\"").unwrap();
        let second = wordpress.find("id=\"postbox-container-2\"").unwrap();
        let at = |title: &str| wordpress.find(&format!(">{title}</h2>")).unwrap();
        assert!(
            first < at("At a Glance") && at("Activity") < second,
            "{wordpress}"
        );
        assert!(second < at("Quick Draft"), "{wordpress}");
        assert!(
            !wordpress.contains("ig-tone-"),
            "no stat cards: {wordpress}"
        );
        choose_theme(&client, "").await;
        sign_out(&client).await;

        // An author: the counts link only to the screens they may open, and Activity only to
        // the posts they may edit (none of the administrator's).
        sign_in(&client, &author).await;
        let (_, dashboard) = get(&client, "/mc-admin").await;
        let glance = widget_rows(&dashboard, "At a Glance");
        assert_eq!(glance[0].1.as_deref(), Some("/mc-admin/post"));
        assert_eq!(glance[1].1, None, "no edit_pages: {dashboard}");
        assert_eq!(glance[5].1, None, "no list_users: {dashboard}");
        assert!(
            widget_rows(&dashboard, "Activity")
                .iter()
                .all(|(_, link, _)| link.is_none()),
            "{dashboard}"
        );
        assert!(widget(&dashboard, "Quick Draft").is_some(), "{dashboard}");
        sign_out(&client).await;

        // A subscriber sees no Quick Draft: they may not create posts.
        sign_in(&client, &subscriber).await;
        let (status, dashboard) = get(&client, "/mc-admin").await;
        assert_eq!(status, Status::Ok, "{dashboard}");
        assert!(widget(&dashboard, "Quick Draft").is_none(), "{dashboard}");
        assert!(widget(&dashboard, "Activity").is_some(), "{dashboard}");
        sign_out(&client).await;
    });
}
