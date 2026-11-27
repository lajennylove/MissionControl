//! Who may use the admin screens of a post type, and what each user sees there (Task A1).
//!
//! The screens are Ignite's generic list, form and delete flows over the post type's runtime
//! resource ([`PostTypeResources`](crate::types::PostTypeResources)). Since Ignite 0.6.0 the
//! panel passes the signed-in user to the resource's queries and asks a [`Policy`] for each
//! action, as Filament asks a Laravel policy, so the screens follow WordPress's capabilities
//! for each user and post:
//!
//! - [`PostTypePolicy`], registered for each post type's slug: the list and the New screen need
//!   the type's `edit_posts` (`edit.php`, `post-new.php`); editing or saving a post needs
//!   `edit_post` on it, and a trashed post is not edited at all (`post.php` answers 409, "You
//!   cannot edit this item because it is in the Trash. Please restore it and try again."), so
//!   its list row has no Edit; Ignite's Delete needs `delete_post` and is for a trashed post
//!   only, as WordPress's list offers "Delete Permanently" only in the Trash (the other rows
//!   have the resource's "Trash" row action). Each is mapped as `map_meta_cap` maps it, with
//!   the site's special pages (an editor may not edit the privacy policy page).
//! - The resource's own queries ([`PostTypeData`](crate::types::PostTypeData)'s `list_for` and
//!   `find_for`) leave out what the user may not see, and its `update_for` writes what WordPress's
//!   `edit_post` writes for them: kses for a user without `unfiltered_html`, no author change
//!   without `edit_others_posts`, "pending" for a user without `publish_posts`.
//!
//! [`authorize_post_screen`] makes the same checks with WordPress's refusal messages, for the
//! routes Mission Control serves itself (the New screen) and for the media library's screen.
//!
//! ```ignore
//! let screen = PostScreen::parse("GET", &["12", "edit"]);
//! if let Some(refusal) = authorize_post_screen(&db, &caps, &def, screen).await? {
//!     // answer refusal.status() with refusal.message()
//! }
//! ```

use ignite_core::{ActingUser, Policy, Row, RuntimeFuture};
use orbit_orm::{Database, Error, Model};
use serde_json::Value;

use crate::content::{MetaError, Post, PostStatus, get_option, get_post_meta};
use crate::rest::id_of;
use crate::types::{Access, PostTypeDef};
use crate::users::{Object, PostContext, SitePages, UserCaps, can, has_cap};

/// One request on the screens of a post type, read from its method and the path segments after
/// `<mount>/<post type>`, as Ignite's routes answer them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostScreen {
    /// `GET <slug>`: the list (`edit.php`).
    List,
    /// `GET <slug>/new`: the New screen (`post-new.php`).
    New,
    /// `POST <slug>`: Ignite's create, the New screen's first save (`post.php`'s `post` action,
    /// `wp_write_post`).
    Create,
    /// `GET <slug>/<id>/edit`: the edit form (`post.php?action=edit`).
    Edit(i64),
    /// `POST <slug>/<id>`: a save of the edit form.
    Update(i64),
    /// `POST <slug>/<id>/delete`: Delete, which moves a post to the trash, or deletes a trashed
    /// post for good (Ruling R117).
    Delete(i64),
    /// Any other request under the post type, which Ignite answers with 404.
    Other,
}

impl PostScreen {
    /// The screen `method` (`GET`, `HEAD`, `POST`, ...) asks for with `rest`, the path segments
    /// after the post type. An id that is not a positive whole number is [`Other`](Self::Other):
    /// Ignite answers it with 404.
    pub fn parse(method: &str, rest: &[&str]) -> Self {
        let read = matches!(method, "GET" | "HEAD");
        let write = method == "POST";
        let id = |text: &str| text.parse::<i64>().ok().filter(|id| *id > 0);
        match rest {
            [] if read => Self::List,
            [] if write => Self::Create,
            ["new"] if read => Self::New,
            [id_text, "edit"] if read => id(id_text).map_or(Self::Other, Self::Edit),
            [id_text] if write => id(id_text).map_or(Self::Other, Self::Update),
            [id_text, "delete"] if write => id(id_text).map_or(Self::Other, Self::Delete),
            _ => Self::Other,
        }
    }
}

