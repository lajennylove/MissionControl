//! A plugin's life on a site: installed from its zip, activated, deactivated, loaded again at
//! every start, and deactivated when it fails (design sections 4.2 and 5.8; Review Focus item
//! 3), after WordPress's plugin screens and its recovery mode.
//!
//! ```ignore
//! let plugins = Arc::new(Plugins::new(hooks, db, registry, &config.plugins_dir, PluginLimits::default())?);
//! plugins.boot().await?;               // at start: runs `init` of every plugin in `active_plugins`
//! plugins.spawn_recovery();            // writes run-time failures to the database
//! let manifest = plugins.install(&zip)?;           // the admin's upload
//! plugins.activate(&manifest.slug, owner).await?;  // the admin's "Activate" link
//! plugins.deactivate(&manifest.slug, owner).await?;
//! ```
//!
//! # Installing
//!
//! Installing unpacks the plugin's zip into the plugins folder ([`Config::plugins_dir`],
//! `MC_PLUGINS_DIR`), one folder per plugin named by its slug, as WordPress unpacks a plugin
//! into `wp-content/plugins/<folder>`. The plugin is not activated.
//!
//! The zip holds [`MANIFEST_FILE`] and [`MODULE_FILE`] (the WebAssembly component), and any
//! other files the plugin reads, either in one folder at its top (WordPress's layout: the
//! folder must be the manifest's slug) or at its root. Finder's `__MACOSX` folder is skipped,
//! as WordPress's unzip skips it. Nothing is written until the whole zip is checked:
//!
//! - its size, its number of entries, the bytes it unpacks to and each entry's compression
//!   ratio are bounded ([`InstallLimits`]), counted on the bytes actually inflated, never on the
//!   sizes the zip claims;
//! - a corrupt or truncated zip, an entry that fails its checksum, an encrypted entry or one
//!   compressed with anything but deflate is refused;
//! - every entry's path must stay inside the plugin's folder: no `..`, `.`, empty or absolute
//!   part, drive letter, backslash or control character; names are ASCII only (so no two names
//!   differ only in Unicode normalization), and no two names that one case-insensitive disk
//!   would take for one file; symbolic links and special files are refused;
//! - the manifest must parse ([`Manifest::parse_bytes`]), its `requires` must not be newer than
//!   the host's plugin API (Ruling R106, as WordPress 5.5's `Plugin_Upgrader::check_package`
//!   refuses such a plugin at upload), and the module must compile and import only what the
//!   manifest grants ([`PluginHost::check`]).
//!
//! A zip that claims more entries than allowed in its end record is refused before its central
//! directory is read; otherwise the central directory, at most [`InstallLimits::zip_max_bytes`],
//! is read whole before its entries are counted.
//!
//! The files are then written to a hidden folder beside the plugins (`.mc-install.*`) and moved
//! into place with one rename, so a plugin's folder is either whole or absent. Installs run one
//! at a time, and the move refuses a target that exists, even an empty folder. A slug that is
//! already installed is refused: replacing a plugin (WordPress's "Replace current with
//! uploaded") is not built. [`Plugins::boot`] removes temporary folders a crash left behind.
//!
//! # Activating and deactivating
//!
//! The active plugins are the option [`ACTIVE_PLUGINS_OPTION`] (`active_plugins`, as
//! WordPress's), a JSON list of slugs kept sorted, at most [`MAX_ACTIVE_PLUGINS`].
//!
//! A plugin's handler has three lifecycle calls (Ruling R105, WordPress's split): `init` runs
//! each time the plugin is loaded, with no user acting, and is the only place it subscribes to
//! hooks and registers types (WordPress's main plugin file); `activate` and `deactivate` run
//! once, when the owner activates or deactivates it, with that user acting
//! (`register_activation_hook`, `register_deactivation_hook`).
//!
//! [`Plugins::activate`] checks the manifest's `requires` again
//! ([`Manifest::check_compatible`]), loads the module in the host with its limits, runs `init`
//! and then `activate`, and adds the slug to the option only when both succeed: a plugin that
//! refuses, traps or passes a limit is left inactive, as WordPress leaves a plugin that "could
//! not be activated because it triggered a fatal error". [`Plugins::deactivate`] runs the
//! plugin's `deactivate` (a failure is logged, and the plugin is deactivated anyway, as in
//! WordPress), removes its hooks ([`Hooks::remove_plugin`]) and the post types, taxonomies and
//! field groups it registered ([`Registry::remove_plugin`]), drops its instance, and takes it
//! off the option.
//!
//! # Starting
//!
//! [`Plugins::boot`] loads every plugin the option lists and runs its `init` (not `activate`),
//! as WordPress includes its active plugins' main files on every request, with no user acting.
//! A plugin whose folder is gone, whose files do not read, that does not load, whose `init`
//! fails, or that is past [`MAX_ACTIVE_PLUGINS`] is deactivated with a notice for the owner, as
//! WordPress's `validate_active_plugins` does, and the site starts anyway. A missing plugins
//! folder means no plugins are installed.
//!
//! # Recovery
//!
//! A plugin that traps, runs out of fuel or memory, in `init`, `activate` or `deactivate` or
//! later inside a hook, is
//! stopped by the host at once: its hooks and types are removed in the call that failed, so the
//! site keeps serving (core's actions return, filters keep their values). The rest is written
//! to the database: the plugin is taken off [`ACTIVE_PLUGINS_OPTION`] and a [`Notice`] with its
//! slug, the time and the error is stored in [`NOTICES_OPTION`]
//! (`mission_control_plugin_notices`), for the plugins screen to show, as WordPress's recovery
//! mode pauses the plugin and tells the owner in an admin notice. No email is sent: Mission
//! Control has no mailer yet.
//!
//! Hook calls are synchronous and may run on any thread, so a failure is not written where it
//! happens: the host's failure handler ([`PluginHost::on_failure`]) queues it, and
//! [`Plugins::recover`] writes the queue. [`Plugins::spawn_recovery`] runs `recover` on a Tokio
//! task each time a failure is queued; activation and boot run it before they return, so their
//! failures are written by then.
//!
//! Notices are text: control and invisible characters escaped, at most [`MAX_NOTICE_CHARS`]
//! characters, one per plugin (the latest), at most [`MAX_NOTICES`]. Escape them as HTML where
//! they are shown, as WordPress escapes on output.
//!
//! # Threads
//!
//! Installing, loading, starting, activating and deactivating a plugin compile WebAssembly and
//! run plugin code: on a multi-thread Tokio runtime they run in `block_in_place`. Call these from a multi-thread
//! runtime (Rocket's) or a blocking thread; on a current-thread runtime a plugin's content
//! calls fail (see the host's module documentation).
//!
//! [`Config::plugins_dir`]: crate::config::Config::plugins_dir

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use orbit_orm::Database;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::Notify;
use zip::{CompressionMethod, ZipArchive};

