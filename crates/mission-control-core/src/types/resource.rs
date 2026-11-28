//! The admin screens of post types: each post type, built-in or custom, as an Ignite runtime
//! resource (design 4.3) whose slug, fields, columns and queries come from its definition.
//!
//! [`post_type_resource`] turns a [`PostTypeDef`] into a [`RuntimeResource`]: its form has the
//! fields of the type's `supports` (as WordPress's edit screen has their boxes), its taxonomies,
//! and the fields of the field groups whose location rules show on the type (design 5.4). Its
//! rows are the type's posts, read and written through [`PostTypeData`], which reuses the core
//! write rules: the slug (`wp_unique_post_slug`), `future` and `publish` settled by the date as
//! `wp_insert_post` settles them, `_wp_old_slug`, term counts, sticky posts, the featured image
//! (`_thumbnail_id`), revisions as `wp_save_post_revision`, the trash as `wp_trash_post` and
//! `wp_untrash_post`, and deleting as `wp_delete_post`.
//!
//! [`PostTypeResources`] registers the resource of every post type the admin shows
//! (`show_ui`) on Ignite, and registers them again when a type is saved or removed, in the admin
//! or by a plugin.
//!
//! # The form
//!
//! The resource's slug is the post type's slug, so `post` is served at `/mc-admin/post` and `book`
//! at `/mc-admin/book`; its title and singular are the labels `name` and `singular_name`. Its
//! fields, in WordPress's names where the REST API has one:
//!
//! | `supports`        | field            | kind                                         |
//! |-------------------|------------------|----------------------------------------------|
//! | `title`           | `title`          | text, required                               |
//! | always            | `slug`           | text: WordPress's Slug box, through the save's slug rules |
//! | `editor`          | `content`        | rich text                                    |
//! | `excerpt`         | `excerpt`        | textarea                                     |
//! | `author`          | `author`         | user: who may edit the type's posts (`post_author_meta_box`), read from Ignite's roles |
//! | `page-attributes` | `parent`         | post object (a hierarchical type only); the post itself is refused ("A page cannot be its own parent.") |
//! | `page` only       | `template`       | select: "Default template" and the theme's page templates, when it has any (`_wp_page_template`) |
//! | `page-attributes` | `menu_order`     | number                                       |
//! | `thumbnail`       | `featured_media` | number: the media id (`_thumbnail_id`); an id that is not an image's is refused with "Invalid featured media ID." (`set_post_thumbnail`) |
//! | always            | `status`         | select: Draft, Pending, Private, Published, Scheduled, and Trash on edit only; blank keeps the post's (Draft on a new post) |
//! | always            | `date`           | date and time, in the site's time zone; blank is "Publish immediately" |
//! | always            | `password`       | text (stored as written, as `post_password`) |
//! | `post` only       | `sticky`         | checkbox; refused with a password ("A post can not be sticky and have a password.") |
//!
//! Each taxonomy the type uses (its `taxonomies`, and those whose `object_type` names it,
//! `category` and `post_tag` for `post`) whose `show_ui` is on is a relationship field
//! `tax_<taxonomy>` (`-` written `_`) holding its terms' ids (a hierarchical taxonomy's choices
//! in tree order, a child indented by three no-break spaces per level), followed by a text field
//! of new terms' names, separated by commas: `new_<taxonomy>` for a hierarchical taxonomy
//! (WordPress's Add New Category), `tax_input_<taxonomy>` for a flat one (the tags typed in the
//! tags box). The save creates each new name the taxonomy does not have and assigns it; a new
//! category needs the taxonomy's `edit_terms`, a new tag its `assign_terms`. The editor's
//! script (`terms.js`) shows them as WordPress's category checklist and tags box. Then come the
//! fields of the field groups that show on the type, in ACF's order of groups; see
//! [`custom_field_name`] for their names. A field that cannot be mapped is left out with a
//! logged reason.
//!
//! The form is laid out as WordPress's editor with its settings sidebar: a main column two
//! thirds wide (title, slug, content, excerpt, field groups) and a sidebar one third wide
//! (Publish, featured image, Page Attributes, taxonomies); see `editor_layout`. The slug follows
//! the title until it is edited by hand (Ignite's `slug_from`), or until the post is published.
//!
//! The list's columns are the title (sortable, searchable), the author, the status, the terms
//! of `category`, `post_tag` and each custom taxonomy with `show_admin_column`, and the date
//! (sortable). Their keys start with `col_`, which no field name does, so a column is never
//! read as a field's stored value. The list has WordPress's status links as tabs (All,
//! Published, Scheduled, Drafts, Pending, Private, Trash; a tab with no posts is left out, All
//! is always shown, and each counts all its posts, whatever the search), and on the All tab
//! the title cell shows the post's state (Draft, Pending, Private or Scheduled) after the title
//! as a small badge; see [`status_tabs`](super::status_tabs).
//!
//! On the edit form, the slug follows the title until it is edited by hand, and not at all once
//! the stored post is published (Ignite's `slug_locked_when`), so a published URL does not change
//! with its title.
//!
//! # Trust
//!
//! Ignite's panel calls the user-aware queries (`list_for`, `find_for`, `update_for`,
//! `delete_for`, `options_for`) with the signed-in user, and asks the type's
//! [`PostTypePolicy`](crate::admin::posts::PostTypePolicy) before each action (Task A1). With the
//! user's capabilities, read through [`Access`], they do what WordPress does for that user:
//!
//! - The list and the records leave out the posts of other users for a user without the type's
//!   `edit_others_posts`, and other users' private posts for one without `read_private_posts`
//!   (a post they leave out is 404 on its form, its save and its Delete).
//! - A save runs kses on the title, content, excerpt and rich text fields for a user without
//!   `unfiltered_html` (`kses_init_filters`); keeps the author for a user without
//!   `edit_others_posts`; turns Published or Scheduled into Pending, and Private into the post's
//!   status, for a user without `publish_posts` (`_wp_translate_postdata`); sets sticky only for
//!   a user with `edit_others_posts` and `publish_posts` (`edit_post`); and moves the post to the
//!   trash, or out of it, only for a user with `delete_post` on it.
//!
//! The methods without a user (`list`, `find`, `update`, `delete`) write as WordPress lets a
//! user with every capability of the type and `unfiltered_html` write; Ignite's panel does not
//! call them.
//!
//! # Creating, saving and deleting
//!
//! - A new post gets no id until it is saved (the owner's rule, which departs from WordPress:
//!   `post-new.php` creates an `auto-draft` with an id when the New screen opens). The New screen
//!   is Ignite's create form, with no record; its first save is [`RuntimeData::create`], which
//!   writes the post with the save's rules and returns it with its id. A blank Status saves a
//!   Draft, and a blank Author makes the user saving it the author, as WordPress's New screen
//!   starts. The title is required, on the New screen and on every save. A save that fails
//!   leaves no post behind.
//! - Auto-drafts made before this rule (or by [`create_auto_draft`]) read as `draft` on the form
//!   and are left out of the list; the daily cleanup deletes them after 7 days (Ruling R125).
//! - A save is the last write (WordPress's classic editor without post locks). One that changes
//!   the title, content or excerpt of a type that supports `revisions` records a revision, as
//!   `wp_save_post_revision` does (Ruling R118).
//! - Saving a trashed post with another status takes it out of the trash first (`draft`, as
//!   WordPress 5.6 and later restore), then applies the form (the data layer's rule; the
//!   admin's policy refuses the form of a trashed post, which is restored from the list).
//!   Choosing Trash moves it to the trash after the save, except on the front page and the
//!   posts page, whose Trash needs `manage_options` (the form refuses it; the list's Trash
//!   action checks `delete_post`, which maps to it).
//! - The trash, as WordPress's list (`WP_Posts_List_Table::handle_row_actions`, `edit.php`):
//!   a post out of the trash has the row action "Trash" (`wp_trash_post`: the slug gets
//!   `__trashed`, so another post can take it, and is kept in `_wp_desired_post_slug`), asked
//!   for nothing; it then shows in the Trash tab, with "Restore" (`wp_untrash_post`: back to
//!   `draft`, with its slug, see [`Post::untrash`]) and "Delete Permanently" (`wp_delete_post`
//!   with its meta, term relationships and revisions, Ruling R117), which asks first with
//!   WordPress's text ([`DELETE_PERMANENTLY_CONFIRMATION`]). Each needs `delete_post` on the
//!   post, and shows only to a user who has it; the flash is WordPress's ("1 post moved to the
//!   Trash."). A trashed post has no Edit: its form is refused (409), as `post.php` refuses it.
//!   Ignite's Delete is for a trashed post only, and deletes it permanently. The list shows
//!   every status but `auto-draft`, with a Status column.
//!
//! Every query is bounded: a list page has at most [`MAX_PER_PAGE`] rows and a field at most
//! [`MAX_OPTIONS`] choices. Values are always bound, never written into the SQL.
//!
//! A stored reference the bounded choices leave out (a term past the limit, a trashed parent)
//! shows unselected on the form and comes back blank; a save keeps it (Ruling R123, as
//! WordPress's dropdowns add the selected value). Ignite's `options` cannot see the post being
//! edited, so the form cannot show it (owner item O9).
//!
//! The page templates are the active theme's
//! ([`Theme::page_templates`](crate::theme::Theme::page_templates)), given to
//! [`PostTypeResources::with_page_templates`]; as WordPress's Page Attributes box, the Template
//! select shows only when the theme has some. A stored value is matched by its template name, so
//! an imported `templates/wide.php` is the theme's `templates/wide`, and is kept as stored while
//! the form leaves it unchanged. A page whose stored template the theme no
//! longer has shows "Default template", and its next save stores none, as `wp_insert_post`
//! resets a template that is not valid. The page set as the posts page still gets the select
//! (WordPress hides it there): the resource does not know which page the form edits.
//!
//! Not on the form yet: comment and ping status (comments, phase 2), the post format, and the
//! field group rules that depend on the post (page template, status, user role) and ACF's
//! `hide_on_screen`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, FixedOffset, NaiveDate, NaiveDateTime};
use ignite_core::{
    ActingUser, ActionAbility, ActionResult, BadgeColor, ColumnDef, Direction,
    FieldDef as IgniteFieldDef, FormLayout, Group, ListFilter, ListQuery, Page, Row, RowAction,
    RuntimeData, RuntimeError, RuntimeFuture, RuntimeResource, Section, ValidationError,
    register_policy, register_runtime, unregister_policy, unregister_runtime,
};
use orbit_orm::{Database, Error, Model, Value as Sql, raw};
use serde_json::{Value, json};

use super::definition::{CapabilityType, FieldDef, FieldKindDef, Origin, PostTypeDef, Support};
use super::location::ScreenContext;
use super::registry::Registry;
use super::status_tabs;
use crate::admin::navigation::{PostTypePositions, place_post_type};
use crate::admin::posts::{PostTypePolicy, Refusal};
use crate::content::{
    MetaError, NewPost, NewTerm, Post, PostStatus, SaveError, Taxonomy, Term, TermError,
    delete_post_meta, get_option, save_revision, set_post_meta,
};
use crate::content::{PostFields, kses_post};
use crate::front::{escape_like, search_terms};
use crate::rest::KsesLimiter;
use crate::rest::id_of;
use crate::rest::posts::{
    loop_free_parent, set_post_terms, set_sticky, settle_status, sync_publication,
};
use crate::theme::{PageTemplate, page_template_name};
use crate::users::{
    IgniteUserRoles, Object, PostContext, PostTypeCaps, UserCaps, UserRoles, can, has_cap,
    users_with_cap,
};

/// The most rows one list page holds, whatever the list asks for.
pub const MAX_PER_PAGE: i64 = 100;

/// The most choices a field loads: users, parent posts or terms. A choice past the limit cannot
/// be picked, and a stored value past it fails the form's check.
pub const MAX_OPTIONS: usize = 1000;

/// The most term names a list cell shows per taxonomy.
const MAX_TERMS_SHOWN: i64 = 20;

/// The most terms of one post the form reads, across its taxonomies.
const MAX_POST_TERMS: i64 = 10_000;

/// The longest search text the list matches, in bytes; longer text is cut.
const MAX_SEARCH_BYTES: usize = 1000;

/// How many times a save is tried again when another save lands between its read and its write.
const MAX_ATTEMPTS: usize = 5;

/// How many field keys one meta read binds.
const KEYS_CHUNK: usize = 200;

// The core fields' names.
const TITLE: &str = "title";
const SLUG: &str = "slug";
const CONTENT: &str = "content";
const EXCERPT: &str = "excerpt";
const AUTHOR: &str = "author";
const PARENT: &str = "parent";
const MENU_ORDER: &str = "menu_order";
const FEATURED_MEDIA: &str = "featured_media";
const STATUS: &str = "status";
const DATE: &str = "date";
const PASSWORD: &str = "password";
const STICKY: &str = "sticky";
const TEMPLATE: &str = "template";

// The columns' keys.
pub(super) const COL_TITLE: &str = "col_title";
const COL_AUTHOR: &str = "col_author";
const COL_STATUS: &str = "col_status";
const COL_DATE: &str = "col_date";

/// The key of each row the user-aware queries give (`list_for`, `find_for`): whether the
/// signed-in user has `delete_post` on the post, which shows the row's "Trash" action. Not a
/// column, and never a field's name (no field name starts with `col_`).
const COL_DELETE_POST: &str = "col_delete_post";

/// WordPress's confirmation before a permanent delete (`showNotice.warn` in
/// `wp-admin/js/common.js`).
pub const DELETE_PERMANENTLY_CONFIRMATION: &str = "You are about to permanently delete these \
     items from your site.\nThis action cannot be undone.\n'Cancel' to stop, 'OK' to delete.";

/// The post meta that holds a post's featured image.
const THUMBNAIL_META: &str = "_thumbnail_id";

/// WordPress's refusal of a featured image that is not a media item
/// (`rest_invalid_featured_media`), shown on the field.
pub const INVALID_FEATURED_MEDIA: &str = "Invalid featured media ID.";

/// The post meta that holds a page's template.
const TEMPLATE_META: &str = "_wp_page_template";

/// The Template select's value for no template, and its label (`page_template_dropdown`).
const DEFAULT_TEMPLATE: (&str, &str) = ("default", "Default template");

/// The statuses the form offers, with the names WordPress's status objects give them. `trash`
/// is offered so a trashed post's form shows its status, and so it can be restored.
const STATUSES: [(PostStatus, &str); 6] = [
    (PostStatus::Draft, "Draft"),
    (PostStatus::Pending, "Pending"),
    (PostStatus::Private, "Private"),
    (PostStatus::Publish, "Published"),
    (PostStatus::Future, "Scheduled"),
    (PostStatus::Trash, "Trash"),
];

/// The title WordPress gives an auto-draft (`get_default_post_to_edit`).
const AUTO_DRAFT_TITLE: &str = "Auto Draft";

/// WordPress's built-in `post` and `page`, as definitions: their labels and supports as
/// `create_initial_post_types` registers them. They are not in the [`Registry`], which refuses
/// their reserved names; [`PostTypeResources`] adds them.
pub fn builtin_post_types() -> [PostTypeDef; 2] {
    let builtin = |slug: &str, name: &str, singular: &str| PostTypeDef {
        public: true,
        show_in_rest: true,
        origin: Origin::Code,
        ..PostTypeDef::new(slug, name, singular)
    };
    [
        PostTypeDef {
            rest_base: Some("posts".to_owned()),
            supports: vec![
                Support::Title,
                Support::Editor,
                Support::Author,
                Support::Thumbnail,
                Support::Excerpt,
                Support::Trackbacks,
                Support::CustomFields,
                Support::Comments,
                Support::Revisions,
                Support::PostFormats,
            ],
            taxonomies: vec!["category".to_owned(), "post_tag".to_owned()],
            ..builtin("post", "Posts", "Post")
        },
        PostTypeDef {
            rest_base: Some("pages".to_owned()),
            hierarchical: true,
            capability_type: CapabilityType::new("page"),
            supports: vec![
                Support::Title,
                Support::Editor,
                Support::Author,
                Support::Thumbnail,
                Support::PageAttributes,
                Support::CustomFields,
                Support::Comments,
                Support::Revisions,
            ],
            ..builtin("page", "Pages", "Page")
        },
    ]
}

