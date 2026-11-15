//! The plugin host: runs a plugin, a WebAssembly component built from the WIT interface in
//! `wit/mission-control.wit`, in wasmtime's sandbox (design sections 4 and 4.2).
//!
//! ```ignore
//! let host = PluginHost::new(Arc::clone(&hooks), Arc::clone(&db))?;
//! let plugin = host.load(manifest, &wasm)?;      // checks the imports against the manifest
//! plugin.init()?;                                 // the plugin subscribes to hooks here
//! with_current_user(owner, || plugin.activate())?; // once, when the owner activates it
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
//! [`Owner::PluginLoad`] with the plugin's slug and a number unique to the load: core's
//! `do_action` and `apply_filters` reach the plugin's `on-action` and `apply-filter` in
//! priority order among the other callbacks. A
//! callback holds the plugin weakly: once the plugin is dropped, its callbacks do nothing.
//!
//! A plugin's handler has three lifecycle exports (Ruling R105, WordPress's split):
//! `init` ([`Plugin::init`]) runs on every load, at every start, with no user acting, as
//! WordPress includes a plugin's main file; `activate` ([`Plugin::activate`]) and `deactivate`
//! ([`Plugin::deactivate`]) run once when the owner activates or deactivates the plugin, with
//! that user acting, as `register_activation_hook` and `register_deactivation_hook`.
//!
//! A plugin subscribes in its `init` and only there; WordPress plugins may add hooks at any
//! time, but here a subscription made elsewhere is refused, so that [`Plugin::remove_hooks`]
//! (Task 12.4's deactivation) removes everything the plugin added and a deactivated plugin
//! cannot come back. A deactivated plugin is never called again. An `init` or `activate` that
//! fails (the plugin refuses, or traps) deactivates it and removes what it subscribed before
//! failing. A load runs `init` once and `activate` at most once. A host loads one plugin per slug at a time
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
//! A plugin that traps (or fails in a call for any other reason wasmtime reports, runs out of
//! fuel, needs more memory than its limit, or during whose call the host panics) is marked
//! [`Plugin::trapped`], its hooks and the types it registered are removed and it is not called
//! again, as WordPress's recovery mode pauses a plugin that fails. The error is logged at
//! `error` level, kept as [`Plugin::failure`] and handed to the host's failure handler
//! ([`PluginHost::on_failure`]), through which [`Plugins`](super::Plugins) deactivates the
//! plugin and tells the owner (Task 12.4). The call that failed returns: an activation returns
//! the error, an action ends, and a filter keeps the value it was given, so the site goes on
//! serving.
//!
//! # Types
//!
//! A plugin with the `register-types` permission registers post types, taxonomies and field
//! groups through the `types` interface, in its `init` only, as for hooks, on the registry
//! given to [`PluginHost::with_registry`], each with [`Origin::Plugin`] and its slug (at most
//! [`MAX_TYPE_DEFINITIONS`]). It may not take a slug or key the site's code or another plugin
//! registered ([`Registry::register_exclusive`]). Deactivating or stopping the plugin removes
//! them ([`Registry::remove_plugin`]), once per load.
//!
//! # Limits
//!
//! Every store is made in one place (`PluginHost::new_store`) with the host's
//! [`PluginLimits`]:
//!
//! - **Fuel.** wasmtime counts the plugin's WebAssembly instructions (about one unit each).
//!   Every call into the plugin (`init`, `activate`, `deactivate`, `on-action`, `apply-filter`, and any export added
//!   later, since all go through one call wrapper), and its instantiation, starts with a fresh
//!   [`PluginLimits::fuel_per_call`]; a call that uses it all traps and fails with
//!   [`HostError::OutOfFuel`]. Fuel counts only the plugin's own code: the time spent in host
//!   calls (a database query, a hook the plugin's call fires) is not counted, and there is no
//!   wall-clock limit (no epoch interruption).
//! - **Host calls** (Ruling R104). So that the host's own work is bounded too, each call of an
//!   import (every function of `logging`, `hooks`, `content-read` and `content-write`) costs
//!   [`PluginLimits::fuel_per_host_call`] fuel, taken before it runs; with less fuel left, the
//!   plugin traps and fails with [`HostError::OutOfFuel`]. The imports are defined in one
//!   place (`PluginHost::linker`), each through `charge`. The defaults allow about 1,000
//!   import calls per plugin call, so about 1,000 database queries at most.
//! - **Log lines.** A plugin call writes at most [`PluginLimits::log_lines_per_call`] lines
//!   and [`PluginLimits::log_bytes_per_call`] bytes to the site's log; the call's lines past
//!   either are dropped, after one warning.
//! - **Memory.** The plugin's linear memories together may not grow past
//!   [`PluginLimits::memory_max_bytes`], nor its tables past
//!   [`PluginLimits::table_max_elements`] elements. A growth past a limit is refused as the
//!   WebAssembly specification has it (`memory.grow` gives -1), so a plugin may handle it; one
//!   that then traps (as Rust's allocator aborts) fails with [`HostError::OutOfMemory`]. A
//!   component whose memory starts over the limit does not load ([`HostError::OutOfMemory`]).
//!   The limit also bounds what the host copies (Task 12.2's M3): into the plugin, a value is
//!   copied through the plugin's own allocator, so into its limited memory; out of it, a string
//!   the plugin passes or returns lies in its memory, so it is at most the limit (1.5 times
//!   that once a UTF-16 string is transcoded to UTF-8) before the host checks its size. One
//!   import call takes up to three strings (`update-post`), so it may make the host allocate
//!   up to three times that for the length of the call. The limit is per plugin: there is no
//!   cap across plugins, so N plugins may hold N times [`PluginLimits::memory_max_bytes`].
//! - **Counts.** A store holds at most [`PluginLimits::instances_max`] core instances,
//!   [`PluginLimits::memories_max`] memories and [`PluginLimits::tables_max`] tables; a
//!   component with more does not load ([`HostError::Instantiate`]).
//! - **Nesting.** A plugin's call may fire hooks that reach other plugins (once content
//!   writes fire core's hooks; they do not yet). Each store has its own wasm stack limit ([`MAX_WASM_STACK_BYTES`]),
//!   all on the one native stack of the thread, so a long chain could overflow it: at most
//!   [`MAX_NESTED_PLUGIN_CALLS`] plugin calls run nested on a thread, and a deeper one is
//!   skipped with a warning, as a nested call of the same plugin is.
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
use wasmtime::component::{Component, Linker, LinkerInstance};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreContextMut, Trap};