use super::host::{
    HostError, MAX_COMPONENT_BYTES, Plugin, PluginHost, PluginLimits, clean, with_current_user,
};
use super::manifest::{MANIFEST_FILE, MAX_MANIFEST_BYTES, Manifest, ManifestError, check_slug};
use crate::content::{get_option, set_option};
use crate::hooks::Hooks;
use crate::types::Registry;
use crate::users::UserCaps;

/// The WebAssembly component's file name in a plugin's zip and folder, beside
/// [`MANIFEST_FILE`].
pub const MODULE_FILE: &str = "plugin.wasm";

/// The option that lists the active plugins' slugs, as WordPress's `active_plugins`: a JSON
/// list, sorted.
pub const ACTIVE_PLUGINS_OPTION: &str = "active_plugins";

/// The option that holds the owner's notices about plugins that failed: a JSON list of
/// [`Notice`]s.
pub const NOTICES_OPTION: &str = "mission_control_plugin_notices";

/// The most notices kept; the oldest go first.
pub const MAX_NOTICES: usize = 100;

/// The longest notice message kept, in characters; a longer one is cut, with `…`.
pub const MAX_NOTICE_CHARS: usize = 1024;

/// The most slugs read from [`ACTIVE_PLUGINS_OPTION`].
pub const MAX_ACTIVE_PLUGINS: usize = 256;

/// The longest path of an entry in a plugin's zip, in bytes.
pub const MAX_ZIP_PATH_BYTES: usize = 512;

/// The most folders deep an entry in a plugin's zip may be.
pub const MAX_ZIP_PATH_DEPTH: usize = 16;

/// The longest part of an entry's path (a file or folder name), in bytes.
const MAX_NAME_BYTES: usize = 255;

/// An entry's compression ratio is checked once it has inflated to this many bytes: small
/// files of repeated text compress well and are harmless.
const RATIO_FLOOR_BYTES: u64 = 1024 * 1024;

/// The folder Finder adds to the zips it makes, skipped as WordPress skips it.
const MACOS_METADATA: &str = "__MACOSX";

/// The most failures waiting to be written; past it the oldest are dropped, with a warning.
const MAX_PENDING_FAILURES: usize = 1024;

/// How long the recovery task waits to try again after the database refused a write.
const RECOVERY_RETRY: Duration = Duration::from_secs(5);

/// Where the lifecycle's messages are logged.
const LOG_TARGET: &str = "mission_control::plugins";

/// The bounds of a plugin zip on install, given to [`Plugins::with_install_limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallLimits {
    /// The largest zip read, in bytes. Default 64 MiB, twice the largest component.
    pub zip_max_bytes: usize,
    /// The most entries (files and folders) a zip may hold. Default 2048.
    pub entries_max: usize,
    /// The most bytes a zip may unpack to, all entries together, counted as they inflate.
    /// Default 128 MiB.
    pub unpacked_max_bytes: u64,
    /// The highest compression ratio an entry may have once it has inflated past 1 MiB (its
    /// bytes over its compressed bytes): a zip bomb's ratio is in the thousands. Default 100.
    pub compression_ratio_max: u64,
}

impl Default for InstallLimits {
    fn default() -> Self {
        Self {
            zip_max_bytes: 64 * 1024 * 1024,
            entries_max: 2048,
            unpacked_max_bytes: 128 * 1024 * 1024,
            compression_ratio_max: 100,
        }
    }
}

/// Why a plugin zip was not installed. Nothing was written. Every name from the zip is escaped
/// and shortened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// The zip is over [`InstallLimits::zip_max_bytes`].
    TooLarge {
        /// Its size, in bytes.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The zip is not a zip, is truncated, or an entry does not inflate or fails its checksum.
    Corrupt(String),
    /// The zip has more entries than [`InstallLimits::entries_max`].
    TooManyEntries {
        /// How many it has.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// An entry's path would leave the plugin's folder, or has a character or part no file
    /// name may have (file names are ASCII only).
    UnsafePath {
        /// The path, escaped.
        path: String,
    },
    /// An entry is a symbolic link or another special file.
    SpecialFile {
        /// The path.
        path: String,
    },
    /// Two entries have the same path, compared without regard to case.
    DuplicatePath {
        /// The second path.
        path: String,
    },
    /// An entry is encrypted, or compressed with a method other than deflate.
    Unsupported {
        /// The path.
        path: String,
        /// What is not supported.
        what: &'static str,
    },
    /// The manifest or the module is over its own limit ([`MAX_MANIFEST_BYTES`],
    /// [`MAX_COMPONENT_BYTES`]).
    FileTooLarge {
        /// The path.
        path: String,
        /// The limit, in bytes.
        max: u64,
    },
    /// The entries unpack to more than [`InstallLimits::unpacked_max_bytes`].
    UnpackedTooLarge {
        /// The limit, in bytes.
        max: u64,
    },
    /// An entry inflates more than [`InstallLimits::compression_ratio_max`] times its size: a
    /// zip bomb.
    CompressionRatio {
        /// The path.
        path: String,
    },
    /// The zip does not hold a plugin as expected: no manifest or module, more than one folder
    /// at its top, or a folder that is not the manifest's slug.
    Layout(String),
    /// The manifest was refused.
    Manifest(ManifestError),
    /// The module does not compile, or imports what the host does not provide or the manifest
    /// does not grant.
    Module(HostError),
    /// A plugin with this slug is installed already.
    AlreadyInstalled {
        /// The slug.
        slug: String,
    },
    /// The plugins folder could not be written.
    Io(String),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len, max } => write!(
                f,
                "The plugin zip is {len} bytes, over the {max}-byte limit."
            ),
            Self::Corrupt(e) => write!(f, "The plugin zip is damaged or incomplete: {e}."),
            Self::TooManyEntries { count, max } => write!(
                f,
                "The plugin zip has {count} files and folders; at most {max} are accepted."
            ),
            Self::UnsafePath { path } => write!(
                f,
                "The plugin zip has a file whose path is not safe (paths must stay in the plugin's \
                 folder and use ASCII letters only): `{path}`."
            ),
            Self::SpecialFile { path } => write!(
                f,
                "The plugin zip has a link or special file, which are not accepted: `{path}`."
            ),
            Self::DuplicatePath { path } => write!(
                f,
                "The plugin zip has two files named `{path}` (ignoring case)."
            ),
            Self::Unsupported { path, what } => {
                write!(f, "The plugin zip has a file that is {what}: `{path}`.")
            }
            Self::FileTooLarge { path, max } => {
                write!(f, "`{path}` in the plugin zip is over {max} bytes.")
            }
            Self::UnpackedTooLarge { max } => write!(
                f,
                "The plugin zip unpacks to more than {max} bytes, the most accepted."
            ),
            Self::CompressionRatio { path } => write!(
                f,
                "`{path}` in the plugin zip unpacks to far more than its size: it looks like a \
                 zip bomb."
            ),
            Self::Layout(e) => write!(f, "The zip is not a Mission Control plugin: {e}."),
            Self::Manifest(e) => e.fmt(f),
            Self::Module(e) => write!(f, "The plugin's module was refused: {e}."),
            Self::AlreadyInstalled { slug } => write!(
                f,
                "A plugin with the slug `{slug}` is already installed. Delete it first."
            ),
            Self::Io(e) => write!(
                f,
                "The plugin could not be written to the plugins folder: {e}."
            ),
        }
    }
}