/// Why a request on an admin screen (the post type screens, the media library, the settings
/// screens and the menus) is refused, with WordPress's message for it. Every refusal is a 403.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The user lacks the type's capability set (`edit.php`'s refusal).
    Screen,
    /// The user lacks the type's capability set, on the New screen (`post-new.php`'s refusal).
    NewScreen,
    /// The user lacks the type's capability set, on the New screen's save (`wp_write_post`'s
    /// refusal).
    Create,
    /// `edit_post` is refused on the post.
    EditPost,
    /// `delete_post` is refused on a post that is not in the trash.
    TrashPost,
    /// `delete_post` is refused on a trashed post.
    DeletePost,
    /// A save of a trashed post (which restores it, or keeps it in the trash) needs
    /// `delete_post`, as `wp_untrash_post` does, and it is refused.
    RestorePost,
    /// The post is in the trash: its edit form and its save are refused for everyone, as
    /// `post.php` refuses to edit a trashed post (with 409). Restore it first.
    InTrash,
    /// The media library's upload page (`<mount>/attachment/new`, WordPress's `media-new.php`)
    /// is refused: the user lacks the screen's capabilities or `upload_files`.
    UploadFiles,
    /// A settings screen needs `manage_options` (`options-general.php` and the others).
    ManageOptions,
    /// The Privacy settings need `manage_privacy_options` (`options-privacy.php`).
    ManagePrivacyOptions,
    /// The Menus screen needs `edit_theme_options` (`nav-menus.php`).
    EditThemeOptions,
    /// A page the admin does not have, such as New or Delete on a settings screen: WordPress's
    /// answer for an admin page it does not know.
    NoSuchPage,
    /// Delete on the default category (the `default_category` option), which WordPress never
    /// deletes (`wp_delete_term` refuses it).
    DefaultCategory,
}

impl Refusal {
    /// The HTTP status: 403, as `wp_die` answers these, or 409 for a post in the trash.
    pub fn status(self) -> u16 {
        match self {
            Self::InTrash => 409,
            _ => 403,
        }
    }

    /// The heading WordPress's error page shows, if any.
    pub fn heading(self) -> Option<&'static str> {
        match self {
            Self::Screen | Self::NewScreen | Self::EditThemeOptions => {
                Some("You need a higher level of permission.")
            }
            Self::Create
            | Self::EditPost
            | Self::TrashPost
            | Self::DeletePost
            | Self::RestorePost
            | Self::InTrash
            | Self::UploadFiles
            | Self::ManageOptions
            | Self::ManagePrivacyOptions
            | Self::NoSuchPage
            | Self::DefaultCategory => None,
        }
    }

    /// WordPress's message.
    pub fn message(self) -> &'static str {
        match self {
            Self::Screen => "Sorry, you are not allowed to edit posts in this post type.",
            Self::NewScreen => "Sorry, you are not allowed to create posts as this user.",
            Self::Create => "Sorry, you are not allowed to create posts as this user.",
            Self::EditPost => "Sorry, you are not allowed to edit this item.",
            Self::TrashPost => "Sorry, you are not allowed to move this item to the Trash.",
            Self::DeletePost => "Sorry, you are not allowed to delete this item.",
            Self::RestorePost => "Sorry, you are not allowed to restore this item from the Trash.",
            Self::InTrash => {
                "You cannot edit this item because it is in the Trash. Please restore it and try \
                 again."
            }
            Self::UploadFiles => "Sorry, you are not allowed to upload files.",
            Self::ManageOptions => "Sorry, you are not allowed to manage options for this site.",
            Self::ManagePrivacyOptions => {
                "Sorry, you are not allowed to manage privacy options on this site."
            }
            Self::EditThemeOptions => {
                "Sorry, you are not allowed to edit theme options on this site."
            }
            Self::NoSuchPage => "Sorry, you are not allowed to access this page.",
            Self::DefaultCategory => "Sorry, you are not allowed to delete this item.",
        }
    }
}

/// The capabilities a user must hold to use the media library's screen (Ruling R115, which the
/// post type screens no longer use since Task A1):
/// every primitive capability of the type (`edit_posts`, `edit_others_posts`,
/// `edit_published_posts`, `edit_private_posts`, `publish_posts`, `read_private_posts`,
/// `delete_posts`, `delete_others_posts`, `delete_published_posts`, `delete_private_posts`, in
/// the type's names) and `unfiltered_html`.
pub fn post_screen_caps(def: &PostTypeDef) -> Vec<String> {
    let caps = def.caps();
    vec![
        caps.edit_posts,
        caps.edit_others_posts,
        caps.edit_published_posts,
        caps.edit_private_posts,
        caps.publish_posts,
        caps.read_private_posts,
        caps.delete_posts,
        caps.delete_others_posts,
        caps.delete_published_posts,
        caps.delete_private_posts,
        "unfiltered_html".to_owned(),
    ]
}