use super::content::{self, ContentError};
use super::manifest::{HOST_API_VERSION, Manifest, Permission};
use crate::hooks::{Hooks, Owner};
use crate::types::{FieldGroupDef, Origin, PostTypeDef, Registry, TaxonomyDef};
use crate::users::UserCaps;

/// The bindings `wasmtime` generates from the WIT world `plugin`.
mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/mission-control.wit",
        world: "plugin",
    });
}

use bindings::mission_control::plugin::{
    content_read, content_write, hooks as wit_hooks, logging, types as wit_types,
};

/// The WIT package of the plugin API, without its version ([`HOST_API_VERSION`]).
pub const PACKAGE: &str = "mission-control:plugin";

/// The interfaces a plugin may import, with the permission each needs; `None` is always linked.
pub const INTERFACES: [(&str, Option<Permission>); 5] = [
    ("logging", None),
    ("hooks", Some(Permission::Hooks)),
    ("content-read", Some(Permission::ReadContent)),
    ("content-write", Some(Permission::WriteContent)),
    ("types", Some(Permission::RegisterTypes)),
];

/// The largest component accepted, in bytes (32 MiB).
pub const MAX_COMPONENT_BYTES: usize = 32 * 1024 * 1024;

/// The longest hook name a plugin may subscribe to, in bytes.
pub const MAX_HOOK_NAME_BYTES: usize = 128;

/// The most subscriptions one plugin may make. Each subscription is a host call, charged
/// [`PluginLimits::fuel_per_host_call`], so it stays below the about 1,000 host calls the
/// default fuel allows in one `init`.
pub const MAX_SUBSCRIPTIONS: usize = 512;

/// The largest hook payload or filter value passed to a plugin, and taken back from it, in
/// bytes of JSON (4 MiB).
pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;

/// The most post types, taxonomies and field groups one plugin may register, together.
pub const MAX_TYPE_DEFINITIONS: usize = 256;

/// The longest log message a plugin may write, in characters; a longer one is cut.
pub const MAX_LOG_MESSAGE_CHARS: usize = 4096;

/// The longest message from a plugin kept in an error or a log line, in characters.
pub const MAX_PLUGIN_MESSAGE_CHARS: usize = 1024;

/// The largest stack a plugin's code may use, in bytes (512 KiB, wasmtime's default, set so it
/// does not change with wasmtime's).
pub const MAX_WASM_STACK_BYTES: usize = 512 * 1024;

/// The limits of a plugin's calls and memory (design section 4.2), given to
/// [`PluginHost::with_limits`]; [`PluginHost::new`] takes the [`Default`] ones. See the module
/// documentation, "Limits".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginLimits {
    /// The fuel each call into a plugin starts with, about one unit per WebAssembly instruction
    /// the plugin runs; a call that uses it all fails with [`HostError::OutOfFuel`]. Default one
    /// billion, measured on an Apple-silicon laptop in a release build: a plugin that loops
    /// forever is stopped after about 0.1 s of a tight loop (longer for code that touches
    /// memory), and a filter that reads and changes every byte of a 256 KiB post uses about 6
    /// million, so a plugin has more than a hundred times that before it is stopped.
    pub fuel_per_call: u64,
    /// The fuel each call of an import (a host call: logging, hooks, content) costs, taken from
    /// the call's [`PluginLimits::fuel_per_call`], so the host's work for a plugin is bounded
    /// too (Ruling R104). Default one million: with the default fuel per call, about 1,000
    /// import calls, so about 1,000 database queries, per plugin call.
    pub fuel_per_host_call: u64,
    /// The most lines a plugin may write to the site's log in one call; the call's lines past
    /// it are dropped, after one warning. Default 100.
    pub log_lines_per_call: usize,
    /// The most bytes of messages a plugin may write to the site's log in one call; the call's
    /// lines past it are dropped, after one warning. Default 64 KiB.
    pub log_bytes_per_call: usize,
    /// The most bytes a plugin's linear memories may hold together. Default 64 MiB: sixteen
    /// times [`MAX_JSON_BYTES`], room for a plugin to parse the largest value it is passed.
    ///
    /// It bounds what the host copies out of the plugin: a string the plugin passes or returns
    /// is at most this many bytes (1.5 times for a UTF-16 string transcoded), and one import
    /// call, with up to three strings, may make the host allocate up to three times that while
    /// it runs. The limit is per plugin; there is no cap across plugins.
    pub memory_max_bytes: usize,
    /// The most elements a plugin's tables may hold together. Default 100,000, far more than
    /// the function tables a compiled plugin has.
    pub table_max_elements: usize,
    /// The most core instances a plugin's store may hold. Default 64; a component built from
    /// Rust has a handful.
    pub instances_max: usize,
    /// The most linear memories a plugin's store may hold. Default 8; a component built from
    /// Rust has one.
    pub memories_max: usize,
    /// The most tables a plugin's store may hold. Default 64; a component built from Rust has
    /// a few.
    pub tables_max: usize,
}

impl Default for PluginLimits {
    fn default() -> Self {
        Self {
            fuel_per_call: 1_000_000_000,
            fuel_per_host_call: 1_000_000,
            log_lines_per_call: 100,
            log_bytes_per_call: 64 * 1024,
            memory_max_bytes: 64 * 1024 * 1024,
            table_max_elements: 100_000,
            instances_max: 64,
            memories_max: 8,
            tables_max: 64,
        }
    }
}

