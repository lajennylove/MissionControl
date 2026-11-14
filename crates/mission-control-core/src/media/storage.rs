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
    /// A file already exists at the path ([`Storage::put_new`] only).
    AlreadyExists(String),
    /// The underlying disk (or network) call failed.
    Io(io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::InvalidPath(path) => write!(f, "invalid storage path {path:?}"),
            StorageError::NotFound(path) => write!(f, "no file at {path:?}"),
            StorageError::AlreadyExists(path) => write!(f, "a file already exists at {path:?}"),
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
    /// use [`Storage::put_new`].
    fn put(&self, path: &str, bytes: &[u8]) -> StorageResult<()>;

    /// Writes `bytes` at `path` only when no file is there yet, or fails with
    /// [`StorageError::AlreadyExists`]. The check and the write are one step,
    /// so two writers racing for one name never overwrite each other. Parent
    /// folders are created, and readers never see half a file.
    fn put_new(&self, path: &str, bytes: &[u8]) -> StorageResult<()>;

    /// Writes `bytes` at the first of `paths` where no file exists yet, and
    /// returns that path. Like [`Storage::put_new`], a taken path is never
    /// replaced, but the bytes are sent once however many paths are taken.
    /// Fails with [`StorageError::AlreadyExists`] (naming the last path)
    /// when every path is taken, or [`StorageError::InvalidPath`] when there
    /// are none.
    ///
    /// The default skips paths [`Storage::exists`] reports taken and uses
    /// `put_new` only to close the race, so the bytes are written again only
    /// when another writer takes a name in between. Implementations that can
    /// write once and claim names cheaply (the local disk) override it.
    fn put_new_first(
        &self,
        paths: &mut dyn Iterator<Item = String>,
        bytes: &[u8],
    ) -> StorageResult<String> {
        let mut last = None;
        for path in paths {
            if !self.exists(&path)? {
                match self.put_new(&path, bytes) {
                    Ok(()) => return Ok(path),
                    Err(StorageError::AlreadyExists(_)) => {}
                    Err(err) => return Err(err),
                }
            }
            last = Some(path);
        }
        Err(match last {
            Some(path) => StorageError::AlreadyExists(path),
            None => StorageError::InvalidPath(String::new()),
        })
    }

    /// Reads the file at `path`, or [`StorageError::NotFound`].
    fn get(&self, path: &str) -> StorageResult<Vec<u8>>;

    /// Deletes the file at `path`. Deleting a file that is not there succeeds,
    /// as WordPress's `wp_delete_file` does (it unlinks without complaint).
    fn delete(&self, path: &str) -> StorageResult<()>;

    /// Whether a file exists at `path`.
    fn exists(&self, path: &str) -> StorageResult<bool>;

    /// The names of what `folder` holds (files and subfolders, not their
    /// contents), in no particular order: names only, without the folder.
    /// A folder that does not exist holds nothing. Names starting with `.`,
    /// which sanitised upload names never do, are left out, so a storage's
    /// own temp files are not listed. [`store`](super::store) reads it to
    /// keep an image's future sub-size names free, as WordPress's
    /// `wp_unique_filename` reads the upload folder.
    fn list(&self, folder: &str) -> StorageResult<Vec<String>>;
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

/// How many temp names [`LocalStorage::write_temp`] tries before giving up.
const MAX_TEMP_ATTEMPTS: u32 = 1_000;

impl LocalStorage {
    /// Checks `path`, creates its folders, and returns the target path.
    fn prepare_write(&self, path: &str) -> StorageResult<PathBuf> {
        let target = self.join(path)?;
        let parent = target
            .parent()
            .ok_or_else(|| StorageError::InvalidPath(path.to_string()))?;
        // Check before creating folders, so a symlink escape creates nothing
        // outside the root; check again after, now that the folders exist.
        self.check_inside(path, &target)?;
        fs::create_dir_all(parent)?;
        self.check_inside(path, &target)?;
        Ok(target)
    }

    /// Writes `bytes` to a new temp file beside `target` and syncs it. The
    /// temp name is a short fixed prefix plus the process id and a counter,
    /// not the target name, so targets near the 255-byte name limit work.
    /// It starts with a dot, which sanitised upload names never do.
    fn write_temp(target: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let parent = target.parent().unwrap_or(Path::new("."));
        let mut attempts = 0;
        loop {
            attempts += 1;
            let temp = parent.join(format!(
                ".mc-tmp.{}.{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
            {
                Ok(file) => file,
                // Left by an earlier process with the same id: try the next.
                Err(err)
                    if err.kind() == io::ErrorKind::AlreadyExists
                        && attempts < MAX_TEMP_ATTEMPTS =>
                {
                    continue;
                }
                Err(err) => return Err(err),
            };
            let written = file.write_all(bytes).and_then(|()| file.sync_all());
            if let Err(err) = written {
                let _ = fs::remove_file(&temp);
                return Err(err);
            }
            return Ok(temp);
        }
    }
}

impl Storage for LocalStorage {
    fn put(&self, path: &str, bytes: &[u8]) -> StorageResult<()> {
        let target = self.prepare_write(path)?;
        let temp = Self::write_temp(&target, bytes)?;
        if let Err(err) = fs::rename(&temp, &target) {
            let _ = fs::remove_file(&temp);
            return Err(err.into());
        }
        Ok(())
    }

    fn put_new(&self, path: &str, bytes: &[u8]) -> StorageResult<()> {
        self.put_new_first(&mut std::iter::once(path.to_string()), bytes)
            .map(|_| ())
    }

    /// Writes the bytes once to a temp file, then hard-links it to each path
    /// in turn until one is free. `hard_link` never replaces: it fails when
    /// anything (a file, a folder, a symlink) is already there. It is in
    /// `std` on Unix and Windows (NTFS). All paths must share the first
    /// one's folder, so the link never crosses file systems.
    fn put_new_first(
        &self,
        paths: &mut dyn Iterator<Item = String>,
        bytes: &[u8],
    ) -> StorageResult<String> {
        let Some(first) = paths.next() else {
            return Err(StorageError::InvalidPath(String::new()));
        };
        let first_target = self.prepare_write(&first)?;
        let folder = first_target.parent().map(Path::to_path_buf);
        let temp = Self::write_temp(&first_target, bytes)?;

        let claimed = (|| {
            let mut last = first.clone();
            for path in std::iter::once(first).chain(paths) {
                let target = self.join(&path)?;
                if target.parent().map(Path::to_path_buf) != folder {
                    return Err(StorageError::InvalidPath(path));
                }
                match fs::hard_link(&temp, &target) {
                    Ok(()) => return Ok(path),
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = path,
                    Err(err) => return Err(err.into()),
                }
            }
            Err(StorageError::AlreadyExists(last))
        })();
        let _ = fs::remove_file(&temp);
        claimed
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
        match fs::metadata(&target) {
            Ok(meta) => Ok(meta.is_file()),
            // Nothing there, or a file where a folder was expected.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn list(&self, folder: &str) -> StorageResult<Vec<String>> {
        let target = self.resolve_existing(folder)?;
        let entries = match fs::read_dir(&target) {
            Ok(entries) => entries,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(err) => return Err(err.into()),
        };
        let mut names = Vec::new();
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                names.push(name);
            }
        }
        Ok(names)
    }
}
