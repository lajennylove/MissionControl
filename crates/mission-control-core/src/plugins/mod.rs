//! Plugins: the manifest a plugin ships (`plugin.toml`), the permissions it asks for, and the
//! host that runs it.
//!
//! A plugin is a zip with a manifest and a WebAssembly component (design section 4). This
//! module reads and checks the manifest, and runs the component in wasmtime's sandbox
//! ([`PluginHost`]), linking only the WIT interfaces (`wit/mission-control.wit`) its
//! permissions grant ([`Manifest::allows`]). Limits, activation and plugin migrations build on
//! it (Tasks 12.3 to 12.6). [`Plugins`] installs plugins from their zips, activates and
//! deactivates them, loads the active ones at start and deactivates one that fails, telling the
//! owner (Task 12.4).
//!
//! ```ignore
//! use mission_control_core::plugins::{Manifest, Permission};
//!
//! let manifest = Manifest::parse(&text)?;
//! manifest.check_compatible()?;          // before activation
//! if manifest.allows(Permission::Http) { /* link the HTTP import */ }
//!
//! let plugin = host.load(manifest, &wasm)?;  // refuses an import the manifest does not grant
//! with_current_user(owner, || plugin.activate())?;
//! ```

mod content;
mod host;
mod lifecycle;
mod manifest;

pub use content::{MAX_META_KEY_BYTES, MAX_POST_FIELD_BYTES};
pub use host::{
    DENIED_HOOK_PREFIXES, DENIED_HOOKS, HostError, INTERFACES, MAX_COMPONENT_BYTES,
    MAX_HOOK_NAME_BYTES, MAX_JSON_BYTES, MAX_LOG_MESSAGE_CHARS, MAX_NESTED_PLUGIN_CALLS,
    MAX_PLUGIN_MESSAGE_CHARS, MAX_SUBSCRIPTIONS, MAX_TYPE_DEFINITIONS, MAX_WASM_STACK_BYTES,
    PACKAGE, Plugin, PluginHost, PluginLimits, is_denied_hook, with_current_user,
};
pub use lifecycle::{
    ACTIVE_PLUGINS_OPTION, InstallError, InstallLimits, LifecycleError, MAX_ACTIVE_PLUGINS,
    MAX_NOTICE_CHARS, MAX_NOTICES, MAX_ZIP_PATH_BYTES, MAX_ZIP_PATH_DEPTH, MODULE_FILE,
    NOTICES_OPTION, Notice, Plugins,
};

pub use manifest::{
    GPL_COMPATIBLE_LICENSES, HOST_API_VERSION, HostPattern, MANIFEST_FILE, MAX_DESCRIPTION_LEN,
    MAX_HOSTS, MAX_MANIFEST_BYTES, MAX_REQUIRES_LEN, MAX_SLUG_LEN, MAX_TEXT_LEN, MAX_URI_LEN,
    MAX_VERSION_LEN, Manifest, ManifestError, Permission, RESERVED_SLUG_PREFIX, RESERVED_SLUGS,
    WORDPRESS_GPL_SPELLINGS,
};