impl PluginLimits {
    /// Refuses limits that leave a plugin nothing to run with: no fuel, a host call costing
    /// nothing or more than a whole call, less memory than one WebAssembly page (64 KiB), or no
    /// instance or memory.
    fn check(&self) -> Result<(), HostError> {
        let invalid = |message: &str| Err(HostError::InvalidLimits(message.to_owned()));
        if self.fuel_per_call == 0 {
            return invalid("`fuel_per_call` is 0: no plugin could run");
        }
        if self.fuel_per_host_call == 0 {
            return invalid("`fuel_per_host_call` is 0: host calls would be free and unbounded");
        }
        if self.fuel_per_host_call > self.fuel_per_call {
            return invalid("`fuel_per_host_call` is over `fuel_per_call`: no host call could run");
        }
        if self.memory_max_bytes < WASM_PAGE_BYTES {
            return invalid("`memory_max_bytes` is under one WebAssembly page (64 KiB)");
        }
        if self.instances_max == 0 || self.memories_max == 0 {
            return invalid("`instances_max` and `memories_max` must be at least 1");
        }
        Ok(())
    }
}

/// The size of a WebAssembly page, the unit linear memory grows by.
const WASM_PAGE_BYTES: usize = 64 * 1024;

/// The most plugin calls that may run nested on one thread (a plugin's call fires a hook that
/// reaches another plugin, and so on); a deeper call is skipped with a warning. Each store has
/// its own wasm stack limit on the thread's one native stack, so the chain is kept short.
pub const MAX_NESTED_PLUGIN_CALLS: usize = 4;

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
    /// The [`PluginLimits`] given to [`PluginHost::with_limits`] leave a plugin nothing to run
    /// with, such as zero fuel.
    InvalidLimits(String),
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
    /// The plugin's `init` refused, with its message: it does not start.
    Init(String),
    /// The plugin's `activate` refused, with its message.
    Activation(String),
    /// The plugin's `deactivate` failed, with its message; it is deactivated anyway.
    Deactivation(String),
    /// The plugin trapped or failed in a call, or had trapped before.
    Trap(String),
    /// The plugin used all the fuel of a call ([`PluginLimits::fuel_per_call`]): it ran too
    /// long, or loops forever.
    OutOfFuel {
        /// The fuel the call started with.
        fuel: u64,
    },
    /// The plugin needed more memory, or more table elements, than its limits
    /// ([`PluginLimits::memory_max_bytes`], [`PluginLimits::table_max_elements`]), with what
    /// was refused.
    OutOfMemory(String),
    /// The plugin was deactivated ([`Plugin::remove_hooks`]); load it again to activate it.
    Deactivated,
    /// The plugin was initialized before: a load runs `init` once.
    AlreadyInitialized,
    /// [`Plugin::activate`] was called before [`Plugin::init`].
    NotInitialized,
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
            Self::InvalidLimits(e) => write!(f, "the plugin limits are not valid: {e}"),
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
            Self::Init(e) => write!(f, "the plugin refused to start: {e}"),
            Self::Activation(e) => write!(f, "the plugin refused to activate: {e}"),
            Self::Deactivation(e) => write!(f, "the plugin's deactivation failed: {e}"),
            Self::Trap(e) => write!(f, "the plugin failed: {e}"),
            Self::OutOfFuel { fuel } => write!(
                f,
                "the plugin used all the fuel of a call ({fuel} units): it ran too long, or \
                 loops forever"
            ),
            Self::OutOfMemory(e) => write!(f, "the plugin needed more memory than it may use: {e}"),
            Self::Deactivated => {
                f.write_str("the plugin is deactivated; load it again to activate it")
            }
            Self::AlreadyInitialized => f.write_str("the plugin is already started"),
            Self::NotInitialized => f.write_str("the plugin was not started before activation"),
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
    limits: PluginLimits,
    /// The runtime the host was created in, for content calls from threads outside it.
    runtime: Option<Handle>,
    /// The plugins loaded, by slug.
    loaded: Mutex<HashMap<String, Weak<Plugin>>>,
    /// The number of the next load, for its hook owner.
    next_load: AtomicU64,
    /// Where the plugins' `types` imports register, if the site has a type registry.
    registry: Option<Arc<Registry>>,
    /// Told when a plugin fails and is stopped.
    on_failure: Option<FailureHandler>,
}

/// What [`PluginHost::on_failure`] calls when a plugin fails and is stopped: the plugin, and why.
type FailureHandler = Arc<dyn Fn(&Plugin, &HostError) + Send + Sync>;

impl fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginHost").finish_non_exhaustive()
    }
}

impl PluginHost {
    /// A host whose plugins subscribe on `hooks` and reach content in `db`, with the default
    /// [`PluginLimits`]. Created inside a Tokio runtime, it keeps that runtime for content
    /// calls made from outside one.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] when wasmtime cannot be set up on this machine.
    pub fn new(hooks: Arc<Hooks>, db: Arc<Database>) -> Result<Self, HostError> {
        Self::with_limits(hooks, db, PluginLimits::default())
    }

