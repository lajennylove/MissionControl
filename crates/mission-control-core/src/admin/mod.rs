//! The admin's rules that need neither Rocket nor Ignite's routes: who may use the screens of a
//! post type ([`posts`], Ruling R115), and the media library's screen, picker and uploads
//! ([`media`], Rulings R127 to R129). The `mission-control` crate mounts Ignite's panel at
//! `/mc-admin` and applies them before Ignite's routes run.

pub mod media;
pub mod posts;

pub use media::{
    ATTACHMENT, AttachmentData, AttachmentPage, AttachmentQuery, Attachments, UploadRefusal,
    attachment_for_js, attachment_post_type, attachment_resource, may_upload,
    prepare_attachment_for_js, query_attachments, register_attachment_resource, upload_to_library,
};
pub use posts::{
    PostScreen, Refusal, authorize_post_screen, may_use_post_screens, post_screen_caps,
};