impl std::error::Error for InstallError {}

/// Why a plugin could not be activated, deactivated or loaded, or the options not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// No plugin with this slug is installed (or the slug is not a valid slug).
    NotInstalled {
        /// The slug, escaped.
        slug: String,
    },
    /// The plugin's files could not be read, or its manifest names another slug.
    Read(String),
    /// The manifest was refused, or the plugin needs a newer plugin API.
    Manifest(ManifestError),
    /// The plugin did not load, or its activation refused, trapped or passed a limit.
    Host(HostError),
    /// [`MAX_ACTIVE_PLUGINS`] plugins are active already.
    TooManyActive {
        /// The limit.
        max: usize,
    },
    /// The database refused to read or write an option.
    Database(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled { slug } => write!(f, "The plugin `{slug}` is not installed."),
            Self::Read(e) => write!(f, "The plugin's files could not be read: {e}."),
            Self::Manifest(e) => e.fmt(f),
            Self::Host(e) => write!(f, "The plugin could not be activated: {e}."),
            Self::TooManyActive { max } => write!(
                f,
                "{max} plugins are active, the most allowed. Deactivate one first."
            ),
            Self::Database(e) => write!(f, "The plugin settings could not be saved: {e}."),
        }
    }
}

impl std::error::Error for LifecycleError {}

/// A notice for the site owner: a plugin failed and was deactivated, as WordPress's recovery
/// mode tells the owner. Stored in [`NOTICES_OPTION`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// The plugin's slug.
    pub slug: String,
    /// When it failed, in UTC (`2026-11-15T10:00:00Z`).
    pub time: String,
    /// What failed, as text: control and invisible characters escaped, at most
    /// [`MAX_NOTICE_CHARS`] characters. Escape it as HTML where it is shown.
    pub message: String,
}

/// A plugin failure waiting to be written.
struct Failure {
    slug: String,
    /// The load that failed, when it was loaded: a newer load of the slug is left active.
    load: Option<u64>,
    time: String,
    message: String,
}

impl Failure {
    fn now(slug: &str, load: Option<u64>, message: &str) -> Self {
        Self {
            slug: slug.to_owned(),
            load,
            time: utc_now(),
            message: notice_text(message),
        }
    }
}

/// The failures waiting to be written, and the signal the recovery task waits on.
#[derive(Default)]
struct Failures {
    queue: Mutex<VecDeque<Failure>>,
    notify: Notify,
    /// Set when the plugins are dropped: the recovery task ends.
    closed: AtomicBool,
}

impl Failures {
    fn lock(&self) -> MutexGuard<'_, VecDeque<Failure>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, failure: Failure) {
        {
            let mut queue = self.lock();
            if queue.len() >= MAX_PENDING_FAILURES {
                queue.pop_front();
                log::warn!(
                    target: LOG_TARGET,
                    "more than {MAX_PENDING_FAILURES} plugin failures wait to be written; the \
                     oldest is dropped"
                );
            }
            queue.push_back(failure);
        }
        self.notify.notify_one();
    }

    fn take(&self) -> Vec<Failure> {
        self.lock().drain(..).collect()
    }

    /// Puts `failures` back in front, after a write failed.
    fn restore(&self, failures: Vec<Failure>) {
        let mut queue = self.lock();
        for failure in failures.into_iter().rev() {
            queue.push_front(failure);
        }
        queue.truncate(MAX_PENDING_FAILURES);
    }
}

/// The site's plugins: the folder they are installed in, the host that runs them, and the
/// active ones. See the module documentation.
pub struct Plugins {
    host: PluginHost,
    hooks: Arc<Hooks>,
    db: Arc<Database>,
    registry: Arc<Registry>,
    dir: PathBuf,
    install_limits: InstallLimits,
    /// The plugins loaded and activated, by slug.
    loaded: Mutex<HashMap<String, Arc<Plugin>>>,
    /// Held by every change of the active plugins and the options, so they are made one at a
    /// time.
    writes: tokio::sync::Mutex<()>,
    /// Held by each install, so installs are made one at a time.
    installs: Mutex<()>,
    failures: Arc<Failures>,
}

impl Drop for Plugins {
    /// Ends the recovery task ([`Plugins::spawn_recovery`]).
    fn drop(&mut self) {
        self.failures.closed.store(true, Ordering::SeqCst);
        self.failures.notify.notify_one();
    }
}

impl fmt::Debug for Plugins {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugins")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Distinguishes the temporary folders of installs running at once.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An entry of a zip, checked.
struct Entry {
    index: usize,
    /// Its path, by part.
    path: Vec<String>,
    dir: bool,
}

impl Plugins {
    /// The plugins installed in `dir`, run in a host with `limits` whose plugins subscribe on
    /// `hooks`, reach content in `db` and register types in `registry`. Create it inside a
    /// Tokio runtime.
    ///
    /// # Errors
    ///
    /// As [`PluginHost::with_limits`].
    pub fn new(
        hooks: Arc<Hooks>,
        db: Arc<Database>,
        registry: Arc<Registry>,
        dir: impl Into<PathBuf>,
        limits: PluginLimits,
    ) -> Result<Self, HostError> {
        let failures = Arc::new(Failures::default());
        let queue = Arc::clone(&failures);
        let host = PluginHost::with_limits(Arc::clone(&hooks), Arc::clone(&db), limits)?
            .with_registry(Arc::clone(&registry))
            .on_failure(move |plugin, error| {
                queue.push(Failure::now(
                    plugin.slug(),
                    Some(plugin.load_number()),
                    &error.to_string(),
                ));
            });
        Ok(Self {
            host,
            hooks,
            db,
            registry,
            dir: dir.into(),
            install_limits: InstallLimits::default(),
            loaded: Mutex::new(HashMap::new()),
            writes: tokio::sync::Mutex::new(()),
            installs: Mutex::new(()),
            failures,
        })
    }

