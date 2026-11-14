//! Media: where uploaded files live.
//!
//! Files are kept behind the [`Storage`] interface, so object storage
//! (S3-compatible) can be added beside the local disk later. Uploads go in
//! `YYYY/MM/` folders, as WordPress's `wp_upload_dir` does with
//! `uploads_use_yearmonth_folders` on (its default). An upload is checked
//! and named by [`upload`] before it is written.

pub mod storage;
pub mod upload;

pub use storage::{LocalStorage, Storage, StorageError, StorageResult, upload_subdir};
pub use upload::{
    MAX_FILE_NAME_BYTES, UploadError, ValidatedUpload, check_size, mime_type_for,
    sanitize_file_name, store, validate,
};
