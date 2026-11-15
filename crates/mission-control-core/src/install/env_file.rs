//! Writes the `.env` file the installer creates, and generates its secret key.
//!
//! The file holds the two secrets Mission Control needs (`DATABASE_URL` and
//! `IGNITE_SECRET_KEY`) and the optional settings the wizard sets. It is created once, with
//! owner-only permissions, and never written over: an existing `.env` (or a link at that name)
//! is refused with a message that names the fix.
//!
//! Nothing here prints, logs or reports a secret: [`EnvFile`]'s `Debug` redacts them, and no
//! error carries a value.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use zeroize::Zeroizing;

use crate::config::{Environment, TrustedProxies};

/// The name of the file the installer creates.
pub const ENV_FILE_NAME: &str = ".env";

/// How old a leftover temporary file must be before [`write_new`] removes it.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// The number of random bytes in the secret key.
pub const SECRET_KEY_BYTES: usize = 32;

/// The values the installer writes to `.env`.
///
/// The two secrets are required. Every optional setting that is `None` is left out of the file,
/// so [`crate::config::load_from`] applies its default.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvFile {
    /// The database connection URL (`DATABASE_URL`). A secret.
    pub database_url: String,
    /// The key that signs sessions and cookies (`IGNITE_SECRET_KEY`). A secret. See
    /// [`generate_secret_key`].
    pub secret_key: String,
    /// Debug mode (`MC_DEBUG`).
    pub debug: Option<bool>,
    /// The largest accepted upload, in bytes (`MC_UPLOAD_MAX_BYTES`).
    pub upload_max_bytes: Option<u64>,
    /// The folder that holds the installed themes (`MC_THEMES_DIR`).
    pub themes_dir: Option<PathBuf>,
    /// The folder uploads are stored in (`MC_UPLOADS_DIR`).
    pub uploads_dir: Option<PathBuf>,
    /// The kind of environment the site runs in (`MC_ENV`).
    pub environment: Option<Environment>,
    /// The proxies whose `X-Forwarded-Proto` header is believed (`MC_TRUSTED_PROXIES`).
    pub trusted_proxies: Option<TrustedProxies>,
}

impl EnvFile {
    /// A file with the two required secrets and no optional settings.
    pub fn new(database_url: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
            secret_key: secret_key.into(),
            debug: None,
            upload_max_bytes: None,
            themes_dir: None,
            uploads_dir: None,
            environment: None,
            trusted_proxies: None,
        }
    }

    /// The file's text: one `NAME='value'` line per setting, in a fixed order.
    ///
    /// Every value is single-quoted, which `dotenvy` reads literally (no `$` substitution, no
    /// escapes, `#` and spaces kept). A single quote or backslash inside a value is closed out of the
    /// quotes and escaped (`'\''`, `'\\'`), because `dotenvy` fails on a backslash before a closing
    /// quote.
    /// The text holds the secrets, so it is zeroed when dropped.
    ///
    /// # Errors
    ///
    /// [`EnvFileError::InvalidValue`] when a value is empty or only whitespace (the two secrets), contains a
    /// newline, carriage return or NUL byte, or is a path that is not valid UTF-8.
    pub fn render(&self) -> Result<Zeroizing<String>, EnvFileError> {
        let mut out = Zeroizing::new(String::new());
        let mut line = |name: &'static str, value: &str| -> Result<(), EnvFileError> {
            if value.contains(['\n', '\r', '\0']) {
                return Err(EnvFileError::InvalidValue {
                    name,
                    reason: "it contains a line break or a NUL character",
                });
            }
            out.push_str(name);
            out.push_str("='");
            for c in value.chars() {
                match c {
                    // Outside the quotes, `\'` and `\\` are escapes `dotenvy` reads exactly.
                    '\'' => out.push_str(r"'\''"),
                    '\\' => out.push_str(r"'\\'"),
                    c => out.push(c),
                }
            }
            out.push_str("'\n");
            Ok(())
        };

        for (name, value) in [
            ("DATABASE_URL", &self.database_url),
            ("IGNITE_SECRET_KEY", &self.secret_key),
        ] {
            if value.trim().is_empty() {
                return Err(EnvFileError::InvalidValue {
                    name,
                    reason: "it is empty",
                });
            }
            line(name, value)?;
        }
        if let Some(debug) = self.debug {
            line("MC_DEBUG", if debug { "true" } else { "false" })?;
        }
        if let Some(bytes) = self.upload_max_bytes {
            line("MC_UPLOAD_MAX_BYTES", &bytes.to_string())?;
        }
        for (name, path) in [
            ("MC_THEMES_DIR", &self.themes_dir),
            ("MC_UPLOADS_DIR", &self.uploads_dir),
        ] {
            if let Some(path) = path {
                let text = path.to_str().ok_or(EnvFileError::InvalidValue {
                    name,
                    reason: "the path is not valid UTF-8",
                })?;
                if text.trim().is_empty() {
                    return Err(EnvFileError::InvalidValue {
                        name,
                        reason: "it is empty",
                    });
                }
                line(name, text)?;
            }
        }
        if let Some(environment) = self.environment {
            line("MC_ENV", environment.as_str())?;
        }
        if let Some(proxies) = &self.trusted_proxies {
            let text = match proxies {
                TrustedProxies::None => String::new(),
                TrustedProxies::All => "*".to_owned(),
                TrustedProxies::Addresses(addresses) => addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            line("MC_TRUSTED_PROXIES", &text)?;
        }
        Ok(out)
    }
}

