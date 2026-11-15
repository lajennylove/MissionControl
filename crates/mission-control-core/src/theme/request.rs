//! The request a theme renders, as menus and widgets need it: what was resolved, where the site
//! lives, and how its links are written.

use chrono::FixedOffset;

use crate::content::PermalinkStructure;
use crate::front::Resolved;

/// One front-end request, for [`theme_menus`](super::theme_menus) (the current item) and
/// [`theme_sidebars`](super::theme_sidebars) (links, dates and the search form).
#[derive(Debug, Clone, Copy)]
pub struct PageRequest<'a> {
    /// What the request resolved to: its queried post or term marks the current menu item.
    pub resolved: &'a Resolved,
    /// The site address (the `home` option) without a trailing slash, an `http` or `https`
    /// address, or empty for links from the root.
    pub home: &'a str,
    /// The request's path from the site's root, still percent-encoded.
    pub path: &'a str,
    /// The query string, without `?`.
    pub query: &'a str,
    /// The permalink structure links are written with.
    pub structure: &'a PermalinkStructure,
    /// The site's offset from UTC (`gmt_offset`), for dates.
    pub utc_offset: FixedOffset,
    /// The search terms as typed, for the search form's value.
    pub search_query: &'a str,
}

impl PageRequest<'_> {
    /// The site's front page address, ending with `/`: `{home}/`, or `/`.
    pub(crate) fn site_url(&self) -> String {
        format!("{}/", self.home)
    }
}