    /// The same plugins, with `limits` on the zips [`Plugins::install`] reads.
    #[must_use]
    pub fn with_install_limits(mut self, limits: InstallLimits) -> Self {
        self.install_limits = limits;
        self
    }

    /// The folder the plugins are installed in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn loaded(&self) -> MutexGuard<'_, HashMap<String, Arc<Plugin>>> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the plugin `slug` is loaded, activated and running: not deactivated, and not
    /// stopped after a failure (even before the failure is written).
    pub fn is_active(&self, slug: &str) -> bool {
        self.loaded()
            .get(slug)
            .is_some_and(|plugin| !plugin.trapped() && !plugin.deactivated())
    }

    // ----- Installing -----

    /// Installs the plugin in `zip` (see the module documentation, "Installing"): checks it
    /// whole, then writes its folder in the plugins folder. The plugin is not activated.
    /// Compiling the module takes CPU time: call it from a blocking thread or a multi-thread
    /// runtime.
    ///
    /// # Errors
    ///
    /// An [`InstallError`] for each way a zip is refused; nothing is written then.
    pub fn install(&self, zip: &[u8]) -> Result<Manifest, InstallError> {
        let limits = self.install_limits;
        if zip.len() > limits.zip_max_bytes {
            return Err(InstallError::TooLarge {
                len: zip.len(),
                max: limits.zip_max_bytes,
            });
        }
        // The count the end record claims, before the zip crate reads the central directory.
        if let Some(count) = claimed_entries(zip)
            && count > limits.entries_max
        {
            return Err(InstallError::TooManyEntries {
                count,
                max: limits.entries_max,
            });
        }
        // One install at a time, so two installs of one slug cannot race to its folder.
        let _install = self.installs.lock().unwrap_or_else(|e| e.into_inner());
        let mut archive = ZipArchive::new(Cursor::new(zip)).map_err(corrupt)?;
        if archive.len() > limits.entries_max {
            return Err(InstallError::TooManyEntries {
                count: archive.len(),
                max: limits.entries_max,
            });
        }
        let entries = check_entries(&mut archive)?;

        // The layout: the manifest at the root, or in the one folder at the top.
        let at_root = entries
            .iter()
            .any(|e| !e.dir && e.path.len() == 1 && e.path[0] == MANIFEST_FILE);
        let folder = if at_root {
            None
        } else {
            Some(top_folder(&entries)?)
        };
        let skip = usize::from(folder.is_some());
        let inside = |e: &Entry| -> Vec<String> { e.path[skip..].to_vec() };
        let find = |name: &str| {
            entries
                .iter()
                .find(|e| !e.dir && e.path.len() == skip + 1 && e.path[skip] == name)
        };
        let manifest_entry = find(MANIFEST_FILE)
            .ok_or_else(|| InstallError::Layout(format!("it has no {MANIFEST_FILE}")))?;
        let module_entry = find(MODULE_FILE)
            .ok_or_else(|| InstallError::Layout(format!("it has no {MODULE_FILE}")))?;

        let mut budget = limits.unpacked_max_bytes;
        let mut manifest_bytes = Vec::new();
        self.unpack(
            &mut archive,
            manifest_entry,
            &mut manifest_bytes,
            MAX_MANIFEST_BYTES as u64,
            &mut budget,
        )?;
        let manifest = Manifest::parse_bytes(&manifest_bytes).map_err(InstallError::Manifest)?;
        // Ruling R106: a plugin that needs a newer plugin API is refused at upload, as WordPress
        // 5.5's `Plugin_Upgrader::check_package` refuses it (and again at activation).
        manifest
            .check_compatible()
            .map_err(InstallError::Manifest)?;
        if let Some(folder) = &folder
            && *folder != manifest.slug
        {
            return Err(InstallError::Layout(format!(
                "its folder `{}` is not the plugin's slug `{}`",
                clean(folder, 64),
                manifest.slug
            )));
        }
        let mut module = Vec::new();
        self.unpack(
            &mut archive,
            module_entry,
            &mut module,
            MAX_COMPONENT_BYTES as u64,
            &mut budget,
        )?;
        blocking(|| self.host.check(&manifest, &module)).map_err(InstallError::Module)?;
        // Every other file is inflated once to nowhere, so a damaged entry, a bomb or too many
        // bytes is refused before anything is written.
        let others = || {
            entries
                .iter()
                .filter(|e| !e.dir && !inside(e).is_empty())
                .filter(|e| !std::ptr::eq(*e, manifest_entry) && !std::ptr::eq(*e, module_entry))
        };
        let mut dry_budget = budget;
        for entry in others() {
            self.unpack(
                &mut archive,
                entry,
                &mut io::sink(),
                u64::MAX,
                &mut dry_budget,
            )?;
        }

        // Everything is checked: write to a temporary folder, then move it into place.
        let io_error = |e: io::Error| InstallError::Io(e.to_string());
        let target = self.dir.join(&manifest.slug);
        let taken = || InstallError::AlreadyInstalled {
            slug: manifest.slug.clone(),
        };
        if fs::symlink_metadata(&target).is_ok() {
            return Err(taken());
        }
        fs::create_dir_all(&self.dir).map_err(io_error)?;
        let temp = temp_dir(&self.dir).map_err(io_error)?;
        let written = (|| {
            for entry in &entries {
                let parts = inside(entry);
                if parts.is_empty() {
                    continue;
                }
                let path: PathBuf = parts.iter().collect();
                let dest = temp.join(&path);
                if entry.dir {
                    fs::create_dir_all(&dest).map_err(io_error)?;
                    continue;
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(io_error)?;
                }
                let mut file = fs::File::create_new(&dest).map_err(io_error)?;
                if std::ptr::eq(entry, manifest_entry) {
                    file.write_all(&manifest_bytes).map_err(io_error)?;
                } else if std::ptr::eq(entry, module_entry) {
                    file.write_all(&module).map_err(io_error)?;
                } else {
                    self.unpack(&mut archive, entry, &mut file, u64::MAX, &mut budget)?;
                }
                file.sync_all().map_err(io_error)?;
            }
            claim_and_move(&temp, &target).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    taken()
                } else {
                    io_error(e)
                }
            })
        })();
        if written.is_err() {
            let _ = fs::remove_dir_all(&temp);
        }
        written.map(|()| manifest)
    }

    /// Inflates `entry` into `out`: at most `max` bytes for this file and what is left of
    /// `budget` for the zip, with the compression ratio checked as it inflates.
    fn unpack(
        &self,
        archive: &mut ZipArchive<Cursor<&[u8]>>,
        entry: &Entry,
        out: &mut impl Write,
        max: u64,
        budget: &mut u64,
    ) -> Result<(), InstallError> {
        let path = entry.path.join("/");
        let mut file = archive.by_index(entry.index).map_err(corrupt)?;
        let compressed = file.compressed_size().max(1);
        let ratio_max = self.install_limits.compression_ratio_max;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(InstallError::Corrupt(format!(
                        "`{}` does not unpack: {e}",
                        clean(&path, 256)
                    )));
                }
            };
            total = total.saturating_add(n as u64);
            if total > RATIO_FLOOR_BYTES && total > ratio_max.saturating_mul(compressed) {
                return Err(InstallError::CompressionRatio {
                    path: clean(&path, 256),
                });
            }
            if total > *budget {
                return Err(InstallError::UnpackedTooLarge {
                    max: self.install_limits.unpacked_max_bytes,
                });
            }
            if total > max {
                return Err(InstallError::FileTooLarge {
                    path: clean(&path, 256),
                    max,
                });
            }
            out.write_all(&buffer[..n])
                .map_err(|e| InstallError::Io(e.to_string()))?;
        }
        *budget -= total;
        Ok(())
    }

    // ----- Activating and deactivating -----

    /// Activates the installed plugin `slug` with `user` acting (the site owner who activates
    /// it), as WordPress's `activate_plugin`: checks its manifest, loads it, runs its `init`
    /// (no user acting) and its `activate` (`user` acting), and adds it to
    /// [`ACTIVE_PLUGINS_OPTION`] when both succeed. A plugin that is active already is left as
    /// it is.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::NotInstalled`], [`LifecycleError::Read`] and
    /// [`LifecycleError::Manifest`] (a newer `requires` included) before it loads;
    /// [`LifecycleError::TooManyActive`] when [`MAX_ACTIVE_PLUGINS`] are active;
    /// [`LifecycleError::Host`] when it does not load or its `init` or `activate` refuses, traps or
    /// passes a limit: it is left inactive, and for a trap or a limit the owner gets a
    /// [`Notice`]; [`LifecycleError::Database`] when the option cannot be written (the plugin
    /// is deactivated again).
    pub async fn activate(&self, slug: &str, user: UserCaps) -> Result<(), LifecycleError> {
        let _writes = self.writes.lock().await;
        let outcome = self.start_locked(slug, Some(user)).await;
        if outcome.is_err()
            && let Err(e) = self.recover_locked().await
        {
            log::error!(target: LOG_TARGET, "a plugin failure could not be written: {e}");
        }
        outcome.map_err(|(error, _)| error)
    }

    /// Loads `slug` and runs its `init`, and with `activating` (the owner's user) its
    /// `activate` too, while [`Plugins::writes`] is held; at start, `init` only. An error says
    /// whether the host queued it as a failure (the plugin was stopped).
    async fn start_locked(
        &self,
        slug: &str,
        activating: Option<UserCaps>,
    ) -> Result<(), (LifecycleError, bool)> {
        if self.is_active(slug) {
            return Ok(());
        }
        let (manifest, wasm) = self.read_installed(slug).map_err(|e| (e, false))?;
        manifest
            .check_compatible()
            .map_err(|e| (LifecycleError::Manifest(e), false))?;
        if activating.is_some() {
            let listed = self.active_slugs().await.map_err(|e| (e, false))?;
            if listed.len() >= MAX_ACTIVE_PLUGINS && !listed.iter().any(|s| s == slug) {
                return Err((
                    LifecycleError::TooManyActive {
                        max: MAX_ACTIVE_PLUGINS,
                    },
                    false,
                ));
            }
        }
        let plugin = blocking(|| self.host.load(manifest, &wasm))
            .map_err(|e| (LifecycleError::Host(e), false))?;
        // `init` runs with no user acting, as WordPress includes a plugin's main file.
        let started = blocking(|| plugin.init()).and_then(|()| match activating {
            Some(user) => blocking(|| with_current_user(user, || plugin.activate())),
            None => Ok(()),
        });
        if let Err(e) = started {
            return Err((LifecycleError::Host(e), plugin.trapped()));
        }
        self.loaded().insert(slug.to_owned(), Arc::clone(&plugin));
        if let Err(e) = self
            .update_active(|slugs| {
                slugs.push(slug.to_owned());
            })
            .await
        {
            self.loaded().remove(slug);
            plugin.remove_hooks();
            return Err((e, false));
        }
        log::info!(target: LOG_TARGET, "[{slug}] the plugin is activated");
        Ok(())
    }

    /// Deactivates the plugin `slug`, as WordPress's `deactivate_plugins`: calls its
    /// `deactivate` with `user` (the owner) acting, then removes its hooks and the types it
    /// registered, drops its instance, and takes it off [`ACTIVE_PLUGINS_OPTION`]. A
    /// `deactivate` that fails is logged and the plugin is deactivated anyway, as in
    /// WordPress. True when it was active (loaded or listed).
    ///
    /// # Errors
    ///
    /// [`LifecycleError::Database`] when the option cannot be read or written; the plugin is
    /// stopped either way.
    pub async fn deactivate(&self, slug: &str, user: UserCaps) -> Result<bool, LifecycleError> {
        let _writes = self.writes.lock().await;
        let loaded = self.loaded().remove(slug);
        let was_loaded = loaded
            .as_ref()
            .is_some_and(|p| !p.deactivated() && !p.trapped());
        if let Some(plugin) = loaded
            && let Err(e) = blocking(|| with_current_user(user, || plugin.deactivate()))
        {
            log::warn!(
                target: LOG_TARGET,
                "[{slug}] the plugin's deactivation failed; it is deactivated anyway: {e}"
            );
        }
        self.hooks.remove_plugin(slug);
        self.registry.remove_plugin(slug);
        let mut listed = false;
        self.update_active(|slugs| {
            listed = slugs.iter().any(|s| s == slug);
            slugs.retain(|s| s != slug);
        })
        .await?;
        if was_loaded || listed {
            log::info!(target: LOG_TARGET, "[{slug}] the plugin is deactivated");
        }
        // A trap in `deactivate` queued a failure: write its notice now.
        if let Err(e) = self.recover_locked().await {
            log::error!(target: LOG_TARGET, "a plugin failure could not be written: {e}");
        }
        Ok(was_loaded || listed)
    }

    /// Reads the manifest and the module of the installed plugin `slug`, each a regular file
    /// within its size limit; the manifest must name `slug`.
    fn read_installed(&self, slug: &str) -> Result<(Manifest, Vec<u8>), LifecycleError> {
        let not_installed = || LifecycleError::NotInstalled {
            slug: clean(slug, 64),
        };
        if check_slug(slug).is_err() {
            return Err(not_installed());
        }
        let folder = self.dir.join(slug);
        match fs::symlink_metadata(&folder) {
            Ok(meta) if meta.is_dir() => {}
            _ => return Err(not_installed()),
        }
        let manifest = read_capped(&folder.join(MANIFEST_FILE), MAX_MANIFEST_BYTES)?;
        let manifest = Manifest::parse_bytes(&manifest).map_err(LifecycleError::Manifest)?;
        if manifest.slug != slug {
            return Err(LifecycleError::Read(format!(
                "the manifest in the folder `{slug}` names the slug `{}`",
                manifest.slug
            )));
        }
        let module = read_capped(&folder.join(MODULE_FILE), MAX_COMPONENT_BYTES)?;
        Ok((manifest, module))
    }

    // ----- Starting -----

    /// Activates every plugin [`ACTIVE_PLUGINS_OPTION`] lists, at the site's start (see the
    /// module documentation, "Starting"), with no user acting. A plugin that fails is
    /// deactivated and the owner gets a [`Notice`]; the others are activated anyway. Entries of
    /// the option that are not slugs are dropped. Returns the plugins that failed, with why.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::Database`] when the options cannot be read or written; the plugins
    /// that activated stay active.
    pub async fn boot(&self) -> Result<Vec<(String, LifecycleError)>, LifecycleError> {
        let _writes = self.writes.lock().await;
        self.sweep_temp_folders();
        // Writing the list back drops what is not a slug.
        let mut slugs = Vec::new();
        self.update_active(|listed| slugs.clone_from(listed))
            .await?;
        let mut failed = Vec::new();
        for (i, slug) in slugs.into_iter().enumerate() {
            let started = if i < MAX_ACTIVE_PLUGINS {
                self.start_locked(&slug, None).await
            } else {
                Err((
                    LifecycleError::TooManyActive {
                        max: MAX_ACTIVE_PLUGINS,
                    },
                    false,
                ))
            };
            if let Err((error, queued)) = started {
                log::error!(
                    target: LOG_TARGET,
                    "[{slug}] the plugin could not be activated at start and is deactivated: \
                     {error}"
                );
                if !queued {
                    self.failures
                        .push(Failure::now(&slug, None, &error.to_string()));
                }
                failed.push((slug, error));
            }
        }
        self.recover_locked().await?;
        Ok(failed)
    }

    /// Removes the temporary folders installs left behind (a crash between writing and moving
    /// into place). Run at start, before any install.
    fn sweep_temp_folders(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let is_temp = name.to_str().is_some_and(|n| n.starts_with(TEMP_PREFIX));
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_temp
                && is_dir
                && let Err(e) = fs::remove_dir_all(entry.path())
            {
                log::warn!(
                    target: LOG_TARGET,
                    "a temporary install folder could not be removed: {e}"
                );
            }
        }
    }

    // ----- Recovery -----

    /// Writes the failures the host queued (see the module documentation, "Recovery"): each
    /// failed plugin is taken off [`ACTIVE_PLUGINS_OPTION`], its hooks and types are removed,
    /// and the owner gets a [`Notice`]. Returns how many failures were written.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::Database`] when an option cannot be read or written; the failures
    /// stay queued for the next call.
    pub async fn recover(&self) -> Result<usize, LifecycleError> {
        let _writes = self.writes.lock().await;
        self.recover_locked().await
    }

    async fn recover_locked(&self) -> Result<usize, LifecycleError> {
        let failures = self.failures.take();
        if failures.is_empty() {
            return Ok(0);
        }
        let mut gone = HashSet::new();
        {
            let mut loaded = self.loaded();
            for failure in &failures {
                let current = loaded.get(&failure.slug).map(|p| p.load_number());
                // A newer load of the slug is running: only the notice is kept.
                if current.is_some() && failure.load.is_some() && current != failure.load {
                    continue;
                }
                if let Some(plugin) = loaded.remove(&failure.slug) {
                    plugin.remove_hooks();
                }
                gone.insert(failure.slug.clone());
            }
        }
        for slug in &gone {
            self.hooks.remove_plugin(slug);
            self.registry.remove_plugin(slug);
        }
        let written = async {
            self.update_active(|slugs| slugs.retain(|s| !gone.contains(s)))
                .await?;
            self.update_notices(|notices| {
                for failure in &failures {
                    put_notice(
                        notices,
                        Notice {
                            slug: failure.slug.clone(),
                            time: failure.time.clone(),
                            message: failure.message.clone(),
                        },
                    );
                }
            })
            .await
        }
        .await;
        match written {
            Ok(()) => {
                for failure in &failures {
                    log::warn!(
                        target: LOG_TARGET,
                        "[{}] the plugin failed, is deactivated, and the owner has a notice",
                        failure.slug
                    );
                }
                Ok(failures.len())
            }
            Err(e) => {
                self.failures.restore(failures);
                Err(e)
            }
        }
    }

    /// Spawns a Tokio task that runs [`Plugins::recover`] each time a plugin fails, so a
    /// failure inside a hook reaches the database soon after. A write the database refuses is
    /// tried again after a few seconds. The task ends when the plugins are dropped.
    pub fn spawn_recovery(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let plugins = Arc::downgrade(self);
        let failures = Arc::clone(&self.failures);
        tokio::spawn(async move {
            loop {
                failures.notify.notified().await;
                if failures.closed.load(Ordering::SeqCst) {
                    break;
                }
                let Some(plugins) = plugins.upgrade() else {
                    break;
                };
                if let Err(e) = plugins.recover().await {
                    log::error!(
                        target: LOG_TARGET,
                        "plugin failures could not be written, and are tried again: {e}"
                    );
                    drop(plugins);
                    tokio::time::sleep(RECOVERY_RETRY).await;
                    failures.notify.notify_one();
                }
            }
        })
    }

    // ----- Notices -----

    /// The owner's notices about plugins that failed, oldest first. A stored value that does
    /// not read is taken as none.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::Database`] when the option cannot be read.
    pub async fn notices(&self) -> Result<Vec<Notice>, LifecycleError> {
        let stored = get_option(&self.db, NOTICES_OPTION)
            .await
            .map_err(database)?;
        Ok(read_notices(stored))
    }

    /// Stores a notice for the owner about plugin `slug`, with `message` (escaped and cut), as
    /// the recovery does; it replaces the plugin's earlier notice.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::NotInstalled`] when `slug` is not a valid slug;
    /// [`LifecycleError::Database`] when the option cannot be read or written.
    pub async fn notify_owner(&self, slug: &str, message: &str) -> Result<(), LifecycleError> {
        valid_slug(slug)?;
        let _writes = self.writes.lock().await;
        let notice = Notice {
            slug: slug.to_owned(),
            time: utc_now(),
            message: notice_text(message),
        };
        self.update_notices(|notices| put_notice(notices, notice))
            .await
    }

    /// Removes the notice about plugin `slug`, as the owner dismisses it. True when there was
    /// one.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::NotInstalled`] when `slug` is not a valid slug;
    /// [`LifecycleError::Database`] when the option cannot be read or written.
    pub async fn dismiss_notice(&self, slug: &str) -> Result<bool, LifecycleError> {
        valid_slug(slug)?;
        let _writes = self.writes.lock().await;
        let mut found = false;
        self.update_notices(|notices| {
            let before = notices.len();
            notices.retain(|n| n.slug != slug);
            found = notices.len() != before;
        })
        .await?;
        Ok(found)
    }

    // ----- Options -----

    /// Reads [`ACTIVE_PLUGINS_OPTION`], lets `change` change the slugs, and writes them back
    /// sorted and without duplicates when they differ from what is stored. Entries that are not
    /// slugs are dropped, with a warning; slugs are never dropped here (past
    /// [`MAX_ACTIVE_PLUGINS`], [`Plugins::boot`] deactivates the rest with a notice).
    async fn update_active(
        &self,
        change: impl FnOnce(&mut Vec<String>),
    ) -> Result<(), LifecycleError> {
        let stored = get_option(&self.db, ACTIVE_PLUGINS_OPTION)
            .await
            .map_err(database)?;
        let mut slugs = listed_slugs(stored.as_ref());
        slugs.sort_unstable();
        slugs.dedup();
        change(&mut slugs);
        slugs.sort_unstable();
        slugs.dedup();
        let value = Value::from(slugs);
        if stored.as_ref() != Some(&value) {
            set_option(&self.db, ACTIVE_PLUGINS_OPTION, &value)
                .await
                .map_err(database)?;
        }
        Ok(())
    }

    /// The slugs [`ACTIVE_PLUGINS_OPTION`] lists.
    async fn active_slugs(&self) -> Result<Vec<String>, LifecycleError> {
        let stored = get_option(&self.db, ACTIVE_PLUGINS_OPTION)
            .await
            .map_err(database)?;
        Ok(listed_slugs(stored.as_ref()))
    }

    /// Reads [`NOTICES_OPTION`], lets `change` change the notices, and writes them back, the
    /// newest [`MAX_NOTICES`].
    async fn update_notices(
        &self,
        change: impl FnOnce(&mut Vec<Notice>),
    ) -> Result<(), LifecycleError> {
        let stored = get_option(&self.db, NOTICES_OPTION)
            .await
            .map_err(database)?;
        let mut notices = read_notices(stored);
        change(&mut notices);
        let excess = notices.len().saturating_sub(MAX_NOTICES);
        notices.drain(..excess);
        let value = serde_json::to_value(&notices).map_err(|e| database(e.to_string()))?;
        set_option(&self.db, NOTICES_OPTION, &value)
            .await
            .map_err(database)
    }
}