/// The Ignite field name of the field group field with `key`: the key in lowercase, with `-`
/// written `_` (`field_65a1B-2` is `field_65a1b_2`), or `None` when that is not a name Ignite
/// accepts (lowercase letters, digits and `_`, starting with a letter, at most 64 bytes). ACF's
/// keys (`field_` and letters, digits, `_` or `-`) always map, and no core field name starts
/// with `field_`, so a custom field never takes a core field's name. Two keys that differ only in
/// case or in `-` and `_` map to one name: the second field is left out, logged.
///
/// The value is stored in post meta as ACF stores it: under the field's `name`, with `_<name>`
/// holding its key, so [`get_field`](super::get_field) reads it.
pub fn custom_field_name(key: &str) -> Option<String> {
    let name: String = key
        .chars()
        .map(|c| {
            if c == '-' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let valid = name.len() <= ignite_core::MAX_FIELD_NAME
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    valid.then_some(name)
}

/// The Ignite field name of the taxonomy named `taxonomy`: `tax_` and the name, `-` written `_`.
fn taxonomy_field_name(taxonomy: &str) -> String {
    format!("tax_{}", taxonomy.replace('-', "_"))
}

/// The Ignite field name of the new terms of the taxonomy named `taxonomy`: `new_<taxonomy>`
/// for a hierarchical one (WordPress's `newcategory`), `tax_input_<taxonomy>` for a flat one
/// (the tags box), `-` written `_`. The editor's script finds the boxes by these prefixes.
fn new_terms_field_name(taxonomy: &str, hierarchical: bool) -> String {
    let name = taxonomy.replace('-', "_");
    if hierarchical {
        format!("new_{name}")
    } else {
        format!("tax_input_{name}")
    }
}

/// The most new terms one save creates in one taxonomy.
const MAX_NEW_TERMS: usize = 100;

/// The indent of one level of a hierarchical taxonomy's choices: three no-break spaces, as
/// `wp_dropdown_categories` pads a child (`&nbsp;&nbsp;&nbsp;`). A term name never starts with
/// one (names are trimmed), so the editor's script reads the depth from it.
const LEVEL_INDENT: &str = "\u{a0}\u{a0}\u{a0}";

/// The names in `text`, separated by commas as the tags box separates them: trimmed, blanks
/// left out, each name once (compared without case), at most [`MAX_NEW_TERMS`].
fn term_names(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty() && seen.insert(name.to_lowercase()))
        .take(MAX_NEW_TERMS)
        .map(str::to_owned)
        .collect()
}

/// One taxonomy field of a resource.
#[derive(Debug, Clone)]
struct TaxonomyField {
    /// The Ignite field name, `tax_<taxonomy>`.
    field: String,
    /// The column key, `col_tax_<taxonomy>`, when the list shows the taxonomy.
    column: Option<String>,
    /// The taxonomy.
    taxonomy: Taxonomy,
    /// The capability that assigns its terms (`assign_terms`).
    assign_terms: String,
    /// The text field of the new terms the form adds: `new_<taxonomy>` (WordPress's
    /// `newcategory`) for a hierarchical taxonomy, `tax_input_<taxonomy>` (the tags box's
    /// `tax_input[post_tag]`) for a flat one. `None` when its name is taken.
    new_field: Option<String>,
    /// The capability that creates a term from the form: `edit_terms` for a hierarchical
    /// taxonomy (the Add New Category link's check), `assign_terms` for a flat one (a typed tag
    /// is created by the save).
    create_terms: String,
}

/// One field group field of a resource.
#[derive(Debug, Clone)]
struct CustomField {
    /// The Ignite field name, from the field's key.
    field: String,
    /// The field as its group defines it: its `name` is the meta key.
    def: FieldDef,
}

/// The rows of one post type: its posts in `posts`, their meta and terms. The
/// [`RuntimeData`] of the resource [`post_type_resource`] builds; see the
/// [module](self) for what it reads and writes, and why it must only be served behind the
/// admin's authorization.
#[derive(Debug, Clone)]
pub struct PostTypeData {
    post_type: String,
    hierarchical: bool,
    title: bool,
    editor: bool,
    excerpt: bool,
    author: bool,
    page_attributes: bool,
    thumbnail: bool,
    revisions: bool,
    sticky: bool,
    /// The type's `edit_posts` capability: who may be the author of its posts.
    edit_posts: String,
    /// The page templates the Template select offers; empty for no select.
    templates: Vec<PageTemplate>,
    taxonomies: Vec<TaxonomyField>,
    custom: Vec<CustomField>,
    /// The type's capabilities.
    caps: PostTypeCaps,
    /// The users' capabilities and the kses limiter, for the user-aware queries.
    access: Access,
}

/// What the admin's user-aware queries need (Task A1): where users' roles come from, and the
/// limiter kses runs under, shared with the REST API.
#[derive(Clone)]
pub struct Access {
    /// The provider of users' roles: the app's (`IgniteUserRoles` by default).
    pub user_roles: Arc<dyn UserRoles>,
    /// The app's kses limiter.
    pub kses: KsesLimiter,
    /// Capabilities read in the last [`CAPS_TTL`], by user: Ignite asks the policy for each row
    /// of a list and for each post type on every admin page, and each ask reads the roles.
    cache: Arc<Mutex<HashMap<i64, (Instant, UserCaps)>>>,
    /// The post each user's last New screen save created, by user and post type, until the
    /// admin takes it to send the browser to the post's edit form
    /// ([`PostTypeResources::take_created`]).
    created: Arc<Mutex<HashMap<(i64, String), i64>>>,
}

/// How long a user's capabilities are reused: a role change reaches the admin's screens within
/// it.
const CAPS_TTL: StdDuration = StdDuration::from_secs(2);

/// The most users whose capabilities are kept; the cache is emptied past it.
const CAPS_CACHED: usize = 1000;

impl Default for Access {
    fn default() -> Self {
        Self::new(Arc::new(IgniteUserRoles), KsesLimiter::default())
    }
}

impl fmt::Debug for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Access").finish_non_exhaustive()
    }
}

impl Access {
    /// Reads users' roles from `user_roles`, and runs kses under `kses`.
    pub fn new(user_roles: Arc<dyn UserRoles>, kses: KsesLimiter) -> Self {
        Self {
            user_roles,
            kses,
            cache: Arc::default(),
            created: Arc::default(),
        }
    }

    /// Records that `user`'s New screen save created post `id` of `post_type`.
    fn record_created(&self, user: i64, post_type: &str, id: i64) {
        let mut created = self.created.lock().unwrap_or_else(PoisonError::into_inner);
        // Bounded: an entry the admin never takes (a save outside its routes) is dropped.
        if created.len() >= CAPS_CACHED {
            created.clear();
        }
        created.insert((user, post_type.to_owned()), id);
    }

    /// The post `user`'s last New screen save of `post_type` created, taken once.
    fn take_created(&self, user: i64, post_type: &str) -> Option<i64> {
        self.created
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(user, post_type.to_owned()))
    }

    /// The capabilities of the acting `user`, from their roles (reused for [`CAPS_TTL`]).
    ///
    /// # Errors
    ///
    /// The database's error while reading the roles.
    pub async fn caps(&self, db: &Database, user: &ActingUser) -> Result<UserCaps, Error> {
        let id = user.id();
        {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((at, caps)) = cache.get(&id)
                && at.elapsed() < CAPS_TTL
            {
                return Ok(caps.clone());
            }
        }
        let caps = self.user_roles.roles(db, id).await?.user_caps(id);
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= CAPS_CACHED {
            cache.clear();
        }
        cache.insert(id, (Instant::now(), caps.clone()));
        Ok(caps)
    }
}

/// Which posts of the type a user's list and records hold (Task A1): `None` for every post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scope {
    /// The user: only their posts, when they lack `edit_others_posts`.
    own_only: bool,
    /// Other users' private posts are left out, when the user lacks `read_private_posts`.
    no_others_private: bool,
    /// The user's id.
    user: i64,
}

impl PostTypeData {
    /// What `caps` (user `user`'s) lets them see of the type's posts.
    fn scope(&self, caps: &UserCaps, user: i64) -> Scope {
        Scope {
            own_only: !has_cap(caps, &self.caps.edit_others_posts),
            no_others_private: !has_cap(caps, &self.caps.read_private_posts),
            user,
        }
    }

    /// Whether `post` is in `scope`.
    fn in_scope(post: &Post, scope: Scope) -> bool {
        let own = post.author_id == scope.user;
        if scope.own_only && !own {
            return false;
        }
        !(scope.no_others_private && post.status == PostStatus::Private && !own)
    }

    /// Whether the type has `parent`: a hierarchical type with `page-attributes`.
    fn has_parent(&self) -> bool {
        self.hierarchical && self.page_attributes
    }
}

/// The resource of the post type `def`: its form, list and rows, as the [module](self)
/// describes. `registry` gives the custom taxonomies attached to the type and the field groups
/// that show on it. The resource is a snapshot: build and register it again when a definition
/// changes ([`PostTypeResources::sync`]). It has no Template select; see
/// [`post_type_resource_with_templates`].
pub fn post_type_resource(def: &PostTypeDef, registry: &Registry) -> RuntimeResource {
    post_type_resource_with_templates(def, registry, &[])
}

/// [`post_type_resource`], with `templates` (the active theme's page templates) in the Template
/// select of `page`, after the parent, as WordPress's Page Attributes box shows it. No select when
/// `templates` is empty or the type is not `page`.
pub fn post_type_resource_with_templates(
    def: &PostTypeDef,
    registry: &Registry,
    templates: &[PageTemplate],
) -> RuntimeResource {
    post_type_resource_with_access(def, registry, templates, Access::default())
}

/// [`post_type_resource_with_templates`], reading the acting user's capabilities through
/// `access` (Task A1).
pub fn post_type_resource_with_access(
    def: &PostTypeDef,
    registry: &Registry,
    templates: &[PageTemplate],
    access: Access,
) -> RuntimeResource {
    let slug = def.slug.as_str();
    let supports = |s: Support| def.supports(s);
    let mut data = PostTypeData {
        post_type: def.slug.clone(),
        hierarchical: def.hierarchical,
        title: supports(Support::Title),
        editor: supports(Support::Editor),
        excerpt: supports(Support::Excerpt),
        author: supports(Support::Author),
        page_attributes: supports(Support::PageAttributes),
        thumbnail: supports(Support::Thumbnail),
        revisions: supports(Support::Revisions),
        sticky: slug == "post",
        edit_posts: def.caps().edit_posts,
        templates: if slug == "page" {
            templates.to_vec()
        } else {
            Vec::new()
        },
        taxonomies: Vec::new(),
        custom: Vec::new(),
        caps: def.caps(),
        access,
    };
    let field = |name: &str, label: &str, kind: FieldKindDef, required: bool| IgniteFieldDef {
        name: name.to_owned(),
        label: label.to_owned(),
        kind,
        required,
    };
    let mut fields = Vec::new();
    if data.title {
        // Required (the owner's rule): WordPress saves an empty title and shows "(no title)".
        fields.push(field(TITLE, "Title", FieldKindDef::Text, true));
    }
    fields.push(field(SLUG, "Slug", FieldKindDef::Text, false));
    if data.editor {
        fields.push(field(CONTENT, "Content", FieldKindDef::Wysiwyg, false));
    }
    if data.excerpt {
        fields.push(field(EXCERPT, "Excerpt", FieldKindDef::Textarea, false));
    }
    if data.author {
        fields.push(field(AUTHOR, "Author", FieldKindDef::User, false));
    }
    if data.has_parent() {
        fields.push(field(PARENT, "Parent", FieldKindDef::PostObject, false));
    }
    if !data.templates.is_empty() {
        let choices =
            std::iter::once((DEFAULT_TEMPLATE.0.to_owned(), DEFAULT_TEMPLATE.1.to_owned()))
                .chain(
                    data.templates
                        .iter()
                        .map(|t| (t.name.clone(), t.title.clone())),
                )
                .collect();
        fields.push(field(
            TEMPLATE,
            "Template",
            FieldKindDef::Select(choices),
            false,
        ));
    }
    if data.page_attributes {
        fields.push(field(MENU_ORDER, "Order", FieldKindDef::Number, false));
    }
    if data.thumbnail {
        let label = def
            .labels
            .featured_image
            .as_deref()
            .unwrap_or("Featured image");
        // A number field holding the media id: the editor's script shows it as WordPress's
        // Featured image box, and the save refuses an id that is not a media item with
        // WordPress's message (`featured_media_refusal`), not Ignite's media check.
        fields.push(field(FEATURED_MEDIA, label, FieldKindDef::Number, false));
    }
    let statuses = STATUSES
        .iter()
        .map(|(status, label)| (status.as_str().to_owned(), (*label).to_owned()))
        .collect();
    // Not required: Ignite's create form has no default value, so the New screen's select starts
    // blank, and a blank Status saves a Draft as WordPress's New screen does (an edit keeps the
    // post's status).
    fields.push(field(
        STATUS,
        "Status",
        FieldKindDef::Select(statuses),
        false,
    ));
    fields.push(field(DATE, "Date", FieldKindDef::DateTime, false));
    fields.push(field(PASSWORD, "Password", FieldKindDef::Text, false));
    if data.sticky {
        fields.push(field(
            STICKY,
            "Stick to the top of the blog",
            FieldKindDef::Checkbox,
            false,
        ));
    }

    let mut names: HashSet<String> = fields.iter().map(|f| f.name.clone()).collect();
    for used in taxonomies_of(def, registry) {
        let TaxonomyUse {
            taxonomy,
            label,
            column,
            assign_terms,
            edit_terms,
            add_new,
        } = used;
        let name = taxonomy_field_name(&taxonomy.name);
        if !names.insert(name.clone()) {
            log::warn!(
                "The taxonomy `{}` is left out of the `{slug}` screens: its field name `{name}` \
                 is taken by another taxonomy.",
                taxonomy.name
            );
            continue;
        }
        fields.push(field(&name, &label, FieldKindDef::Relationship, false));
        // The text field of the terms the form adds: the Add New Category link, or the tags
        // typed in the tags box.
        let new_name = new_terms_field_name(&taxonomy.name, taxonomy.hierarchical);
        let new_field = if names.insert(new_name.clone()) {
            fields.push(field(&new_name, &add_new, FieldKindDef::Text, false));
            Some(new_name)
        } else {
            log::warn!(
                "The taxonomy `{}` of the `{slug}` screens has no field for new terms: its name \
                 `{new_name}` is taken by another taxonomy.",
                taxonomy.name
            );
            None
        };
        let create_terms = if taxonomy.hierarchical {
            edit_terms
        } else {
            assign_terms.clone()
        };
        data.taxonomies.push(TaxonomyField {
            column: column.then(|| format!("col_{name}")),
            field: name,
            taxonomy,
            assign_terms,
            new_field,
            create_terms,
        });
    }

    // The field groups' fields, one card per group in the main column.
    let mut group_sections: Vec<Section> = Vec::new();
    for group in registry.groups_for(&ScreenContext::post(slug)) {
        let mut section = Section::make(group.title.clone());
        for def in group.fields {
            let skip = |reason: &str| {
                log::warn!(
                    "The field `{}` ({}) of the field group `{}` is left out of the `{slug}` \
                     screens: {reason}.",
                    def.name,
                    def.key,
                    group.key
                );
            };
            match def.kind {
                FieldKindDef::Clone(_) => {
                    skip("a clone field's values are stored as its cloned fields, not yet mapped");
                    continue;
                }
                FieldKindDef::Password => {
                    skip(
                        "ACF stores a password field as text and Ignite's password kind stores \
                         a hash",
                    );
                    continue;
                }
                FieldKindDef::Lookup => {
                    skip("ACF has no lookup field");
                    continue;
                }
                _ => {}
            }
            let Some(name) = custom_field_name(&def.key) else {
                skip("its key is not a name Ignite accepts");
                continue;
            };
            if !names.insert(name.clone()) {
                skip(&format!("another field's key also maps to `{name}`"));
                continue;
            }
            let mut ignite = def.to_ignite();
            ignite.name = name.clone();
            fields.push(ignite);
            section = section.schema([name.clone()]);
            data.custom.push(CustomField { field: name, def });
        }
        group_sections.push(section);
    }

    let column = |key: &str, label: &str, sortable: bool, searchable: bool| ColumnDef {
        key: key.to_owned(),
        label: label.to_owned(),
        sortable,
        searchable,
    };
    // The post's state is not a column: it shows as a badge inside the title cell on the All
    // tab, after the title as `_post_states` writes it ([`status_tabs`]).
    let mut columns = vec![column(COL_TITLE, "Title", true, true)];
    if data.author {
        columns.push(column(COL_AUTHOR, "Author", false, false));
    }
    columns.push(column(COL_STATUS, "Status", false, false));
    for taxonomy in &data.taxonomies {
        if let Some(key) = &taxonomy.column {
            let label = fields
                .iter()
                .find(|f| f.name == taxonomy.field)
                .map_or_else(|| taxonomy.taxonomy.name.to_string(), |f| f.label.clone());
            columns.push(column(key, &label, false, false));
        }
    }
    columns.push(column(COL_DATE, "Date", true, false));

    let title = if def.labels.name.is_empty() {
        def.slug.clone()
    } else {
        def.labels.name.clone()
    };
    let singular = if def.labels.singular_name.is_empty() {
        title.clone()
    } else {
        def.labels.singular_name.clone()
    };
    let layout = editor_layout(&data, group_sections);
    let has_title = data.title;
    let actions = trash_actions(&data);
    // `edit.php`, `post-new.php` and `post.php` need the type's `edit_posts` (its `create_posts`
    // is `edit_posts`), and deleting its `delete_posts`; the type's policy then checks each
    // post (`edit_post`, `delete_post`, and so `edit_others_posts` on another's post).
    let caps = def.caps();
    let screen_caps = crate::admin::ScreenCaps {
        view: &caps.edit_posts,
        create: &caps.edit_posts,
        update: &caps.edit_posts,
        delete: &caps.delete_posts,
    };
    let mut resource = screen_caps
        .apply(RuntimeResource::new(
            def.slug.clone(),
            title,
            singular,
            data,
        ))
        // The New screen in the type's menu group, as WordPress's Posts > Add New Post.
        .navigation_create_item(true)
        .navigation_create_label(crate::admin::navigation::CREATE_LABEL);
    status_tabs::add_status_tabs(&mut resource);
    resource.fields = fields;
    resource.columns = columns;
    resource.form_layout = layout;
    resource.row_actions = actions;
    // A trashed row shows only WordPress's Restore and Delete Permanently, not Ignite's Delete.
    resource = resource
        .hide_default_delete_when(ListFilter::new().where_eq(STATUS, PostStatus::Trash.as_str()));
    if has_title {
        // WordPress's permalink follows the title until it is edited by hand, and stays as it
        // is once the post is published, so its URL does not change with the title.
        resource = resource
            .slug_from(SLUG, TITLE)
            .slug_locked_when(SLUG, |form| {
                form.record()
                    .and_then(|record| record.get(STATUS))
                    .and_then(Value::as_str)
                    == Some(PostStatus::Publish.as_str())
            });
    }
    resource
}

