//! The admin's rules that need neither Rocket nor Ignite's routes: who may use the screens of a
//! post type ([`posts`], Ruling R115), the media library's screen, picker and uploads
//! ([`media`], Rulings R127 to R129), the settings screens ([`settings`], Ruling R132) and the
//! menus screen ([`menus`], Ruling R133). The `mission-control` crate mounts Ignite's panel at
//! `/mc-admin` and applies them before Ignite's routes run.

pub mod media;
pub mod menus;
pub mod posts;
pub mod settings;

pub use media::{
    ATTACHMENT, AttachmentData, AttachmentPage, AttachmentQuery, Attachments, UploadRefusal,
    attachment_for_js, attachment_post_type, attachment_resource, may_upload,
    prepare_attachment_for_js, query_attachments, register_attachment_resource, upload_to_library,
};
pub use menus::{
    NAV_MENUS, NavMenusData, authorize_menus_screen, nav_menus_resource,
    register_nav_menus_resource,
};
pub use posts::{
    PostScreen, Refusal, authorize_post_screen, may_use_post_screens, post_screen_caps,
};
pub use settings::{
    SETTINGS_RECORD, SettingsData, SettingsErrors, SettingsScreen, SettingsServices,
    apply_media_sizes, authorize_settings_screen, read_settings, register_settings_resources,
    save_settings, settings_choices, settings_fields, settings_resource, timezone_choices,
};