/// Checks every entry of `archive` (path, kind, compression), without inflating any; Finder's
/// `__MACOSX` folder is left out.
fn check_entries(archive: &mut ZipArchive<Cursor<&[u8]>>) -> Result<Vec<Entry>, InstallError> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for index in 0..archive.len() {
        let file = archive.by_index_raw(index).map_err(corrupt)?;
        let name = file.name().map_err(corrupt)?.into_owned();
        if name == MACOS_METADATA || name.starts_with(&format!("{MACOS_METADATA}/")) {
            continue;
        }
        let (path, dir) = entry_path(&name)?;
        let shown = || clean(&name, 256);
        // The file type bits of a Unix mode: none, a regular file or a folder are accepted.
        let special = file
            .unix_mode()
            .is_some_and(|mode| !matches!(mode & 0o170_000, 0 | 0o100_000 | 0o040_000));
        if file.is_symlink() || special {
            return Err(InstallError::SpecialFile { path: shown() });
        }
        if file.encrypted() {
            return Err(InstallError::Unsupported {
                path: shown(),
                what: "encrypted",
            });
        }
        if !dir
            && !matches!(
                file.compression(),
                CompressionMethod::Stored | CompressionMethod::Deflated
            )
        {
            return Err(InstallError::Unsupported {
                path: shown(),
                what: "compressed with a method other than deflate",
            });
        }
        if !seen.insert(path.join("/").to_ascii_lowercase()) {
            return Err(InstallError::DuplicatePath { path: shown() });
        }
        entries.push(Entry { index, path, dir });
    }
    Ok(entries)
}

