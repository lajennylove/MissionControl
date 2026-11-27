//! The status links of a post type's list (`WP_Posts_List_Table::get_views`) as Ignite's list
//! tabs, at `?tab=`: All (`all`, every status but Trash, whose `show_in_admin_all_list` is off),
//! Published (`publish`), Scheduled (`future`), Drafts (`draft`), Pending (`pending`), Private
//! (`private`) and Trash (`trash`), in the order `register_post_status` registers them. Each
//! tab filters on the post's `status` (the form's field key), which the list's SQL applies
//! ([`status_conditions`]).
//!
//! On the All tab, the title cell shows the post's state after the title, as `_post_states`
//! names it (Draft, Pending, Private or Scheduled; a published post has none), as a small badge
//! (Ignite's `badge_inside` and `badge_on_tabs`): the state has no column of its own. WordPress
//! leaves out the state of the status a view lists, so the other tabs, each one status, show
//! none.
//!
//! Each tab's count is the number of all its posts, whatever the search, as WordPress's status
//! links count them, and a tab with no posts is left out, as WordPress leaves out a status link
//! with none; All is always shown (Ignite's defaults).

use ignite_core::{BadgeColor, ColumnBadges, FilterCondition, ListFilter, RuntimeResource, Tab};
use orbit_orm::Value as Sql;
use serde_json::Value;

use crate::content::PostStatus;

/// The key the tabs filter on: the form's status field, which holds `PostStatus::as_str`.
const STATUS: &str = "status";

/// The key of the All tab.
pub const ALL_TAB: &str = "all";

/// The key of the post's state in each list row, shown as a badge inside the title cell on
/// the All tab. It is not a list column.
pub const COL_STATE: &str = "col_state";

/// The status links after All, with WordPress's labels.
const STATUS_TABS: [(PostStatus, &str); 6] = [
    (PostStatus::Publish, "Published"),
    (PostStatus::Future, "Scheduled"),
    (PostStatus::Draft, "Drafts"),
    (PostStatus::Pending, "Pending"),
    (PostStatus::Private, "Private"),
    (PostStatus::Trash, "Trash"),
];

/// The All tab's conditions: every status but Trash.
fn all_tab_filter() -> ListFilter {
    ListFilter::new().where_ne(STATUS, PostStatus::Trash.as_str())
}

/// The list's tabs: All, then each status.
pub fn status_tabs() -> Vec<Tab> {
    let mut tabs = vec![Tab::make(ALL_TAB).label("All").filter(all_tab_filter())];
    tabs.extend(STATUS_TABS.iter().map(|(status, label)| {
        Tab::make(status.as_str())
            .label(*label)
            .where_eq(STATUS, status.as_str())
    }));
    tabs
}

/// Gives `resource` (a post type's list) the status tabs, and the state badge inside the title
/// cell on the All tab.
pub fn add_status_tabs(resource: &mut RuntimeResource) {
    resource.tabs = status_tabs();
    resource.column_badges = ColumnBadges::new()
        .badge_color(COL_STATE, state_color)
        .badge_inside(COL_STATE, super::resource::COL_TITLE)
        .badge_on_tabs(COL_STATE, [ALL_TAB]);
}

/// The state `_get_post_states` names for a post of `status`, shown on the All tab: none for a
/// published or trashed post. Password protected, Sticky and the front page's states are not
/// statuses and are not shown.
pub fn post_state(status: PostStatus) -> Option<&'static str> {
    match status {
        PostStatus::Draft => Some("Draft"),
        PostStatus::Pending => Some("Pending"),
        PostStatus::Private => Some("Private"),
        PostStatus::Future => Some("Scheduled"),
        _ => None,
    }
}

/// The colour of a state badge, by the state's name.
pub fn state_color(state: &str) -> BadgeColor {
    match state {
        "Pending" => BadgeColor::Warning,
        "Private" => BadgeColor::Info,
        "Scheduled" => BadgeColor::Primary,
        _ => BadgeColor::Gray,
    }
}

/// Adds the status conditions of a tab's `filter` to the list's SQL `WHERE` (`p` is the posts
/// table). The tabs filter on the status only; a condition on another key is not one of theirs
/// and is left out.
pub fn status_conditions(filter: &ListFilter, sql: &mut String, binds: &mut Vec<Sql>) {
    let text = |value: &Value| match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    for condition in filter.conditions() {
        match condition {
            FilterCondition::Eq(key, value) if key == STATUS => {
                sql.push_str(" AND p.status = ?");
                binds.push(text(value).into());
            }
            FilterCondition::Ne(key, value) if key == STATUS => {
                sql.push_str(" AND p.status <> ?");
                binds.push(text(value).into());
            }
            FilterCondition::In(key, values) if key == STATUS => {
                if values.is_empty() {
                    sql.push_str(" AND 1 = 0");
                } else {
                    let marks = vec!["?"; values.len()].join(", ");
                    sql.push_str(&format!(" AND p.status IN ({marks})"));
                    binds.extend(values.iter().map(|v| Sql::from(text(v))));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_are_wordpresss_status_links() {
        let tabs = status_tabs();
        let keys: Vec<&str> = tabs.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "all", "publish", "future", "draft", "pending", "private", "trash"
            ]
        );
        let labels: Vec<&str> = tabs.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "All",
                "Published",
                "Scheduled",
                "Drafts",
                "Pending",
                "Private",
                "Trash"
            ]
        );
        assert_eq!(tabs[0].filter, all_tab_filter());
    }

    #[test]
    fn conditions_become_sql_on_the_status() {
        let mut sql = String::new();
        let mut binds = Vec::new();
        status_conditions(&all_tab_filter(), &mut sql, &mut binds);
        status_conditions(
            &ListFilter::new()
                .where_eq(STATUS, "draft")
                .where_in(STATUS, ["a", "b"])
                .where_in(STATUS, Vec::<String>::new())
                .where_eq("other", "x"),
            &mut sql,
            &mut binds,
        );
        assert_eq!(
            sql,
            " AND p.status <> ? AND p.status = ? AND p.status IN (?, ?) AND 1 = 0"
        );
        assert_eq!(binds.len(), 4);
    }
}
