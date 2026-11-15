//! Meta capabilities: a check on one object, such as `edit_post` on post 12, mapped to the
//! primitive capabilities a user must hold, as WordPress's `map_meta_cap` maps them.
//!
//! Ported from WordPress 6.7 (`wp-includes/capabilities.php`, single site) for the post meta
//! capabilities: `edit_post`, `delete_post`, `read_post` and `publish_post`, their `_page`
//! names, and `edit_post_meta`, `add_post_meta` and `delete_post_meta`; and for the term meta
//! capabilities `edit_term`, `delete_term` and `assign_term`, with the category and tag
//! capabilities WordPress maps (`edit_post_tags` and the like to `manage_categories`,
//! `assign_categories` and `assign_post_tags` to `edit_posts`). Every other capability maps to
//! itself, so `edit_posts` or `read_private_posts` are checked as they are.
//!
//! The site's special pages count as WordPress counts them: deleting the front page or the
//! posts page needs `manage_options`, and editing or deleting the privacy policy page needs it
//! too (see [`SitePages`]). A protected meta key (one starting with `_`) is refused through
//! `edit_post_meta` and its siblings unless the meta capability itself is granted by name, as
//! WordPress refuses it unless a filter allows it.
//!
//! `read_post` does not look at a post's password, as WordPress's does not: the front end and
//! the REST API must check it themselves (WordPress's `post_password_required`).
//!
//! What this port leaves out, for now:
//!
//! - **Post type capabilities.** WordPress derives a type's capability names from its
//!   registration. A check that gives the type's names ([`PostContext::post_type_caps`], which
//!   the REST controllers fill from the type registry) uses them, and maps the type's own meta
//!   capability names (`edit_book`, `read_book`, `delete_book`) as `edit_post`, `read_post` and
//!   `delete_post`, as WordPress's `$post_type_meta_caps` does for a type with `map_meta_cap`. A
//!   check that does not give them falls back to the interim rule of ruling R24
//!   ([`PostTypeCaps::for_post_type`]), and a custom meta capability name then maps to itself.
//! - **The `map_meta_cap`, `user_has_cap` and `auth_post_meta_*` filters**, and the user,
//!   comment, term meta (`edit_term_meta`), plugin, theme and multisite meta capabilities: they
//!   map to themselves here, so they are refused unless granted by name.
//! - **Taxonomy capabilities.** As for post types: a check that gives the taxonomy's names
//!   ([`TermContext::caps`]) uses them, and one that does not falls back to
//!   [`TaxonomyCaps::for_taxonomy`].

use crate::content::{Post, PostStatus};

use super::capabilities::{DO_NOT_ALLOW, UserCaps, has_cap};

/// The object a meta capability is checked on.
#[derive(Debug, Clone, Copy)]
pub enum Object<'a> {
    /// A post of any type, with nothing known beyond its row: a trashed post counts as one
    /// trashed from a draft (as WordPress does when `_wp_trash_meta_status` is missing), and a
    /// revision, or an attachment with a parent, cannot be checked (its parent is not given).
    Post(&'a Post),
    /// A post with what WordPress reads beside its row: its status before trashing, its parent
    /// and the site's special pages.
    PostWithContext(PostContext<'a>),
    /// One meta key of a post, for `edit_post_meta`, `add_post_meta` and `delete_post_meta`.
    PostMeta {
        /// The post and its context.
        post: PostContext<'a>,
        /// The meta key, such as `_thumbnail_id` or `subtitle`.
        key: &'a str,
    },
    /// A term, for `edit_term`, `delete_term` and `assign_term`.
    Term(TermContext<'a>),
}

impl<'a> Object<'a> {
    /// The post and its context, for a post object.
    fn post(&self) -> Option<PostContext<'a>> {
        match *self {
            Self::Post(post) => Some(PostContext::new(post)),
            Self::PostWithContext(context) | Self::PostMeta { post: context, .. } => Some(context),
            Self::Term(_) => None,
        }
    }
}

/// A term and what `map_meta_cap` reads beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermContext<'a> {
    /// The term's `term_id`.
    pub term_id: i64,
    /// The term's taxonomy, such as `category`.
    pub taxonomy: &'a str,
    /// The taxonomy's default terms, which nobody may delete: the ids in the
    /// `default_{taxonomy}` option (`default_category`) and the `default_term_{taxonomy}` option,
    /// as [`default_term_ids`](crate::content::default_term_ids) reads them.
    pub default_terms: &'a [i64],
    /// The taxonomy's capability names, from its registration. `None` takes
    /// [`TaxonomyCaps::for_taxonomy`].
    pub caps: Option<&'a TaxonomyCaps>,
}