/// The trash's row actions of a post type's list, as WordPress's (see the [module](self)):
/// "Trash" on a post out of the trash, "Restore" and "Delete Permanently" on a trashed one.
fn trash_actions(data: &PostTypeData) -> Vec<RowAction> {
    let in_trash = || ListFilter::new().where_eq(STATUS, PostStatus::Trash.as_str());
    let noun = data.noun();
    let trash = data.clone();
    let restore = data.clone();
    let delete = data.clone();
    vec![
        // `_x( 'Trash', 'verb' )`: the update ability (the row's Edit), and `delete_post`,
        // which the row's flag shows and the handler checks.
        RowAction::make("trash")
            .label("Trash")
            .color(BadgeColor::Danger)
            .visible(|row| {
                row.get(STATUS).and_then(Value::as_str) != Some(PostStatus::Trash.as_str())
                    && row.get(COL_DELETE_POST) == Some(&Value::Bool(true))
            })
            .action(move |db, user, id, _| trash.clone().trash_row(db, user, id))
            .success_notification_title(format!("1 {noun} moved to the Trash.")),
        RowAction::make("untrash")
            .label("Restore")
            .authorize(ActionAbility::Delete)
            .visible_when(in_trash())
            .action(move |db, user, id, _| restore.clone().untrash_row(db, user, id))
            .success_notification_title(format!("1 {noun} restored from the Trash.")),
        RowAction::make("delete")
            .label("Delete Permanently")
            .color(BadgeColor::Danger)
            .authorize(ActionAbility::Delete)
            .modal_description(DELETE_PERMANENTLY_CONFIRMATION)
            .visible_when(in_trash())
            .action(move |db, user, id, _| delete.clone().delete_row(db, user, id))
            .success_notification_title(format!("1 {noun} permanently deleted.")),
    ]
}

/// The layout of a post type's edit form, as WordPress's editor with its settings sidebar
/// (Filament's `->columns(3)` with a main group spanning 2 and a sidebar group spanning 1):
///
/// - the main column (two thirds): the title, the slug and the content in one card, the
///   excerpt, and one card per field group (`group_sections`, headed by the group's title);
/// - the sidebar (one third): Publish (status, date, password, sticky, author), the featured
///   image, Page Attributes (parent, template, order), and one card per taxonomy with its
///   field for new terms.
///
/// A field the type does not have is not on the form, and Ignite leaves out a card with none.
fn editor_layout(data: &PostTypeData, group_sections: Vec<Section>) -> FormLayout {
    let mut main = vec![
        Section::make("").schema([TITLE, SLUG, CONTENT]),
        Section::make("Excerpt").schema([EXCERPT]),
    ];
    main.extend(group_sections);
    let mut sidebar = vec![
        Section::make("Publish").schema([STATUS, DATE, PASSWORD, STICKY, AUTHOR]),
        Section::make("").schema([FEATURED_MEDIA]),
        Section::make("Page Attributes").schema([PARENT, TEMPLATE, MENU_ORDER]),
    ];
    for taxonomy in &data.taxonomies {
        let mut section = Section::make("").schema([taxonomy.field.clone()]);
        if let Some(new_field) = &taxonomy.new_field {
            section = section.schema([new_field.clone()]);
        }
        sidebar.push(section);
    }
    FormLayout::new().columns(3).schema([
        Group::make(main).column_span(2),
        Group::make(sidebar).column_span(1),
    ])
}

/// One taxonomy a post type's form shows; see [`taxonomies_of`].
struct TaxonomyUse {
    taxonomy: Taxonomy,
    /// The field's label: "Categories".
    label: String,
    /// Whether the list shows a column of its terms.
    column: bool,
    /// The capability that assigns its terms.
    assign_terms: String,
    /// The capability that creates and edits its terms.
    edit_terms: String,
    /// The label of the new terms' field: "Add New Category".
    add_new: String,
}

/// The taxonomies the type `def` uses, with their field label and whether the list shows them:
/// those `def` names, then each custom taxonomy whose `object_type` names the type, as
/// `register_taxonomy` attaches it. A taxonomy whose `show_ui` is off, or that is not
/// registered, is left out. Categories and tags have WordPress's labels and always show in the
/// list; a custom taxonomy shows with `show_admin_column`.
fn taxonomies_of(def: &PostTypeDef, registry: &Registry) -> Vec<TaxonomyUse> {
    let custom = registry.all_taxonomies();
    let mut names: Vec<&str> = def.taxonomies.iter().map(String::as_str).collect();
    for taxonomy in &custom {
        if taxonomy.object_type.contains(&def.slug) && !names.contains(&taxonomy.slug.as_str()) {
            names.push(&taxonomy.slug);
        }
    }
    let mut out = Vec::new();
    for name in names {
        match name {
            // `assign_terms` of categories and tags is `edit_posts`, and `edit_terms` is
            // `manage_categories`, as `map_meta_cap` maps them.
            "category" => out.push(TaxonomyUse {
                taxonomy: Taxonomy::CATEGORY,
                label: "Categories".to_owned(),
                column: true,
                assign_terms: "edit_posts".to_owned(),
                edit_terms: "manage_categories".to_owned(),
                add_new: "Add New Category".to_owned(),
            }),
            "post_tag" => out.push(TaxonomyUse {
                taxonomy: Taxonomy::POST_TAG,
                label: "Tags".to_owned(),
                column: true,
                assign_terms: "edit_posts".to_owned(),
                edit_terms: "manage_categories".to_owned(),
                add_new: "Add New Tags (separate tags with commas)".to_owned(),
            }),
            _ => match custom.iter().find(|t| t.slug == name) {
                Some(t) if t.effective_show_ui() => {
                    let label = if t.labels.name.is_empty() {
                        t.slug.clone()
                    } else {
                        t.labels.name.clone()
                    };
                    let singular = if t.labels.singular_name.is_empty() {
                        label.clone()
                    } else {
                        t.labels.singular_name.clone()
                    };
                    let add_new = t
                        .labels
                        .add_new_item
                        .clone()
                        .unwrap_or_else(|| format!("Add New {singular}"));
                    let add_new = if t.hierarchical {
                        add_new
                    } else {
                        let separate = t
                            .labels
                            .separate_items_with_commas
                            .clone()
                            .unwrap_or_else(|| "separate them with commas".to_owned());
                        format!("{add_new} ({separate})")
                    };
                    let caps = t.caps();
                    out.push(TaxonomyUse {
                        taxonomy: t.to_taxonomy(),
                        label,
                        column: t.show_admin_column,
                        assign_terms: caps.assign_terms,
                        edit_terms: caps.edit_terms,
                        add_new,
                    });
                }
                Some(_) => {}
                None => log::debug!(
                    "The post type `{}` names the taxonomy `{name}`, which is not registered.",
                    def.slug
                ),
            },
        }
    }
    out
}

/// Creates an `auto-draft`, as `get_default_post_to_edit` does with `$create_in_db`: a post of
/// `post_type` by `author_id`, titled "Auto Draft", with no date. The form shows it with a blank
/// title and the status `draft`; its first save makes it what the form says. The list leaves
/// auto-drafts out, and the daily cleanup deletes them after 7 days.
///
/// The admin's New screen no longer creates one when it opens (a new post gets no id until it is
/// saved); its first save uses one as the row it writes, within the save, and deletes it when the
/// save fails.
///
/// The caller decides who may create one: the admin checks the type's `edit_posts` first.
///
/// # Errors
///
/// [`Error::ForeignKeyViolation`] when the author does not exist, or the database's error.
pub async fn create_auto_draft(
    db: &Database,
    post_type: &str,
    author_id: i64,
) -> Result<Post, Error> {
    Post::create(
        db,
        NewPost {
            status: PostStatus::AutoDraft,
            title: AUTO_DRAFT_TITLE.to_owned(),
            ..NewPost::new(author_id, post_type)
        },
    )
    .await
}

/// A post type whose resource Ignite refused, and why: its slug is a panel page or another
/// resource's (Ignite reserves `login`, `profile` and its own resources' slugs, such as `users`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedResource {
    /// The post type's slug.
    pub post_type: String,
    /// Ignite's reason.
    pub error: RuntimeError,
}

impl fmt::Display for RefusedResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the post type `{}` has no admin screens: {}",
            self.post_type, self.error
        )
    }
}

impl std::error::Error for RefusedResource {}

/// The resources of the post types the admin shows, on Ignite's runtime registry: `post`,
/// `page`, and each post type in use in the [`Registry`] whose `show_ui` is on (by default, a
/// public one). Call [`sync`](Self::sync) at start and after every change to a post type,
/// taxonomy or field group (saved or deleted in the admin, registered or removed by a plugin);
/// it is idempotent.
///
/// Ignite's registry is one per process: keep one `PostTypeResources` per app.
///
/// ```ignore
/// let resources = PostTypeResources::new(Arc::clone(&registry));
/// for refused in resources.sync() {
///     log::warn!("{refused}");
/// }
/// ```
#[derive(Debug)]
pub struct PostTypeResources {
    registry: Arc<Registry>,
    /// The page templates of `page`'s Template select.
    page_templates: Vec<PageTemplate>,
    /// The users' capabilities and kses, for the resources' queries and policies.
    access: Access,
    /// The slugs this value registered, in order. Held only by synchronous code.
    registered: Mutex<Vec<String>>,
}

