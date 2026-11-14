//! The storage interface and its local-disk implementation.
//!
//! Paths are relative to the storage root and `/`-separated, for example
//! `2026/11/photo.jpg`. Anything that could leave the root is refused with
//! [`StorageError::InvalidPath`]: `..` components, absolute paths, backslashes,
//! NUL bytes, empty components and symlinks that point outside the root.
//!
//! The local implementation uses blocking `std::fs`: local disk calls are
//! short, and callers that need async can wrap them in `spawn_blocking`.
//!
//! ```
//! use mission_control_core::media::{LocalStorage, Storage, upload_subdir};
//! use chrono::NaiveDate;
//!
//! let root = std::env::temp_dir().join(format!("mc-doc-{}", std::process::id()));
//! let storage = LocalStorage::new(&root);
//! let day = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
//! let path = format!("{}/hello.txt", upload_subdir(day));
//! assert_eq!(path, "2026/11/hello.txt");
//! storage.put(&path, b"hi").unwrap();
//! assert_eq!(storage.get(&path).unwrap(), b"hi");
//! storage.delete(&path).unwrap();
//! # std::fs::remove_dir_all(&root).unwrap();
//! ```

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Datelike;

/// What a storage call can fail with.
#[derive(Debug)]
pub enum StorageError {
    /// The path is not a safe relative path inside the storage root.
    InvalidPath(String),
    /// No file exists at the path.
    NotFound(String),
    /// The underlying disk (or network) call failed.
    Io(io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::InvalidPath(path) => write!(f, "invalid storage path {path:?}"),
            StorageError::NotFound(path) => write!(f, "no file at {path:?}"),
            StorageError::Io(err) => write!(f, "storage I/O error: {err}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(err: io::Error) -> Self {
        StorageError::Io(err)
    }
}

/// The result of a storage call.
pub type StorageResult<T> = Result<T, StorageError>;

/// A place to keep uploaded files. Object-safe, so it can be held as
/// `Arc<dyn Storage>` and swapped for S3-compatible storage later.
pub trait Storage: Send + Sync {
    /// Writes `bytes` at `path`, replacing any file already there. Parent
    /// folders are created. The write is atomic: readers see the old file or
    /// the whole new one, never half a file. Callers that must not overwrite
    /// check [`Storage::exists`] first.
    fn put(&self, path: &str, bytes: &[u8]) -> StorageResult<()>;

    /// Reads the file at `path`, or [`StorageError::NotFound`].
    fn get(&self, path: &str) -> StorageResult<Vec<u8>>;

    /// Deletes the file at `path`. Deleting a file that is not there succeeds,
    /// as WordPress's `wp_delete_file` does (it unlinks without complaint).
    fn delete(&self, path: &str) -> StorageResult<()>;

    /// Whether a file exists at `path`.
    fn exists(&self, path: &str) -> StorageResult<bool>;
}

/// The `YYYY/MM` folder an upload made on `date` goes in (WordPress's
/// `wp_upload_dir` with year and month folders on). The caller supplies the
/// date, so storage never reads the clock.
pub fn upload_subdir(date: impl Datelike) -> String {
    format!("{:04}/{:02}", date.year(), date.month())
}

/// Storage on the local disk, under one root folder.
#[derive(Debug, Clone)]
pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    /// Stores files under `root`. The folder is created on the first `put`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Checks the syntax of `path` and joins it onto the root.
    fn join(&self, path: &str) -> StorageResult<PathBuf> {
        let invalid = || StorageError::InvalidPath(path.to_string());
        if path.is_empty() || path.contains(['\0', '\\']) || path.starts_with('/') {
            return Err(invalid());
        }
        let mut joined = self.root.clone();
        for part in path.split('/') {
            // Rejects empty parts (`a//b`, trailing `/`), `.` and `..`, and
            // Windows drive prefixes.
            let mut components = Path::new(part).components();
            match (components.next(), components.next()) {
                (Some(Component::Normal(_)), None) if !part.is_empty() => joined.push(part),
                _ => return Err(invalid()),
            }
        }
        Ok(joined)
    }

    /// Refuses `target` when the folder it sits in resolves (through
    /// symlinks) outside the root. Folders that do not exist yet are checked
    /// through their nearest existing parent.
    fn check_inside(&self, path: &str, target: &Path) -> StorageResult<()> {
        let root = match self.root.canonicalize() {
            Ok(root) => root,
            // No root yet: nothing inside it can be a symlink.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let mut ancestor = target.parent();
        while let Some(dir) = ancestor {
            match dir.canonicalize() {
                Ok(resolved) if resolved.starts_with(&root) => return Ok(()),
                Ok(_) => return Err(StorageError::InvalidPath(path.to_string())),
                Err(err) if err.kind() == io::ErrorKind::NotFound => ancestor = dir.parent(),
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    /// Resolves a safe path for reading: also refuses a final component that
    /// is itself a symlink to somewhere outside the root.
    fn resolve_existing(&self, path: &str) -> StorageResult<PathBuf> {
        let target = self.join(path)?;
        self.check_inside(path, &target)?;
        if let (Ok(root), Ok(resolved)) = (self.root.canonicalize(), target.canonicalize())
            && !resolved.starts_with(&root)
        {
            return Err(StorageError::InvalidPath(path.to_string()));
        }
        Ok(target)
    }
}

/// Distinguishes temp files of concurrent writers in one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Storage for LocalStorage {
    fn put(&self, path: &str, bytes: &[u8]) -> StorageResult<()> {
        let target = self.join(path)?;
        let parent = target
            .parent()
            .ok_or_else(|| StorageError::InvalidPath(path.to_string()))?;
        // Check before creating folders, so a symlink escape creates nothing
        // outside the root; check again after, now that the folders exist.
        self.check_inside(path, &target)?;
        fs::create_dir_all(parent)?;
        self.check_inside(path, &target)?;

        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let temp = parent.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temp, &target)
        })();
        if let Err(err) = written {
            let _ = fs::remove_file(&temp);
            return Err(err.into());
        }
        Ok(())
    }

    fn get(&self, path: &str) -> StorageResult<Vec<u8>> {
        let target = self.resolve_existing(path)?;
        fs::read(&target).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => StorageError::NotFound(path.to_string()),
            _ => err.into(),
        })
    }

    fn delete(&self, path: &str) -> StorageResult<()> {
        let target = self.join(path)?;
        // The folder must be inside the root; the file itself may be a
        // symlink, which is removed as a link and never followed.
        self.check_inside(path, &target)?;
        match fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn exists(&self, path: &str) -> StorageResult<bool> {
        let target = self.resolve_existing(path)?;
        Ok(target.is_file())
    }
}