/// The primitive capability names of one taxonomy, as `register_taxonomy` sets its `cap`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxonomyCaps {
    /// `manage_terms`: see the taxonomy's terms in the admin.
    pub manage_terms: String,
    /// `edit_terms`: create and edit terms.
    pub edit_terms: String,
    /// `delete_terms`.
    pub delete_terms: String,
    /// `assign_terms`: give a post terms.
    pub assign_terms: String,
}

impl TaxonomyCaps {
    /// The capability names of `taxonomy`, as WordPress registers its own taxonomies:
    ///
    /// - `category`: `manage_categories`, `edit_categories`, `delete_categories` and
    ///   `assign_categories`, as WordPress 4.7 names them; [`map_meta_cap`] maps them to
    ///   `manage_categories` and `edit_posts`.
    /// - `post_tag`: `manage_post_tags`, `edit_post_tags`, `delete_post_tags` and
    ///   `assign_post_tags`, which [`map_meta_cap`] maps to `manage_categories` and `edit_posts`.
    /// - `nav_menu`: `edit_theme_options` for everything.
    /// - Any other taxonomy: `register_taxonomy`'s defaults, as for categories.
    ///
    /// A registered custom taxonomy's names come from its definition
    /// ([`TaxonomyDef::caps`](crate::types::TaxonomyDef::caps)).
    pub fn for_taxonomy(taxonomy: &str) -> Self {
        let caps = |manage: &str, edit: &str, delete: &str, assign: &str| Self {
            manage_terms: manage.to_owned(),
            edit_terms: edit.to_owned(),
            delete_terms: delete.to_owned(),
            assign_terms: assign.to_owned(),
        };
        match taxonomy {
            "post_tag" => caps(
                "manage_post_tags",
                "edit_post_tags",
                "delete_post_tags",
                "assign_post_tags",
            ),
            "nav_menu" => caps(
                "edit_theme_options",
                "edit_theme_options",
                "edit_theme_options",
                "edit_theme_options",
            ),
            "category" => caps(
                "manage_categories",
                "edit_categories",
                "delete_categories",
                "assign_categories",
            ),
            _ => caps(
                "manage_categories",
                "manage_categories",
                "manage_categories",
                "edit_posts",
            ),
        }
    }
}

/// The ids of the site's special pages, from the options WordPress keeps them in. `None` means
/// the option is not set (WordPress's 0).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SitePages {
    /// The static front page (`page_on_front`).
    pub page_on_front: Option<i64>,
    /// The page that lists the posts (`page_for_posts`).
    pub page_for_posts: Option<i64>,
    /// The privacy policy page (`wp_page_for_privacy_policy`).
    pub privacy_policy: Option<i64>,
}

impl SitePages {
    /// A site with none of the special pages set.
    pub const NONE: Self = Self {
        page_on_front: None,
        page_for_posts: None,
        privacy_policy: None,
    };
}

/// A post and the facts `map_meta_cap` reads beside its row.
#[derive(Debug, Clone, Copy)]
pub struct PostContext<'a> {
    /// The post.
    pub post: &'a Post,
    /// For a trashed post, the status it had before, from its `_wp_trash_meta_status` meta.
    pub trash_meta_status: Option<PostStatus>,
    /// The post's parent: the post a revision belongs to, or an attachment is attached to. Used
    /// only when its id is the post's `parent_id`.
    pub parent: Option<&'a PostContext<'a>>,
    /// The site's special pages. When they are not given, a page is checked as if it might be
    /// any of them: `manage_options` is added to editing or deleting it.
    pub site_pages: Option<SitePages>,
    /// The capability names of the post's type, from its registration. `None` takes
    /// [`PostTypeCaps::for_post_type`]. A revision is checked as its parent, with the parent's
    /// own `post_type_caps`.
    pub post_type_caps: Option<&'a PostTypeCaps>,
}

impl<'a> PostContext<'a> {
    /// `post` with no trash status, no parent and no special pages known.
    pub fn new(post: &'a Post) -> Self {
        Self {
            post,
            trash_meta_status: None,
            parent: None,
            site_pages: None,
            post_type_caps: None,
        }
    }

