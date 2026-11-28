//! The admin's rules that need neither Rocket nor Ignite's routes: who may use the screens of a
//! post type ([`posts`], Task A1), the media library's screen, picker and uploads
//! ([`media`], Rulings R127 to R129), the settings screens ([`settings`], Ruling R132) and the
//! menus screen ([`menus`], Ruling R133), the Categories and Tags screens ([`terms`]), the
//! sidebar's WordPress order ([`navigation`]), the dashboard's widgets ([`dashboard`]), and the
//! WordPress capability each screen's Ignite abilities are ([`ScreenCaps`]). The
//! `mission-control` crate mounts Ignite's panel at `/mc-admin` and applies them before Ignite's
//! routes run.

use ignite_core::{RedirectAfter, ResourceAction, RuntimeResource};

pub mod dashboard;
pub mod locks;
pub mod managers;
pub mod media;
pub mod menus;
pub mod navigation;
pub mod posts;
pub mod settings;
pub mod terms;

pub use media::{
    ATTACHMENT, AttachmentData, AttachmentPage, AttachmentPolicy, AttachmentQuery, Attachments,
    UploadRefusal, attachment_for_js, attachment_post_type, attachment_resource, may_upload,
    prepare_attachment_for_js, query_attachments, register_attachment_resource, upload_to_library,
};
pub use menus::{
    NAV_MENUS, NavMenusData, authorize_menus_screen, nav_menus_resource,
    register_nav_menus_resource,
};
pub use posts::{
    PostScreen, PostTypePolicy, Refusal, authorize_post_screen, may_use_post_screens,
    post_screen_caps,
};
pub use settings::{
    SETTINGS_RECORD, SettingsData, SettingsErrors, SettingsScreen, SettingsServices,
    apply_media_sizes, authorize_settings_screen, read_settings, register_settings_resources,
    save_settings, settings_choices, settings_fields, settings_resource, timezone_choices,
};
pub use terms::{
    CATEGORY_SCREEN, POST_TAG_SCREEN, TERM_SCREENS, TaxonomyScreens, TermsData, TermsPolicy,
    authorize_term_screen, custom_terms_resource, register_term_resources, term_screen_taxonomy,
    terms_resource,
};

/// `resource` with its saves staying on the edit page, as WordPress's screens do: a create goes
/// to the new record's edit form (`post-new.php` to `post.php?action=edit`), and an update comes
/// back to the same form with its message (`post.php?message=1`, `term.php?message=3`,
/// `options.php` back to its screen), as Filament's default `getRedirectUrl()` (Ignite's
/// [`RedirectAfter::Edit`]) where Ignite's own default is the list.
pub fn stay_on_edit(resource: RuntimeResource) -> RuntimeResource {
    resource
        .redirect_after_create(RedirectAfter::Edit)
        .redirect_after_update(RedirectAfter::Edit)
}

/// The WordPress capabilities a screen's Ignite abilities are: the list (`viewAny`, and `view`),
/// the New screen and its save (`create`), the edit form and its save (`update`) and Delete
/// (`delete`). Ignite checks them before the screen's policy, hides the sidebar entry of a user
/// without `view`, and answers 403; the Roles screen grants them, as WordPress's roles hold its
/// capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCaps<'a> {
    /// The capability the list and the sidebar entry need, such as `edit_posts`.
    pub view: &'a str,
    /// The capability the New screen and its save need.
    pub create: &'a str,
    /// The capability the edit form and its save need.
    pub update: &'a str,
    /// The capability Delete needs.
    pub delete: &'a str,
}

impl<'a> ScreenCaps<'a> {
    /// Every action needs `cap`, as the settings screens need `manage_options`.
    pub const fn all(cap: &'a str) -> Self {
        Self {
            view: cap,
            create: cap,
            update: cap,
            delete: cap,
        }
    }

    /// Gives `resource` these capabilities as its Ignite abilities
    /// (`RuntimeResource::ability`).
    pub fn apply(self, resource: RuntimeResource) -> RuntimeResource {
        resource
            .ability(ResourceAction::ViewAny, self.view)
            .ability(ResourceAction::View, self.view)
            .ability(ResourceAction::Create, self.create)
            .ability(ResourceAction::Update, self.update)
            .ability(ResourceAction::Delete, self.delete)
    }
}