/// Whether `user` holds every capability of [`post_screen_caps`] for `def`.
pub fn may_use_post_screens(user: &UserCaps, def: &PostTypeDef) -> bool {
    post_screen_caps(def).iter().all(|cap| has_cap(user, cap))
}

/// Why `user` may not make the request `screen` on the screens of the post type `def`, or `None`
/// when they may, as WordPress's `edit.php`, `post-new.php` and `post.php` decide. A post that
/// does not exist, or is of another type, is not refused here: Ignite answers it with 404, as
/// WordPress says the item does not exist.
///
/// # Errors
///
/// The database's error, while reading the post, its trash status or the site's special pages.
pub async fn authorize_post_screen(
    db: &Database,
    user: &UserCaps,
    def: &PostTypeDef,
    screen: PostScreen,
) -> Result<Option<Refusal>, Error> {
    let caps = def.caps();
    let (id, edit) = match screen {
        PostScreen::Other => return Ok(None),
        // `edit.php`, `post-new.php` and its save (`wp_write_post`) need the type's
        // `edit_posts` (`create_posts` is `edit_posts` unless a type maps it).
        PostScreen::List | PostScreen::New | PostScreen::Create => {
            if has_cap(user, &caps.edit_posts) {
                return Ok(None);
            }
            return Ok(Some(match screen {
                PostScreen::New => Refusal::NewScreen,
                PostScreen::Create => Refusal::Create,
                _ => Refusal::Screen,
            }));
        }
        PostScreen::Edit(id) | PostScreen::Update(id) => (id, true),
        PostScreen::Delete(id) => (id, false),
    };
    let Some(post) = Post::query()
        .find(db, id)
        .await?
        .filter(|post| post.post_type == def.slug)
    else {
        return Ok(None);
    };
    let context = PostContext {
        post: &post,
        trash_meta_status: trash_status(db, &post).await?,
        parent: None,
        site_pages: Some(site_pages(db).await?),
        post_type_caps: Some(&caps),
    };
    let object = Object::PostWithContext(context);
    let trashed = post.status == PostStatus::Trash;
    if !edit {
        if can(user, "delete_post", Some(&object)) {
            return Ok(None);
        }
        return Ok(Some(if trashed {
            Refusal::DeletePost
        } else {
            Refusal::TrashPost
        }));
    }
    if !can(user, "edit_post", Some(&object)) {
        return Ok(Some(Refusal::EditPost));
    }
    // `post.php`: a trashed post is restored from the list (its Restore row action), not edited.
    if trashed {
        return Ok(Some(Refusal::InTrash));
    }
    Ok(None)
}

/// The policy of a post type's resource (Task A1): each of Ignite's actions decided by
/// [`authorize_post_screen`] for the acting user's capabilities, read through `access`. Ignite
/// asks it after the user's coarse ability (`post.update`, ...), as a Filament Shield policy is
/// asked after the permission. Register it with [`ignite_core::register_policy`] under the type's
/// slug ([`PostTypeResources::sync`](crate::types::PostTypeResources::sync) does).
pub struct PostTypePolicy {
    def: PostTypeDef,
    access: Access,
}

impl PostTypePolicy {
    /// The policy of the post type `def`, reading users' capabilities through `access`.
    pub fn new(def: PostTypeDef, access: Access) -> Self {
        Self { def, access }
    }

    /// Whether `user` may make the request `screen`.
    async fn allows(
        &self,
        db: &Database,
        user: &ActingUser,
        screen: PostScreen,
    ) -> Result<bool, Error> {
        let caps = self.access.caps(db, user).await?;
        Ok(authorize_post_screen(db, &caps, &self.def, screen)
            .await?
            .is_none())
    }
}

/// The id of a record Ignite gives a policy, when it has one.
fn record_id(record: &Row) -> Option<i64> {
    record.get("id").and_then(id_of)
}