    /// The capability names of the post's type: those given, or the interim rule's.
    fn type_caps(&self) -> PostTypeCaps {
        self.post_type_caps
            .cloned()
            .unwrap_or_else(|| PostTypeCaps::for_post_type(&self.post.post_type))
    }

    /// The parent, when the post has one and the one given is it.
    fn parent(&self) -> Option<&'a PostContext<'a>> {
        let parent_id = self.post.parent_id?;
        self.parent.filter(|parent| parent.post.id == parent_id)
    }

    /// The post's status as WordPress's `get_post_status` reads it, or `None` when it depends
    /// on a parent that was not given. An attachment's `inherit` status is its parent's (the
    /// status before trashing, when the parent is trashed), or `publish` when it has no parent;
    /// an attachment in any status but `private`, `trash` or `auto-draft` reads as `publish`.
    fn status(&self) -> Option<PostStatus> {
        let post = self.post;
        if post.post_type != "attachment" {
            return Some(post.status);
        }
        match post.status {
            PostStatus::Inherit => match post.parent_id {
                None => Some(PostStatus::Publish),
                Some(parent_id) if parent_id == post.id => Some(PostStatus::Publish),
                Some(_) => {
                    let parent = self.parent()?;
                    match parent.status()? {
                        PostStatus::Trash => {
                            Some(parent.trash_meta_status.unwrap_or(PostStatus::Publish))
                        }
                        status => Some(status),
                    }
                }
            },
            status @ (PostStatus::Private | PostStatus::Trash | PostStatus::AutoDraft) => {
                Some(status)
            }
            _ => Some(PostStatus::Publish),
        }
    }

    /// Whether `user_id` wrote this post. A post with no author (id 0) is nobody's.
    fn is_by(&self, user_id: i64) -> bool {
        self.post.author_id != 0 && self.post.author_id == user_id
    }

    /// Whether the post is published or scheduled, or was before it was trashed.
    fn was_published(&self) -> bool {
        match self.post.status {
            PostStatus::Publish | PostStatus::Future => true,
            PostStatus::Trash => matches!(
                self.trash_meta_status,
                Some(PostStatus::Publish | PostStatus::Future)
            ),
            _ => false,
        }
    }
}

/// The primitive capability names of one post type, as WordPress's
/// `get_post_type_capabilities` derives them from a `capability_type` with `map_meta_cap` on:
/// `post` gives `edit_posts`, `edit_others_posts`, ...; `page` gives `edit_pages`, ...
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostTypeCaps {
    /// The meta capability `edit_{singular}`, such as `edit_book`: mapped as `edit_post` on a
    /// post whose [`PostContext::post_type_caps`] name it (WordPress's `$post_type_meta_caps`).
    pub edit_post: String,
    /// The meta capability `read_{singular}`, mapped as `read_post`.
    pub read_post: String,
    /// The meta capability `delete_{singular}`, mapped as `delete_post`.
    pub delete_post: String,
    /// `read`.
    pub read: String,
    /// `edit_{plural}`.
    pub edit_posts: String,
    /// `edit_others_{plural}`.
    pub edit_others_posts: String,
    /// `edit_published_{plural}`.
    pub edit_published_posts: String,
    /// `edit_private_{plural}`.
    pub edit_private_posts: String,
    /// `delete_{plural}`.
    pub delete_posts: String,
    /// `delete_others_{plural}`.
    pub delete_others_posts: String,
    /// `delete_published_{plural}`.
    pub delete_published_posts: String,
    /// `delete_private_{plural}`.
    pub delete_private_posts: String,
    /// `publish_{plural}`.
    pub publish_posts: String,
    /// `read_private_{plural}`.
    pub read_private_posts: String,
}

impl PostTypeCaps {
    /// The capability names for a `capability_type` such as `post` or `page`, whose plural is
    /// the name with an `s`, as WordPress makes it.
    pub fn for_capability_type(capability_type: &str) -> Self {
        Self::for_singular_and_plural(capability_type, &format!("{capability_type}s"))
    }