impl PostTypeResources {
    /// Resources for the post types of `registry`; nothing is registered until
    /// [`sync`](Self::sync).
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            page_templates: Vec::new(),
            access: Access::default(),
            registered: Mutex::new(Vec::new()),
        }
    }

    /// Reads the acting users' capabilities through `access` (the app's roles provider and kses
    /// limiter), from the next [`sync`](Self::sync). The default reads Ignite's roles.
    #[must_use]
    pub fn with_access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }

    /// Gives `page` a Template select with `templates`, the active theme's page templates
    /// ([`Theme::page_templates`](crate::theme::Theme::page_templates)), from the next
    /// [`sync`](Self::sync). A theme switch takes effect at the next start, as the front end's.
    #[must_use]
    pub fn with_page_templates(mut self, templates: Vec<PageTemplate>) -> Self {
        self.page_templates = templates;
        self
    }

    /// The definition of `post_type` when this value has registered its resource: the post type
    /// behind the admin screens at `<mount>/<post_type>`.
    pub fn post_type(&self, post_type: &str) -> Option<PostTypeDef> {
        if !self.registered().iter().any(|slug| slug == post_type) {
            return None;
        }
        self.post_types()
            .into_iter()
            .find(|def| def.slug == post_type)
    }

    /// The post types that get a resource: `post`, `page`, then the registry's in use whose
    /// `show_ui` is on.
    pub fn post_types(&self) -> Vec<PostTypeDef> {
        builtin_post_types()
            .into_iter()
            .chain(
                self.registry
                    .all_post_types()
                    .into_iter()
                    .filter(PostTypeDef::effective_show_ui),
            )
            .collect()
    }

    /// Builds the resource of every post type of [`post_types`](Self::post_types) and
    /// registers it on Ignite, replacing the one this value registered before; unregisters the
    /// resources of types that are gone or no longer shown. Returns the types Ignite refused,
    /// which have no admin screens. From the next request on, the panel serves the new
    /// definitions.
    pub fn sync(&self) -> Vec<RefusedResource> {
        let mut registered = self
            .registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let wanted = self.post_types();
        for slug in registered.iter() {
            // Ours, so it is there unless something else removed it.
            let _ = unregister_runtime(slug);
            unregister_policy(slug);
        }
        registered.clear();
        let mut refused = Vec::new();
        let mut positions = PostTypePositions::default();
        for def in &wanted {
            let mut resource = post_type_resource_with_access(
                def,
                &self.registry,
                &self.page_templates,
                self.access.clone(),
            );
            // The sidebar's WordPress order: the positions are given in the types' order.
            let position = positions.next(def);
            place_post_type(&mut resource, def, position);
            // Each action on the type's posts is WordPress's for the user (Task A1). The policy
            // is in place before the resource, so the resource is never served without it; a
            // slug another registration holds keeps its own policy.
            if ignite_core::runtime_resource(&def.slug).is_some()
                || ignite_core::policy(&def.slug).is_some()
            {
                if let Err(error) = register_runtime(resource) {
                    refused.push(RefusedResource {
                        post_type: def.slug.clone(),
                        error,
                    });
                }
                continue;
            }
            register_policy(
                def.slug.clone(),
                PostTypePolicy::new(def.clone(), self.access.clone()),
            );
            match register_runtime(resource) {
                Ok(()) => registered.push(def.slug.clone()),
                Err(error) => {
                    unregister_policy(&def.slug);
                    refused.push(RefusedResource {
                        post_type: def.slug.clone(),
                        error,
                    });
                }
            }
        }
        refused
    }

    /// Unregisters the resource of `post_type`, if this value registered it, as when the type
    /// is removed and [`sync`](Self::sync) is not run. True when there was one.
    pub fn remove(&self, post_type: &str) -> bool {
        let mut registered = self
            .registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(index) = registered.iter().position(|s| s == post_type) else {
            return false;
        };
        registered.remove(index);
        let removed = unregister_runtime(post_type).is_ok();
        unregister_policy(post_type);
        removed
    }

    /// Whether `slug` is a post type this value would serve, whose Ignite resource is registered
    /// by something else: another `PostTypeResources` in the process, or code that took the slug
    /// first (`sync` then refused it). The admin refuses such a slug's screens, since the rules
    /// it checks belong to this value's post types (fail closed).
    pub fn is_unowned(&self, slug: &str) -> bool {
        !self.registered().iter().any(|s| s == slug)
            && ignite_core::runtime_resource(slug).is_some()
            && self.post_types().iter().any(|def| def.slug == slug)
    }

    /// The id of the post that `user`'s last save of the New screen of `post_type` created, once:
    /// a second call returns `None`. Ignite's create answers with a redirect to the list; the
    /// admin uses this to send the browser to the new post's edit form instead, as WordPress
    /// does after the first save. When the same user saves two New screens of one type at the
    /// same moment, one of the two saves may be sent to the other's post, or to the list.
    pub fn take_created(&self, user: i64, post_type: &str) -> Option<i64> {
        self.access.take_created(user, post_type)
    }

    /// The post types whose resource this value has registered, in order.
    pub fn registered(&self) -> Vec<String> {
        self.registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Errors

/// The database error of a meta read or write. A stored value that is not JSON cannot be written
/// by these functions, which only write JSON values; it is reported as a decode error.
fn meta_error(e: MetaError) -> Error {
    match e {
        MetaError::Database(e) => e,
        MetaError::InvalidJson(e) => Error::Database(sqlx::Error::Decode(Box::new(e))),
    }
}

/// The database error of a scheduler call; the scheduler fails otherwise only on a payload that
/// does not serialise, which a post id always does.
fn scheduler_error(e: crate::scheduler::Error) -> Error {
    match e {
        crate::scheduler::Error::Database(e) => e,
        other => Error::Database(sqlx::Error::Protocol(other.to_string())),
    }
}

/// What a save refused: no post of this type has the id.
fn not_found() -> Error {
    Error::NotFound { table: "posts" }
}

// ---------------------------------------------------------------------------------------------
// Values

/// A UTC time as the site's local time, as the form and the list show it.
fn local_text(utc: NaiveDateTime, offset: FixedOffset) -> String {
    (utc + Duration::seconds(i64::from(offset.local_minus_utc())))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// The form's local date and time (`YYYY-MM-DD HH:MM:SS`, as Ignite's validation gives it) in
/// UTC.
fn utc_from_local(text: &str, offset: FixedOffset) -> Option<NaiveDateTime> {
    let local = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%dT%H:%M"))
        .ok()?;
    Some(local - Duration::seconds(i64::from(offset.local_minus_utc())))
}

/// The text of a row value: a string, or a number written out.
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The ids a relationship value holds: its JSON list (as text) of ids.
/// The `default_category` option's term id, or `None` when it is unset or not an id.
async fn default_category(db: &Database) -> Result<Option<i64>, Error> {
    match get_option(db, "default_category").await {
        Ok(value) => Ok(value.as_ref().and_then(id_of)),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e),
    }
}

fn row_ids(value: &Value) -> Vec<i64> {
    let list = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let mut ids: Vec<i64> = list
        .as_array()
        .map(|items| items.iter().filter_map(id_of).collect())
        .unwrap_or_default();
    ids.sort_unstable();
    ids.dedup();
    // A post's own terms are read up to MAX_POST_TERMS, so a merged list (R123) keeps as many.
    ids.truncate(usize::try_from(MAX_POST_TERMS).unwrap_or(usize::MAX));
    ids
}

/// A field group field's value from the form, as it is stored in post meta, as ACF stores it
/// (JSON, since Mission Control's meta is JSON): text as text, a number as a number, a
/// true/false as a boolean, a date as `Ymd` (ACF's `date_picker` format), a media, post, user
/// or term id as a number, and the structured kinds (link, relationship, gallery, repeater,
/// group, flexible content) as their JSON. A cleared value is `null`.
fn meta_value(kind: &FieldKindDef, value: &Value) -> Value {
    let Value::String(text) = value else {
        // Null, a number (number, decimal) or a boolean (checkbox): stored as it is.
        return value.clone();
    };
    match kind {
        FieldKindDef::Date => match NaiveDate::parse_from_str(text, "%Y-%m-%d") {
            Ok(date) => Value::from(date.format("%Y%m%d").to_string()),
            Err(_) => value.clone(),
        },
        FieldKindDef::Image
        | FieldKindDef::File
        | FieldKindDef::PostObject
        | FieldKindDef::User
        | FieldKindDef::Taxonomy => text
            .trim()
            .parse::<i64>()
            .map_or_else(|_| value.clone(), Value::from),
        FieldKindDef::Link
        | FieldKindDef::Relationship
        | FieldKindDef::Gallery
        | FieldKindDef::Repeater(_)
        | FieldKindDef::Group(_)
        | FieldKindDef::FlexibleContent(_) => {
            serde_json::from_str(text).unwrap_or_else(|_| value.clone())
        }
        _ => value.clone(),
    }
}

/// A stored field group value as the form shows it: the shape Ignite's
/// `validate_update` gives the field (text for most kinds, a number, a boolean, the structured
/// kinds' JSON as text).
fn form_value(kind: &FieldKindDef, stored: Value) -> Value {
    match (kind, stored) {
        (_, Value::Null) => Value::Null,
        (FieldKindDef::Checkbox, value) => Value::Bool(match value {
            Value::Bool(b) => b,
            Value::Number(n) => n.as_f64() != Some(0.0),
            Value::String(s) => matches!(s.trim(), "1" | "true" | "on"),
            _ => false,
        }),
        (FieldKindDef::Number | FieldKindDef::Decimal(_), Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| s.trim().parse::<f64>().map(Value::from))
            .unwrap_or(Value::String(s)),
        (FieldKindDef::Number | FieldKindDef::Decimal(_), value) => value,
        (FieldKindDef::Date, Value::String(s)) => {
            match NaiveDate::parse_from_str(s.trim(), "%Y%m%d") {
                Ok(date) => Value::from(date.format("%Y-%m-%d").to_string()),
                Err(_) => Value::String(s),
            }
        }
        (
            FieldKindDef::Link
            | FieldKindDef::Relationship
            | FieldKindDef::Gallery
            | FieldKindDef::Repeater(_)
            | FieldKindDef::Group(_)
            | FieldKindDef::FlexibleContent(_),
            value,
        ) => match value {
            Value::String(s) => Value::String(s),
            other => Value::String(other.to_string()),
        },
        (_, Value::String(s)) => Value::String(s),
        (_, Value::Number(n)) => Value::String(n.to_string()),
        (_, Value::Bool(b)) => Value::String(if b { "1" } else { "0" }.to_owned()),
        (_, other) => Value::String(other.to_string()),
    }
}

/// The name WordPress's status object gives `status`, as the Status column shows it.
fn status_label(status: PostStatus) -> &'static str {
    match status {
        PostStatus::AutoDraft => "Draft",
        PostStatus::Inherit => "Inherit",
        other => STATUSES
            .iter()
            .find(|(s, _)| *s == other)
            .map_or("", |(_, label)| label),
    }
}

/// Question marks for `n` binds.
fn marks(n: usize) -> String {
    vec!["?"; n].join(", ")
}

// ---------------------------------------------------------------------------------------------
// Reads

impl PostTypeData {
    /// Post `id` if it is of this type.
    async fn post(&self, db: &Database, id: i64) -> Result<Option<Post>, Error> {
        Ok(Post::query()
            .find(db, id)
            .await?
            .filter(|post| post.post_type == self.post_type))
    }

    /// The names of the users among `ids`.
    async fn user_names(&self, db: &Database, ids: &[i64]) -> Result<HashMap<i64, String>, Error> {
        let mut ids: Vec<i64> = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut names = HashMap::new();
        for chunk in ids.chunks(500) {
            let rows: Vec<(i64, String)> = raw::fetch_all(
                db,
                &format!(
                    "SELECT id, name FROM users WHERE id IN ({})",
                    marks(chunk.len())
                ),
                &chunk.iter().map(|id| Sql::from(*id)).collect::<Vec<_>>(),
            )
            .await?;
            names.extend(rows);
        }
        Ok(names)
    }

    /// The terms of the posts `ids` in this type's taxonomies, by post and taxonomy name: each
    /// term's `term_id` and name, ordered by name, at most `per_post` per post and taxonomy.
    async fn post_terms(
        &self,
        db: &Database,
        ids: &[i64],
        per_post: i64,
    ) -> Result<HashMap<(i64, String), Vec<(i64, String)>>, Error> {
        let mut out: HashMap<(i64, String), Vec<(i64, String)>> = HashMap::new();
        if ids.is_empty() || self.taxonomies.is_empty() {
            return Ok(out);
        }
        let taxonomies: Vec<&str> = self
            .taxonomies
            .iter()
            .map(|t| t.taxonomy.name.as_ref())
            .collect();
        for chunk in ids.chunks(500) {
            let mut binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
            binds.extend(taxonomies.iter().map(|t| Sql::from(*t)));
            binds.push(per_post.into());
            // `ROW_NUMBER()` keeps at most `per_post` terms of each post and taxonomy, so a post
            // with many terms is never read whole.
            let sql = format!(
                "SELECT m_post, m_tax, m_term, m_name FROM (SELECT r.object_id AS m_post, \
                 tt.taxonomy AS m_tax, t.id AS m_term, t.name AS m_name, ROW_NUMBER() OVER \
                 (PARTITION BY r.object_id, tt.taxonomy ORDER BY t.name, t.id) AS m_rank \
                 FROM term_relationships r \
                 INNER JOIN term_taxonomy tt ON tt.id = r.term_taxonomy_id \
                 INNER JOIN terms t ON t.id = tt.term_id \
                 WHERE r.object_id IN ({}) AND tt.taxonomy IN ({})) ranked \
                 WHERE m_rank <= ? ORDER BY m_post, m_tax, m_name, m_term",
                marks(chunk.len()),
                marks(taxonomies.len())
            );
            let rows: Vec<(i64, String, i64, String)> = raw::fetch_all(db, &sql, &binds).await?;
            for (post, taxonomy, term, name) in rows {
                // Exact comparison: MySQL compares text without case.
                if taxonomies.contains(&taxonomy.as_str()) {
                    out.entry((post, taxonomy)).or_default().push((term, name));
                }
            }
        }
        Ok(out)
    }

    /// The column values of `post`: the title ("(no title)" when empty), the author's name, the
    /// status, the term names, the date (published, or last modified when it has none) and the
    /// state `_post_states` names, empty for a published post, which the list shows inside the
    /// title cell on the All tab ([`status_tabs`]).
    fn columns(
        &self,
        row: &mut Row,
        post: &Post,
        authors: &HashMap<i64, String>,
        terms: &HashMap<(i64, String), Vec<(i64, String)>>,
        offset: FixedOffset,
    ) {
        let title = if post.title.trim().is_empty() || post.status == PostStatus::AutoDraft {
            "(no title)".to_owned()
        } else {
            post.title.clone()
        };
        row.insert(COL_TITLE.into(), Value::from(title));
        row.insert(
            status_tabs::COL_STATE.into(),
            Value::from(status_tabs::post_state(post.status).unwrap_or_default()),
        );
        if self.author {
            row.insert(
                COL_AUTHOR.into(),
                Value::from(authors.get(&post.author_id).cloned().unwrap_or_default()),
            );
        }
        row.insert(COL_STATUS.into(), Value::from(status_label(post.status)));
        // The stored status, which the row actions' rules read (a list row has no form values;
        // a form row has its own, a draft for an auto-draft).
        row.entry(STATUS)
            .or_insert_with(|| Value::from(post.status.as_str()));
        for taxonomy in &self.taxonomies {
            if let Some(key) = &taxonomy.column {
                let names: Vec<&str> = terms
                    .get(&(post.id, taxonomy.taxonomy.name.to_string()))
                    .map(|list| list.iter().map(|(_, name)| name.as_str()).collect())
                    .unwrap_or_default();
                let text = if names.is_empty() {
                    "—".to_owned()
                } else {
                    names.join(", ")
                };
                row.insert(key.clone(), Value::from(text));
            }
        }
        let date = post.published_at.unwrap_or(post.modified_at);
        row.insert(COL_DATE.into(), Value::from(local_text(date, offset)));
    }

    /// The stored values of the field group fields of post `id`, by meta key: the first value of
    /// each key.
    async fn custom_values(&self, db: &Database, id: i64) -> Result<HashMap<String, Value>, Error> {
        let mut values = HashMap::new();
        let keys: Vec<&str> = self.custom.iter().map(|c| c.def.name.as_str()).collect();
        for chunk in keys.chunks(KEYS_CHUNK) {
            let rows =
                crate::content::meta_rows(db, crate::content::MetaTable::Post, &[id], chunk, 1)
                    .await?;
            for (_, key, text) in rows {
                // A stored value that is not JSON was not written by Mission Control: it reads as
                // none.
                let value = serde_json::from_str(&text).unwrap_or(Value::Null);
                values.entry(key).or_insert(value);
            }
        }
        Ok(values)
    }

