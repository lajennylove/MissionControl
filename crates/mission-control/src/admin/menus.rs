//! The Menus screen's menu builder: WordPress's `nav-menus.php` on top of Ignite's form of the
//! Menus screen (`mission_control_core::admin::menus`).
//!
//! - The views (`views/admin/menus/`): the "Add menu items" column, one box per post type that
//!   can be added to menus, Custom Links, then one box per taxonomy (as `wp_nav_menu_setup`
//!   registers the meta boxes), and the "Delete Menu" form under an existing menu's form.
//! - The script (`assets/editor/nav-menus.js`) and its stylesheet (`assets/editor/nav-menus.css`),
//!   served at `/mc-includes/js/nav-menus.js` and `/mc-includes/css/nav-menus.css` with a `?ver=`
//!   of their own (cached for a year with the current version, as the editor's bundle). The
//!   script shows the column beside the form, and the items field as WordPress's Menu
//!   Structure: a nested list whose items open with their settings. The items field is the
//!   builder's own value: Ignite's field of JSON rows becomes a hidden input (no raw JSON
//!   textarea on the page, as WordPress's `nav-menus.php` keeps its items in the structure's
//!   hidden inputs), which the script reads and writes back, so the form saves through Ignite
//!   as before. Without the script, the column stays hidden, and a save keeps the items.
//! - A fairing adds them to Ignite's New and edit forms of the screen and to the form a refused
//!   save answers (422).
//! - `GET /mc-admin/nav-menus/quick-search` lists a box's items (published posts of a post
//!   type, or a taxonomy's terms) a page at a time, with a search, as WordPress's
//!   `wp_nav_menu_item_post_type_meta_box`, `wp_nav_menu_item_taxonomy_meta_box` and
//!   `_wp_ajax_menu_quick_search` do, and reads the titles of a menu's items. Only the boxes'
//!   post types and taxonomies are listed. The admin's fairing checks `edit_theme_options` on
//!   it, as on the rest of the screen.

use std::io::{self, Cursor};
use std::sync::{Arc, OnceLock};

use ignite_panel::{AdminUser, CsrfToken};
use mission_control_core::admin::NAV_MENUS;
use mission_control_core::admin::menus::{
    MenuObjectType, MenuSourceQuery, menu_source_items, nav_menu_choices,
};
use mission_control_core::theme::MAX_MENU_DEPTH;
use mission_control_core::types::{PostTypeResources, Registry};
use orbit_orm::Database;
use propel_runtime::Loader;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Header, Method, Status};
use rocket::{Request, Response, Route, State};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{ADMIN_PATH, AdminScreens};
use crate::editor::INCLUDES_PATH;

/// The deepest an item can be nested in the builder, as nav-menu.js's `globalMaxDepth` (11).
const MAX_DEPTH: usize = 11;

/// The script and its stylesheet.
static SCRIPT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/editor/nav-menus.js"
));
static STYLESHEET: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/editor/nav-menus.css"
));

/// The version in the script's and the stylesheet's URLs: the start of a SHA-256 of both.
fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(SCRIPT);
        hasher.update([0]);
        hasher.update(STYLESHEET);
        hasher
            .finalize()
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    })
}

fn script_url() -> String {
    format!("{INCLUDES_PATH}/js/nav-menus.js?ver={}", version())
}

fn stylesheet_url() -> String {
    format!("{INCLUDES_PATH}/css/nav-menus.css?ver={}", version())
}

/// A box of the "Add menu items" column.
#[derive(Debug, Clone)]
pub(crate) struct MenuSource {
    /// `post_type`, `taxonomy` or `custom`.
    kind: &'static str,
    /// The post type or taxonomy (the item's `type` on the form), or `custom`.
    object: String,
    /// The singular name, the item's type label in the Menu Structure ("Page").
    singular: String,
    /// The plural name, the box's title ("Pages").
    plural: String,
    /// Whether this is the Custom Links box.
    custom: bool,
    object_type: MenuObjectType,
}

impl MenuSource {
    /// The box as the builder's view reads it.
    fn context(&self) -> serde_json::Value {
        json!({
            "kind": self.kind,
            "object": self.object,
            "singular": self.singular,
            "plural": self.plural,
            "custom": self.custom,
        })
    }
}