impl fmt::Debug for EnvFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvFile")
            .field("database_url", &"[redacted]")
            .field("secret_key", &"[redacted]")
            .field("debug", &self.debug)
            .field("upload_max_bytes", &self.upload_max_bytes)
            .field("themes_dir", &self.themes_dir)
            .field("uploads_dir", &self.uploads_dir)
            .field("environment", &self.environment)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish()
    }
}

/// Why the `.env` file could not be written. No variant carries a secret.
#[derive(Debug)]
pub enum EnvFileError {
    /// A `.env` file (or a link with that name) is already there, and is left untouched.
    AlreadyExists {
        /// The path of the existing `.env`.
        path: PathBuf,
    },
    /// A value cannot be written to a `.env` file.
    InvalidValue {
        /// The variable name.
        name: &'static str,
        /// Why the value is refused.
        reason: &'static str,
    },
    /// The operating system could not supply random bytes for the secret key.
    Random(String),
    /// A file operation failed.
    Io {
        /// What Mission Control was doing.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The operating system's error.
        source: io::Error,
    },
}

impl fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists { path } => write!(
                f,
                "{} already exists, so Mission Control did not change it. Keep it if it is \
                 correct, or move or delete it and run the installer again.",
                path.display()
            ),
            Self::InvalidValue { name, reason } => write!(
                f,
                "{name} cannot be saved because {reason}. Change that setting and try again."
            ),
            Self::Random(why) => write!(
                f,
                "The operating system could not provide random bytes for the secret key ({why}). \
                 Check that the system's random number device is available and try again."
            ),
            Self::Io {
                action,
                path,
                source,
            } => write!(
                f,
                "Mission Control could not {action} {} ({source}). Check that the folder exists \
                 and the user that runs Mission Control can write to it.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for EnvFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A new secret key for `IGNITE_SECRET_KEY`: 32 bytes from the operating system's CSPRNG,
/// standard base64 encoded (44 characters).
///
/// # Errors
///
/// [`EnvFileError::Random`] when the operating system cannot supply random bytes.
pub fn generate_secret_key() -> Result<String, EnvFileError> {
    let mut bytes = Zeroizing::new([0u8; SECRET_KEY_BYTES]);
    getrandom::fill(&mut bytes[..]).map_err(|e| EnvFileError::Random(e.to_string()))?;
    Ok(STANDARD.encode(&bytes[..]))
}

/// Creates `.env` in `dir` from `env` and returns its path.
///
/// The file is never written over: when `.env` exists, even as a dangling symbolic link, this
/// returns [`EnvFileError::AlreadyExists`] and changes nothing. A link at that name is never
/// followed.
///
/// On Unix the file has mode `0600` from the moment it exists: the content is written to a
/// uniquely named temporary file created with that mode, then linked to `.env` (which fails
/// instead of replacing), and the temporary name is removed. The secret is never in a file with
/// wider permissions. If the file system cannot hard link, `.env` is created exclusively with
/// the same mode and written in place.
///
/// Before anything else it removes temporary files an earlier crashed run left in `dir`: regular
/// files (never links) named exactly `.env.mission-control-*.tmp` that are over a minute old, so
/// a run that is still writing is not disturbed.
///
/// On Windows there is no mode to set: the file takes the permissions of its folder, so keep the
/// install folder private to the user that runs Mission Control.
///
/// # Errors
///
/// [`EnvFileError::AlreadyExists`], [`EnvFileError::InvalidValue`] (nothing is created), or
/// [`EnvFileError::Io`].
pub fn write_new(dir: &Path, env: &EnvFile) -> Result<PathBuf, EnvFileError> {
    let content = env.render()?;
    let target = dir.join(ENV_FILE_NAME);
    remove_stale_temp_files(dir);
    let io_err = |action, path: &Path| {
        let path = path.to_path_buf();
        move |source| EnvFileError::Io {
            action,
            path,
            source,
        }
    };

    // `symlink_metadata` does not follow a link, so a dangling link counts as existing.
    if fs::symlink_metadata(&target).is_ok() {
        return Err(EnvFileError::AlreadyExists { path: target });
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temp = dir.join(format!(
        ".env.mission-control-{}-{nanos}.tmp",
        std::process::id()
    ));
    let mut file = create_private(&temp).map_err(io_err("create a file in", dir))?;
    let written = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(source) = written {
        let _ = fs::remove_file(&temp);
        return Err(io_err("write", &target)(source));
    }

    let linked = fs::hard_link(&temp, &target);
    let _ = fs::remove_file(&temp);
    match linked {
        Ok(()) => {
            sync_dir(dir);
            Ok(target)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(EnvFileError::AlreadyExists { path: target })
        }
        // No hard links here (some network and FAT file systems): create in place instead.
        Err(e) if hard_links_unsupported(&e) => {
            let result = write_in_place(&target, &content).map_err(|e| match e.kind() {
                io::ErrorKind::AlreadyExists => EnvFileError::AlreadyExists {
                    path: target.clone(),
                },
                _ => io_err("write", &target)(e),
            });
            if result.is_ok() {
                sync_dir(dir);
            }
            result
        }
        Err(e) => Err(io_err("create", &target)(e)),
    }
}

/// True when `error` says the file system cannot make hard links (`EPERM`, `ENOTSUP`,
/// `EOPNOTSUPP` or [`io::ErrorKind::Unsupported`]), as opposed to a real failure.
fn hard_links_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return [libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&code);
    }
    false
}

/// Flushes the folder entry of a new file to disk (Unix), so `.env` survives a crash right after
/// it is created. Best effort: the file already exists and is complete when this runs.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Removes `.env.mission-control-*.tmp` regular files over a minute old from `dir`. Links,
/// folders and every other name are left alone; errors are ignored.
fn remove_stale_temp_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(".env.mission-control-") && name.ends_with(".tmp")) {
            continue;
        }
        // `DirEntry::file_type` does not follow links, so a link is not a regular file here.
        let is_file = entry.file_type().is_ok_and(|t| t.is_file());
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= STALE_AFTER));
        if is_file && old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Creates `path` exclusively with the owner-only mode, writes `content`, and removes the file
/// again if the write fails, so a half-written `.env` is not left behind.
fn write_in_place(path: &Path, content: &str) -> io::Result<PathBuf> {
    let mut file = create_private(path)?;
    let written = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    match written {
        Ok(()) => Ok(path.to_path_buf()),
        Err(e) => {
            let _ = fs::remove_file(path);
            Err(e)
        }
    }
}

/// Creates a new file that must not exist, readable and writable by its owner only on Unix.
/// `create_new` is `O_EXCL`, which also refuses a symbolic link at `path`.
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
