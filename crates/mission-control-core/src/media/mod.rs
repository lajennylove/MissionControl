//! Media: where uploaded files live.
//!
//! Files are kept behind the [`Storage`] interface, so object storage
//! (S3-compatible) can be added beside the local disk later. Uploads go in
//! `YYYY/MM/` folders, as WordPress's `wp_upload_dir` does with
//! `uploads_use_yearmonth_folders` on (its default).

pub mod storage;

pub use storage::{LocalStorage, Storage, StorageError, StorageResult, upload_subdir};