/// The one folder every entry is in, when the manifest is not at the root.
fn top_folder(entries: &[Entry]) -> Result<String, InstallError> {
    let missing = || InstallError::Layout(format!("it has no {MANIFEST_FILE}"));
    let top = &entries.first().ok_or_else(missing)?.path[0];
    let in_top = |e: &Entry| &e.path[0] == top && (e.dir || e.path.len() > 1);
    if !entries.iter().all(in_top) {
        return Err(InstallError::Layout(format!(
            "its {MANIFEST_FILE} is neither at its root nor in the one folder at its top"
        )));
    }
    Ok(top.clone())
}

/// The parts of an entry's path, and whether it is a folder (it ends with `/`); refused when it
/// could leave the plugin's folder or has a part no file may be named.
fn entry_path(name: &str) -> Result<(Vec<String>, bool), InstallError> {
    let refused = || InstallError::UnsafePath {
        path: clean(name, 256),
    };
    if name.is_empty()
        || name.len() > MAX_ZIP_PATH_BYTES
        || name.starts_with('/')
        || name.contains(['\\', ':', '\0'])
    {
        return Err(refused());
    }
    let (body, dir) = match name.strip_suffix('/') {
        Some(body) => (body, true),
        None => (name, false),
    };
    let parts: Vec<&str> = body.split('/').collect();
    if parts.len() > MAX_ZIP_PATH_DEPTH {
        return Err(refused());
    }
    for part in &parts {
        // ASCII only: names that differ in Unicode normalization (NFC, NFD) or case beyond
        // ASCII could be one file on some disks.
        let bad_char = |c: char| {
            !c.is_ascii() || c.is_ascii_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')
        };
        if part.is_empty()
            || *part == "."
            || *part == ".."
            || part.len() > MAX_NAME_BYTES
            || part.chars().any(bad_char)
        {
            return Err(refused());
        }
    }
    Ok((parts.into_iter().map(str::to_owned).collect(), dir))
}