    /// The capability names for a `capability_type` given as `[singular, plural]`, such as
    /// `["story", "stories"]`: the meta capabilities take the singular (`edit_story`), the
    /// primitive ones the plural (`edit_stories`).
    pub fn for_singular_and_plural(singular: &str, plural: &str) -> Self {
        Self {
            edit_post: format!("edit_{singular}"),
            read_post: format!("read_{singular}"),
            delete_post: format!("delete_{singular}"),
            read: "read".to_owned(),
            edit_posts: format!("edit_{plural}"),
            edit_others_posts: format!("edit_others_{plural}"),
            edit_published_posts: format!("edit_published_{plural}"),
            edit_private_posts: format!("edit_private_{plural}"),
            delete_posts: format!("delete_{plural}"),
            delete_others_posts: format!("delete_others_{plural}"),
            delete_published_posts: format!("delete_published_{plural}"),
            delete_private_posts: format!("delete_private_{plural}"),
            publish_posts: format!("publish_{plural}"),
            read_private_posts: format!("read_private_{plural}"),
        }
    }

    /// Every capability name set to `cap`, the meta capability names included.
    pub fn uniform(cap: &str) -> Self {
        Self {
            edit_post: cap.to_owned(),
            read_post: cap.to_owned(),
            delete_post: cap.to_owned(),
            read: cap.to_owned(),
            edit_posts: cap.to_owned(),
            edit_others_posts: cap.to_owned(),
            edit_published_posts: cap.to_owned(),
            edit_private_posts: cap.to_owned(),
            delete_posts: cap.to_owned(),
            delete_others_posts: cap.to_owned(),
            delete_published_posts: cap.to_owned(),
            delete_private_posts: cap.to_owned(),
            publish_posts: cap.to_owned(),
            read_private_posts: cap.to_owned(),
        }
    }

    /// The post meta capability (`edit_post`, `read_post` or `delete_post`) that `cap` names
    /// on this type, when `cap` is one of the type's meta capability names (`edit_book`) and not
    /// also one of its primitive names (a [`uniform`](Self::uniform) type's single name).
    fn meta_cap(&self, cap: &str) -> Option<&'static str> {
        let primitive = [
            &self.read,
            &self.edit_posts,
            &self.edit_others_posts,
            &self.edit_published_posts,
            &self.edit_private_posts,
            &self.delete_posts,
            &self.delete_others_posts,
            &self.delete_published_posts,
            &self.delete_private_posts,
            &self.publish_posts,
            &self.read_private_posts,
        ];
        if primitive.iter().any(|name| *name == cap) {
            return None;
        }
        if cap == self.edit_post {
            Some("edit_post")
        } else if cap == self.read_post {
            Some("read_post")
        } else if cap == self.delete_post {
            Some("delete_post")
        } else {
            None
        }
    }

    /// The capability names of `post_type` when its registration is not given, by the interim
    /// rule of ruling R24:
    ///
    /// - `post`, `attachment` and `revision` use the `post` names, and `page` the `page` names,
    ///   as WordPress registers them.
    /// - WordPress's theme types (`nav_menu_item`, `wp_template`, `wp_template_part`,
    ///   `wp_global_styles`, `wp_navigation`) need `edit_theme_options` for everything. WordPress
    ///   lets anyone with `read` read a menu item; here reading one needs `edit_theme_options`
    ///   too.
    /// - `customize_changeset` needs `customize` for everything, but editing a published
    ///   changeset is never allowed, as WordPress registers it.
    /// - Every other type is taken as not registered, so everything on it needs
    ///   `edit_others_posts`, as WordPress maps a post of an unregistered type.
    ///
    /// A registered custom type's names come from its definition
    /// ([`PostTypeDef::caps`](crate::types::PostTypeDef::caps)); a check that gives them in
    /// [`PostContext::post_type_caps`] uses them instead.
    pub fn for_post_type(post_type: &str) -> Self {
        match post_type {
            "post" | "attachment" | "revision" => Self::for_capability_type("post"),
            "page" => Self::for_capability_type("page"),
            "nav_menu_item" | "wp_template" | "wp_template_part" | "wp_global_styles"
            | "wp_navigation" => Self::uniform("edit_theme_options"),
            "customize_changeset" => Self {
                edit_published_posts: DO_NOT_ALLOW.to_owned(),
                ..Self::uniform("customize")
            },
            _ => Self::uniform("edit_others_posts"),
        }
    }
}