impl Policy for PostTypePolicy {
    fn view_any<'a>(&'a self, db: &'a Database, user: &'a ActingUser) -> RuntimeFuture<'a, bool> {
        Box::pin(self.allows(db, user, PostScreen::List))
    }

    fn view<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(async move {
            match record_id(record) {
                Some(id) => self.allows(db, user, PostScreen::Edit(id)).await,
                // A record without an id is no post: refused.
                None => Ok(false),
            }
        })
    }

    fn create<'a>(&'a self, db: &'a Database, user: &'a ActingUser) -> RuntimeFuture<'a, bool> {
        Box::pin(self.allows(db, user, PostScreen::New))
    }

    fn update<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(async move {
            match record_id(record) {
                Some(id) => self.allows(db, user, PostScreen::Update(id)).await,
                // A record without an id is no post: refused.
                None => Ok(false),
            }
        })
    }

    /// Ignite's Delete, and the row actions that need the delete ability (Restore and Delete
    /// Permanently): `delete_post`, on a post in the trash only. A post out of the trash goes
    /// there with the resource's "Trash" row action, as WordPress's list has no Delete for it
    /// (`WP_Posts_List_Table::handle_row_actions`, with `EMPTY_TRASH_DAYS` on).
    fn delete<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        record: &'a Row,
    ) -> RuntimeFuture<'a, bool> {
        Box::pin(async move {
            let Some(id) = record_id(record) else {
                // A record without an id is no post: refused.
                return Ok(false);
            };
            let trashed = Post::query()
                .find(db, id)
                .await?
                .is_some_and(|post| post.status == PostStatus::Trash);
            Ok(trashed && self.allows(db, user, PostScreen::Delete(id)).await?)
        })
    }
}

/// The status a trashed post had before (`_wp_trash_meta_status`), as the REST layer reads it.
pub(crate) async fn trash_status(db: &Database, post: &Post) -> Result<Option<PostStatus>, Error> {
    if post.status != PostStatus::Trash {
        return Ok(None);
    }
    match get_post_meta(db, post.id, "_wp_trash_meta_status").await {
        Ok(Some(Value::String(status))) => Ok(status.parse().ok()),
        Ok(_) | Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e),
    }
}

/// The site's special pages, from their options; a value that is not valid JSON is unset.
pub(crate) async fn site_pages(db: &Database) -> Result<SitePages, Error> {
    let page = async |name: &str| -> Result<Option<i64>, Error> {
        match get_option(db, name).await {
            Ok(value) => Ok(value.as_ref().and_then(id_of)),
            Err(MetaError::InvalidJson(_)) => Ok(None),
            Err(MetaError::Database(e)) => Err(e),
        }
    };
    Ok(SitePages {
        page_on_front: page("page_on_front").await?,
        page_for_posts: page("page_for_posts").await?,
        privacy_policy: page("wp_page_for_privacy_policy").await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_map_to_screens() {
        assert_eq!(PostScreen::parse("GET", &[]), PostScreen::List);
        assert_eq!(PostScreen::parse("HEAD", &[]), PostScreen::List);
        assert_eq!(PostScreen::parse("POST", &[]), PostScreen::Create);
        assert_eq!(PostScreen::parse("GET", &["new"]), PostScreen::New);
        assert_eq!(
            PostScreen::parse("GET", &["12", "edit"]),
            PostScreen::Edit(12)
        );
        assert_eq!(PostScreen::parse("POST", &["12"]), PostScreen::Update(12));
        assert_eq!(
            PostScreen::parse("POST", &["12", "delete"]),
            PostScreen::Delete(12)
        );
        assert_eq!(PostScreen::parse("GET", &["x", "edit"]), PostScreen::Other);
        assert_eq!(PostScreen::parse("POST", &["0"]), PostScreen::Other);
        assert_eq!(PostScreen::parse("PUT", &["12"]), PostScreen::Other);
        assert_eq!(PostScreen::parse("GET", &["12"]), PostScreen::Other);
    }

    #[test]
    fn the_screens_need_the_whole_capability_set_and_unfiltered_html() {
        use std::collections::BTreeMap;

        use crate::types::builtin_post_types;
        use crate::users::{ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, SUBSCRIBER};

        let [post, page] = builtin_post_types();
        let caps = |role| UserCaps::with_default_roles(1, &[role], &BTreeMap::new());
        for def in [&post, &page] {
            assert!(may_use_post_screens(&caps(ADMINISTRATOR), def));
            assert!(may_use_post_screens(&caps(EDITOR), def));
            for role in [AUTHOR, CONTRIBUTOR, SUBSCRIBER] {
                assert!(!may_use_post_screens(&caps(role), def), "{role}");
            }
        }
        assert!(post_screen_caps(&page).contains(&"edit_others_pages".to_owned()));
        // An editor with `unfiltered_html` taken away is refused, as kses would apply to them.
        let mut without = BTreeMap::new();
        without.insert("unfiltered_html".to_owned(), false);
        let editor = UserCaps::with_default_roles(1, &[EDITOR], &without);
        assert!(!may_use_post_screens(&editor, &post));
    }
}