    /// The form row of `post`: every field and column key.
    async fn row(&self, db: &Database, post: &Post) -> Result<Row, Error> {
        let offset = crate::content::gmt_offset(db).await?;
        let mut row = Row::new();
        row.insert("id".into(), Value::from(post.id));
        let auto_draft = post.status == PostStatus::AutoDraft;
        if self.title {
            // `get_default_post_to_edit` shows an auto-draft with a blank title.
            let title = if auto_draft && post.title == AUTO_DRAFT_TITLE {
                ""
            } else {
                post.title.as_str()
            };
            row.insert(TITLE.into(), Value::from(title));
        }
        row.insert(SLUG.into(), Value::from(post.slug.clone()));
        if self.editor {
            row.insert(CONTENT.into(), Value::from(post.content.clone()));
        }
        if self.excerpt {
            row.insert(EXCERPT.into(), Value::from(post.excerpt.clone()));
        }
        if self.author {
            row.insert(AUTHOR.into(), Value::from(post.author_id.to_string()));
        }
        if self.has_parent() {
            let parent = post.parent_id.filter(|p| *p > 0);
            row.insert(
                PARENT.into(),
                parent.map_or(Value::Null, |p| Value::from(p.to_string())),
            );
        }
        if self.page_attributes {
            row.insert(MENU_ORDER.into(), Value::from(post.menu_order));
        }
        if !self.templates.is_empty() {
            let stored = stored_template(db, post.id).await?;
            // A stored value is matched by its template name, so WordPress's `wide.php` is the
            // theme's `wide`; one the theme does not have (any more) shows as the default.
            let template = stored
                .and_then(|value| self.template_named(&value))
                .map_or_else(|| DEFAULT_TEMPLATE.0.to_owned(), |t| t.name.clone());
            row.insert(TEMPLATE.into(), Value::from(template));
        }
        if self.thumbnail {
            let thumbnail = match crate::content::get_post_meta(db, post.id, THUMBNAIL_META).await {
                Ok(value) => value.as_ref().and_then(id_of),
                Err(MetaError::InvalidJson(_)) => None,
                Err(MetaError::Database(e)) => return Err(e),
            };
            row.insert(
                FEATURED_MEDIA.into(),
                thumbnail.map_or(Value::Null, |id| Value::from(id.to_string())),
            );
        }
        let status = if auto_draft {
            PostStatus::Draft
        } else {
            post.status
        };
        row.insert(STATUS.into(), Value::from(status.as_str()));
        row.insert(
            DATE.into(),
            post.published_at
                .map_or(Value::Null, |d| Value::from(local_text(d, offset))),
        );
        row.insert(
            PASSWORD.into(),
            post.password
                .clone()
                .filter(|p| !p.is_empty())
                .map_or(Value::Null, Value::from),
        );
        if self.sticky {
            let sticky = match get_option(db, "sticky_posts").await {
                Ok(Some(Value::Array(ids))) => ids.iter().filter_map(id_of).any(|i| i == post.id),
                Ok(_) | Err(MetaError::InvalidJson(_)) => false,
                Err(MetaError::Database(e)) => return Err(e),
            };
            row.insert(STICKY.into(), Value::Bool(sticky));
        }
        let terms = self.post_terms(db, &[post.id], MAX_POST_TERMS).await?;
        for taxonomy in &self.taxonomies {
            let ids: Vec<String> = terms
                .get(&(post.id, taxonomy.taxonomy.name.to_string()))
                .map(|list| list.iter().map(|(id, _)| id.to_string()).collect())
                .unwrap_or_default();
            row.insert(taxonomy.field.clone(), Value::from(json!(ids).to_string()));
        }
        let mut values = self.custom_values(db, post.id).await?;
        for custom in &self.custom {
            let stored = values.remove(&custom.def.name).unwrap_or(Value::Null);
            row.insert(custom.field.clone(), form_value(&custom.def.kind, stored));
        }
        let authors = if self.author {
            self.user_names(db, &[post.author_id]).await?
        } else {
            HashMap::new()
        };
        self.columns(&mut row, post, &authors, &terms, offset);
        Ok(row)
    }

    /// One page of the list: every post of the type but auto-drafts, in the tab's statuses
    /// ([`status_tabs`]), matching the search on the title, excerpt and content as
    /// `WP_Query::parse_search` splits it. With `per_page` 0 (the panel counting a tab), only
    /// the total.
    async fn list_page(
        &self,
        db: &Database,
        query: &ListQuery,
        scope: Option<Scope>,
    ) -> Result<Page<Row>, Error> {
        let per_page = query.per_page.clamp(1, MAX_PER_PAGE);
        let offset_rows = query.page.max(1).saturating_sub(1).saturating_mul(per_page);
        let mut filter = String::from("FROM posts p WHERE p.post_type = ? AND p.status <> ?");
        let mut binds: Vec<Sql> =
            vec![self.post_type.as_str().into(), PostStatus::AutoDraft.into()];
        status_tabs::status_conditions(&query.filter, &mut filter, &mut binds);
        if let Some(scope) = scope {
            if scope.own_only {
                filter.push_str(" AND p.author_id = ?");
                binds.push(scope.user.into());
            } else if scope.no_others_private {
                filter.push_str(" AND (p.status <> ? OR p.author_id = ?)");
                binds.push(PostStatus::Private.into());
                binds.push(scope.user.into());
            }
        }
        if let Some(search) = &query.search {
            let mut cut = search.len().min(MAX_SEARCH_BYTES);
            while !search.is_char_boundary(cut) {
                cut -= 1;
            }
            for term in search_terms(&search[..cut]) {
                let like = format!("%{}%", escape_like(&term.text.to_lowercase()));
                filter.push_str(if term.exclude { " AND NOT " } else { " AND " });
                filter.push_str(
                    "(LOWER(p.title) LIKE ? ESCAPE '!' OR LOWER(p.excerpt) LIKE ? ESCAPE '!' \
                     OR LOWER(p.content) LIKE ? ESCAPE '!')",
                );
                binds.extend(std::iter::repeat_n(Sql::from(like), 3));
            }
        }
        let total: i64 =
            raw::fetch_scalar(db, &format!("SELECT COUNT(*) {filter}"), &binds).await?;
        if query.per_page <= 0 {
            return Ok(Page {
                rows: Vec::new(),
                total,
            });
        }
        let date = "COALESCE(p.published_at, p.modified_at)";
        let order = match &query.sort {
            Some((key, direction)) => {
                let dir = match direction {
                    Direction::Asc => "ASC",
                    Direction::Desc => "DESC",
                };
                match key.as_str() {
                    COL_TITLE => format!("p.title {dir}, p.id {dir}"),
                    _ => format!("{date} {dir}, p.id {dir}"),
                }
            }
            // `wp_edit_posts_query`: a hierarchical type by order and title, others newest first.
            None if self.hierarchical => "p.menu_order ASC, p.title ASC, p.id ASC".to_owned(),
            None => format!("{date} DESC, p.id DESC"),
        };
        let mut page_binds = binds;
        page_binds.push(per_page.into());
        page_binds.push(offset_rows.into());
        let ids: Vec<(i64,)> = raw::fetch_all(
            db,
            &format!("SELECT p.id {filter} ORDER BY {order} LIMIT ? OFFSET ?"),
            &page_binds,
        )
        .await?;
        let ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
        if ids.is_empty() {
            return Ok(Page {
                rows: Vec::new(),
                total,
            });
        }
        let mut posts = Post::query()
            .where_in(Post::PRIMARY_KEY, ids.clone())
            .get(db)
            .await?;
        posts.sort_by_key(|post| ids.iter().position(|id| *id == post.id));
        let offset = crate::content::gmt_offset(db).await?;
        let authors = if self.author {
            let ids: Vec<i64> = posts.iter().map(|p| p.author_id).collect();
            self.user_names(db, &ids).await?
        } else {
            HashMap::new()
        };
        let terms = self.post_terms(db, &ids, MAX_TERMS_SHOWN).await?;
        let rows = posts
            .iter()
            .map(|post| {
                let mut row = Row::new();
                row.insert("id".into(), Value::from(post.id));
                self.columns(&mut row, post, &authors, &terms, offset);
                row
            })
            .collect();
        Ok(Page { rows, total })
    }

    /// The `(value, label)` choices of the field `field`, at most [`MAX_OPTIONS`].
    async fn choices(&self, db: &Database, field: &str) -> Result<Vec<(String, String)>, Error> {
        if field == AUTHOR && self.author {
            // `post_author_meta_box`: the users who may edit posts of the type.
            let users = users_with_cap(db, &self.edit_posts, MAX_OPTIONS).await?;
            return Ok(users
                .into_iter()
                .map(|(id, name)| (id.to_string(), name))
                .collect());
        }
        if field == PARENT && self.has_parent() {
            // `page_attributes_meta_box`'s `wp_dropdown_pages` lists published posts only.
            return published_posts_of(db, &self.post_type).await;
        }
        if let Some(taxonomy) = self.taxonomies.iter().find(|t| t.field == field) {
            if taxonomy.taxonomy.hierarchical {
                return terms_tree_of(db, &taxonomy.taxonomy.name).await;
            }
            return terms_of(db, &taxonomy.taxonomy.name).await;
        }
        let Some(custom) = self.custom.iter().find(|c| c.field == field) else {
            return Ok(Vec::new());
        };
        let setting = |name: &str| -> Vec<String> {
            match custom.def.settings.get(name) {
                Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .take(super::definition::MAX_LIST_ITEMS)
                    .collect(),
                _ => Vec::new(),
            }
        };
        match custom.def.kind {
            FieldKindDef::User => users(db).await,
            FieldKindDef::PostObject | FieldKindDef::Relationship => {
                let types = setting("post_type");
                let types: Vec<&str> = types.iter().map(String::as_str).collect();
                posts_of(db, &types).await
            }
            FieldKindDef::Taxonomy => match setting("taxonomy").first() {
                Some(taxonomy) => terms_of(db, taxonomy).await,
                None => Ok(Vec::new()),
            },
            _ => Ok(Vec::new()),
        }
    }
}

/// Post `id`'s stored `_wp_page_template`, when it is text.
async fn stored_template(db: &Database, id: i64) -> Result<Option<String>, Error> {
    match crate::content::get_post_meta(db, id, TEMPLATE_META).await {
        Ok(value) => Ok(value.and_then(|v| v.as_str().map(str::to_owned))),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e),
    }
}

/// The users, by name: `(id, name)`.
async fn users(db: &Database) -> Result<Vec<(String, String)>, Error> {
    let rows: Vec<(i64, String)> = raw::fetch_all(
        db,
        "SELECT id, name FROM users ORDER BY name, id LIMIT ?",
        &[(MAX_OPTIONS as i64).into()],
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| (id.to_string(), name))
        .collect())
}

/// The published posts of `post_type`, by order and title: `(id, title)`, "(no title)" for none.
async fn published_posts_of(
    db: &Database,
    post_type: &str,
) -> Result<Vec<(String, String)>, Error> {
    let rows: Vec<(i64, String)> = raw::fetch_all(
        db,
        "SELECT id, title FROM posts WHERE status = ? AND post_type = ? \
         ORDER BY menu_order, title, id LIMIT ?",
        &[
            PostStatus::Publish.into(),
            post_type.into(),
            (MAX_OPTIONS as i64).into(),
        ],
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, title)| {
            let title = if title.trim().is_empty() {
                "(no title)".to_owned()
            } else {
                title
            };
            (id.to_string(), title)
        })
        .collect())
}

/// The posts of `types` (every type but revisions, attachments and menu items when empty) that
/// are not trashed or auto-drafts, by order and title: `(id, title)`, "(no title)" for none.
async fn posts_of(db: &Database, types: &[&str]) -> Result<Vec<(String, String)>, Error> {
    let mut sql = String::from("SELECT id, title FROM posts WHERE status NOT IN (?, ?)");
    let mut binds: Vec<Sql> = vec![PostStatus::Trash.into(), PostStatus::AutoDraft.into()];
    if types.is_empty() {
        sql.push_str(" AND post_type NOT IN (?, ?, ?)");
        binds.extend(["revision", "attachment", "nav_menu_item"].map(Sql::from));
    } else {
        sql.push_str(&format!(" AND post_type IN ({})", marks(types.len())));
        binds.extend(types.iter().map(|t| Sql::from(*t)));
    }
    sql.push_str(" ORDER BY menu_order, title, id LIMIT ?");
    binds.push((MAX_OPTIONS as i64).into());
    let rows: Vec<(i64, String)> = raw::fetch_all(db, &sql, &binds).await?;
    Ok(rows
        .into_iter()
        .map(|(id, title)| {
            let title = if title.trim().is_empty() {
                "(no title)".to_owned()
            } else {
                title
            };
            (id.to_string(), title)
        })
        .collect())
}

/// The terms of the taxonomy named `taxonomy`, by name: `(term_id, name)`.
async fn terms_of(db: &Database, taxonomy: &str) -> Result<Vec<(String, String)>, Error> {
    let rows: Vec<(i64, String, String)> = raw::fetch_all(
        db,
        "SELECT t.id, t.name, tt.taxonomy FROM term_taxonomy tt \
         INNER JOIN terms t ON t.id = tt.term_id WHERE tt.taxonomy = ? \
         ORDER BY t.name, t.id LIMIT ?",
        &[taxonomy.into(), (MAX_OPTIONS as i64).into()],
    )
    .await?;
    Ok(rows
        .into_iter()
        // Exact comparison: MySQL compares text without case.
        .filter(|(_, _, name)| name == taxonomy)
        .map(|(id, name, _)| (id.to_string(), name))
        .collect())
}

/// The terms of the hierarchical taxonomy named `taxonomy` as a tree, as WordPress's category
/// checklist lists them: each term after its parent, siblings by name, a child's label indented
/// by [`LEVEL_INDENT`] per level. A term whose parent is not among the choices is at the top.
async fn terms_tree_of(db: &Database, taxonomy: &str) -> Result<Vec<(String, String)>, Error> {
    let rows: Vec<(i64, String, String, Option<i64>)> = raw::fetch_all(
        db,
        "SELECT t.id, t.name, tt.taxonomy, tt.parent_id FROM term_taxonomy tt \
         INNER JOIN terms t ON t.id = tt.term_id WHERE tt.taxonomy = ? \
         ORDER BY t.name, t.id LIMIT ?",
        &[taxonomy.into(), (MAX_OPTIONS as i64).into()],
    )
    .await?;
    let terms: Vec<(i64, String, Option<i64>)> = rows
        .into_iter()
        // Exact comparison: MySQL compares text without case.
        .filter(|(_, _, name, _)| name == taxonomy)
        .map(|(id, name, _, parent)| (id, name, parent))
        .collect();
    Ok(term_tree(&terms))
}

/// `terms` (`term_id`, name, parent), in name order, as a tree; see [`terms_tree_of`].
fn term_tree(terms: &[(i64, String, Option<i64>)]) -> Vec<(String, String)> {
    let ids: HashSet<i64> = terms.iter().map(|(id, _, _)| *id).collect();
    let mut children: HashMap<Option<i64>, Vec<usize>> = HashMap::new();
    for (index, (id, _, parent)) in terms.iter().enumerate() {
        // A parent outside the choices (or the term itself) puts the term at the top.
        let parent = parent.filter(|p| ids.contains(p) && p != id);
        children.entry(parent).or_default().push(index);
    }
    let mut out = Vec::with_capacity(terms.len());
    let mut placed = HashSet::new();
    // Depth-first, without recursion; a loop in the parents is cut where it repeats.
    let mut stack: Vec<(usize, usize)> = children
        .get(&None)
        .map(|top| top.iter().rev().map(|&i| (i, 0)).collect())
        .unwrap_or_default();
    loop {
        while let Some((index, depth)) = stack.pop() {
            if !placed.insert(index) {
                continue;
            }
            let (id, name, _) = &terms[index];
            out.push((
                id.to_string(),
                format!("{}{name}", LEVEL_INDENT.repeat(depth)),
            ));
            if let Some(kids) = children.get(&Some(*id)) {
                stack.extend(kids.iter().rev().map(|&i| (i, depth + 1)));
            }
        }
        // Terms only reachable through a loop of parents: listed at the top.
        match (0..terms.len()).find(|i| !placed.contains(i)) {
            Some(index) => stack.push((index, 0)),
            None => break,
        }
    }
    out
}