    /// A host as [`PluginHost::new`] makes, whose plugins run within `limits`.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidLimits`] when `limits` leave a plugin nothing to run with (zero
    /// fuel, a host call costing nothing or more than a call, less than 64 KiB of memory, no
    /// instance or memory); [`HostError::Engine`] when wasmtime cannot be set up on this
    /// machine.
    pub fn with_limits(
        hooks: Arc<Hooks>,
        db: Arc<Database>,
        limits: PluginLimits,
    ) -> Result<Self, HostError> {
        limits.check()?;
        let mut config = Config::new();
        config.wasm_component_model(true).consume_fuel(true);
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
            limits,
            runtime: Handle::try_current().ok(),
            loaded: Mutex::new(HashMap::new()),
            next_load: AtomicU64::new(0),
            registry: None,
            on_failure: None,
        })
    }

    /// The host with `registry` as where its plugins' `types` imports (permission
    /// `register-types`) register post types, taxonomies and field groups, each with
    /// [`Origin::Plugin`] and the plugin's slug. Without a registry, every registration fails.
    /// A plugin's definitions are removed when it is deactivated ([`Plugin::remove_hooks`]) or
    /// stopped after a failure.
    #[must_use]
    pub fn with_registry(mut self, registry: Arc<Registry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The host with `handler` called each time one of its plugins fails and is stopped
    /// ([`Plugin::trapped`]), with the plugin and its [`Plugin::failure`], after its hooks and
    /// types are removed. Task 12.4's recovery records the failure there; the handler runs on
    /// the thread that called the plugin, inside the call that failed, so it must not call the
    /// plugin, block or wait on the database: it hands the failure on.
    #[must_use]
    pub fn on_failure(
        mut self,
        handler: impl Fn(&Plugin, &HostError) + Send + Sync + 'static,
    ) -> Self {
        self.on_failure = Some(Arc::new(handler));
        self
    }

    /// Compiles `wasm` and checks its imports against `manifest`, as [`PluginHost::load`] does,
    /// without instantiating or loading it: for an install, to refuse a module that would not
    /// load before its files are written.
    ///
    /// # Errors
    ///
    /// [`HostError::Compile`], [`HostError::UnknownImport`] and
    /// [`HostError::MissingPermission`], as [`PluginHost::load`].
    pub fn check(&self, manifest: &Manifest, wasm: &[u8]) -> Result<(), HostError> {
        let component = self.compile(wasm)?;
        self.check_imports(manifest, &component)
    }

    /// Compiles `wasm`, a binary component of at most [`MAX_COMPONENT_BYTES`].
    fn compile(&self, wasm: &[u8]) -> Result<Component, HostError> {
        if wasm.len() > MAX_COMPONENT_BYTES {
            return Err(HostError::Compile(format!(
                "it is {} bytes; at most {MAX_COMPONENT_BYTES} are accepted",
                wasm.len()
            )));
        }
        Component::from_binary(&self.engine, wasm).map_err(|e| HostError::Compile(format!("{e:#}")))
    }

    /// The limits the host's plugins run within.
    pub fn limits(&self) -> &PluginLimits {
        &self.limits
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
    /// and instantiates it. The plugin is not started: call [`Plugin::init`].
    ///
    /// Compiling takes CPU time (tens to hundreds of milliseconds); call it from a blocking
    /// thread in a server.
    ///
    /// # Errors
    ///
    /// [`HostError::Compile`] for bytes that are not a component (a core module, text) or are
    /// over [`MAX_COMPONENT_BYTES`]; [`HostError::UnknownImport`] and
    /// [`HostError::MissingPermission`] for an import the host does not provide or the manifest
    /// does not grant; [`HostError::Instantiate`] when instantiation fails, or the component
    /// has more instances, memories or tables than [`PluginLimits`] allows;
    /// [`HostError::OutOfMemory`] when its memory or tables start over the limits;
    /// [`HostError::OutOfFuel`] when its start functions use all the fuel;
    /// [`HostError::AlreadyLoaded`] when a plugin with the same slug is loaded in this host and
    /// neither deactivated nor stopped (its hooks are owned by the slug).
    pub fn load(&self, manifest: Manifest, wasm: &[u8]) -> Result<Arc<Plugin>, HostError> {
        let already = || HostError::AlreadyLoaded {
            slug: manifest.slug.clone(),
        };
        if Self::is_loaded(&self.loaded(), &manifest.slug) {
            return Err(already());
        }
        let component = self.compile(wasm)?;
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
            registry: self.registry.clone(),
            limits: self.limits,
            limiter: Limiter::new(self.limits),
            log: LogBudget::default(),
        })?;
        let bindings = match bindings::Plugin::instantiate(&mut store, &component, &linker) {
            Ok(bindings) => bindings,
            Err(e) => {
                return Err(failure(&e, &mut store, self.limits.fuel_per_call)
                    .unwrap_or_else(|| HostError::Instantiate(format!("{e:#}"))));
            }
        };
        let mut loaded = self.loaded();
        if Self::is_loaded(&loaded, &manifest.slug) {
            return Err(already());
        }
        let slug = manifest.slug.clone();
        let plugin = Arc::new(Plugin {
            manifest,
            owner,
            hooks: Arc::clone(&self.hooks),
            registry: self.registry.clone(),
            on_failure: self.on_failure.clone(),
            instance: Mutex::new(Instance { store, bindings }),
            fuel_per_call: self.limits.fuel_per_call,
            trapped: AtomicBool::new(false),
            failure: Mutex::new(None),
            deactivated: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            initializing: AtomicBool::new(false),
            activated: AtomicBool::new(false),
            subscriptions: AtomicUsize::new(0),
            definitions: AtomicUsize::new(0),
            released: AtomicBool::new(false),
        });
        plugin.lock().store.data_mut().plugin = Arc::downgrade(&plugin);
        loaded.insert(slug, Arc::downgrade(&plugin));
        Ok(plugin)
    }

    /// The store a plugin runs in, with the host's limits: every store is made here. It holds
    /// the fuel of one call, for the instantiation; [`Plugin::call`] refills it for each call.
    fn new_store(&self, state: HostState) -> Result<Store<HostState>, HostError> {
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limiter);
        store
            .set_fuel(self.limits.fuel_per_call)
            .map_err(|e| HostError::Engine(format!("{e:#}")))?;
        Ok(store)
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

    /// A linker with the interfaces `manifest` grants, and only those. Every import is defined
    /// here, and each runs through [`charge`], which takes
    /// [`PluginLimits::fuel_per_host_call`] fuel before the import runs (Ruling R104).
    fn linker(&self, manifest: &Manifest) -> Result<Linker<HostState>, HostError> {
        let mut linker = Linker::new(&self.engine);
        let mut define =
            |interface: &str,
             imports: fn(&mut LinkerInstance<'_, HostState>) -> wasmtime::Result<()>|
             -> Result<(), HostError> {
                let name = format!("{PACKAGE}/{interface}@{HOST_API_VERSION}");
                linker
                    .instance(&name)
                    .and_then(|mut instance| imports(&mut instance))
                    .map_err(|e| HostError::Engine(format!("{e:#}")))
            };
        define("logging", define_logging)?;
        if manifest.allows(Permission::Hooks) {
            define("hooks", define_hooks)?;
        }
        if manifest.allows(Permission::ReadContent) {
            define("content-read", define_content_read)?;
        }
        if manifest.allows(Permission::WriteContent) {
            define("content-write", define_content_write)?;
        }
        if manifest.allows(Permission::RegisterTypes) {
            define("types", define_types)?;
        }
        Ok(linker)
    }
}

