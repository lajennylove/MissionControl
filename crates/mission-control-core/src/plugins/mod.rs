//! Plugins: the manifest a plugin ships (`plugin.toml`) and the permissions it asks for.
//!
//! A plugin is a zip with a manifest and a WebAssembly module (design section 4). This module
//! reads and checks the manifest; the WASM host, its limits, activation and plugin migrations
//! build on it (Tasks 12.2 to 12.6) and gate what a plugin may do by [`Manifest::allows`] and
//! [`Manifest::allows_host`].
//!
//! ```ignore
//! use mission_control_core::plugins::{Manifest, Permission};
//!
//! let manifest = Manifest::parse(&text)?;
//! manifest.check_compatible()?;          // before activation
//! if manifest.allows(Permission::Http) { /* link the HTTP import */ }
//! ```

mod manifest;

pub use manifest::{
    GPL_COMPATIBLE_LICENSES, HOST_API_VERSION, HostPattern, MANIFEST_FILE, MAX_DESCRIPTION_LEN,
    MAX_HOSTS, MAX_MANIFEST_BYTES, MAX_REQUIRES_LEN, MAX_SLUG_LEN, MAX_TEXT_LEN, MAX_URI_LEN,
    MAX_VERSION_LEN, Manifest, ManifestError, Permission, RESERVED_SLUG_PREFIX, RESERVED_SLUGS,
    WORDPRESS_GPL_SPELLINGS,
};
