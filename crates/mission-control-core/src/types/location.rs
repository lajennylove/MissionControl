//! Field group locations: where a group's box shows (design 5.4).
//!
//! A group's [`location`](FieldGroupDef::location) is ACF's list of lists: the outer list is
//! **or**, each inner list is **and**. [`LocationRule`] is the rule as stored; [`Rule`] is the
//! typed view the matcher uses. [`matches`] decides, for a [`ScreenContext`] (what the edit
//! screen knows), whether a group shows there; [`Registry::groups_for`] gives the groups of a
//! screen in ACF's order.
//!
//! ACF's semantics are kept:
//!
//! - A group with no location shows nowhere, and so does an inactive group.
//! - A rule whose parameter Mission Control does not evaluate (`post_status`, `page_type`, ...)
//!   does not match, with either operator (fail closed). It is not an error.
//! - A rule about something the screen does not have (`taxonomy` on a post screen) does not
//!   match either, with `!=` as well as `==`, as ACF returns false when the screen lacks the
//!   parameter.
//! - A group with an empty and-list never matches (ACF drops such lists when it saves).

use super::definition::{FieldGroupDef, LocationOperator, LocationParam, LocationRule};
use super::registry::Registry;

/// What an edit screen knows, to match location rules against. A field that does not apply to
/// the screen is `None`, and a rule about it does not match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenContext {
    /// The post type of the post being edited.
    pub post_type: Option<String>,
    /// The page template of the post being edited: a file name, or `default`/empty for none.
    pub page_template: Option<String>,
    /// The taxonomy whose term is being edited.
    pub taxonomy: Option<String>,
    /// The roles of the user being edited (the user profile screen).
    pub user_roles: Option<Vec<String>>,
    /// The roles of the user who is signed in.
    pub current_user_roles: Option<Vec<String>>,
    /// The slug of the options page being shown.
    pub options_page: Option<String>,
}

impl ScreenContext {
    /// The screen of a post of `post_type`.
    pub fn post(post_type: &str) -> Self {
        Self {
            post_type: Some(post_type.to_owned()),
            ..Self::default()
        }
    }

    /// Sets the page template of the post.
    pub fn with_page_template(mut self, template: &str) -> Self {
        self.page_template = Some(template.to_owned());
        self
    }

    /// The screen of a term of `taxonomy`.
    pub fn term(taxonomy: &str) -> Self {
        Self {
            taxonomy: Some(taxonomy.to_owned()),
            ..Self::default()
        }
    }

    /// The screen of a user being edited, who has `roles`.
    pub fn user(roles: &[&str]) -> Self {
        Self {
            user_roles: Some(roles.iter().map(|r| (*r).to_owned()).collect()),
            ..Self::default()
        }
    }

    /// The screen of the options page `slug`.
    pub fn options_page(slug: &str) -> Self {
        Self {
            options_page: Some(slug.to_owned()),
            ..Self::default()
        }
    }

    /// Sets the roles of the signed-in user.
    pub fn with_current_user_roles(mut self, roles: &[&str]) -> Self {
        self.current_user_roles = Some(roles.iter().map(|r| (*r).to_owned()).collect());
        self
    }
}

/// A location rule Mission Control evaluates, without its operator: the typed view of a
/// [`LocationRule`]'s parameter and value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// `post_type`: the post's type.
    PostType(String),
    /// `page_template`: the template of a page; `default` is none. Only pages have it.
    PageTemplate(String),
    /// `post_template`: the template of a post of any type; `default` is none.
    PostTemplate(String),
    /// `taxonomy`: the taxonomy of the term form; `all` is any.
    Taxonomy(String),
    /// `user_role`: a role of the user being edited.
    User(String),
    /// `current_user_role`: a role of the signed-in user.
    CurrentUser(String),
    /// `options_page`: the options page's slug.
    OptionsPage(String),
}

impl Rule {
    /// The typed view of `rule`'s parameter and value, or `None` for a parameter Mission
    /// Control does not evaluate.
    pub fn from_location(rule: &LocationRule) -> Option<Self> {
        let value = rule.value.clone();
        Some(match rule.param {
            LocationParam::PostType => Self::PostType(value),
            LocationParam::PageTemplate => Self::PageTemplate(value),
            LocationParam::PostTemplate => Self::PostTemplate(value),
            LocationParam::Taxonomy => Self::Taxonomy(value),
            LocationParam::UserRole => Self::User(value),
            LocationParam::CurrentUserRole => Self::CurrentUser(value),
            LocationParam::OptionsPage => Self::OptionsPage(value),
            _ => return None,
        })
    }

    /// Whether the rule holds on `context`; `None` when the screen has nothing for the rule.
    pub fn applies(&self, context: &ScreenContext) -> Option<bool> {
        match self {
            Self::PostType(value) => context.post_type.as_ref().map(|t| t == value),
            // ACF: a page without a template has the template `default`; other post types
            // have no page template, so the rule has nothing to test.
            Self::PageTemplate(value) => match context.post_type.as_deref() {
                Some("page") => Some(template_or_default(context) == value),
                _ => None,
            },
            // ACF: any post without a template has the template `default`.
            Self::PostTemplate(value) => context
                .post_type
                .as_ref()
                .map(|_| template_or_default(context) == value),
            Self::Taxonomy(value) => context
                .taxonomy
                .as_ref()
                .map(|t| value == "all" || t == value),
            Self::User(value) => context
                .user_roles
                .as_ref()
                .map(|roles| roles.iter().any(|r| r == value)),
            Self::CurrentUser(value) => context
                .current_user_roles
                .as_ref()
                .map(|roles| roles.iter().any(|r| r == value)),
            Self::OptionsPage(value) => context.options_page.as_ref().map(|p| p == value),
        }
    }
}

/// The template of the screen's post, `default` when it has none.
fn template_or_default(context: &ScreenContext) -> &str {
    match context.page_template.as_deref() {
        None | Some("") => "default",
        Some(template) => template,
    }
}

/// Whether one stored rule matches `context`, with its operator applied.
fn rule_matches(rule: &LocationRule, context: &ScreenContext) -> bool {
    let Some(applies) = Rule::from_location(rule).and_then(|r| r.applies(context)) else {
        return false;
    };
    match rule.operator {
        LocationOperator::Is => applies,
        LocationOperator::IsNot => !applies,
    }
}

/// Whether `group` shows on `context`: the group is active and any of its or-lists has every
/// rule matching. A group without location rules shows nowhere.
pub fn matches(group: &FieldGroupDef, context: &ScreenContext) -> bool {
    group.active
        && group
            .location
            .iter()
            .any(|all| !all.is_empty() && all.iter().all(|rule| rule_matches(rule, context)))
}

impl Registry {
    /// The field groups in use that show on `context`, ordered by `menu_order`, title, then key, as
    /// ACF orders them (the key makes the order stable). Groups come from [`all_field_groups`](Registry::all_field_groups), so
    /// code and plugin groups and the post types' `field_groups` are included.
    pub fn groups_for(&self, context: &ScreenContext) -> Vec<FieldGroupDef> {
        let mut groups: Vec<_> = self
            .all_field_groups()
            .into_iter()
            .filter(|g| matches(g, context))
            .collect();
        groups.sort_by(|a, b| {
            (a.menu_order, &a.title, &a.key).cmp(&(b.menu_order, &b.title, &b.key))
        });
        groups
    }
}
