//! The plugin host: runs a plugin, a WebAssembly component built from the WIT interface in
//! `wit/mission-control.wit`, in wasmtime's sandbox (design sections 4 and 4.2).
//!
//! ```ignore
//! let host = PluginHost::new(Arc::clone(&hooks), Arc::clone(&db))?;
//! let plugin = host.load(manifest, &wasm)?;      // checks the imports against the manifest
//! with_current_user(owner, || plugin.activate())?; // the plugin subscribes to hooks here
//! with_current_user(editor, || hooks.do_action(SAVE_POST, &post.id)); // reaches the plugin
//! plugin.remove_hooks();                          // on deactivation
//! ```
//!
//! # Imports and permissions
//!
//! Each interface the plugin may import belongs to one permission of its manifest
//! ([`INTERFACES`]). [`PluginHost::load`] reads the component's imports before linking: an
//! import of an interface the manifest does not grant fails with
//! [`HostError::MissingPermission`], naming the import and the permission, and an import the
//! host does not provide (another package, another API version) with
//! [`HostError::UnknownImport`]. The linker then defines only the granted interfaces, so the
//! sandbox has nothing else to call.
//!
//! # Hooks
//!
//! A subscription made through the `hooks` interface is a JSON callback on the site's
//! [`Hooks`] ([`Hooks::add_json_action`], [`Hooks::add_json_filter`]), owned by
//! [`Owner::PluginLoad`] with the plugin's slug and a number unique to the load: core's `do_action` and `apply_filters` reach the
//! plugin's `on-action` and `apply-filter` in priority order among the other callbacks. A
//! callback holds the plugin weakly: once the plugin is dropped, its callbacks do nothing.
//!
//! A plugin subscribes in its `activate` and only there; WordPress plugins may add hooks at any
//! time, but here a subscription made later is refused, so that [`Plugin::remove_hooks`]
//! (Task 12.4's deactivation) removes everything the plugin added and a deactivated plugin
//! cannot come back. A deactivated plugin is never called again. An activation that fails
//! (the plugin refuses, or traps) deactivates it and removes what it subscribed before failing.
//! A plugin is activated once per load. A host loads one plugin per slug at a time
//! ([`HostError::AlreadyLoaded`]); [`Hooks::remove_plugin`] removes every load's callbacks of a
//! slug.
//!
//! A plugin may subscribe to any hook name, as in WordPress, except [`DENIED_HOOKS`] and the
//! names starting with [`DENIED_HOOK_PREFIXES`]: the hooks that decide who may do what, carry
//! credentials or switch a security check off (Ruling R102). A subscriber sees each payload and
//! value, and a filter's new value is taken as given: it is not passed through kses or any
//! other check.
//!
//! The denylist must grow with core: when core fires a new hook of that kind, add it to
//! [`DENIED_HOOKS`]. A better long-term design (a follow-up, not built) is a "restricted"
//! property on the hook constant itself (`Action::restricted`), so a sensitive hook is refused
//! where it is declared.
//!
//! # The acting user
//!
//! Content calls run with the capabilities of the user the site acts for, set per thread with
//! [`with_current_user`] around the code that fires hooks or activates a plugin, as WordPress's
//! `wp_set_current_user` sets its global user. With no user set, a call runs as a visitor (id
//! 0, no capabilities), so it fails closed. Hooks run synchronously on the thread that fires
//! them, so the plugin's calls see the user set there.
//!
//! # Threads and the database
//!
//! Hook callbacks are synchronous ([`Hooks`]) and the database is async (Orbit), so wasmtime
//! runs synchronously (no `async` feature) and each content call blocks on the Tokio runtime
//! until its query is done. The rule:
//!
//! - On a multi-thread runtime (Rocket's), every call into a plugin (the wait for its mutex and
//!   its code) runs in `block_in_place`, so the runtime moves its other tasks off the worker,
//!   and each content call inside it blocks on the runtime with `block_on`.
//! - On a blocking thread (`spawn_blocking`, as the scheduler runs hooks) or a thread outside
//!   the runtime (then on the runtime the host was created in), the call blocks with
//!   `block_on`.
//! - On the thread of a current-thread runtime, blocking is impossible: a content call fails
//!   with `failed` and an error is logged. Fire hooks that reach plugins from `spawn_blocking`
//!   there.
//!
//! A plugin runs one call at a time: its store is behind a mutex, so callers on other threads
//! wait. A hook the plugin's own call fires again on the same thread (its handler causes the
//! hook it handles) is not passed to it again: the nested call is skipped with a warning,
//! where WordPress would recurse.
//!
//! # Failures
//!
//! A plugin that traps (or fails in a call for any other reason wasmtime reports, or during
//! whose call the host panics) is marked [`Plugin::trapped`], its hooks are removed and it is
//! not called again. The error is logged. Task 12.4 deactivates such a plugin and tells the owner; Task 12.3 adds fuel and
//! memory limits to the store, made in one place (`PluginHost::new_store`).
//!
//! # Bounds
//!
//! A component is at most [`MAX_COMPONENT_BYTES`], a hook name at most [`MAX_HOOK_NAME_BYTES`]
//! bytes of printable ASCII, a plugin has at most [`MAX_SUBSCRIPTIONS`] subscriptions, a
//! payload or filter value crosses at most [`MAX_JSON_BYTES`] bytes either way (a larger one is
//! not passed, and a larger answer is not taken), a log message keeps
//! [`MAX_LOG_MESSAGE_CHARS`] characters, and a message from the plugin in a [`HostError`] or a
//! log line [`MAX_PLUGIN_MESSAGE_CHARS`]. The content API's bounds are in its WIT
//! documentation.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use orbit_orm::Database;
use semver::Version;
use tokio::runtime::{Handle, RuntimeFlavor};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};