/// The primitive capabilities `user` must hold, all of them, for the capability `cap` on
/// `object`, as WordPress's `map_meta_cap` gives them. A meta capability with no object, or on
/// an object it cannot be checked on, maps to [`DO_NOT_ALLOW`]; a primitive capability maps to
/// itself.
pub fn map_meta_cap(cap: &str, user: &UserCaps, object: Option<&Object>) -> Vec<String> {
    let caps = match cap {
        "edit_post" | "edit_page" => {
            on_post(object, |post| edit_or_delete(user, post, Action::Edit))
        }
        "delete_post" | "delete_page" => on_post(object, |post| {
            if post.post.post_type == "revision" {
                return deny();
            }
            edit_or_delete(user, post, Action::Delete)
        }),
        "read_post" | "read_page" => on_post(object, |post| read(user, post)),
        "publish_post" => on_post(object, |post| vec![post.type_caps().publish_posts]),
        "edit_post_meta" | "add_post_meta" | "delete_post_meta" => {
            on_post(object, |post| post_meta(cap, user, post, object))
        }
        "manage_post_tags" | "edit_categories" | "edit_post_tags" | "delete_categories"
        | "delete_post_tags" => vec!["manage_categories".to_owned()],
        "assign_categories" | "assign_post_tags" => vec!["edit_posts".to_owned()],
        "edit_term" | "delete_term" | "assign_term" => match object {
            Some(Object::Term(term)) => {
                if cap == "delete_term" && term.default_terms.contains(&term.term_id) {
                    deny()
                } else {
                    let caps = term
                        .caps
                        .cloned()
                        .unwrap_or_else(|| TaxonomyCaps::for_taxonomy(term.taxonomy));
                    let primitive = match cap {
                        "edit_term" => caps.edit_terms,
                        "delete_term" => caps.delete_terms,
                        _ => caps.assign_terms,
                    };
                    map_meta_cap(&primitive, user, None)
                }
            }
            _ => deny(),
        },
        // A custom type's own meta capability names (`edit_book`), as WordPress's
        // `$post_type_meta_caps` maps them for a type registered with `map_meta_cap`: the same
        // as `edit_post`, `read_post` or `delete_post` on the post.
        _ => match object
            .and_then(Object::post)
            .and_then(|post| post.post_type_caps)
            .and_then(|caps| caps.meta_cap(cap))
        {
            Some(meta) => return map_meta_cap(meta, user, object),
            None => vec![cap.to_owned()],
        },
    };
    dedup(caps)
}

/// `caps` without repeats, in order. A uniform type, such as an unregistered one, would
/// otherwise name `edit_others_posts` twice.
fn dedup(caps: Vec<String>) -> Vec<String> {
    let mut seen = Vec::with_capacity(caps.len());
    for cap in caps {
        if !seen.contains(&cap) {
            seen.push(cap);
        }
    }
    seen
}

/// Whether `key` is protected: its first printable ASCII character (0x20 to 0x7E) is `_`.
///
/// WordPress's `is_protected_meta` drops the bytes that are neither printable ASCII nor
/// letters (`/[^\x20-\x7E\p{L}]/` without the `u` flag, so it tests bytes, not characters) and
/// looks at the first byte left. Skipping every non-ASCII character here protects at least the
/// keys WordPress protects, and sometimes more (`é_x`), never fewer.
pub(crate) fn is_protected_meta(key: &str) -> bool {
    key.chars().find(|c| matches!(c, ' '..='~')) == Some('_')
}

/// The `edit_post_meta`, `add_post_meta` and `delete_post_meta` branch of `map_meta_cap`:
/// what editing the post needs, and, for a protected meta key, the meta capability itself,
/// which nobody holds unless granted by name. With no key given (any object but
/// [`Object::PostMeta`]), the key is taken to be protected, so the check fails closed.
fn post_meta(
    cap: &str,
    user: &UserCaps,
    post: PostContext,
    object: Option<&Object>,
) -> Vec<String> {
    let mut caps = edit_or_delete(user, post, Action::Edit);
    let protected = match object {
        Some(Object::PostMeta { key, .. }) => is_protected_meta(key),
        _ => true,
    };
    if protected {
        caps.push(cap.to_owned());
    }
    caps
}

/// Whether `user` may do `cap`, on `object` when it is a meta capability: `cap` is mapped with
/// [`map_meta_cap`] and every primitive capability it maps to must be held, as WordPress's
/// `user_can` decides. `can(user, "edit_post", Some(&Object::Post(&post)))` is WordPress's
/// `user_can( $user, 'edit_post', $post->ID )`.
pub fn can(user: &UserCaps, cap: &str, object: Option<&Object>) -> bool {
    map_meta_cap(cap, user, object)
        .iter()
        .all(|primitive| has_cap(user, primitive))
}