/// The `term_id`s of the terms of `taxonomy` named `names`, each created when the taxonomy has
/// none of that name (at the top level), as WordPress's save creates the tags typed in the tags
/// box and the Add New Category link creates a category. A name that cannot be a term (too long)
/// is left out, logged.
async fn term_ids_named(
    db: &Database,
    taxonomy: &Taxonomy,
    names: &[String],
) -> Result<Vec<i64>, Error> {
    let mut ids = Vec::new();
    for name in names {
        match Term::create(db, taxonomy, NewTerm::new(name)).await {
            Ok(term) => ids.push(term.term_id),
            Err(TermError::Exists { term_taxonomy_id }) => {
                if let Some(term) = Term::find(db, term_taxonomy_id).await? {
                    ids.push(term.term_id);
                }
            }
            Err(TermError::Database(e)) => return Err(e),
            Err(e) => log::warn!(
                "The new {} term `{name}` was not created: {e}.",
                taxonomy.name
            ),
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------------------------
// Writes

/// What a form changes on the post row, read and checked before the save.
#[derive(Debug, Default)]
struct Changes {
    title: Option<String>,
    slug: Option<String>,
    content: Option<String>,
    excerpt: Option<String>,
    author: Option<i64>,
    /// `Some(None)` for no parent.
    parent: Option<Option<i64>>,
    menu_order: Option<i32>,
    status: Option<PostStatus>,
    /// `Some(None)` for "Publish immediately".
    date: Option<Option<NaiveDateTime>>,
    /// `Some(None)` for no password.
    password: Option<Option<String>>,
}

impl Changes {
    /// `stored` with the changes made, its status and date settled as `wp_insert_post` settles
    /// them. A status of `trash` is not set here: the post is trashed after the save.
    fn apply(&self, stored: &Post) -> Post {
        let mut post = stored.clone();
        if let Some(title) = &self.title {
            post.title = title.clone();
        }
        if let Some(slug) = &self.slug {
            post.slug = slug.clone();
        }
        if let Some(content) = &self.content {
            post.content = content.clone();
        }
        if let Some(excerpt) = &self.excerpt {
            post.excerpt = excerpt.clone();
        }
        if let Some(author) = self.author {
            post.author_id = author;
        }
        if let Some(parent) = self.parent {
            post.parent_id = parent;
        }
        if let Some(order) = self.menu_order {
            post.menu_order = order;
        }
        if let Some(password) = &self.password {
            post.password = password.clone();
        }
        if let Some(date) = self.date {
            post.published_at = date;
        }
        let status = match self.status {
            Some(PostStatus::Trash) | None => post.status,
            Some(status) => status,
        };
        if self.status.is_some() || self.date.is_some() {
            let (status, date) = settle_status(status, post.published_at);
            post.status = status;
            post.published_at = date;
        }
        // `edit_post`: the private visibility clears the password.
        if self.status == Some(PostStatus::Private) {
            post.password = None;
        }
        post
    }
}

impl PostTypeData {
    /// Reads the post row's changes from the validated `input`, checking what refers to other
    /// rows: an author that does not exist is ignored, and so is a parent that is not a post of
    /// this type; a parent that would make a loop is none, as
    /// `wp_check_post_hierarchy_for_loops` breaks it.
    async fn changes(
        &self,
        db: &Database,
        stored: &Post,
        input: &Row,
        offset: FixedOffset,
    ) -> Result<Changes, Error> {
        let id = stored.id;
        let text = |key: &str| input.get(key).map(|v| text_of(v).unwrap_or_default());
        let mut changes = Changes::default();
        if self.title {
            changes.title = text(TITLE);
        }
        // The slug the form showed is no change: a trashed post's `__trashed` slug sent back
        // unchanged does not replace the slug its restore brings back. A changed slug goes
        // through the save's slug rules (sanitized, made unique once published), as the REST
        // API's `slug` does.
        changes.slug = text(SLUG).filter(|slug| *slug != stored.slug);
        if self.editor {
            changes.content = text(CONTENT);
        }
        if self.excerpt {
            changes.excerpt = text(EXCERPT);
        }
        if self.author
            && let Some(author) = input.get(AUTHOR).and_then(id_of)
        {
            let found: i64 = raw::fetch_scalar(
                db,
                "SELECT COUNT(*) FROM users WHERE id = ?",
                &[author.into()],
            )
            .await?;
            // A blank or unknown author keeps the post's: a post always has one.
            if found > 0 {
                changes.author = Some(author);
            }
        }
        if self.has_parent()
            && let Some(value) = input.get(PARENT)
        {
            let parent = match id_of(value) {
                Some(parent) => self.post(db, parent).await?.map(|p| p.id),
                None => None,
            };
            changes.parent = Some(loop_free_parent(db, id, parent).await?);
        }
        if self.page_attributes
            && let Some(value) = input.get(MENU_ORDER)
        {
            changes.menu_order = Some(value.as_i64().map_or(0, |n| {
                n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
            }));
        }
        if let Some(status) = input.get(STATUS).and_then(Value::as_str) {
            changes.status = STATUSES
                .iter()
                .map(|(s, _)| *s)
                .find(|s| s.as_str() == status);
        }
        if let Some(value) = input.get(DATE) {
            changes.date = Some(value.as_str().and_then(|t| utc_from_local(t, offset)));
        }
        if let Some(value) = input.get(PASSWORD) {
            changes.password = Some(text_of(value).filter(|p| !p.is_empty()));
        }
        Ok(changes)
    }

    /// Saves `changes` on post `id` (read as `stored`), as the last write: tried again on a
    /// post another save changed meanwhile. A save that changes the title, content or excerpt of
    /// a type with `revisions` records a revision, as `wp_save_post_revision` does; any other is
    /// a plain save. Returns the saved post.
    async fn save(
        &self,
        db: &Database,
        mut stored: Post,
        changes: &Changes,
    ) -> Result<Post, Error> {
        for _ in 0..MAX_ATTEMPTS {
            let post = changes.apply(&stored);
            let revised = post.title != stored.title
                || post.content != stored.content
                || post.excerpt != stored.excerpt;
            if !(self.revisions && revised) {
                let mut post = post;
                post.save(db).await?;
                return Ok(post);
            }
            match save_revision(db, &post, stored.modified_at).await {
                Ok(_) => return self.post(db, post.id).await?.ok_or_else(not_found),
                Err(SaveError::Conflict { .. }) => {
                    stored = self.post(db, stored.id).await?.ok_or_else(not_found)?;
                }
                Err(SaveError::NotFound { .. } | SaveError::IsRevision { .. }) => {
                    return Err(not_found());
                }
                Err(SaveError::Database(e)) => return Err(e),
            }
        }
        log::error!(
            "Post {} changed under {MAX_ATTEMPTS} saves in a row; the admin's save gave up.",
            stored.id
        );
        Err(Error::Database(sqlx::Error::Protocol(format!(
            "post {} kept changing during the save",
            stored.id
        ))))
    }

    /// Ruling R123: keeps the stored references the form could not show. [`RuntimeData::options`]
    /// does not know which post the form edits, so a stored term, parent, author or field
    /// group reference that is not among the bounded choices (past [`MAX_OPTIONS`], or a trashed
    /// parent) shows unselected and comes back blank. As WordPress's dropdowns add the selected
    /// value (`include_selected`), such a value is kept: a relationship's new list is the ids
    /// sent and the stored ids outside the choices; a single reference sent blank keeps its
    /// stored value when that value is not a choice. A value that is a choice can still be
    /// cleared. Rewrites `input` before the save.
    async fn keep_unshown(
        &self,
        db: &Database,
        stored: &Post,
        input: &mut Row,
    ) -> Result<(), Error> {
        let mut singles: Vec<(String, Option<String>)> = Vec::new();
        let mut lists: Vec<(String, Vec<String>)> = Vec::new();
        if self.author && input.contains_key(AUTHOR) {
            singles.push((AUTHOR.to_owned(), Some(stored.author_id.to_string())));
        }
        if self.has_parent() && input.contains_key(PARENT) {
            let parent = stored.parent_id.filter(|p| *p > 0).map(|p| p.to_string());
            singles.push((PARENT.to_owned(), parent));
        }
        if self.taxonomies.iter().any(|t| input.contains_key(&t.field)) {
            let terms = self.post_terms(db, &[stored.id], MAX_POST_TERMS).await?;
            for taxonomy in &self.taxonomies {
                if input.contains_key(&taxonomy.field) {
                    let ids = terms
                        .get(&(stored.id, taxonomy.taxonomy.name.to_string()))
                        .map(|list| list.iter().map(|(id, _)| id.to_string()).collect())
                        .unwrap_or_default();
                    lists.push((taxonomy.field.clone(), ids));
                }
            }
        }
        let referring: Vec<&CustomField> = self
            .custom
            .iter()
            .filter(|c| c.def.kind.loads_options() && input.contains_key(&c.field))
            .collect();
        if !referring.is_empty() {
            let mut values = self.custom_values(db, stored.id).await?;
            for custom in referring {
                let stored = form_value(
                    &custom.def.kind,
                    values.remove(&custom.def.name).unwrap_or(Value::Null),
                );
                if custom.def.kind == FieldKindDef::Relationship {
                    let ids = row_ids(&stored).iter().map(i64::to_string).collect();
                    lists.push((custom.field.clone(), ids));
                } else {
                    singles.push((custom.field.clone(), text_of(&stored)));
                }
            }
        }
        for (field, stored) in singles {
            let Some(stored) = stored.filter(|s| !s.is_empty()) else {
                continue;
            };
            let blank = input
                .get(&field)
                .is_none_or(|v| v.is_null() || text_of(v).is_some_and(|t| t.trim().is_empty()));
            if blank && !self.is_choice(db, &field, &stored).await? {
                input.insert(field, Value::from(stored));
            }
        }
        for (field, stored) in lists {
            if stored.is_empty() {
                continue;
            }
            let choices: HashSet<String> = self
                .choices(db, &field)
                .await?
                .into_iter()
                .map(|(value, _)| value)
                .collect();
            let mut ids: Vec<String> = input
                .get(&field)
                .map(|v| row_ids(v).iter().map(i64::to_string).collect())
                .unwrap_or_default();
            for id in stored {
                if !choices.contains(&id) && !ids.contains(&id) {
                    ids.push(id);
                }
            }
            input.insert(field, Value::from(json!(ids).to_string()));
        }
        Ok(())
    }

    /// Whether `value` is among the choices the form of `field` shows.
    async fn is_choice(&self, db: &Database, field: &str, value: &str) -> Result<bool, Error> {
        Ok(self
            .choices(db, field)
            .await?
            .iter()
            .any(|(choice, _)| choice == value))
    }

    /// The writes after the post row, for the keys `input` has: sticky, the featured image, the
    /// terms and the field group fields; then the scheduled publication.
    async fn save_extras(&self, db: &Database, post: &Post, input: &Row) -> Result<(), Error> {
        if self.sticky
            && let Some(Value::Bool(sticky)) = input.get(STICKY)
        {
            // The classic editor's visibility: a password-protected or private post is never
            // sticky.
            let protected = post.password.as_deref().is_some_and(|p| !p.is_empty());
            let sticky = *sticky && !protected && post.status != PostStatus::Private;
            set_sticky(db, post.id, sticky).await.map_err(meta_error)?;
        }
        if self.thumbnail
            && let Some(value) = input.get(FEATURED_MEDIA)
        {
            match id_of(value) {
                None => {
                    delete_post_meta(db, post.id, THUMBNAIL_META)
                        .await
                        .map_err(meta_error)?;
                }
                // `set_post_thumbnail` sets only an attachment; any other id is ignored.
                Some(media) => {
                    let is_attachment = Post::query()
                        .find(db, media)
                        .await?
                        .is_some_and(|p| p.post_type == "attachment");
                    if is_attachment {
                        set_post_meta(db, post.id, THUMBNAIL_META, &json!(media))
                            .await
                            .map_err(meta_error)?;
                    }
                }
            }
        }
        if !self.templates.is_empty()
            && let Some(value) = input.get(TEMPLATE)
        {
            // As the REST API stores it: the default (or none) deletes the meta. A name the
            // theme does not offer cannot pass the form's select; it is ignored here too.
            match text_of(value).filter(|t| !t.is_empty() && t != DEFAULT_TEMPLATE.0) {
                None => {
                    delete_post_meta(db, post.id, TEMPLATE_META)
                        .await
                        .map_err(meta_error)?;
                }
                Some(name) if self.templates.iter().any(|t| t.name == name) => {
                    // The same template, stored with another spelling (an imported
                    // `templates/wide.php`), is kept as it is.
                    let stored = stored_template(db, post.id).await?;
                    let same = stored
                        .as_deref()
                        .and_then(|value| self.template_named(value))
                        .is_some_and(|t| t.name == name);
                    if !same {
                        set_post_meta(db, post.id, TEMPLATE_META, &json!(name))
                            .await
                            .map_err(meta_error)?;
                    }
                }
                Some(_) => {}
            }
        }
        for taxonomy in &self.taxonomies {
            // `wp_insert_post`: a post (not an auto-draft) with no category gets the default.
            let gives_default = post.post_type == "post"
                && taxonomy.taxonomy.name == Taxonomy::CATEGORY.name
                && post.status != PostStatus::AutoDraft;
            let Some(value) = input.get(&taxonomy.field) else {
                // A form that sends no categories keeps the post's own; a post that has none
                // gets the default one.
                if gives_default
                    && post.terms(db, &Taxonomy::CATEGORY).await?.is_empty()
                    && let Some(default) = default_category(db).await?
                {
                    set_post_terms(db, post, &taxonomy.taxonomy, &[default]).await?;
                }
                continue;
            };
            let mut ids = row_ids(value);
            // The terms the form adds, created when the taxonomy has none of their name.
            let names = taxonomy
                .new_field
                .as_ref()
                .and_then(|field| input.get(field))
                .and_then(text_of)
                .map(|text| term_names(&text))
                .unwrap_or_default();
            if !names.is_empty() {
                for id in term_ids_named(db, &taxonomy.taxonomy, &names).await? {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
            if ids.is_empty() && gives_default {
                ids.extend(default_category(db).await?);
            }
            set_post_terms(db, post, &taxonomy.taxonomy, &ids).await?;
        }
        for custom in &self.custom {
            let Some(value) = input.get(&custom.field) else {
                continue;
            };
            let name = &custom.def.name;
            set_post_meta(db, post.id, name, &meta_value(&custom.def.kind, value))
                .await
                .map_err(meta_error)?;
            set_post_meta(db, post.id, &format!("_{name}"), &json!(custom.def.key))
                .await
                .map_err(meta_error)?;
        }
        sync_publication(db, post).await.map_err(scheduler_error)
    }

    /// Rewrites the form's `input` for `actor` before the save, as WordPress's `edit_post` and
    /// `_wp_translate_postdata` treat a user's submission (Task A1): the author is kept for a
    /// user without `edit_others_posts`, sticky is left alone for a user without
    /// `edit_others_posts` and `publish_posts`, and a user without `publish_posts` gets Pending
    /// for Published or Scheduled, and the post's own status for Private.
    fn for_actor(&self, actor: &Actor, stored: &Post, input: &mut Row) {
        let caps = &actor.caps;
        let edit_others = has_cap(caps, &self.caps.edit_others_posts);
        let publish = has_cap(caps, &self.caps.publish_posts);
        if !edit_others {
            input.remove(AUTHOR);
        }
        if !(edit_others && publish) {
            input.remove(STICKY);
        }
        // `wp_insert_post` leaves the terms of a taxonomy whose `assign_terms` the user lacks.
        for taxonomy in &self.taxonomies {
            if !has_cap(caps, &taxonomy.assign_terms) {
                input.remove(&taxonomy.field);
            }
            // A new category needs `edit_terms` (the Add New Category link's check); a typed tag
            // needs `assign_terms`.
            if let Some(new_field) = &taxonomy.new_field
                && !(has_cap(caps, &taxonomy.assign_terms) && has_cap(caps, &taxonomy.create_terms))
            {
                input.remove(new_field);
            }
        }
        if !publish {
            // `_wp_translate_postdata` unsets the password of a user who cannot publish.
            input.remove(PASSWORD);
        }
        // A post already published or scheduled keeps its status for a user who may edit it
        // (`_wp_translate_postdata` checks `publish_posts` only on a new publication).
        let published = matches!(stored.status, PostStatus::Publish | PostStatus::Future);
        if !publish && let Some(status) = input.get(STATUS).and_then(Value::as_str) {
            let demoted = match status {
                "publish" | "future" if !published => Some(PostStatus::Pending),
                // `_wp_translate_postdata` keeps the previous status; an auto-draft's form
                // shows Draft.
                "private" => Some(match stored.status {
                    PostStatus::AutoDraft => PostStatus::Draft,
                    status => status,
                }),
                _ => None,
            };
            if let Some(status) = demoted {
                input.insert(STATUS.into(), Value::from(status.as_str()));
            }
        }
    }

    /// The rich text of the field group fields of `input` through kses, for a user without
    /// `unfiltered_html` (Ruling R139): a rich text field, and each rich text sub-field of a
    /// repeater, group, flexible content or clone field (their JSON value, walked by the
    /// field's sub-fields).
    async fn kses_custom(&self, actor: &Actor, input: &mut Row) -> Result<(), Error> {
        if has_cap(&actor.caps, "unfiltered_html") {
            return Ok(());
        }
        for custom in &self.custom {
            if !has_rich_text(&custom.def.kind) {
                continue;
            }
            let Some(text) = input.get(&custom.field).and_then(text_of) else {
                continue;
            };
            if text.len() > crate::content::MAX_FILTERED_BYTES {
                return Err(refused(
                    "a rich text field longer than kses filters for a user without \
                     unfiltered_html",
                ));
            }
            let kind = custom.def.kind.clone();
            let filtered = self
                .access
                .kses
                .run(move || {
                    if kind == FieldKindDef::Wysiwyg {
                        return kses_post(&text);
                    }
                    match serde_json::from_str::<Value>(&text) {
                        Ok(mut value) => {
                            kses_value(&kind, &mut value);
                            value.to_string()
                        }
                        // Not JSON: Ignite's check refuses it; nothing to filter.
                        Err(_) => text,
                    }
                })
                .await
                .map_err(|e| kses_error(&e))?;
            input.insert(custom.field.clone(), Value::from(filtered));
        }
        Ok(())
    }

    /// The update of post `id` from the validated `input`, for `actor` when the panel gives one;
    /// see the [module](self).
    async fn update_post(
        &self,
        db: &Database,
        id: i64,
        input: Row,
        actor: Option<&Actor>,
    ) -> Result<Row, Error> {
        let mut stored = self.post(db, id).await?.ok_or_else(not_found)?;
        let mut input = input;
        if let Some(actor) = actor {
            if !Self::in_scope(&stored, self.scope(&actor.caps, actor.id)) {
                return Err(not_found());
            }
            // The policy checked the post Ignite found; the post read here is checked again.
            if !self.may(db, actor, &stored, "edit_post").await? {
                return Err(refused("editing a post without edit_post on it"));
            }
            self.for_actor(actor, &stored, &mut input);
            self.kses_custom(actor, &mut input).await?;
        }
        self.keep_unshown(db, &stored, &mut input).await?;
        let offset = crate::content::gmt_offset(db).await?;
        let mut changes = self.changes(db, &stored, &input, offset).await?;
        if let Some(actor) = actor
            && !has_cap(&actor.caps, "unfiltered_html")
        {
            // kses runs on the whole post, as `wp_update_post` filters the merged fields.
            let fields = self
                .access
                .kses
                .filter(
                    &actor.caps,
                    PostFields {
                        title: changes
                            .title
                            .clone()
                            .unwrap_or_else(|| stored.title.clone()),
                        content: changes
                            .content
                            .clone()
                            .unwrap_or_else(|| stored.content.clone()),
                        excerpt: changes
                            .excerpt
                            .clone()
                            .unwrap_or_else(|| stored.excerpt.clone()),
                    },
                )
                .await
                .map_err(|e| kses_error(&e))?;
            changes.title = Some(fields.title);
            changes.content = Some(fields.content);
            changes.excerpt = Some(fields.excerpt);
        }
        let wants_trash = changes.status == Some(PostStatus::Trash);
        let leaves_trash =
            stored.status == PostStatus::Trash && changes.status.is_some() && !wants_trash;
        if let Some(actor) = actor
            && (leaves_trash || (wants_trash && stored.status != PostStatus::Trash))
            && !self.may(db, actor, &stored, "delete_post").await?
        {
            // `wp_trash_post` and `wp_untrash_post` need `delete_post` on the post.
            return Err(refused(
                "moving the post to or out of the trash without delete_post",
            ));
        }
        if actor.is_none()
            && wants_trash
            && stored.status != PostStatus::Trash
            && self.is_site_page(db, id).await?
        {
            log::warn!(
                "Post {id} is the front page or the posts page: the form cannot move it to the \
                 trash; the list's Trash action can, for a user with `delete_post` on it."
            );
            return Err(Error::Unsupported {
                operation: "moving the front page or the posts page to the trash from the form \
                            (use the list's Trash action, which checks delete_post)",
                driver: "Mission Control's post type resources",
            });
        }
        // A trashed post saved with another status leaves the trash first, as a draft. Not one
        // transaction with the save: `Post::untrash`, `save_revision` and `Post::save` each run
        // their own. A save that fails after the restore leaves the post restored, as a draft,
        // with the form's changes lost, as WordPress's two steps (restore, then edit) would.
        if leaves_trash {
            stored.untrash(db).await?;
        }
        let mut saved = self.save(db, stored, &changes).await?;
        self.save_extras(db, &saved, &input).await?;
        if wants_trash && saved.status != PostStatus::Trash {
            saved.trash(db).await?;
            sync_publication(db, &saved)
                .await
                .map_err(scheduler_error)?;
        }
        let saved = self.post(db, id).await?.ok_or_else(not_found)?;
        self.row(db, &saved).await
    }

    /// The New screen's first save, from the validated `input`, for `actor` when the panel gives
    /// one: the post is written as an `auto-draft` by the author (the actor, or the `author` of
    /// `input` without one) and saved at once with [`update_post`](Self::update_post), so it
    /// goes through the same rules as every later save (slug, status, kses, terms, revisions).
    /// A blank status is Draft. When the save fails, the row is deleted, so a refused save leaves
    /// no post behind.
    async fn create_post(
        &self,
        db: &Database,
        mut input: Row,
        actor: Option<&Actor>,
    ) -> Result<Row, Error> {
        let author = match actor {
            Some(actor) => actor.id,
            None => input
                .get(AUTHOR)
                .and_then(id_of)
                .ok_or_else(|| refused("creating a post without an author"))?,
        };
        // WordPress's New screen starts as a draft: "Save Draft" unless another status is chosen.
        if input.get(STATUS).and_then(Value::as_str).is_none() {
            input.insert(STATUS.into(), Value::from(PostStatus::Draft.as_str()));
        }
        let draft = create_auto_draft(db, &self.post_type, author).await?;
        match self.update_post(db, draft.id, input, actor).await {
            Ok(row) => Ok(row),
            Err(error) => {
                if let Err(e) = draft.delete(db).await {
                    log::error!(
                        "Could not delete post {} after its first save failed: {e}; the daily \
                         auto-draft cleanup will.",
                        draft.id
                    );
                }
                Err(error)
            }
        }
    }

    /// The theme's page template a stored `_wp_page_template` value names: compared by template
    /// name ([`page_template_name`]: the file extension dropped), as the front end loads it.
    fn template_named(&self, stored: &str) -> Option<&PageTemplate> {
        let name = page_template_name(stored)?;
        self.templates.iter().find(|t| t.name == name)
    }

    /// Whether `actor` has the meta capability `cap` (`edit_post`, `delete_post`) on `post`, as
    /// `map_meta_cap` maps it.
    async fn may(
        &self,
        db: &Database,
        actor: &Actor,
        post: &Post,
        cap: &str,
    ) -> Result<bool, Error> {
        let context = PostContext {
            post,
            trash_meta_status: crate::admin::posts::trash_status(db, post).await?,
            parent: None,
            site_pages: Some(crate::admin::posts::site_pages(db).await?),
            post_type_caps: Some(&self.caps),
        };
        Ok(can(
            &actor.caps,
            cap,
            Some(&Object::PostWithContext(context)),
        ))
    }

    /// The acting `user` with their capabilities.
    async fn actor(&self, db: &Database, user: &ActingUser) -> Result<Actor, Error> {
        Ok(Actor {
            caps: self.access.caps(db, user).await?,
            id: user.id(),
        })
    }

    /// Whether post `id` is the static front page (`page_on_front`) or the posts page
    /// (`page_for_posts`), which only a user with `manage_options` may move to the trash
    /// (`map_meta_cap`'s `delete_post`). The data layer does not know the user, so the form's
    /// Trash status is refused on them for everyone; the list's Trash action, which checks
    /// `delete_post`, still trashes them.
    async fn is_site_page(&self, db: &Database, id: i64) -> Result<bool, Error> {
        for option in ["page_on_front", "page_for_posts"] {
            let page = match get_option(db, option).await {
                Ok(value) => value.as_ref().and_then(id_of),
                Err(MetaError::InvalidJson(_)) => None,
                Err(MetaError::Database(e)) => return Err(e),
            };
            if page == Some(id) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The noun of WordPress's list messages for the type (`$bulk_messages`): "page" for
    /// `page`, "post" for the others.
    fn noun(&self) -> &'static str {
        if self.post_type == "page" {
            "page"
        } else {
            "post"
        }
    }

    /// The post `id` and whether `user` has `delete_post` on it, or the message for a post
    /// that is gone: the row action's checks after Ignite's.
    async fn post_for_action(
        &self,
        db: &Database,
        user: &ActingUser,
        id: i64,
    ) -> Result<Result<(Post, bool), String>, Error> {
        let Some(post) = self.post(db, id).await? else {
            return Ok(Err(
                "You attempted to edit an item that does not exist. Perhaps it was deleted?"
                    .to_owned(),
            ));
        };
        let actor = self.actor(db, user).await?;
        let may = self.may(db, &actor, &post, "delete_post").await?;
        Ok(Ok((post, may)))
    }

    /// The row action "Trash": `wp_trash_post`, for a user with `delete_post` on the post.
    fn trash_row<'a>(
        self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
    ) -> RuntimeFuture<'a, ActionResult> {
        Box::pin(async move {
            let (post, may) = match self.post_for_action(db, user, id).await? {
                Ok(found) => found,
                Err(message) => return Ok(Err(message)),
            };
            if !may {
                return Ok(Err(Refusal::TrashPost.message().to_owned()));
            }
            if post.status != PostStatus::Trash {
                self.delete_post(db, id).await?;
            }
            Ok(Ok(()))
        })
    }

    /// The row action "Restore": `wp_untrash_post` (a draft, with its slug), for a user with
    /// `delete_post` on the post.
    fn untrash_row<'a>(
        self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
    ) -> RuntimeFuture<'a, ActionResult> {
        Box::pin(async move {
            let (mut post, may) = match self.post_for_action(db, user, id).await? {
                Ok(found) => found,
                Err(message) => return Ok(Err(message)),
            };
            if !may {
                return Ok(Err(Refusal::RestorePost.message().to_owned()));
            }
            post.untrash(db).await?;
            sync_publication(db, &post).await.map_err(scheduler_error)?;
            Ok(Ok(()))
        })
    }

    /// The row action "Delete Permanently": `wp_delete_post` on a trashed post, for a user with
    /// `delete_post` on it.
    fn delete_row<'a>(
        self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
    ) -> RuntimeFuture<'a, ActionResult> {
        Box::pin(async move {
            let (post, may) = match self.post_for_action(db, user, id).await? {
                Ok(found) => found,
                Err(message) => return Ok(Err(message)),
            };
            if !may {
                return Ok(Err(Refusal::DeletePost.message().to_owned()));
            }
            if post.status == PostStatus::Trash {
                post.delete(db).await?;
            }
            Ok(Ok(()))
        })
    }

    /// Adds to each of `rows` whether `actor` has `delete_post` on its post
    /// ([`COL_DELETE_POST`]).
    async fn flag_delete_post(
        &self,
        db: &Database,
        actor: &Actor,
        rows: &mut [Row],
    ) -> Result<(), Error> {
        for row in rows {
            let Some(id) = row.get("id").and_then(id_of) else {
                continue;
            };
            let may = match self.post(db, id).await? {
                Some(post) => self.may(db, actor, &post, "delete_post").await?,
                None => false,
            };
            row.insert(COL_DELETE_POST.into(), Value::Bool(may));
        }
        Ok(())
    }

    /// Trashes post `id`, or deletes a trashed one for good. Returns how many posts went: 0 when
    /// no post of this type has the id.
    async fn delete_post(&self, db: &Database, id: i64) -> Result<u64, Error> {
        let Some(mut post) = self.post(db, id).await? else {
            return Ok(0);
        };
        if post.status == PostStatus::Trash {
            return Ok(u64::from(post.delete(db).await?));
        }
        post.trash(db).await?;
        sync_publication(db, &post).await.map_err(scheduler_error)?;
        Ok(1)
    }
}