/// The boxes of the "Add menu items" column, in WordPress's order: the post types, Custom
/// Links, then the taxonomies.
pub(crate) struct MenuSources(Vec<MenuSource>);

/// The boxes for `types` (the item types the Menus screen offers, with their singular names),
/// named with the plural names of `resources`' post types and `registry`'s taxonomies.
pub(crate) fn menu_sources(
    types: &[(MenuObjectType, String)],
    resources: &PostTypeResources,
    registry: &Registry,
) -> MenuSources {
    let plural = |kind: &MenuObjectType, singular: &str| -> String {
        let found = match kind {
            MenuObjectType::PostType(slug) => resources
                .post_type(slug)
                .or_else(|| registry.post_type(slug))
                .map(|def| def.labels.name),
            MenuObjectType::Taxonomy(slug) => registry
                .all_taxonomies()
                .into_iter()
                .find(|def| def.slug == *slug)
                .map(|def| def.labels.name),
            MenuObjectType::Custom => None,
        };
        found
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| match kind.value() {
                "page" => "Pages".to_owned(),
                "post" => "Posts".to_owned(),
                "category" => "Categories".to_owned(),
                "post_tag" => "Tags".to_owned(),
                _ => singular.to_owned(),
            })
    };
    let source = |kind: &MenuObjectType, singular: &str| MenuSource {
        kind: match kind {
            MenuObjectType::Custom => "custom",
            MenuObjectType::PostType(_) => "post_type",
            MenuObjectType::Taxonomy(_) => "taxonomy",
        },
        object: kind.value().to_owned(),
        singular: singular.to_owned(),
        plural: if *kind == MenuObjectType::Custom {
            "Custom Links".to_owned()
        } else {
            plural(kind, singular)
        },
        custom: *kind == MenuObjectType::Custom,
        object_type: kind.clone(),
    };
    let order = |kind: &MenuObjectType| match kind {
        MenuObjectType::PostType(_) => 0,
        MenuObjectType::Custom => 1,
        MenuObjectType::Taxonomy(_) => 2,
    };
    let mut sources: Vec<MenuSource> = types
        .iter()
        .map(|(kind, singular)| source(kind, singular))
        .collect();
    // A stable sort keeps the screen's order within each group.
    sources.sort_by_key(|s| order(&s.object_type));
    MenuSources(sources)
}

/// The views, built into the binary.
struct Views;

impl Loader for Views {
    fn source(&self, name: &str) -> io::Result<String> {
        let source = match name {
            "menus.builder" => include_str!("../../views/admin/menus/builder.propel"),
            "menus.delete" => include_str!("../../views/admin/menus/delete.propel"),
            "menus.switch" => include_str!("../../views/admin/menus/switch.propel"),
            _ => return Err(io::Error::from(io::ErrorKind::NotFound)),
        };
        Ok(source.to_owned())
    }
}

/// `view` rendered with `context`, or `None` (logged) when it cannot render.
fn render(view: &str, context: &serde_json::Value) -> Option<String> {
    match propel_runtime::render(&Views, view, context) {
        Ok(html) => Some(html),
        Err(e) => {
            log::error!("The menu builder's view `{view}` could not be rendered: {e}");
            None
        }
    }
}

/// The quick search and the "Select a menu to edit" form's address, mounted at the admin path
/// before Ignite's routes of the screen.
pub(crate) fn routes() -> Vec<Route> {
    let mut routes = rocket::routes![quick_search, switch_menu];
    for route in &mut routes {
        route.rank = QUICK_SEARCH_RANK;
    }
    routes
}

/// The rank of the quick search: before Ignite's `/<slug>/<id>`.
const QUICK_SEARCH_RANK: isize = -20;

/// The routes under [`INCLUDES_PATH`], mounted at the root.
pub(crate) fn include_routes() -> Vec<Route> {
    rocket::routes![script, stylesheet]
}

/// A file of the builder, with its type and cache header.
struct Asset {
    bytes: &'static [u8],
    content_type: ContentType,
    current: bool,
}