use super::content::{self, ContentError};
use super::manifest::{HOST_API_VERSION, Manifest, Permission};
use crate::hooks::{Hooks, Owner};
use crate::users::UserCaps;

/// The bindings `wasmtime` generates from the WIT world `plugin`.
mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/mission-control.wit",
        world: "plugin",
    });
}

use bindings::mission_control::plugin::{content_read, content_write, hooks as wit_hooks, logging};

/// The WIT package of the plugin API, without its version ([`HOST_API_VERSION`]).
pub const PACKAGE: &str = "mission-control:plugin";

/// The interfaces a plugin may import, with the permission each needs; `None` is always linked.
pub const INTERFACES: [(&str, Option<Permission>); 4] = [
    ("logging", None),
    ("hooks", Some(Permission::Hooks)),
    ("content-read", Some(Permission::ReadContent)),
    ("content-write", Some(Permission::WriteContent)),
];

/// The largest component accepted, in bytes (32 MiB).
pub const MAX_COMPONENT_BYTES: usize = 32 * 1024 * 1024;

/// The longest hook name a plugin may subscribe to, in bytes.
pub const MAX_HOOK_NAME_BYTES: usize = 128;

/// The most subscriptions one plugin may make.
pub const MAX_SUBSCRIPTIONS: usize = 1024;

/// The largest hook payload or filter value passed to a plugin, and taken back from it, in
/// bytes of JSON (4 MiB).
pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;

/// The longest log message a plugin may write, in characters; a longer one is cut.
pub const MAX_LOG_MESSAGE_CHARS: usize = 4096;

/// The longest message from a plugin kept in an error or a log line, in characters.
pub const MAX_PLUGIN_MESSAGE_CHARS: usize = 1024;

/// The largest stack a plugin's code may use, in bytes (512 KiB, wasmtime's default, set so it
/// does not change with wasmtime's).
pub const MAX_WASM_STACK_BYTES: usize = 512 * 1024;

/// Hooks no plugin may subscribe to (Ruling R102): those that decide who may do what, carry
/// credentials (passwords, reset keys, mail with either), or switch a security check off
/// (allowed HTML, upload types, redirect hosts), from WordPress. A plugin may subscribe to any
/// other name, as in WordPress. Names starting with one of [`DENIED_HOOK_PREFIXES`] are refused
/// too. See [`is_denied_hook`].
///
/// Most of these hooks are not fired by Mission Control yet; they are listed before core fires
/// them. **When core adds a hook that is sensitive in this way, add it here.**
pub const DENIED_HOOKS: &[&str] = &[
    // Capabilities and roles.
    "user_has_cap",
    "map_meta_cap",
    "role_has_cap",
    "editable_roles",
    "set_user_role",
    "add_user_role",
    "remove_user_role",
    "grant_super_admin",
    "revoke_super_admin",
    // Who the current user is.
    "determine_current_user",
    "set_current_user",
    "authenticate",
    "wp_authenticate",
    "wp_authenticate_user",
    "wp_login",
    "wp_login_failed",
    "rest_authentication_errors",
    // Passwords.
    "check_password",
    "wp_set_password",
    "password_reset",
    "after_password_reset",
    "retrieve_password",
    "retrieve_password_key",
    "retrieve_password_title",
    "retrieve_password_message",
    "retrieve_password_notification_email",
    "lostpassword_post",
    "lostpassword_errors",
    "validate_password_reset",
    "allow_password_reset",
    "password_needs_rehash",
    "wp_hash_password_algorithm",
    "wp_hash_password_options",
    "random_password",
    // Application passwords.
    "application_password_is_api_request",
    "application_password_did_authenticate",
    "application_password_failed_authentication",
    "wp_authenticate_application_password",
    "wp_authenticate_application_password_errors",
    "wp_is_application_passwords_available",
    "wp_is_application_passwords_available_for_user",
    "wp_is_application_passwords_supported",
    "wp_create_application_password",
    "wp_update_application_password",
    // Mail, which carries reset links and new users' passwords.
    "wp_mail",
    "pre_wp_mail",
    "phpmailer_init",
    "wp_new_user_notification_email",
    "wp_new_user_notification_email_admin",
    "password_change_email",
    "email_change_email",
    // Users as they are written (passwords, roles).
    "user_register",
    "profile_update",
    "wp_pre_insert_user_data",
    "insert_user_meta",
    // Switches for security checks: allowed HTML, protocols, CSS, uploads and redirects.
    "wp_kses_allowed_html",
    "pre_kses",
    "kses_allowed_protocols",
    "safe_style_css",
    "upload_mimes",
    "wp_check_filetype_and_ext",
    "allowed_redirect_hosts",
    "login_redirect",
    // Cookies, sessions and nonces.
    "send_auth_cookies",
    "set_auth_cookie",
    "set_logged_in_cookie",
    "clear_auth_cookie",
    "logged_in",
    "nonce_life",
    "salt",
    // Plain-text passwords and confirmation keys in emails.
    "check_passwords",
    "new_user_email_content",
    "new_admin_email_content",
    "user_request_action_email_content",
    "wp_privacy_personal_data_email_content",
    // Options that decide who may sign up and with which role, and where the site lives.
    "pre_option_default_role",
    "option_default_role",
    "default_option_default_role",
    "pre_update_option_default_role",
    "update_option_default_role",
    "pre_option_users_can_register",
    "option_users_can_register",
    "default_option_users_can_register",
    "pre_update_option_users_can_register",
    "update_option_users_can_register",
    "pre_option_siteurl",
    "option_siteurl",
    "default_option_siteurl",
    "pre_update_option_siteurl",
    "update_option_siteurl",
    "pre_option_home",
    "option_home",
    "default_option_home",
    "pre_update_option_home",
    "update_option_home",
    "pre_option_admin_email",
    "option_admin_email",
    "default_option_admin_email",
    "pre_update_option_admin_email",
    "update_option_admin_email",
];