/// Takes [`PluginLimits::fuel_per_host_call`] fuel from the plugin's store for a call of an
/// import; with less left, the plugin traps out of fuel and the import does not run.
fn charge(store: &mut StoreContextMut<'_, HostState>) -> wasmtime::Result<()> {
    let cost = store.data().limits.fuel_per_host_call;
    let left = store.get_fuel()?;
    match left.checked_sub(cost) {
        Some(rest) => store.set_fuel(rest),
        None => {
            store.set_fuel(0)?;
            Err(Trap::OutOfFuel.into())
        }
    }
}

/// The `logging` interface's import, charged.
fn define_logging(imports: &mut LinkerInstance<'_, HostState>) -> wasmtime::Result<()> {
    imports.func_wrap(
        "log",
        |mut store: StoreContextMut<'_, HostState>, (level, message): (logging::Level, String)| {
            charge(&mut store)?;
            logging::Host::log(store.data_mut(), level, message);
            Ok(())
        },
    )
}

/// The `hooks` interface's imports, charged.
fn define_hooks(imports: &mut LinkerInstance<'_, HostState>) -> wasmtime::Result<()> {
    imports.func_wrap(
        "add-action",
        |mut store: StoreContextMut<'_, HostState>, (name, priority): (String, i32)| {
            charge(&mut store)?;
            Ok((wit_hooks::Host::add_action(
                store.data_mut(),
                name,
                priority,
            ),))
        },
    )?;
    imports.func_wrap(
        "add-filter",
        |mut store: StoreContextMut<'_, HostState>, (name, priority): (String, i32)| {
            charge(&mut store)?;
            Ok((wit_hooks::Host::add_filter(
                store.data_mut(),
                name,
                priority,
            ),))
        },
    )
}

/// The `content-read` interface's imports, charged.
fn define_content_read(imports: &mut LinkerInstance<'_, HostState>) -> wasmtime::Result<()> {
    imports.func_wrap(
        "get-post",
        |mut store: StoreContextMut<'_, HostState>, (id,): (i64,)| {
            charge(&mut store)?;
            Ok((content_read::Host::get_post(store.data_mut(), id),))
        },
    )?;
    imports.func_wrap(
        "get-post-meta",
        |mut store: StoreContextMut<'_, HostState>, (post_id, key): (i64, String)| {
            charge(&mut store)?;
            Ok((content_read::Host::get_post_meta(
                store.data_mut(),
                post_id,
                key,
            ),))
        },
    )
}

/// The `content-write` interface's imports, charged.
fn define_content_write(imports: &mut LinkerInstance<'_, HostState>) -> wasmtime::Result<()> {
    imports.func_wrap(
        "update-post",
        |mut store: StoreContextMut<'_, HostState>,
         (id, changes): (i64, content_write::PostChanges)| {
            charge(&mut store)?;
            Ok((content_write::Host::update_post(
                store.data_mut(),
                id,
                changes,
            ),))
        },
    )?;
    imports.func_wrap(
        "set-post-meta",
        |mut store: StoreContextMut<'_, HostState>,
         (post_id, key, value_json): (i64, String, String)| {
            charge(&mut store)?;
            Ok((content_write::Host::set_post_meta(
                store.data_mut(),
                post_id,
                key,
                value_json,
            ),))
        },
    )?;
    imports.func_wrap(
        "delete-post-meta",
        |mut store: StoreContextMut<'_, HostState>, (post_id, key): (i64, String)| {
            charge(&mut store)?;
            Ok((content_write::Host::delete_post_meta(
                store.data_mut(),
                post_id,
                key,
            ),))
        },
    )
}

