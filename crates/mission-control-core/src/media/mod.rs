//! Media: where uploaded files live.
//!
//! Files are kept behind the [`Storage`] interface, so object storage
//! (S3-compatible) can be added beside the local disk later. Uploads go in
//! `YYYY/MM/` folders, as WordPress's `wp_upload_dir` does with
//! `uploads_use_yearmonth_folders` on (its default). An upload is checked
//! and named by [`upload`] before it is written. Images get resized copies
//! for each registered size ([`sizes`]). A stored file is an attachment, a
//! `posts` row with its details ([`attachment`]).

pub mod attachment;
pub mod sizes;
pub mod storage;
pub mod upload;

pub use attachment::{
    Attachment, AttachmentError, AttachmentTitle, create_attachment, create_attachment_titled,
    delete_attachment, get_attachment, update_attachment_details,
};
pub use sizes::{
    AttachmentMetadata, ImageSize, ImageSizes, JPEG_QUALITY, MAX_DECODE_BYTES, MAX_IMAGE_DIMENSION,
    ResizeDimensions, SubSize, constrain_dimensions, create_image_subsizes,
    image_resize_dimensions,
};
pub use storage::{LocalStorage, OpenedFile, Storage, StorageError, StorageResult, upload_subdir};
pub use upload::{
    MAX_FILE_NAME_BYTES, UploadError, ValidatedUpload, check_size, mime_type_for,
    sanitize_file_name, store, validate,
};