impl<'r> rocket::response::Responder<'r, 'static> for Asset {
    fn respond_to(self, _: &'r Request<'_>) -> rocket::response::Result<'static> {
        Response::build()
            .status(Status::Ok)
            .header(self.content_type)
            .header(Header::new("X-Content-Type-Options", "nosniff"))
            .header(Header::new(
                "Cache-Control",
                if self.current {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
            ))
            .sized_body(self.bytes.len(), Cursor::new(self.bytes))
            .ok()
    }
}

#[rocket::get("/mc-includes/js/nav-menus.js?<ver>")]
fn script(ver: Option<&str>) -> Asset {
    Asset {
        bytes: SCRIPT,
        content_type: ContentType::new("text", "javascript").with_params(("charset", "utf-8")),
        current: ver == Some(version()),
    }
}

#[rocket::get("/mc-includes/css/nav-menus.css?<ver>")]
fn stylesheet(ver: Option<&str>) -> Asset {
    Asset {
        bytes: STYLESHEET,
        content_type: ContentType::CSS,
        current: ver == Some(version()),
    }
}

/// The "Select a menu to edit" form's address (`nav-menus.php?menu=<id>`): sends the browser to
/// the edit form of menu `menu` (303), or to New without one. The admin's fairing has checked
/// `edit_theme_options`; a visitor fails [`AdminUser`] and is sent to the sign-in page.
#[rocket::get("/nav-menus/switch?<menu>")]
fn switch_menu(_user: AdminUser, menu: Option<i64>) -> rocket::response::Redirect {
    rocket::response::Redirect::to(match menu.filter(|id| *id > 0) {
        Some(id) => format!("{ADMIN_PATH}/{NAV_MENUS}/{id}/edit"),
        None => format!("{ADMIN_PATH}/{NAV_MENUS}/new"),
    })
}

/// The quick search's query: `kind` and `object` name the box, `q` searches the titles, `page`
/// is the page (from 1), and `include` (ids separated by commas) reads those items only.
#[derive(rocket::FromForm)]
struct SearchQuery<'r> {
    kind: &'r str,
    object: &'r str,
    q: Option<&'r str>,
    page: Option<i64>,
    include: Option<&'r str>,
}

/// A box's items, a page at a time; see [`SearchQuery`]. Answers
/// `{"items": [{"id", "title", "status"}], "more": bool}`; 404 for a box the column does not
/// have.
#[rocket::get("/nav-menus/quick-search?<query..>")]
async fn quick_search(
    _user: AdminUser,
    sources: &State<MenuSources>,
    db: &State<Arc<Database>>,
    query: SearchQuery<'_>,
) -> (Status, (ContentType, String)) {
    let SearchQuery {
        kind,
        object,
        q,
        page,
        include,
    } = query;
    let Some(source) = sources
        .0
        .iter()
        .find(|s| s.kind == kind && s.object == object && !s.custom)
    else {
        return (
            Status::NotFound,
            (
                ContentType::JSON,
                json!({"items": [], "more": false}).to_string(),
            ),
        );
    };
    let ids: Vec<i64> = include
        .unwrap_or_default()
        .split(',')
        .filter_map(|id| id.trim().parse().ok())
        .collect();
    let query = MenuSourceQuery {
        search: q,
        page: page.unwrap_or(1),
        include: &ids,
    };
    match menu_source_items(db, &source.object_type, &query).await {
        Ok(found) => (
            Status::Ok,
            (
                ContentType::JSON,
                serde_json::to_string(&found).unwrap_or_default(),
            ),
        ),
        Err(e) => {
            log::error!("Could not list the menu items of `{object}`: {e}");
            (
                Status::InternalServerError,
                (
                    ContentType::JSON,
                    json!({"items": [], "more": false}).to_string(),
                ),
            )
        }
    }
}

/// A page of the Menus screen the fairing changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuPage {
    /// The list of menus.
    List,
    /// The form: New (`None`) or the edit of menu `id`, or the form a refused save answers.
    Form(Option<i64>),
}

/// Which page of the Menus screen a response is, from the path after `/mc-admin/nav-menus`.
fn menu_page(method: Method, status: Status, rest: &str) -> Option<MenuPage> {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match (method, status.code, segments.as_slice()) {
        (Method::Get, 200, []) => Some(MenuPage::List),
        (Method::Get, 200, ["new"]) | (Method::Post, 422, []) => Some(MenuPage::Form(None)),
        (Method::Get, 200, [id, "edit"]) | (Method::Post, 422, [id]) => id
            .parse()
            .ok()
            .filter(|id| *id > 0)
            .map(|id| MenuPage::Form(Some(id))),
        _ => None,
    }
}