/// The slugs `stored` (the value of [`ACTIVE_PLUGINS_OPTION`]) lists, sorted, without
/// duplicates; an entry that is not a slug is left out with a warning.
fn listed_slugs(stored: Option<&Value>) -> Vec<String> {
    let mut slugs = Vec::new();
    match stored {
        Some(Value::Array(items)) => {
            for item in items {
                match item.as_str() {
                    Some(slug) if check_slug(slug).is_ok() => slugs.push(slug.to_owned()),
                    _ => log::warn!(
                        target: LOG_TARGET,
                        "`{ACTIVE_PLUGINS_OPTION}` lists `{}`, which is not a plugin slug; it is \
                         dropped",
                        clean(&item.to_string(), 64)
                    ),
                }
            }
        }
        None => {}
        Some(other) => log::warn!(
            target: LOG_TARGET,
            "`{ACTIVE_PLUGINS_OPTION}` is not a list ({}); it is read as none",
            clean(&other.to_string(), 64)
        ),
    }
    slugs.sort_unstable();
    slugs.dedup();
    slugs
}

/// Refuses a `slug` that is not a plugin slug.
fn valid_slug(slug: &str) -> Result<(), LifecycleError> {
    check_slug(slug).map_err(|_| LifecycleError::NotInstalled {
        slug: clean(slug, 64),
    })
}