/// The `types` interface's imports, charged.
fn define_types(imports: &mut LinkerInstance<'_, HostState>) -> wasmtime::Result<()> {
    for (name, kind) in [
        ("register-post-type", TypeKind::PostType),
        ("register-taxonomy", TypeKind::Taxonomy),
        ("register-field-group", TypeKind::FieldGroup),
    ] {
        imports.func_wrap(
            name,
            move |mut store: StoreContextMut<'_, HostState>, (json,): (String,)| {
                charge(&mut store)?;
                Ok((store.data_mut().register(kind, &json),))
            },
        )?;
    }
    Ok(())
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
pub(super) fn clean(text: &str, max: usize) -> String {
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

/// What a failure of the plugin's code in `store` was, when it was a limit: fuel used up (the
/// store started with `fuel`), or a growth the limiter refused. `None` for any other failure.
/// Takes the limiter's refusal, so the next call starts without one.
fn failure(error: &wasmtime::Error, store: &mut Store<HostState>, fuel: u64) -> Option<HostError> {
    let refused = store.data_mut().limiter.refused.take();
    if error.downcast_ref::<Trap>() == Some(&Trap::OutOfFuel) {
        return Some(HostError::OutOfFuel { fuel });
    }
    refused.map(HostError::OutOfMemory)
}

/// `bytes` as a message shows it: in MiB or KiB when it is a whole number of them.
fn size(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB && bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// The resource limiter of a plugin's store: [`PluginLimits`] on its memories, tables and
/// instances. The memory and table limits count all the plugin's memories (tables) together.
/// A growth past a limit is refused (`memory.grow` and `table.grow` give -1, an instantiation
/// fails) and noted, so a trap that follows is reported as [`HostError::OutOfMemory`].
struct Limiter {
    limits: PluginLimits,
    /// The bytes of the plugin's linear memories, together.
    memory_bytes: usize,
    /// The elements of the plugin's tables, together.
    table_elements: usize,
    /// What the last refused growth asked for, until the call ends.
    refused: Option<String>,
}

impl Limiter {
    fn new(limits: PluginLimits) -> Self {
        Self {
            limits,
            memory_bytes: 0,
            table_elements: 0,
            refused: None,
        }
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|maximum| desired > maximum) {
            return Ok(false);
        }
        let total = self
            .memory_bytes
            .saturating_sub(current)
            .saturating_add(desired);
        if total > self.limits.memory_max_bytes {
            self.refused = Some(format!(
                "its linear memory would hold {}; the limit is {}",
                size(total),
                size(self.limits.memory_max_bytes)
            ));
            return Ok(false);
        }
        self.memory_bytes = total;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|maximum| desired > maximum) {
            return Ok(false);
        }
        let total = self
            .table_elements
            .saturating_sub(current)
            .saturating_add(desired);
        if total > self.limits.table_max_elements {
            self.refused = Some(format!(
                "its tables would hold {total} elements; the limit is {}",
                self.limits.table_max_elements
            ));
            return Ok(false);
        }
        self.table_elements = total;
        Ok(true)
    }

    fn instances(&self) -> usize {
        self.limits.instances_max
    }

    fn tables(&self) -> usize {
        self.limits.tables_max
    }

    fn memories(&self) -> usize {
        self.limits.memories_max
    }
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
    /// Where its types are registered, if the host has a registry.
    registry: Option<Arc<Registry>>,
    /// Told when the plugin fails and is stopped.
    on_failure: Option<FailureHandler>,
    instance: Mutex<Instance>,
    /// The fuel each call starts with ([`PluginLimits::fuel_per_call`]).
    fuel_per_call: u64,
    trapped: AtomicBool,
    /// Why the plugin was stopped, once it is.
    failure: Mutex<Option<HostError>>,
    deactivated: AtomicBool,
    /// Set by the first `init`: a load is initialized once.
    initialized: AtomicBool,
    /// Set while `init` runs: the only time the plugin may subscribe or register types.
    initializing: AtomicBool,
    /// Set by the first `activate`: a load is activated once.
    activated: AtomicBool,
    subscriptions: AtomicUsize,
    /// How many definitions it registered.
    definitions: AtomicUsize,
    /// Set once its hooks and types are removed for good (deactivated or stopped), so a later
    /// call removes nothing a newer load of the slug added.
    released: AtomicBool,
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
    /// [`MAX_NESTED_PLUGIN_CALLS`] plugin calls are already running on this thread.
    TooDeep,
    /// The plugin was deactivated.
    Deactivated,
    /// The plugin failed (trapped, ran out of fuel or memory), now or before.
    Failed(HostError),
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

    /// The number of this load of the plugin, unique in its host: two loads of one slug (after
    /// a deactivation) have different numbers.
    pub fn load_number(&self) -> u64 {
        match &self.owner {
            Owner::PluginLoad { load, .. } => *load,
            _ => 0,
        }
    }

    /// Whether the plugin trapped (or failed in a call, or the host panicked in one): its hooks
    /// are removed and it is no longer called.
    pub fn trapped(&self) -> bool {
        self.trapped.load(Ordering::SeqCst)
    }

    /// Why the plugin was stopped ([`Plugin::trapped`]): [`HostError::Trap`],
    /// [`HostError::OutOfFuel`] or [`HostError::OutOfMemory`]; `None` while it runs. This is
    /// what the owner is told (Task 12.4).
    pub fn failure(&self) -> Option<HostError> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether the plugin was deactivated ([`Plugin::remove_hooks`]): it is no longer called.
    pub fn deactivated(&self) -> bool {
        self.deactivated.load(Ordering::SeqCst)
    }

    /// Calls the plugin's `init`, where it subscribes to hooks and registers types (Ruling
    /// R105, WordPress's main plugin file): once per load, at every start and before
    /// [`Plugin::activate`], with no user acting unless [`with_current_user`] sets one.
    ///
    /// # Errors
    ///
    /// [`HostError::Init`] when the plugin refuses, with its message;
    /// [`HostError::Trap`] when it traps or had trapped, or is already running a call on this
    /// thread, or too many plugin calls are nested ([`MAX_NESTED_PLUGIN_CALLS`]);
    /// [`HostError::OutOfFuel`] and [`HostError::OutOfMemory`] when it passes a limit;
    /// [`HostError::Deactivated`] when it was deactivated;
    /// [`HostError::AlreadyInitialized`] when it was initialized before. Any failure
    /// deactivates the plugin and removes the hooks and types it added before failing.
    pub fn init(&self) -> Result<(), HostError> {
        self.check_usable()?;
        if self.initialized.swap(true, Ordering::SeqCst) {
            return Err(HostError::AlreadyInitialized);
        }
        let outcome = self.call(|instance| {
            struct Window<'a>(&'a AtomicBool);
            impl Drop for Window<'_> {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            self.initializing.store(true, Ordering::SeqCst);
            let _window = Window(&self.initializing);
            instance
                .bindings
                .mission_control_plugin_handler()
                .call_init(&mut instance.store)
        });
        self.finish(outcome, HostError::Init)
    }

    /// Calls the plugin's `activate`, once, when the owner activates it (WordPress's
    /// `register_activation_hook`), with the user set by [`with_current_user`] acting. It must
    /// follow [`Plugin::init`]; the plugin may not subscribe or register types here.
    ///
    /// # Errors
    ///
    /// [`HostError::Activation`] when the plugin refuses, with its message;
    /// [`HostError::NotInitialized`] before [`Plugin::init`];
    /// [`HostError::AlreadyActivated`] when it was activated before; and as [`Plugin::init`].
    /// Any failure deactivates the plugin and removes its hooks and types: it is not activated.
    pub fn activate(&self) -> Result<(), HostError> {
        self.check_usable()?;
        if !self.initialized.load(Ordering::SeqCst) {
            return Err(HostError::NotInitialized);
        }
        if self.activated.swap(true, Ordering::SeqCst) {
            return Err(HostError::AlreadyActivated);
        }
        let outcome = self.call(|instance| {
            instance
                .bindings
                .mission_control_plugin_handler()
                .call_activate(&mut instance.store)
        });
        self.finish(outcome, HostError::Activation)
    }

    /// Calls the plugin's `deactivate` (WordPress's `register_deactivation_hook`), with the
    /// user set by [`with_current_user`] acting, then deactivates it ([`Plugin::remove_hooks`])
    /// whatever `deactivate` gave, as WordPress deactivates a plugin whose deactivation hook
    /// fails. A plugin that was stopped or deactivated before is not called.
    ///
    /// # Errors
    ///
    /// [`HostError::Deactivation`] when the plugin's `deactivate` fails, with its message, and
    /// the errors of a failed call as [`Plugin::init`] (a trap also stops the plugin as any
    /// trap does). The plugin is deactivated either way.
    pub fn deactivate(&self) -> Result<(), HostError> {
        if self.check_usable().is_err() || !self.initialized.load(Ordering::SeqCst) {
            self.remove_hooks();
            return Ok(());
        }
        let outcome = self.call(|instance| {
            instance
                .bindings
                .mission_control_plugin_handler()
                .call_deactivate(&mut instance.store)
        });
        let outcome = self.finish(outcome, HostError::Deactivation);
        self.remove_hooks();
        outcome
    }

    /// Refuses a call into a plugin that was deactivated or stopped.
    fn check_usable(&self) -> Result<(), HostError> {
        if self.deactivated() {
            return Err(HostError::Deactivated);
        }
        if self.trapped() {
            return Err(HostError::Trap(
                "it failed before and is stopped".to_owned(),
            ));
        }
        Ok(())
    }

    /// The outcome of a lifecycle call (`init`, `activate`, `deactivate`): a refusal becomes
    /// `refused` with the plugin's message; any failure deactivates the plugin.
    fn finish(
        &self,
        outcome: Result<Result<(), String>, CallError>,
        refused: fn(String) -> HostError,
    ) -> Result<(), HostError> {
        let error = match outcome {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(message)) => refused(shown(&message)),
            Err(CallError::Failed(e)) => e,
            Err(CallError::Deactivated) => HostError::Deactivated,
            Err(CallError::Nested) => {
                HostError::Trap("the plugin is already running a call on this thread".to_owned())
            }
            Err(CallError::TooDeep) => HostError::Trap(format!(
                "{MAX_NESTED_PLUGIN_CALLS} plugin calls are already running on this thread"
            )),
        };
        self.remove_hooks();
        Err(error)
    }

    /// Deactivates the plugin: removes every hook callback it subscribed and stops it, so it is
    /// never called again and cannot subscribe again. To activate it again, load it again.
    /// The callbacks are owned by this load ([`Owner::PluginLoad`]), so a plugin loaded since
    /// with the same slug keeps its own.
    ///
    /// The post types, taxonomies and field groups it registered are removed too, the first
    /// time this load is deactivated or stopped (not again, so a newer load's stay).
    pub fn remove_hooks(&self) {
        self.deactivated.store(true, Ordering::SeqCst);
        self.hooks.remove_owner(&self.owner);
        self.subscriptions.store(0, Ordering::SeqCst);
        self.release_types();
    }

    /// Removes the definitions the plugin registered from the registry, once per load and
    /// only when this load registered any.
    fn release_types(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.definitions.load(Ordering::SeqCst) > 0
            && let Some(registry) = &self.registry
        {
            registry.remove_plugin(self.slug());
        }
    }

    fn lock(&self) -> MutexGuard<'_, Instance> {
        self.instance.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` on the instance: one call at a time, never nested on one thread, never after a
    /// trap or a deactivation, with a fresh [`PluginLimits::fuel_per_call`]. A failure (a limit
    /// passed included), or a panic of the host during the call, marks the plugin trapped,
    /// removes its hooks and is logged.
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
        if RUNNING.with(|running| running.borrow().len()) >= MAX_NESTED_PLUGIN_CALLS {
            return Err(CallError::TooDeep);
        }
        self.check_running()?;
        RUNNING.with(|running| running.borrow_mut().push(id));
        let _running = Running(id);
        let locked_call = || {
            let mut instance = self.lock();
            self.check_running()?;
            let instance = &mut *instance;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let state = instance.store.data_mut();
                state.limiter.refused = None;
                state.log = LogBudget::default();
                instance.store.set_fuel(self.fuel_per_call)?;
                f(instance)
            }));
            match outcome {
                Ok(Ok(value)) => {
                    if let Some(refused) = instance.store.data_mut().limiter.refused.take() {
                        log::warn!(
                            target: LOG_TARGET,
                            "[{}] a memory growth was refused ({refused}); the plugin went on",
                            self.slug()
                        );
                    }
                    Ok(value)
                }
                Ok(Err(e)) => Err(self.stop(
                    failure(&e, &mut instance.store, self.fuel_per_call)
                        .unwrap_or_else(|| HostError::Trap(shown(&format!("{e:#}")))),
                )),
                Err(panic) => Err(self.stop(HostError::Trap(shown(&format!(
                    "the site panicked during the call: {}",
                    panic_message(panic.as_ref())
                ))))),
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
            return Err(CallError::Failed(HostError::Trap(
                "it failed before and is stopped".to_owned(),
            )));
        }
        Ok(())
    }

    /// Stops the plugin after a failure: marks it trapped, keeps `error` as its
    /// [`Plugin::failure`], removes its hooks and logs `error`. Task 12.4 tells the owner from
    /// here.
    fn stop(&self, error: HostError) -> CallError {
        *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
        self.trapped.store(true, Ordering::SeqCst);
        self.hooks.remove_owner(&self.owner);
        self.release_types();
        log::error!(
            target: LOG_TARGET,
            "[{}] the plugin failed and is stopped: {error}",
            self.slug()
        );
        if let Some(handler) = &self.on_failure {
            // A panicking handler must not take the call down with it.
            if catch_unwind(AssertUnwindSafe(|| handler(self, &error))).is_err() {
                log::error!(
                    target: LOG_TARGET,
                    "[{}] the failure handler panicked",
                    self.slug()
                );
            }
        }
        CallError::Failed(error)
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
            Ok(Ok(())) | Err(CallError::Failed(_) | CallError::Deactivated) => {}
            Ok(Err(message)) => log::error!(
                target: LOG_TARGET,
                "[{}] the handler of `{name}` failed: {}",
                self.slug(),
                shown(&message)
            ),
            Err(CallError::Nested) => self.warn_nested(name),
            Err(CallError::TooDeep) => self.warn_too_deep(name),
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
            Err(CallError::Failed(_) | CallError::Deactivated) => None,
            Err(CallError::Nested) => {
                self.warn_nested(name);
                None
            }
            Err(CallError::TooDeep) => {
                self.warn_too_deep(name);
                None
            }
        }
    }

    fn warn_too_deep(&self, name: &str) {
        log::warn!(
            target: LOG_TARGET,
            "[{}] `{name}` fired with {MAX_NESTED_PLUGIN_CALLS} plugin calls already running on \
             this thread; the call is skipped",
            self.slug()
        );
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
    /// Where the `types` imports register.
    registry: Option<Arc<Registry>>,
    /// The host's limits.
    limits: PluginLimits,
    /// The store's limits on memories, tables and instances.
    limiter: Limiter,
    /// What the current call has written to the log.
    log: LogBudget,
}

/// What a plugin's call has written to the site's log, against
/// [`PluginLimits::log_lines_per_call`] and [`PluginLimits::log_bytes_per_call`].
#[derive(Default)]
struct LogBudget {
    lines: usize,
    bytes: usize,
    /// Set once the budget is passed: the call's other lines are dropped.
    dropping: bool,
}

/// Which kind of definition a `types` import registers.
#[derive(Clone, Copy)]
enum TypeKind {
    PostType,
    Taxonomy,
    FieldGroup,
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
            return Err("subscribe in init, not while the plugin loads".to_owned());
        };
        if plugin.deactivated() {
            return Err("the plugin is deactivated".to_owned());
        }
        if !plugin.initializing.load(Ordering::SeqCst) {
            return Err("subscribe in init: hooks cannot be added elsewhere".to_owned());
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

    /// Registers the definition `json` describes, of `kind`, for the plugin: during its
    /// `init` only, with [`Origin::Plugin`] and its slug, never over a definition made in
    /// code or by another plugin.
    fn register(&mut self, kind: TypeKind, json: &str) -> Result<(), String> {
        let Some(plugin) = self.plugin.upgrade() else {
            return Err("register types in init, not while the plugin loads".to_owned());
        };
        if plugin.deactivated() {
            return Err("the plugin is deactivated".to_owned());
        }
        if !plugin.initializing.load(Ordering::SeqCst) {
            return Err("register types in init: they cannot be added elsewhere".to_owned());
        }
        let Some(registry) = &self.registry else {
            return Err("this site has no type registry".to_owned());
        };
        if plugin.definitions.fetch_add(1, Ordering::SeqCst) >= MAX_TYPE_DEFINITIONS {
            plugin.definitions.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "the plugin already registered {MAX_TYPE_DEFINITIONS} definitions, the most allowed"
            ));
        }
        let origin = Origin::Plugin(self.slug.clone());
        let registered = match kind {
            TypeKind::PostType => PostTypeDef::from_json(json, origin)
                .map_err(|e| e.to_string())
                .and_then(|def| registry.register_exclusive(def).map_err(|e| e.to_string())),
            TypeKind::Taxonomy => TaxonomyDef::from_json(json, origin)
                .map_err(|e| e.to_string())
                .and_then(|def| registry.register_exclusive(def).map_err(|e| e.to_string())),
            TypeKind::FieldGroup => FieldGroupDef::from_json(json, origin)
                .map_err(|e| e.to_string())
                .and_then(|def| registry.register_exclusive(def).map_err(|e| e.to_string())),
        };
        // A deactivation on another thread between the check above and the registration would
        // leave this definition behind.
        if registered.is_ok() && plugin.deactivated() {
            registry.remove_plugin(&self.slug);
            return Err("the plugin is deactivated".to_owned());
        }
        registered.map_err(|e| shown(&e))
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
        if self.log.dropping {
            return;
        }
        let message = clean(&message, MAX_LOG_MESSAGE_CHARS);
        if self.log.lines >= self.limits.log_lines_per_call
            || self.log.bytes.saturating_add(message.len()) > self.limits.log_bytes_per_call
        {
            self.log.dropping = true;
            log::warn!(
                target: LOG_TARGET,
                "[{}] the plugin wrote more than {} lines or {} to the log in one call; the \
                 call's other lines are dropped",
                self.slug,
                self.limits.log_lines_per_call,
                size(self.limits.log_bytes_per_call)
            );
            return;
        }
        self.log.lines += 1;
        self.log.bytes += message.len();
        log::log!(target: LOG_TARGET, level, "[{}] {message}", self.slug);
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

impl wit_types::Host for HostState {
    fn register_post_type(&mut self, json: String) -> Result<(), String> {
        self.register(TypeKind::PostType, &json)
    }

    fn register_taxonomy(&mut self, json: String) -> Result<(), String> {
        self.register(TypeKind::Taxonomy, &json)
    }

    fn register_field_group(&mut self, json: String) -> Result<(), String> {
        self.register(TypeKind::FieldGroup, &json)
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