impl PostTypeData {
    /// WordPress's check of the featured image (`rest_invalid_featured_media`): a value sent
    /// that is not empty, `0` (no featured image) or the id of an attachment is refused with
    /// [`INVALID_FEATURED_MEDIA`] on the field.
    async fn featured_media_refusal(
        &self,
        db: &Database,
        input: &Row,
    ) -> Result<Option<ValidationError>, Error> {
        if !self.thumbnail {
            return Ok(None);
        }
        let refusal = || {
            Some(ValidationError::with_messages([(
                FEATURED_MEDIA,
                INVALID_FEATURED_MEDIA,
            )]))
        };
        let id = match input.get(FEATURED_MEDIA) {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::String(text)) if text.trim().is_empty() || text.trim() == "0" => {
                return Ok(None);
            }
            Some(Value::Number(number)) if number.as_i64() == Some(0) => return Ok(None),
            Some(value) => match id_of(value) {
                Some(id) => id,
                None => return Ok(refusal()),
            },
        };
        // `set_post_thumbnail` sets only an image (`wp_get_attachment_image` is empty for any
        // other file), and the REST API answers its failure with this message.
        let is_image = Post::query()
            .find(db, id)
            .await?
            .is_some_and(|post| post.post_type == "attachment" && is_image_mime(&post.mime_type));
        Ok(if is_image { None } else { refusal() })
    }

    /// The form's checks before a save of post `id` (`None` on the New screen), by field: a new
    /// post with the Trash status ([`NEW_POST_TRASH`]), WordPress's check of the featured image
    /// ([`featured_media_refusal`](Self::featured_media_refusal)), a post chosen as its own
    /// parent ([`own_parent_message`]; the edit form's Parent leaves the post and its
    /// descendants out, as `wp_dropdown_pages`' `exclude_tree`), and a sticky post
    /// with a password ([`STICKY_WITH_PASSWORD`], the REST API's refusal: WordPress's editor hides
    /// "Stick to the top of the blog" for a password-protected post, and `edit_post` unsticks
    /// it).
    async fn form_refusal(
        &self,
        db: &Database,
        id: Option<i64>,
        input: &Row,
    ) -> Result<Option<ValidationError>, Error> {
        let mut messages: Vec<(&str, String)> = Vec::new();
        // WordPress's New screen offers no Trash: a post goes to the trash once it exists. The
        // New screen's Status leaves it out (the app's form fairing); a create that sends it is
        // refused on the field with Ignite's message for a choice the select does not offer.
        if id.is_none()
            && input.get(STATUS).and_then(Value::as_str) == Some(PostStatus::Trash.as_str())
        {
            messages.push((STATUS, NEW_POST_TRASH.to_owned()));
        }
        if self.featured_media_refusal(db, input).await?.is_some() {
            messages.push((FEATURED_MEDIA, INVALID_FEATURED_MEDIA.to_owned()));
        }
        if self.has_parent()
            && let Some(id) = id
            && input.get(PARENT).and_then(id_of) == Some(id)
        {
            messages.push((PARENT, own_parent_message(self.noun())));
        }
        let password = matches!(input.get(PASSWORD), Some(Value::String(p)) if !p.is_empty());
        if self.sticky && password && input.get(STICKY) == Some(&Value::Bool(true)) {
            messages.push((STICKY, STICKY_WITH_PASSWORD.to_owned()));
        }
        Ok((!messages.is_empty()).then(|| ValidationError::with_messages(messages)))
    }
}