/// The top of the Menus screen (`menus.switch`): the "Edit Menus" and "Create New Menu" tabs and
/// the "Select a menu to edit" dropdown of `menus`, with the edited menu chosen.
fn switcher(menus: &[(i64, String)], page: MenuPage) -> Option<String> {
    let selected = match page {
        MenuPage::Form(id) => id,
        MenuPage::List => None,
    };
    render(
        "menus.switch",
        &json!({
            "list_url": format!("{ADMIN_PATH}/{NAV_MENUS}"),
            "new_url": format!("{ADMIN_PATH}/{NAV_MENUS}/new"),
            "switch_url": format!("{ADMIN_PATH}/{NAV_MENUS}/switch"),
            "editing": page != MenuPage::Form(None),
            "creating": page == MenuPage::Form(None),
            "has_menus": !menus.is_empty(),
            "menus": menus
                .iter()
                .map(|(id, name)| json!({
                    "id": id,
                    "name": name,
                    "selected": Some(*id) == selected,
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

/// `html` with `top` after the page's heading (the first `</h1>` in `<main>`), and the
/// builder's stylesheet in the head. A page without a heading is returned as it is.
fn with_switcher(html: &str, top: &str) -> String {
    let from = html.find("<main").unwrap_or(0);
    let Some(at) = html[from..]
        .find("</h1>")
        .map(|at| from + at + "</h1>".len())
    else {
        return html.to_owned();
    };
    let mut out = String::with_capacity(html.len() + top.len());
    out.push_str(&html[..at]);
    out.push_str(top);
    out.push_str(&html[at..]);
    if !out.contains(&stylesheet_url()) {
        out = out.replacen(
            "</head>",
            &format!(
                "<link rel=\"stylesheet\" href=\"{}\">\n</head>",
                stylesheet_url()
            ),
            1,
        );
    }
    out
}

/// Ignite's form page `html` with the builder: the items field as its hidden value
/// ([`items_as_hidden`]), its stylesheet in the head, the Delete Menu form
/// after the menu's form (for menu `id`), and the "Add menu items" column and the script before
/// the end of the body. A page without the menu's form is returned as it is.
fn with_builder(html: &str, sources: &MenuSources, id: Option<i64>, token: &str) -> String {
    let html = items_as_hidden(html);
    let html = html.as_str();
    let action = format!("action=\"{ADMIN_PATH}/{NAV_MENUS}");
    let Some(form_end) = html.find(&action).and_then(|at| {
        html[at..]
            .find("</form>")
            .map(|end| at + end + "</form>".len())
    }) else {
        return html.to_owned();
    };
    let Some(builder) = render(
        "menus.builder",
        &json!({
            "sources": sources.0.iter().map(MenuSource::context).collect::<Vec<_>>(),
            "search_url": format!("{ADMIN_PATH}/{NAV_MENUS}/quick-search"),
            "max_depth": MAX_DEPTH.min(MAX_MENU_DEPTH),
        }),
    ) else {
        return html.to_owned();
    };
    let delete = match id {
        Some(id) => render(
            "menus.delete",
            &json!({
                "delete_url": format!("{ADMIN_PATH}/{NAV_MENUS}/{id}/delete"),
                "token": token,
            }),
        )
        .unwrap_or_default(),
        None => String::new(),
    };
    let mut out = String::with_capacity(html.len() + builder.len() + delete.len() + 256);
    out.push_str(&html[..form_end]);
    out.push_str(&delete);
    out.push_str(&html[form_end..]);
    let head = format!(
        "<link rel=\"stylesheet\" href=\"{}\">\n</head>",
        stylesheet_url()
    );
    let body = format!(
        "{builder}<script src=\"{}\" defer></script>\n</body>",
        script_url()
    );
    out.replacen("</head>", &head, 1)
        .replacen("</body>", &body, 1)
}

/// `html` with Ignite's items field (its label and its textarea of JSON rows) as the builder's
/// hidden value: `<input type="hidden" name="items">` holding the same JSON. An error under the
/// field stays. A page without the field is returned as it is.
fn items_as_hidden(html: &str) -> String {
    let Some(open) = html
        .match_indices("<textarea")
        .map(|(at, _)| at)
        .find(|at| {
            html[*at..]
                .find('>')
                .is_some_and(|end| html[*at..*at + end].contains(" name=\"items\""))
        })
    else {
        return html.to_owned();
    };
    let Some(content) = html[open..].find('>').map(|end| open + end + 1) else {
        return html.to_owned();
    };
    let Some(close) = html[content..].find("</textarea>").map(|end| content + end) else {
        return html.to_owned();
    };
    // The textarea's text is escaped; a quote is escaped again for the attribute (the same
    // value), and the newline after the opening tag is not part of it.
    let value = html[content..close]
        .strip_prefix('\n')
        .unwrap_or(&html[content..close])
        .replace('"', "&quot;");
    let input = format!("<input type=\"hidden\" id=\"items\" name=\"items\" value=\"{value}\">");
    let mut start = open;
    // The field's label goes with it.
    if let Some(label) = html[..open].rfind("<label")
        && let Some(label_end) = html[label..open].find("</label>")
        && html[label..label + label_end].contains("for=\"items\"")
    {
        start = label;
    }
    let mut out = String::with_capacity(html.len());
    out.push_str(&html[..start]);
    out.push_str(&input);
    out.push_str(&html[close + "</textarea>".len()..]);
    out
}

/// Adds the builder to the Menus screen's forms; see the [module](self).
pub(crate) struct MenuBuilderViews;

#[rocket::async_trait]
impl Fairing for MenuBuilderViews {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control menu builder",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let base = format!("{ADMIN_PATH}/{NAV_MENUS}");
        let Some(rest) = request.uri().path().as_str().strip_prefix(&base) else {
            return;
        };
        if !rest.is_empty() && !rest.starts_with('/') {
            return;
        }
        let Some(page) = menu_page(request.method(), response.status(), rest) else {
            return;
        };
        let rocket = request.rocket();
        if response.content_type() != Some(ContentType::HTML)
            || !rocket.state::<AdminScreens>().is_some_and(|s| s.menus)
        {
            return;
        }
        let (Some(sources), Some(db)) = (
            rocket.state::<MenuSources>(),
            rocket.state::<Arc<Database>>(),
        ) else {
            return;
        };
        let Some(token) = request.guard::<CsrfToken>().await.succeeded() else {
            return;
        };
        let menus = match nav_menu_choices(db).await {
            Ok(menus) => menus,
            Err(e) => {
                log::error!("Could not list the menus for the Menus screen: {e}");
                Vec::new()
            }
        };
        let Ok(mut html) = response.body_mut().to_string().await else {
            return;
        };
        if let MenuPage::Form(id) = page {
            html = with_builder(&html, sources, id, token.value());
        }
        if let Some(top) = switcher(&menus, page) {
            html = with_switcher(&html, &top);
        }
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> MenuSources {
        MenuSources(vec![
            MenuSource {
                kind: "post_type",
                object: "page".into(),
                singular: "Page".into(),
                plural: "Pages".into(),
                custom: false,
                object_type: MenuObjectType::PostType("page".into()),
            },
            MenuSource {
                kind: "custom",
                object: "custom".into(),
                singular: "Custom Link".into(),
                plural: "Custom Links".into(),
                custom: true,
                object_type: MenuObjectType::Custom,
            },
        ])
    }

    const PAGE: &str = "<html><head></head><body><h1>Edit Menu</h1>\
        <form method=\"post\" action=\"/mc-admin/nav-menus/7\"><textarea name=\"items\"></textarea>\
        </form><footer></footer></body></html>";

    #[test]
    fn the_pages_are_the_list_the_forms_and_a_refused_save() {
        let form = |id| Some(MenuPage::Form(id));
        assert_eq!(menu_page(Method::Get, Status::Ok, "/new"), form(None));
        assert_eq!(menu_page(Method::Get, Status::Ok, "/7/edit"), form(Some(7)));
        assert_eq!(
            menu_page(Method::Post, Status::UnprocessableEntity, ""),
            form(None)
        );
        assert_eq!(
            menu_page(Method::Post, Status::UnprocessableEntity, "/7"),
            form(Some(7))
        );
        assert_eq!(menu_page(Method::Get, Status::Ok, ""), Some(MenuPage::List));
        assert_eq!(menu_page(Method::Post, Status::SeeOther, "/7"), None);
        assert_eq!(menu_page(Method::Get, Status::Ok, "/x/edit"), None);
    }

    #[test]
    fn the_switcher_lists_the_menus_after_the_heading() {
        let menus = [(3, "Footer".to_owned()), (7, "Main <b>".to_owned())];
        let top = switcher(&menus, MenuPage::Form(Some(7))).unwrap();
        assert!(top.contains("Select a menu to edit:"), "{top}");
        assert!(top.contains("<option value=\"3\">Footer</option>"), "{top}");
        assert!(
            top.contains("<option value=\"7\" selected>Main &lt;b&gt;</option>"),
            "{top}"
        );
        assert!(
            top.contains("action=\"/mc-admin/nav-menus/switch\""),
            "{top}"
        );
        assert!(top.contains(">Create New Menu</a>"), "{top}");
        let page = with_switcher(
            "<html><head></head><body><main><h1>Menus</h1><p>x</p>",
            &top,
        );
        assert!(
            page.contains("</h1>\n<div class=\"mc-menu-manage\">"),
            "{page}"
        );
        assert!(page.contains("nav-menus.css"), "{page}");
        let none = switcher(&[], MenuPage::Form(None)).unwrap();
        assert!(none.contains("No menus yet"), "{none}");
        assert!(
            none.contains("aria-current=\"page\">Create New Menu"),
            "{none}"
        );
    }

    #[test]
    fn the_builder_joins_the_form() {
        let page = with_builder(PAGE, &sources(), Some(7), "t\"k");
        assert!(page.contains("id=\"mc-menu-builder\""), "{page}");
        assert!(page.contains("<summary>Pages</summary>"), "{page}");
        assert!(page.contains("<summary>Custom Links</summary>"), "{page}");
        assert!(page.contains("data-object=\"page\""), "{page}");
        assert!(page.contains("/mc-includes/js/nav-menus.js?ver="), "{page}");
        assert!(
            page.contains("/mc-includes/css/nav-menus.css?ver="),
            "{page}"
        );
        let form_end = page.find("</form>").unwrap();
        let delete = page
            .find("action=\"/mc-admin/nav-menus/7/delete\"")
            .unwrap();
        assert!(delete > form_end, "{page}");
        assert!(page.contains("value=\"t&quot;k\""), "{page}");
        // New: no Delete Menu.
        let page = with_builder(PAGE, &sources(), None, "t");
        assert!(!page.contains("/delete"), "{page}");
    }

    #[test]
    fn the_items_field_is_the_builders_hidden_value() {
        let page = "<form action=\"/mc-admin/nav-menus/7\"><div data-ignite-field=\"items\">\
            <label class=\"ig-label\" for=\"items\">Menu items</label>\n  \
            <textarea class=\"ig-input\" id=\"items\" name=\"items\">[{&quot;title&quot;:\
            &quot;A &lt;b&gt;&quot;}]</textarea><p class=\"ig-error\">Bad</p></div></form>";
        let out = items_as_hidden(page);
        assert!(!out.contains("<textarea"), "{out}");
        assert!(!out.contains("<label"), "{out}");
        assert!(
            out.contains(
                "<input type=\"hidden\" id=\"items\" name=\"items\" \
                 value=\"[{&quot;title&quot;:&quot;A &lt;b&gt;&quot;}]\">"
            ),
            "{out}"
        );
        assert!(out.contains("<p class=\"ig-error\">Bad</p>"), "{out}");
        let page = with_builder(PAGE, &sources(), Some(7), "t");
        assert!(
            page.contains("type=\"hidden\" id=\"items\" name=\"items\" value=\"\""),
            "{page}"
        );
        assert_eq!(items_as_hidden("<p>none</p>"), "<p>none</p>");
    }

    #[test]
    fn another_page_is_left_alone() {
        assert_eq!(
            with_builder("<html></html>", &sources(), None, "t"),
            "<html></html>"
        );
    }
}