/// Prefixes of hook names no plugin may subscribe to: `auth_` (`auth_cookie`,
/// `auth_cookie_valid`, `auth_post_meta_{key}`, `auth_redirect`, ...), `secure_auth_`,
/// `secure_logged_in_` and `session_token_manager`.
pub const DENIED_HOOK_PREFIXES: [&str; 4] = [
    "auth_",
    "secure_auth_",
    "secure_logged_in_",
    "session_token_manager",
];

/// Where plugins' log lines and the host's messages about plugins are logged.
const LOG_TARGET: &str = "mission_control::plugins";

/// Why a plugin could not be loaded, activated or called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// wasmtime could not be set up.
    Engine(String),
    /// The bytes are not a valid WebAssembly component, or are too large.
    Compile(String),
    /// The component imports something the host does not provide: another package, or an API
    /// version this host does not have.
    UnknownImport {
        /// The import's name, such as `acme:vault/secrets@1.0.0`.
        import: String,
    },
    /// The component imports an interface its manifest does not grant.
    MissingPermission {
        /// The import's name, such as `mission-control:plugin/content-write@0.1.0`.
        import: String,
        /// The permission the interface needs.
        permission: Permission,
    },
    /// The component could not be instantiated: it lacks the `handler` export, an import has
    /// the wrong type, or its start trapped.
    Instantiate(String),
    /// The plugin's `activate` refused, with its message.
    Activation(String),
    /// The plugin trapped or failed in a call, or had trapped before.
    Trap(String),
    /// The plugin was deactivated ([`Plugin::remove_hooks`]); load it again to activate it.
    Deactivated,
    /// The plugin was activated before: a load is activated once.
    AlreadyActivated,
    /// A plugin with this slug is already loaded in this host, and neither deactivated nor
    /// stopped.
    AlreadyLoaded {
        /// The slug.
        slug: String,
    },
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(e) => write!(f, "the plugin host could not start: {e}"),
            Self::Compile(e) => write!(f, "the plugin is not a valid WebAssembly component: {e}"),
            Self::UnknownImport { import } => write!(
                f,
                "the plugin imports `{import}`, which this site does not provide (it provides \
                 {PACKAGE}@{HOST_API_VERSION})"
            ),
            Self::MissingPermission { import, permission } => write!(
                f,
                "the plugin imports `{import}`, which needs the `{permission}` permission, and \
                 its manifest does not ask for it"
            ),
            Self::Instantiate(e) => write!(f, "the plugin could not be started: {e}"),
            Self::Activation(e) => write!(f, "the plugin refused to activate: {e}"),
            Self::Trap(e) => write!(f, "the plugin failed: {e}"),
            Self::Deactivated => {
                f.write_str("the plugin is deactivated; load it again to activate it")
            }
            Self::AlreadyActivated => f.write_str("the plugin is already activated"),
            Self::AlreadyLoaded { slug } => {
                write!(f, "a plugin with the slug `{slug}` is already loaded")
            }
        }
    }
}

impl std::error::Error for HostError {}

thread_local! {
    /// The user the site acts for on this thread, set by [`with_current_user`].
    static CURRENT_USER: RefCell<Option<UserCaps>> = const { RefCell::new(None) };
    /// The plugins running a call on this thread, by address, to skip a nested call.
    static RUNNING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with `user` as the user plugins act for on this thread, as WordPress's
/// `wp_set_current_user`; the user set before is restored afterwards, even when `f` panics.
/// Plugin content calls made while `f` runs (from hooks it fires, or an activation) are checked
/// against `user`'s capabilities.
pub fn with_current_user<R>(user: UserCaps, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<UserCaps>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT_USER.with(|current| *current.borrow_mut() = previous);
        }
    }
    let previous = CURRENT_USER.with(|current| current.replace(Some(user)));
    let _restore = Restore(previous);
    f()
}

/// The user plugins act for on this thread: a visitor (id 0, no capabilities) when none is set.
fn current_user() -> UserCaps {
    CURRENT_USER
        .with(|current| current.borrow().clone())
        .unwrap_or_else(|| UserCaps::from_roles(0, &[], &BTreeMap::new()))
}