/// Whether `mime` is an image's MIME type, as `wp_attachment_is_image` decides for the featured
/// image.
fn is_image_mime(mime: &str) -> bool {
    mime.to_ascii_lowercase().starts_with("image/")
}

/// The refusal of a new post saved with the Trash status, on the Status field: Ignite's message
/// for a choice its select does not offer (the New screen has no Trash).
pub const NEW_POST_TRASH: &str = "Choose one of the options.";

/// The refusal of a post chosen as its own parent, on the Parent field: "A page cannot be its
/// own parent.".
pub fn own_parent_message(noun: &str) -> String {
    format!("A {noun} cannot be its own parent.")
}

/// The refusal of a sticky post with a password, on the sticky field: the REST API's message
/// (`WP_REST_Posts_Controller::prepare_item_for_database`).
pub const STICKY_WITH_PASSWORD: &str = "A post can not be sticky and have a password.";

impl RuntimeData for PostTypeData {
    fn list<'a>(&'a self, db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move { self.list_page(db, query, None).await })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move {
            match self.post(db, id).await? {
                Some(post) => Ok(Some(self.row(db, &post).await?)),
                None => Ok(None),
            }
        })
    }

    /// The New screen's first save, with every capability of the type and `unfiltered_html`:
    /// the post, by the `author` of `input` (required here, as no user is given), written with
    /// the save's rules; see [`create_for`](RuntimeData::create_for).
    fn create<'a>(&'a self, db: &'a Database, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move { self.create_post(db, input, None).await })
    }

    fn update<'a>(&'a self, db: &'a Database, id: i64, input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move { self.update_post(db, id, input, None).await })
    }

    fn delete<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, u64> {
        Box::pin(async move { self.delete_post(db, id).await })
    }

    /// The users for `author`; the type's other posts for `parent` (the post itself among them,
    /// since the choices do not know which post the form edits: choosing it saves no parent);
    /// the terms for a taxonomy field; and for a field group field, the users, the posts of its
    /// `post_type` setting (any type when unset), or the terms of its `taxonomy` setting.
    fn options<'a>(
        &'a self,
        db: &'a Database,
        field: &'a str,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move { self.choices(db, field).await })
    }

    /// The list for `user`: their own posts only without `edit_others_posts`, and without other
    /// users' private posts without `read_private_posts` (Task A1).
    fn list_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        query: &'a ListQuery,
    ) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move {
            let actor = self.actor(db, user).await?;
            let scope = self.scope(&actor.caps, actor.id);
            let mut page = self.list_page(db, query, Some(scope)).await?;
            self.flag_delete_post(db, &actor, &mut page.rows).await?;
            Ok(page)
        })
    }

    /// Post `id` when it is in `user`'s list; otherwise `None`, which Ignite answers with 404.
    fn find_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
    ) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move {
            let actor = self.actor(db, user).await?;
            let scope = self.scope(&actor.caps, actor.id);
            match self.post(db, id).await? {
                Some(post) if Self::in_scope(&post, scope) => {
                    let mut row = self.row(db, &post).await?;
                    let may = self.may(db, &actor, &post, "delete_post").await?;
                    row.insert(COL_DELETE_POST.into(), Value::Bool(may));
                    Ok(Some(row))
                }
                _ => Ok(None),
            }
        })
    }

    /// The New screen's first save for `user`: the post gets its id now, with `user` as its
    /// author unless the form names another they may choose, and the save's rules for them (see
    /// the [module](self)'s Trust section). The policy has checked the type's `edit_posts`.
    /// Records the new post for [`PostTypeResources::take_created`].
    fn create_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        input: Row,
    ) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            let actor = self.actor(db, user).await?;
            let row = self.create_post(db, input, Some(&actor)).await?;
            if let Some(id) = row.get("id").and_then(id_of) {
                self.access.record_created(actor.id, &self.post_type, id);
            }
            Ok(row)
        })
    }

    /// [`create_for`](RuntimeData::create_for), after the form's checks
    /// ([`form_refusal`](Self::form_refusal)).
    fn try_create_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move {
            if let Some(refused) = self.form_refusal(db, None, &input).await? {
                return Ok(Err(refused));
            }
            self.create_for(db, user, input).await.map(Ok)
        })
    }

    /// [`update_for`](RuntimeData::update_for), after the form's checks
    /// ([`form_refusal`](Self::form_refusal)).
    fn try_update_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
        input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move {
            if let Some(refused) = self.form_refusal(db, Some(id), &input).await? {
                return Ok(Err(refused));
            }
            self.update_for(db, user, id, input).await.map(Ok)
        })
    }

    /// The save of post `id` for `user`; see the [module](self)'s Trust section.
    fn update_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
        input: Row,
    ) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            let actor = self.actor(db, user).await?;
            self.update_post(db, id, input, Some(&actor)).await
        })
    }

    /// Delete for `user`: Ignite has found the post with [`find_for`](RuntimeData::find_for)
    /// and the policy has checked `delete_post` on it.
    fn delete_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
    ) -> RuntimeFuture<'a, u64> {
        Box::pin(async move {
            let actor = self.actor(db, user).await?;
            match self.post(db, id).await? {
                Some(post) if Self::in_scope(&post, self.scope(&actor.caps, actor.id)) => {
                    if !self.may(db, &actor, &post, "delete_post").await? {
                        return Err(refused("deleting a post without delete_post on it"));
                    }
                    self.delete_post(db, id).await
                }
                _ => Ok(0),
            }
        })
    }

    /// The choices for `user`: a user without `edit_others_posts` is offered only themselves as
    /// the author (WordPress shows them no Author box).
    fn options_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        field: &'a str,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move {
            if field == AUTHOR && self.author {
                let actor = self.actor(db, user).await?;
                if !has_cap(&actor.caps, &self.caps.edit_others_posts) {
                    return Ok(vec![(user.id().to_string(), user.name().to_owned())]);
                }
            }
            self.choices(db, field).await
        })
    }
}

/// Whether a field of `kind` holds rich text, itself or in a sub-field.
fn has_rich_text(kind: &FieldKindDef) -> bool {
    match kind {
        FieldKindDef::Wysiwyg => true,
        FieldKindDef::Repeater(fields)
        | FieldKindDef::Group(fields)
        | FieldKindDef::Clone(fields) => fields.iter().any(|f| has_rich_text(&f.kind)),
        FieldKindDef::FlexibleContent(blocks) => blocks
            .iter()
            .any(|b| b.fields.iter().any(|f| has_rich_text(&f.kind))),
        _ => false,
    }
}

/// Runs kses on every rich text value inside `value`, a field of `kind`: a repeater's rows and
/// a group's or clone's object are keyed by sub-field name; a flexible content row is
/// `{"type": <block>, "data": {...}}`, as Ignite stores them.
fn kses_value(kind: &FieldKindDef, value: &mut Value) {
    let object = |fields: &[IgniteFieldDef], object: &mut serde_json::Map<String, Value>| {
        for field in fields {
            if let Some(inner) = object.get_mut(&field.name) {
                kses_value(&field.kind, inner);
            }
        }
    };
    match (kind, value) {
        (FieldKindDef::Wysiwyg, Value::String(text)) => *text = kses_post(text),
        (FieldKindDef::Repeater(fields), Value::Array(rows)) => {
            for row in rows {
                if let Value::Object(row) = row {
                    object(fields, row);
                }
            }
        }
        (FieldKindDef::Group(fields) | FieldKindDef::Clone(fields), Value::Object(row)) => {
            object(fields, row);
        }
        (FieldKindDef::FlexibleContent(blocks), Value::Array(rows)) => {
            for row in rows {
                let Value::Object(row) = row else { continue };
                let block = row
                    .get("type")
                    .and_then(Value::as_str)
                    .and_then(|name| blocks.iter().find(|b| b.name == name))
                    .cloned();
                if let (Some(block), Some(Value::Object(data))) = (block, row.get_mut("data")) {
                    object(&block.fields, data);
                }
            }
        }
        _ => {}
    }
}

/// The acting user of a save, with their capabilities (Task A1).
#[derive(Debug, Clone)]
struct Actor {
    caps: UserCaps,
    id: i64,
}

/// A save the acting user may not make: the panel shows a server error, and nothing is written.
fn refused(operation: &'static str) -> Error {
    log::warn!("An admin save was refused: {operation}");
    Error::Unsupported {
        operation,
        driver: "Mission Control's post type resources",
    }
}

/// kses's error (a field too long to filter, or its thread failing) as a database error.
fn kses_error(error: &crate::rest::RestError) -> Error {
    Error::Database(sqlx::Error::Protocol(format!(
        "the admin's kses filter refused the save: {error}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kses_reaches_rich_text_in_nested_fields() {
        let field = |name: &str, kind: FieldKindDef| IgniteFieldDef {
            name: name.to_owned(),
            label: name.to_owned(),
            kind,
            required: false,
        };
        let group = FieldKindDef::Group(vec![
            field("body", FieldKindDef::Wysiwyg),
            field("note", FieldKindDef::Text),
        ]);
        let repeater = FieldKindDef::Repeater(vec![
            field("text", FieldKindDef::Wysiwyg),
            field("inner", group.clone()),
        ]);
        let flexible = FieldKindDef::FlexibleContent(vec![ignite_core::BlockDef {
            name: "hero".to_owned(),
            label: "Hero".to_owned(),
            fields: vec![field("copy", FieldKindDef::Wysiwyg)],
        }]);
        assert!(has_rich_text(&repeater) && has_rich_text(&flexible));
        assert!(!has_rich_text(&FieldKindDef::Group(vec![field(
            "x",
            FieldKindDef::Text
        )])));
        let bad = "<p>a<script>x()</script></p>";
        let mut value = json!([{"text": bad, "inner": {"body": bad, "note": bad}}]);
        kses_value(&repeater, &mut value);
        assert!(!value[0]["text"].as_str().unwrap().contains("<script"));
        assert!(
            !value[0]["inner"]["body"]
                .as_str()
                .unwrap()
                .contains("<script")
        );
        // Plain text sub-fields are post meta, as WordPress stores them.
        assert_eq!(value[0]["inner"]["note"], bad);
        let mut value = json!([{"type": "hero", "data": {"copy": bad}}]);
        kses_value(&flexible, &mut value);
        assert!(
            !value[0]["data"]["copy"]
                .as_str()
                .unwrap()
                .contains("<script")
        );
    }

    #[test]
    fn hierarchical_choices_are_a_tree_indented_by_level() {
        let terms = vec![
            (3, "Apples".to_owned(), Some(1)),
            (1, "Fruit".to_owned(), None),
            (4, "Loop".to_owned(), Some(4)),
            (2, "Red apples".to_owned(), Some(3)),
            (5, "Zoo".to_owned(), Some(99)),
        ];
        let tree: Vec<(String, String)> = term_tree(&terms);
        let indent = LEVEL_INDENT;
        assert_eq!(
            tree,
            [
                ("1".to_owned(), "Fruit".to_owned()),
                ("3".to_owned(), format!("{indent}Apples")),
                ("2".to_owned(), format!("{indent}{indent}Red apples")),
                ("4".to_owned(), "Loop".to_owned()),
                ("5".to_owned(), "Zoo".to_owned()),
            ]
        );
        // Two terms that are each other's parent are both listed.
        let looped = vec![(1, "A".to_owned(), Some(2)), (2, "B".to_owned(), Some(1))];
        assert_eq!(term_tree(&looped).len(), 2);
    }

    #[test]
    fn new_term_names_are_split_by_commas() {
        assert_eq!(
            term_names(" News, sports ,,News, Sports , Rust"),
            ["News", "sports", "Rust"]
        );
        assert!(term_names(" , ").is_empty());
        assert_eq!(new_terms_field_name("category", true), "new_category");
        assert_eq!(
            new_terms_field_name("post_tag", false),
            "tax_input_post_tag"
        );
        assert_eq!(new_terms_field_name("book-genre", true), "new_book_genre");
    }

    #[test]
    fn the_post_editor_has_a_main_column_and_a_sidebar() {
        use ignite_core::{ColumnSpan, Component};

        let registry = Registry::default();
        let [post, page] = builtin_post_types();
        for def in [&post, &page] {
            let resource = post_type_resource(def, &registry);
            let layout = &resource.form_layout;
            assert_eq!(layout.column_count(), Some(3));
            let columns: Vec<(ColumnSpan, Vec<String>)> = layout
                .components()
                .iter()
                .map(|component| {
                    let Component::Group(group) = component else {
                        panic!("a top-level component that is not a column");
                    };
                    let fields = group
                        .components()
                        .iter()
                        .flat_map(|inner| match inner {
                            Component::Section(section) => section.fields().to_vec(),
                            _ => Vec::new(),
                        })
                        .collect();
                    (group.get_column_span(), fields)
                })
                .collect();
            assert_eq!(columns.len(), 2);
            assert_eq!(columns[0].0, ColumnSpan::Columns(2));
            assert_eq!(columns[1].0, ColumnSpan::Columns(1));
            for name in [TITLE, SLUG, CONTENT, EXCERPT] {
                assert!(columns[0].1.iter().any(|f| f == name), "{name}");
            }
            for name in [AUTHOR, FEATURED_MEDIA, STATUS, DATE, PASSWORD] {
                assert!(columns[1].1.iter().any(|f| f == name), "{name}");
            }
            // Every field of the form is placed.
            for field in &resource.fields {
                assert!(
                    columns
                        .iter()
                        .any(|(_, fields)| fields.contains(&field.name)),
                    "{} is not placed",
                    field.name
                );
            }
            assert_eq!(resource.conditions.slug_source(SLUG), Some(TITLE));
        }
        let resource = post_type_resource(&post, &registry);
        let names: Vec<&str> = resource.fields.iter().map(|f| f.name.as_str()).collect();
        for name in [
            "tax_category",
            "new_category",
            "tax_post_tag",
            "tax_input_post_tag",
        ] {
            assert!(names.contains(&name), "{name}");
        }
    }

    #[test]
    fn keys_map_to_ignite_names() {
        assert_eq!(
            custom_field_name("field_65a1B-2").as_deref(),
            Some("field_65a1b_2")
        );
        assert_eq!(custom_field_name("field_x").as_deref(), Some("field_x"));
        assert_eq!(
            custom_field_name(&format!("field_{}", "a".repeat(60))),
            None
        );
        assert_eq!(custom_field_name("1field"), None);
    }

    #[test]
    fn dates_are_stored_as_acf_stores_them() {
        let stored = meta_value(&FieldKindDef::Date, &json!("2026-11-15"));
        assert_eq!(stored, json!("20261115"));
        assert_eq!(form_value(&FieldKindDef::Date, stored), json!("2026-11-15"));
    }

    #[test]
    fn structured_values_are_stored_as_json() {
        let kind = FieldKindDef::Relationship;
        let stored = meta_value(&kind, &json!("[\"3\",\"5\"]"));
        assert_eq!(stored, json!(["3", "5"]));
        assert_eq!(form_value(&kind, stored), json!("[\"3\",\"5\"]"));
        let image = meta_value(&FieldKindDef::Image, &json!("12"));
        assert_eq!(image, json!(12));
        assert_eq!(form_value(&FieldKindDef::Image, image), json!("12"));
    }

    #[test]
    fn local_times_round_trip() {
        let offset = FixedOffset::east_opt(2 * 3600).unwrap();
        let utc = utc_from_local("2026-03-01 12:00:00", offset).unwrap();
        assert_eq!(utc.to_string(), "2026-03-01 10:00:00");
        assert_eq!(local_text(utc, offset), "2026-03-01 12:00:00");
    }
}