/// The capabilities nobody holds.
fn deny() -> Vec<String> {
    vec![DO_NOT_ALLOW.to_owned()]
}

/// Maps a post meta capability with `map` when `object` is a post, or to [`DO_NOT_ALLOW`] when
/// there is no object or it is not a post.
fn on_post(object: Option<&Object>, map: impl FnOnce(PostContext) -> Vec<String>) -> Vec<String> {
    match object.and_then(Object::post) {
        Some(post) => map(post),
        None => deny(),
    }
}

/// `edit_post` or `delete_post`, whose rules are the same with their own capability names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// `edit_post`.
    Edit,
    /// `delete_post`.
    Delete,
}

/// The `edit_post` and `delete_post` branches of `map_meta_cap`. A revision is checked as its
/// parent (for `edit_post`; `delete_post` refuses revisions before this). The author needs
/// `edit_published_posts` for a published, scheduled, or once published trashed post, and
/// `edit_posts` otherwise; anyone else needs `edit_others_posts`, with `edit_published_posts`
/// for a published or scheduled post or `edit_private_posts` for a private one.
///
/// The site's special pages come first, as in WordPress: deleting the front page or the posts
/// page needs `manage_options` alone, and editing or deleting the privacy policy page adds
/// `manage_options` (WordPress's `manage_privacy_options` on a single site). When the special
/// pages are not given, a page might be any of them, so `manage_options` is added to both
/// editing and deleting it: the strictest of WordPress's outcomes.
fn edit_or_delete(user: &UserCaps, post: PostContext, action: Action) -> Vec<String> {
    let Some(post) = resolve_revision(post) else {
        return deny();
    };
    let id = Some(post.post.id);
    let front_or_posts_page = post
        .site_pages
        .is_some_and(|pages| pages.page_on_front == id || pages.page_for_posts == id);
    if action == Action::Delete && front_or_posts_page {
        return vec![MANAGE_OPTIONS.to_owned()];
    }
    let mut caps = primitive_caps(user, post, action);
    let privacy = match post.site_pages {
        Some(pages) => pages.privacy_policy == id,
        None => post.post.post_type == "page",
    };
    if privacy {
        caps.push(MANAGE_OPTIONS.to_owned());
    }
    caps
}

/// The capability WordPress requires for the site's special pages.
const MANAGE_OPTIONS: &str = "manage_options";

/// The post type capabilities `edit_post` or `delete_post` needs on `post`, before the special
/// pages are counted.
fn primitive_caps(user: &UserCaps, post: PostContext, action: Action) -> Vec<String> {
    let caps = post.type_caps();
    let (own, published, others, private) = match action {
        Action::Edit => (
            caps.edit_posts,
            caps.edit_published_posts,
            caps.edit_others_posts,
            caps.edit_private_posts,
        ),
        Action::Delete => (
            caps.delete_posts,
            caps.delete_published_posts,
            caps.delete_others_posts,
            caps.delete_private_posts,
        ),
    };
    if post.is_by(user.id()) {
        return vec![if post.was_published() { published } else { own }];
    }
    match post.post.status {
        PostStatus::Publish | PostStatus::Future => vec![others, published],
        PostStatus::Private => vec![others, private],
        _ => vec![others],
    }
}

/// The `read_post` branch of `map_meta_cap`. A published post needs `read`, as does the
/// author's own post; someone else's private post needs `read_private_posts`; any other post
/// (scheduled, draft, pending, trashed) needs what editing it needs.
fn read(user: &UserCaps, post: PostContext) -> Vec<String> {
    let Some(post) = resolve_revision(post) else {
        return deny();
    };
    let Some(status) = post.status() else {
        return deny();
    };
    let caps = post.type_caps();
    if status == PostStatus::Publish || post.is_by(user.id()) {
        return vec![caps.read];
    }
    if status == PostStatus::Private {
        return vec![caps.read_private_posts];
    }
    edit_or_delete(user, post, Action::Edit)
}

/// The post a check is made on: a revision's parent, or the post itself. `None` for a revision
/// whose parent was not given, which WordPress refuses as a missing post. The parent keeps the
/// revision's special pages when it was given none of its own.
fn resolve_revision(post: PostContext) -> Option<PostContext> {
    if post.post.post_type == "revision" {
        let mut parent = *post.parent()?;
        parent.site_pages = parent.site_pages.or(post.site_pages);
        Some(parent)
    } else {
        Some(post)
    }
}