/// The host of the site's plugins: one wasmtime engine, shared by every plugin, and what their
/// imports reach (the hooks and the database).
pub struct PluginHost {
    engine: Engine,
    hooks: Arc<Hooks>,
    db: Arc<Database>,
    /// The runtime the host was created in, for content calls from threads outside it.
    runtime: Option<Handle>,
    /// The plugins loaded, by slug.
    loaded: Mutex<HashMap<String, Weak<Plugin>>>,
    /// The number of the next load, for its hook owner.
    next_load: AtomicU64,
}

impl fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginHost").finish_non_exhaustive()
    }
}

impl PluginHost {
    /// A host whose plugins subscribe on `hooks` and reach content in `db`. Created inside a
    /// Tokio runtime, it keeps that runtime for content calls made from outside one.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] when wasmtime cannot be set up on this machine.
    pub fn new(hooks: Arc<Hooks>, db: Arc<Database>) -> Result<Self, HostError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Only the proposals plugins need: what Rust's and C's wasm32 targets emit by default
        // (bulk memory, multi-value, reference types, SIMD, ...). Threads and the async
        // component model are not compiled in.
        config
            .wasm_memory64(false)
            .wasm_multi_memory(false)
            .wasm_relaxed_simd(false)
            .wasm_gc(false)
            .wasm_shared_everything_threads(false)
            .wasm_stack_switching(false)
            .wasm_custom_page_sizes(false)
            .wasm_wide_arithmetic(false)
            .wasm_component_model_gc(false)
            .wasm_component_model_memory64(false)
            .max_wasm_stack(MAX_WASM_STACK_BYTES);
        let engine = Engine::new(&config).map_err(|e| HostError::Engine(format!("{e:#}")))?;
        Ok(Self {
            engine,
            hooks,
            db,
            runtime: Handle::try_current().ok(),
            loaded: Mutex::new(HashMap::new()),
            next_load: AtomicU64::new(0),
        })
    }

    /// Whether a plugin with `slug` is loaded and neither deactivated nor stopped.
    fn is_loaded(loaded: &HashMap<String, Weak<Plugin>>, slug: &str) -> bool {
        loaded
            .get(slug)
            .and_then(Weak::upgrade)
            .is_some_and(|plugin| !plugin.deactivated() && !plugin.trapped())
    }

    fn loaded(&self) -> MutexGuard<'_, HashMap<String, Weak<Plugin>>> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Compiles `wasm`, a binary WebAssembly component, checks its imports against `manifest`
    /// and instantiates it. The plugin is not activated: call [`Plugin::activate`].
    ///
    /// Compiling takes CPU time (tens to hundreds of milliseconds); call it from a blocking
    /// thread in a server.
    ///
    /// # Errors
    ///
    /// [`HostError::Compile`] for bytes that are not a component (a core module, text) or are
    /// over [`MAX_COMPONENT_BYTES`]; [`HostError::UnknownImport`] and
    /// [`HostError::MissingPermission`] for an import the host does not provide or the manifest
    /// does not grant; [`HostError::Instantiate`] when instantiation fails;
    /// [`HostError::AlreadyLoaded`] when a plugin with the same slug is loaded in this host and
    /// neither deactivated nor stopped (its hooks are owned by the slug).
    pub fn load(&self, manifest: Manifest, wasm: &[u8]) -> Result<Arc<Plugin>, HostError> {
        let already = || HostError::AlreadyLoaded {
            slug: manifest.slug.clone(),
        };
        if Self::is_loaded(&self.loaded(), &manifest.slug) {
            return Err(already());
        }
        if wasm.len() > MAX_COMPONENT_BYTES {
            return Err(HostError::Compile(format!(
                "it is {} bytes; at most {MAX_COMPONENT_BYTES} are accepted",
                wasm.len()
            )));
        }
        let component = Component::from_binary(&self.engine, wasm)
            .map_err(|e| HostError::Compile(format!("{e:#}")))?;
        self.check_imports(&manifest, &component)?;
        let linker = self.linker(&manifest)?;
        let owner = Owner::PluginLoad {
            slug: manifest.slug.clone(),
            load: self.next_load.fetch_add(1, Ordering::SeqCst),
        };
        let mut store = self.new_store(HostState {
            slug: manifest.slug.clone(),
            owner: owner.clone(),
            hooks: Arc::clone(&self.hooks),
            db: Arc::clone(&self.db),
            runtime: self.runtime.clone(),
            plugin: Weak::new(),
        });
        let bindings = bindings::Plugin::instantiate(&mut store, &component, &linker)
            .map_err(|e| HostError::Instantiate(format!("{e:#}")))?;
        let mut loaded = self.loaded();
        if Self::is_loaded(&loaded, &manifest.slug) {
            return Err(already());
        }
        let slug = manifest.slug.clone();
        let plugin = Arc::new(Plugin {
            manifest,
            owner,
            hooks: Arc::clone(&self.hooks),
            instance: Mutex::new(Instance { store, bindings }),
            trapped: AtomicBool::new(false),
            deactivated: AtomicBool::new(false),
            activated: AtomicBool::new(false),
            activating: AtomicBool::new(false),
            subscriptions: AtomicUsize::new(0),
        });
        plugin.lock().store.data_mut().plugin = Arc::downgrade(&plugin);
        loaded.insert(slug, Arc::downgrade(&plugin));
        Ok(plugin)
    }

    /// The store a plugin runs in. Every store is made here, so the limits of Task 12.3 (fuel,
    /// memory) are set in one place.
    fn new_store(&self, state: HostState) -> Store<HostState> {
        Store::new(&self.engine, state)
    }

    /// Refuses an import the host does not provide or `manifest` does not grant.
    fn check_imports(&self, manifest: &Manifest, component: &Component) -> Result<(), HostError> {
        for (import, _) in component.component_type().imports(&self.engine) {
            match interface_permission(import) {
                None => {
                    return Err(HostError::UnknownImport {
                        import: shown(import),
                    });
                }
                Some(Some(permission)) if !manifest.allows(permission) => {
                    return Err(HostError::MissingPermission {
                        import: shown(import),
                        permission,
                    });
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// A linker with the interfaces `manifest` grants, and only those.
    fn linker(&self, manifest: &Manifest) -> Result<Linker<HostState>, HostError> {
        let mut linker = Linker::new(&self.engine);
        let engine_error = |e: wasmtime::Error| HostError::Engine(format!("{e:#}"));
        logging::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(engine_error)?;
        if manifest.allows(Permission::Hooks) {
            wit_hooks::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
                .map_err(engine_error)?;
        }
        if manifest.allows(Permission::ReadContent) {
            content_read::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
                .map_err(engine_error)?;
        }
        if manifest.allows(Permission::WriteContent) {
            content_write::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
                .map_err(engine_error)?;
        }
        Ok(linker)
    }
}

/// For an import named `mission-control:plugin/<interface>@<version>`, with an interface of
/// [`INTERFACES`] and a version this host serves, the permission it needs (`Some(None)` for
/// none); `None` for any other import.
///
/// A version is served when it is not newer than [`HOST_API_VERSION`] and compatible with it
/// by Cargo's rule (the same major version, and the same minor version while the major is 0),
/// as wasmtime's linker matches versions.
fn interface_permission(import: &str) -> Option<Option<Permission>> {
    let rest = import.strip_prefix(PACKAGE)?.strip_prefix('/')?;
    let (interface, version) = rest.split_once('@')?;
    let version = Version::parse(version).ok()?;
    let compatible = version.major == HOST_API_VERSION.major
        && (version.major > 0 || version.minor == HOST_API_VERSION.minor)
        && version <= HOST_API_VERSION;
    if !compatible {
        return None;
    }
    INTERFACES
        .iter()
        .find(|(name, _)| *name == interface)
        .map(|(_, permission)| *permission)
}

/// `text` as it may be shown in a message: control characters escaped, cut at
/// [`MAX_PLUGIN_MESSAGE_CHARS`] characters.
fn shown(text: &str) -> String {
    clean(text, MAX_PLUGIN_MESSAGE_CHARS)
}

/// `text` with control characters escaped (so a message stays on one log line), cut at `max`
/// characters with `…` marking the cut.
fn clean(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for (i, c) in text.chars().enumerate() {
        if i == max {
            out.push('…');
            break;
        }
        if c.is_control() || is_format_char(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `c` is a Unicode format character (general category Cf): invisible characters that
/// reorder or hide text (bidirectional overrides, zero-width joiners, the byte order mark, tag
/// characters), escaped in log lines so a message cannot disguise itself. The ranges are those
/// of Unicode 16.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// The message of a panic's payload.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// Whether this thread is in a multi-thread Tokio runtime, where blocking is done in
/// `block_in_place` so the runtime moves its other tasks to another worker.
fn on_multi_thread_runtime() -> bool {
    Handle::try_current().is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::MultiThread)
}

/// A plugin's store and its instance.
struct Instance {
    store: Store<HostState>,
    bindings: bindings::Plugin,
}

/// A loaded plugin: its manifest and its running instance.
pub struct Plugin {
    manifest: Manifest,
    owner: Owner,
    hooks: Arc<Hooks>,
    instance: Mutex<Instance>,
    trapped: AtomicBool,
    deactivated: AtomicBool,
    /// Set by the first `activate`: a load is activated once.
    activated: AtomicBool,
    /// Set while `activate` runs: the only time the plugin may subscribe.
    activating: AtomicBool,
    subscriptions: AtomicUsize,
}

impl fmt::Debug for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugin")
            .field("slug", &self.manifest.slug)
            .field("trapped", &self.trapped())
            .field("deactivated", &self.deactivated())
            .finish_non_exhaustive()
    }
}

/// Why a call into the plugin did not run or did not finish.
enum CallError {
    /// The plugin is already running a call on this thread.
    Nested,
    /// The plugin was deactivated.
    Deactivated,
    /// The plugin trapped, now or before.
    Trapped(String),
}

impl Plugin {
    /// The plugin's manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The plugin's slug.
    pub fn slug(&self) -> &str {
        &self.manifest.slug
    }

    /// Whether the plugin trapped (or failed in a call, or the host panicked in one): its hooks
    /// are removed and it is no longer called.
    pub fn trapped(&self) -> bool {
        self.trapped.load(Ordering::SeqCst)
    }

    /// Whether the plugin was deactivated ([`Plugin::remove_hooks`]): it is no longer called.
    pub fn deactivated(&self) -> bool {
        self.deactivated.load(Ordering::SeqCst)
    }

    /// Calls the plugin's `activate`, where it subscribes to hooks, with the user set by
    /// [`with_current_user`] acting.
    ///
    /// # Errors
    ///
    /// [`HostError::Activation`] when the plugin refuses, with its message;
    /// [`HostError::Trap`] when it traps or had trapped, or is already running a call on this
    /// thread; [`HostError::Deactivated`] when it was deactivated;
    /// [`HostError::AlreadyActivated`] when it was activated before. Any failure deactivates
    /// the plugin and removes the hooks it subscribed before failing.
    pub fn activate(&self) -> Result<(), HostError> {
        if self.deactivated() {
            return Err(HostError::Deactivated);
        }
        if self.trapped() {
            return Err(HostError::Trap(
                "it failed before and is stopped".to_owned(),
            ));
        }
        if self.activated.swap(true, Ordering::SeqCst) {
            return Err(HostError::AlreadyActivated);
        }
        let outcome = self.call(|instance| {
            struct Activating<'a>(&'a AtomicBool);
            impl Drop for Activating<'_> {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            self.activating.store(true, Ordering::SeqCst);
            let _activating = Activating(&self.activating);
            instance
                .bindings
                .mission_control_plugin_handler()
                .call_activate(&mut instance.store)
        });
        let error = match outcome {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(message)) => HostError::Activation(shown(&message)),
            Err(CallError::Trapped(e)) => HostError::Trap(e),
            Err(CallError::Deactivated) => HostError::Deactivated,
            Err(CallError::Nested) => {
                HostError::Trap("the plugin is already running a call on this thread".to_owned())
            }
        };
        self.remove_hooks();
        Err(error)
    }

    /// Deactivates the plugin: removes every hook callback it subscribed and stops it, so it is
    /// never called again and cannot subscribe again. To activate it again, load it again.
    /// The callbacks are owned by this load ([`Owner::PluginLoad`]), so a plugin loaded since
    /// with the same slug keeps its own.
    pub fn remove_hooks(&self) {
        self.deactivated.store(true, Ordering::SeqCst);
        self.hooks.remove_owner(&self.owner);
        self.subscriptions.store(0, Ordering::SeqCst);
    }

    fn lock(&self) -> MutexGuard<'_, Instance> {
        self.instance.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` on the instance: one call at a time, never nested on one thread, never after a
    /// trap or a deactivation. A failure, or a panic of the host during the call, marks the
    /// plugin trapped, removes its hooks and is logged.
    ///
    /// The store's mutex and the plugin's code may block for the length of a call, so on a
    /// worker of a multi-thread Tokio runtime both run in `block_in_place`; a content call
    /// nested in it then blocks in place too, and never holds a worker the runtime counts on.
    fn call<R>(
        &self,
        f: impl FnOnce(&mut Instance) -> wasmtime::Result<R>,
    ) -> Result<R, CallError> {
        struct Running(usize);
        impl Drop for Running {
            fn drop(&mut self) {
                RUNNING.with(|running| running.borrow_mut().retain(|id| *id != self.0));
            }
        }

        let id = self as *const Plugin as usize;
        if RUNNING.with(|running| running.borrow().contains(&id)) {
            return Err(CallError::Nested);
        }
        self.check_running()?;
        RUNNING.with(|running| running.borrow_mut().push(id));
        let _running = Running(id);
        let locked_call = || {
            let mut instance = self.lock();
            self.check_running()?;
            match catch_unwind(AssertUnwindSafe(|| f(&mut instance))) {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(e)) => Err(self.stop(&format!("{e:#}"))),
                Err(panic) => Err(self.stop(&format!(
                    "the site panicked during the call: {}",
                    panic_message(panic.as_ref())
                ))),
            }
        };
        if on_multi_thread_runtime() {
            tokio::task::block_in_place(locked_call)
        } else {
            locked_call()
        }
    }

    /// Refuses a call into a plugin that trapped or was deactivated.
    fn check_running(&self) -> Result<(), CallError> {
        if self.deactivated() {
            return Err(CallError::Deactivated);
        }
        if self.trapped() {
            return Err(CallError::Trapped(
                "it failed before and is stopped".to_owned(),
            ));
        }
        Ok(())
    }

    /// Stops the plugin after a failure: marks it trapped, removes its hooks and logs `error`.
    fn stop(&self, error: &str) -> CallError {
        let message = shown(error);
        self.trapped.store(true, Ordering::SeqCst);
        self.hooks.remove_owner(&self.owner);
        log::error!(
            target: LOG_TARGET,
            "[{}] the plugin failed and is stopped: {message}",
            self.slug()
        );
        CallError::Trapped(message)
    }

    /// Passes the action `name`, with `payload` as JSON text, to the plugin's `on-action`.
    fn on_action(&self, name: &str, payload: &str) {
        if payload.len() > MAX_JSON_BYTES {
            log::warn!(
                target: LOG_TARGET,
                "[{}] the payload of `{name}` is {} bytes, over {MAX_JSON_BYTES}; it is not passed",
                self.slug(),
                payload.len()
            );
            return;
        }
        let outcome = self.call(|instance| {
            instance
                .bindings
                .mission_control_plugin_handler()
                .call_on_action(&mut instance.store, name, payload)
        });
        match outcome {
            Ok(Ok(())) | Err(CallError::Trapped(_) | CallError::Deactivated) => {}
            Ok(Err(message)) => log::error!(
                target: LOG_TARGET,
                "[{}] the handler of `{name}` failed: {}",
                self.slug(),
                shown(&message)
            ),
            Err(CallError::Nested) => self.warn_nested(name),
        }
    }

    /// Passes the filter `name`'s `value`, as JSON text, through the plugin's `apply-filter`;
    /// the new value, or `None` to keep it.
    fn apply_filter(&self, name: &str, value: &str) -> Option<String> {
        if value.len() > MAX_JSON_BYTES {
            log::warn!(
                target: LOG_TARGET,
                "[{}] the value of `{name}` is {} bytes, over {MAX_JSON_BYTES}; it is not passed",
                self.slug(),
                value.len()
            );
            return None;
        }
        let outcome = self.call(|instance| {
            instance
                .bindings
                .mission_control_plugin_handler()
                .call_apply_filter(&mut instance.store, name, value, "[]")
        });
        match outcome {
            Ok(Ok(filtered)) if filtered.len() > MAX_JSON_BYTES => {
                log::warn!(
                    target: LOG_TARGET,
                    "[{}] the filter `{name}` gave {} bytes, over {MAX_JSON_BYTES}; the value is \
                     kept",
                    self.slug(),
                    filtered.len()
                );
                None
            }
            Ok(Ok(filtered)) => Some(filtered),
            Ok(Err(message)) => {
                log::error!(
                    target: LOG_TARGET,
                    "[{}] the filter `{name}` failed: {}; the value is kept",
                    self.slug(),
                    shown(&message)
                );
                None
            }
            Err(CallError::Trapped(_) | CallError::Deactivated) => None,
            Err(CallError::Nested) => {
                self.warn_nested(name);
                None
            }
        }
    }

    fn warn_nested(&self, name: &str) {
        log::warn!(
            target: LOG_TARGET,
            "[{}] `{name}` fired again while the plugin handled a hook; the nested call is skipped",
            self.slug()
        );
    }
}

/// What a plugin's imports reach: the data of its store.
struct HostState {
    slug: String,
    owner: Owner,
    hooks: Arc<Hooks>,
    db: Arc<Database>,
    runtime: Option<Handle>,
    /// The plugin this store belongs to, set once it is loaded.
    plugin: Weak<Plugin>,
}

/// Which kind of hook a subscription is on.
#[derive(Clone, Copy)]
enum HookKind {
    Action,
    Filter,
}

impl HostState {
    /// Subscribes the plugin to the hook `name`.
    fn subscribe(&mut self, kind: HookKind, name: String, priority: i32) -> Result<(), String> {
        check_hook_name(&name)?;
        check_hook_allowed(&name)?;
        let Some(plugin) = self.plugin.upgrade() else {
            return Err("subscribe in activate, not while the plugin loads".to_owned());
        };
        if plugin.deactivated() {
            return Err("the plugin is deactivated".to_owned());
        }
        if !plugin.activating.load(Ordering::SeqCst) {
            return Err("subscribe in activate: hooks cannot be added later".to_owned());
        }
        if plugin.subscriptions.fetch_add(1, Ordering::SeqCst) >= MAX_SUBSCRIPTIONS {
            plugin.subscriptions.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "the plugin already has {MAX_SUBSCRIPTIONS} subscriptions, the most allowed"
            ));
        }
        let weak = Arc::downgrade(&plugin);
        let hook: Arc<str> = name.as_str().into();
        let owner = self.owner.clone();
        match kind {
            HookKind::Action => self
                .hooks
                .add_json_action(&name, priority, owner, move |json| {
                    if let Some(plugin) = weak.upgrade() {
                        plugin.on_action(&hook, json);
                    }
                }),
            HookKind::Filter => self
                .hooks
                .add_json_filter(&name, priority, owner, move |json| {
                    weak.upgrade()?.apply_filter(&hook, json)
                }),
        }
        // A deactivation on another thread between the check above and the insert would leave
        // this callback behind, owned by this load alone.
        if plugin.deactivated() {
            self.hooks.remove_owner(&self.owner);
            return Err("the plugin is deactivated".to_owned());
        }
        Ok(())
    }

    /// Runs a content call to its end, blocking this thread on the Tokio runtime (see the
    /// module documentation).
    fn run<T>(
        &self,
        call: impl Future<Output = Result<T, ContentError>>,
    ) -> Result<T, ContentError> {
        let unavailable =
            || ContentError::Failed("the site could not complete the call".to_owned());
        let Some(handle) = Handle::try_current().ok().or_else(|| self.runtime.clone()) else {
            log::error!(
                target: LOG_TARGET,
                "[{}] a content call ran outside a Tokio runtime",
                self.slug
            );
            return Err(unavailable());
        };
        if handle.runtime_flavor() == RuntimeFlavor::MultiThread {
            // A panic of the query goes up to `Plugin::call`, which stops the plugin.
            return tokio::task::block_in_place(|| handle.block_on(call));
        }
        // A current-thread runtime: blocking works on its blocking threads and outside it, and
        // Tokio panics on the runtime's own thread, which cannot be told apart beforehand.
        match catch_unwind(AssertUnwindSafe(|| handle.block_on(call))) {
            Ok(outcome) => outcome,
            Err(panic) => {
                let message = panic_message(panic.as_ref());
                if message.contains("Cannot start a runtime from within a runtime") {
                    log::error!(
                        target: LOG_TARGET,
                        "[{}] a content call cannot block on the thread of a current-thread \
                         runtime; fire hooks that reach plugins from a blocking thread",
                        self.slug
                    );
                    Err(unavailable())
                } else {
                    std::panic::resume_unwind(panic)
                }
            }
        }
    }
}

/// Whether no plugin may subscribe to the hook `name`: it is one of [`DENIED_HOOKS`] or starts
/// with one of [`DENIED_HOOK_PREFIXES`] (Ruling R102).
pub fn is_denied_hook(name: &str) -> bool {
    DENIED_HOOKS.contains(&name)
        || DENIED_HOOK_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Refuses a hook [`is_denied_hook`] names.
fn check_hook_allowed(name: &str) -> Result<(), String> {
    if is_denied_hook(name) {
        return Err(format!(
            "plugins may not subscribe to `{name}`: it decides who may do what, carries \
             credentials or turns a security check off"
        ));
    }
    Ok(())
}

/// Refuses a hook name that is empty, too long or not printable ASCII.
fn check_hook_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the hook name is empty".to_owned());
    }
    if name.len() > MAX_HOOK_NAME_BYTES {
        return Err(format!(
            "the hook name is {} bytes; at most {MAX_HOOK_NAME_BYTES} are allowed",
            name.len()
        ));
    }
    if !name.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!(
            "the hook name `{}` has a character that is not printable ASCII",
            clean(name, MAX_HOOK_NAME_BYTES)
        ));
    }
    Ok(())
}

impl logging::Host for HostState {
    fn log(&mut self, level: logging::Level, message: String) {
        let level = match level {
            logging::Level::Error => log::Level::Error,
            logging::Level::Warn => log::Level::Warn,
            logging::Level::Info => log::Level::Info,
            logging::Level::Debug => log::Level::Debug,
            logging::Level::Trace => log::Level::Trace,
        };
        log::log!(
            target: LOG_TARGET,
            level,
            "[{}] {}",
            self.slug,
            clean(&message, MAX_LOG_MESSAGE_CHARS)
        );
    }
}

impl wit_hooks::Host for HostState {
    fn add_action(&mut self, name: String, priority: i32) -> Result<(), String> {
        self.subscribe(HookKind::Action, name, priority)
    }

    fn add_filter(&mut self, name: String, priority: i32) -> Result<(), String> {
        self.subscribe(HookKind::Filter, name, priority)
    }
}

impl From<ContentError> for content_read::Error {
    fn from(error: ContentError) -> Self {
        match error {
            ContentError::Forbidden(m) => Self::Forbidden(m),
            ContentError::NotFound(m) => Self::NotFound(m),
            ContentError::Invalid(m) => Self::Invalid(m),
            ContentError::Failed(m) => Self::Failed(m),
        }
    }
}

impl From<ContentError> for content_write::Error {
    fn from(error: ContentError) -> Self {
        match error {
            ContentError::Forbidden(m) => Self::Forbidden(m),
            ContentError::NotFound(m) => Self::NotFound(m),
            ContentError::Invalid(m) => Self::Invalid(m),
            ContentError::Failed(m) => Self::Failed(m),
        }
    }
}

/// A UTC time as the WIT documents it: `2026-11-15T10:00:00Z`.
fn utc(time: chrono::NaiveDateTime) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl content_read::Host for HostState {
    fn get_post(&mut self, id: i64) -> Result<content_read::Post, content_read::Error> {
        let user = current_user();
        let view = self.run(content::read_post(&self.db, &user, id))?;
        let post = view.post;
        Ok(content_read::Post {
            id: post.id,
            post_type: post.post_type,
            status: post.status.to_string(),
            title: post.title,
            slug: post.slug,
            content: post.content,
            excerpt: post.excerpt,
            author_id: post.author_id,
            parent_id: post.parent_id,
            menu_order: post.menu_order,
            published_at: post.published_at.map(utc),
            modified_at: utc(post.modified_at),
            password_protected: view.password_protected,
        })
    }

    fn get_post_meta(
        &mut self,
        post_id: i64,
        key: String,
    ) -> Result<Option<String>, content_read::Error> {
        let user = current_user();
        Ok(self.run(content::read_post_meta(&self.db, &user, post_id, &key))?)
    }
}

impl content_write::Host for HostState {
    fn update_post(
        &mut self,
        id: i64,
        changes: content_write::PostChanges,
    ) -> Result<(), content_write::Error> {
        let user = current_user();
        let changes = content::PostChanges {
            title: changes.title,
            content: changes.content,
            excerpt: changes.excerpt,
        };
        Ok(self.run(content::update_post(&self.db, &user, id, changes))?)
    }

    fn set_post_meta(
        &mut self,
        post_id: i64,
        key: String,
        value_json: String,
    ) -> Result<(), content_write::Error> {
        let user = current_user();
        let call = content::write_post_meta(&self.db, &user, post_id, &key, &value_json);
        Ok(self.run(call)?)
    }

    fn delete_post_meta(
        &mut self,
        post_id: i64,
        key: String,
    ) -> Result<bool, content_write::Error> {
        let user = current_user();
        Ok(self.run(content::remove_post_meta(&self.db, &user, post_id, &key))?)
    }
}
