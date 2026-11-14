//! Meta capabilities: a check on one object, such as `edit_post` on post 12, mapped to the
//! primitive capabilities a user must hold, as WordPress's `map_meta_cap` maps them.
//!
//! Ported from WordPress 6.7 (`wp-includes/capabilities.php`, single site) for the post meta
//! capabilities: `edit_post`, `delete_post`, `read_post` and `publish_post`, their `_page`
//! names, and `edit_post_meta`, `add_post_meta` and `delete_post_meta`. Every other
//! capability maps to itself, so `edit_posts` or `read_private_posts` are checked as they are.
//!
//! What this port leaves out, for now:
//!
//! - **Post type capabilities.** WordPress derives a type's capability names from its
//!   `capability_type` (see [`PostTypeCaps`]). There is no post type registry yet: `page` uses
//!   the `page` names and every other type the `post` names.
//!   TODO(Task 5.2): read `capability_type` from the post type registry, and map a custom type's
//!   own meta capabilities (`edit_book`), as WordPress's `$post_type_meta_caps` does.
//! - **Site options.** WordPress requires `manage_options` to delete the front page or the posts
//!   page, and adds it to editing or deleting the privacy policy page. Those pages are options
//!   this pure function does not read; the caller must check them until it does.
//! - **Protected meta keys.** `edit_post_meta` with a meta key starting with `_` is refused
//!   unless a filter allows it. The key is not part of [`Object`] yet, so only the `edit_post`
//!   part is mapped.
//! - **The `map_meta_cap` and `user_has_cap` filters**, and the user, comment, term, plugin,
//!   theme and multisite meta capabilities: they map to themselves here, so they are refused
//!   unless granted by name.

use crate::content::{Post, PostStatus};

use super::capabilities::{DO_NOT_ALLOW, UserCaps, has_cap};

/// The object a meta capability is checked on.
#[derive(Debug, Clone, Copy)]
pub enum Object<'a> {
    /// A post of any type, with nothing known beyond its row: a trashed post counts as one
    /// trashed from a draft (as WordPress does when `_wp_trash_meta_status` is missing), and a
    /// revision, or an attachment with a parent, cannot be checked (its parent is not given).
    Post(&'a Post),
    /// A post with what WordPress reads beside its row: its status before trashing and its
    /// parent.
    PostWithContext(PostContext<'a>),
}

impl<'a> Object<'a> {
    /// The post and its context.
    fn post(&self) -> PostContext<'a> {
        match *self {
            Self::Post(post) => PostContext::new(post),
            Self::PostWithContext(context) => context,
        }
    }
}

/// A post and the facts `map_meta_cap` reads beside its row.
#[derive(Debug, Clone, Copy)]
pub struct PostContext<'a> {
    /// The post.
    pub post: &'a Post,
    /// For a trashed post, the status it had before, from its `_wp_trash_meta_status` meta.
    pub trash_meta_status: Option<PostStatus>,
    /// The post's parent: the post a revision belongs to, or an attachment is attached to.
    pub parent: Option<&'a PostContext<'a>>,
}

impl<'a> PostContext<'a> {
    /// `post` with no trash status and no parent known.
    pub fn new(post: &'a Post) -> Self {
        Self {
            post,
            trash_meta_status: None,
            parent: None,
        }
    }

    /// The parent, when the post has one and it was given.
    fn parent(&self) -> Option<&'a PostContext<'a>> {
        self.parent.filter(|_| self.post.parent_id.is_some())
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
        let plural = format!("{capability_type}s");
        Self {
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

    /// The capability names of `post_type`: `page` uses the `page` names, every other type the
    /// `post` names.
    ///
    /// TODO(Task 5.2): read the type's `capability_type` (and any custom capability names) from
    /// the post type registry.
    pub fn for_post_type(post_type: &str) -> Self {
        Self::for_capability_type(if post_type == "page" { "page" } else { "post" })
    }
}

/// The primitive capabilities `user` must hold, all of them, for the capability `cap` on
/// `object`, as WordPress's `map_meta_cap` gives them. A meta capability with no object, or on
/// an object it cannot be checked on, maps to [`DO_NOT_ALLOW`]; a primitive capability maps to
/// itself.
pub fn map_meta_cap(cap: &str, user: &UserCaps, object: Option<&Object>) -> Vec<String> {
    match cap {
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
        "publish_post" => on_post(object, |post| {
            vec![PostTypeCaps::for_post_type(&post.post.post_type).publish_posts]
        }),
        "edit_post_meta" | "add_post_meta" | "delete_post_meta" => {
            map_meta_cap("edit_post", user, object)
        }
        _ => vec![cap.to_owned()],
    }
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
/// there is no object.
fn on_post(object: Option<&Object>, map: impl FnOnce(PostContext) -> Vec<String>) -> Vec<String> {
    match object {
        Some(object) => map(object.post()),
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
fn edit_or_delete(user: &UserCaps, post: PostContext, action: Action) -> Vec<String> {
    let Some(post) = resolve_revision(post) else {
        return deny();
    };
    let caps = PostTypeCaps::for_post_type(&post.post.post_type);
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
    let caps = PostTypeCaps::for_post_type(&post.post.post_type);
    if status == PostStatus::Publish || post.is_by(user.id()) {
        return vec![caps.read];
    }
    if status == PostStatus::Private {
        return vec![caps.read_private_posts];
    }
    edit_or_delete(user, post, Action::Edit)
}

/// The post a check is made on: a revision's parent, or the post itself. `None` for a revision
/// whose parent was not given, which WordPress refuses as a missing post.
fn resolve_revision(post: PostContext) -> Option<PostContext> {
    if post.post.post_type == "revision" {
        post.parent().copied()
    } else {
        Some(post)
    }
}