/// The number of entries the zip's end of central directory record claims, when it can be
/// found cheaply (the last 64 KiB plus the record) and is not a Zip64 placeholder. It lets a
/// zip that claims too many entries be refused before the zip crate reads its central
/// directory; the count is checked again once it is read.
fn claimed_entries(zip: &[u8]) -> Option<usize> {
    const SIGNATURE: &[u8] = b"PK\x05\x06";
    const RECORD: usize = 22;
    let start = zip.len().saturating_sub(RECORD + usize::from(u16::MAX));
    let tail = &zip[start..];
    let at = tail
        .windows(SIGNATURE.len())
        .rposition(|w| w == SIGNATURE)?;
    let record = tail.get(at..at + RECORD)?;
    let total = u16::from_le_bytes([record[10], record[11]]);
    (total != u16::MAX).then_some(usize::from(total))
}

/// Moves `temp` to `target`, refusing (`AlreadyExists`) when anything is at `target`: the
/// target is claimed by creating it, which fails when it exists, and the claimed empty folder
/// is then replaced by the rename (on Unix; elsewhere it is removed first).
fn claim_and_move(temp: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir(target)?;
    #[cfg(not(unix))]
    fs::remove_dir(target)?;
    fs::rename(temp, target).inspect_err(|_| {
        let _ = fs::remove_dir(target);
    })
}

/// The start of the name of an install's temporary folder.
const TEMP_PREFIX: &str = ".mc-install.";

/// Creates a new, empty, hidden folder in `dir` for an install to write to.
fn temp_dir(dir: &Path) -> io::Result<PathBuf> {
    loop {
        let temp = dir.join(format!(
            "{TEMP_PREFIX}{}.{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&temp) {
            Ok(()) => return Ok(temp),
            // Left by an earlier process with the same id: try the next.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// Reads `path`, a regular file (not a link) of at most `max` bytes.
fn read_capped(path: &Path, max: usize) -> Result<Vec<u8>, LifecycleError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(LifecycleError::Read(format!("{name} is not a file"))),
        Err(e) => return Err(LifecycleError::Read(format!("{name}: {e}"))),
    }
    let file = fs::File::open(path).map_err(|e| LifecycleError::Read(format!("{name}: {e}")))?;
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| LifecycleError::Read(format!("{name}: {e}")))?;
    if bytes.len() > max {
        return Err(LifecycleError::Read(format!("{name} is over {max} bytes")));
    }
    Ok(bytes)
}

/// Runs `f`, which takes CPU time, in `block_in_place` on a multi-thread runtime's worker.
fn blocking<R>(f: impl FnOnce() -> R) -> R {
    let multi_thread = Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::MultiThread);
    if multi_thread {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// The notices stored in `value`; anything that does not read is left out.
fn read_notices(value: Option<Value>) -> Vec<Notice> {
    match value {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| serde_json::from_value(item).ok())
            .take(MAX_NOTICES)
            .collect(),
        _ => Vec::new(),
    }
}

/// Adds `notice` at the end, replacing the plugin's earlier one.
fn put_notice(notices: &mut Vec<Notice>, notice: Notice) {
    notices.retain(|n| n.slug != notice.slug);
    notices.push(notice);
}

/// `message` as a notice keeps it: control and invisible characters escaped, at most
/// [`MAX_NOTICE_CHARS`] characters.
fn notice_text(message: &str) -> String {
    let cleaned = clean(message, MAX_NOTICE_CHARS);
    // Escapes lengthen the text: cut it again.
    if cleaned.chars().count() > MAX_NOTICE_CHARS {
        let mut cut: String = cleaned.chars().take(MAX_NOTICE_CHARS).collect();
        cut.push('…');
        cut
    } else {
        cleaned
    }
}

/// Now, in UTC, as a notice gives its time.
fn utc_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn corrupt(error: zip::result::ZipError) -> InstallError {
    InstallError::Corrupt(clean(&error.to_string(), 256))
}

fn database(error: impl fmt::Display) -> LifecycleError {
    LifecycleError::Database(error.to_string())
}
