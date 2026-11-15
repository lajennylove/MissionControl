//! The plugin manifest: `plugin.toml`, the equivalent of the header comment of a WordPress
//! plugin's main PHP file, plus the permissions the plugin asks for (design section 4.2).
//!
//! ```toml
//! slug = "acme-forms"                 # the plugin's folder name in WordPress
//! name = "Acme Forms"                 # Plugin Name
//! version = "1.2.0"                   # Version (semantic versioning)
//! license = "GPL-2.0-or-later"        # License (an SPDX expression is recommended)
//! license_uri = "https://www.gnu.org/licenses/gpl-2.0.html"   # License URI
//! requires = "0.1"                    # the host API it needs (Requires at least)
//! description = "Contact forms."      # Description
//! plugin_uri = "https://acme.example/forms"                   # Plugin URI
//! author = "Acme"                     # Author
//! author_uri = "https://acme.example" # Author URI
//! permissions = ["hooks", "read-content", "http"]
//! hosts = ["api.acme.example", "*.cdn.acme.example", "hooks.acme.example:8443"]
//! ```
//!
//! Keys are spelled as in `theme.toml`: `license` (Cargo's and WordPress's spelling; `licence` is
//! an unknown key) and WordPress's header names in snake case. Parsing is strict, as for
//! `theme.toml`: an unknown key, an unknown permission or a value of the wrong type is refused, so
//! a typo does not pass silently. Errors never echo a value raw: a value is named escaped and
//! shortened, so a message is safe for logs and the admin.
//!
//! `requires` is WordPress's "Requires at least", with the same meaning as `theme.toml`'s
//! `requires`: the oldest plugin API version ([`HOST_API_VERSION`], the version of the WIT
//! interface, not of Mission Control) the plugin works with. It is a version, not a requirement
//! expression: `"0.1"` means 0.1.0 or newer. Breaking changes of the API are caught when the
//! module is linked, by the WIT package version it imports, not by this field. A plugin that
//! needs a newer API still parses, so the plugins screen can list it; installing and activation
//! refuse it with [`Manifest::check_compatible`] (Ruling R106), as WordPress refuses to install
//! (since 5.5) and to activate a plugin that needs a newer WordPress.
//!
//! The licence is required but never judged: WordPress core installs a plugin under any licence,
//! and only its directory requires a GPL-compatible one. [`Manifest::license_is_known_gpl_compatible`]
//! tells the plugins screen whether to mark it.

use std::fmt;
use std::str::FromStr;

use semver::Version;
use serde::Deserialize;

/// The manifest's file name, at the root of the plugin's zip.
pub const MANIFEST_FILE: &str = "plugin.toml";

/// The largest accepted manifest, in bytes (64 KiB), as for `theme.toml`.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// The version of the plugin API this Mission Control provides: the WIT interface a plugin is
/// built against. A plugin is compatible when this is at least its `requires`. It changes with
/// the WIT interface, not with Mission Control's own version.
pub const HOST_API_VERSION: Version = Version::new(0, 1, 0);

/// The longest slug, in bytes (32).
///
/// The slug prefixes the plugin's table names (`acme_forms__entries`, Task 12.6, see
/// [`Manifest::table_prefix`]): the prefix is at most 34 bytes. PostgreSQL cuts identifiers at
/// 63 bytes and MySQL refuses names over 64, and index names are built from the table name, so
/// the rest of a table name has about 29 bytes.
pub const MAX_SLUG_LEN: usize = 32;

/// Slugs a plugin cannot take, because its REST namespace (`<slug>/v1`) would sit beside one of
/// WordPress's (`wp/v2`, `oembed/1.0`, `batch/v1`) or Mission Control's own. Every slug that
/// starts with [`RESERVED_SLUG_PREFIX`] is reserved too.
pub const RESERVED_SLUGS: [&str; 4] = ["wp", "oembed", "batch", "mission-control"];

/// The slug prefix kept for WordPress's own REST namespaces (`wp-site-health/v1`,
/// `wp-block-editor/v1`, `wp-abilities/v1` and those to come).
pub const RESERVED_SLUG_PREFIX: &str = "wp-";

/// The longest one-line text (`name`, `author`, `license`), in characters.
pub const MAX_TEXT_LEN: usize = 200;

/// The longest `description`, in characters.
pub const MAX_DESCRIPTION_LEN: usize = 4096;

/// The longest URI (`plugin_uri`, `author_uri`, `license_uri`), in characters.
pub const MAX_URI_LEN: usize = 2048;

/// The longest `version`, in bytes.
pub const MAX_VERSION_LEN: usize = 64;

/// The longest `requires`, in bytes.
pub const MAX_REQUIRES_LEN: usize = 64;

/// The most entries in `hosts`.
pub const MAX_HOSTS: usize = 64;

/// The longest value repeated in an error message, in characters.
const MAX_SHOWN: usize = 64;

/// A plugin's details and permissions, read from its `plugin.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The plugin's slug (WordPress's plugin folder name): lowercase ASCII letters, digits and
    /// single `-`, starting with a letter and ending with a letter or digit, at most
    /// [`MAX_SLUG_LEN`] bytes, not one of [`RESERVED_SLUGS`] and not starting with
    /// [`RESERVED_SLUG_PREFIX`]. It names the REST namespace
    /// ([`Manifest::rest_namespace`]) and prefixes table names ([`Manifest::table_prefix`]).
    pub slug: String,
    /// The plugin's name (`Plugin Name`), trimmed. Required.
    pub name: String,
    /// The plugin's version (`Version`), a semantic version such as `1.2.0`.
    pub version: Version,
    /// The plugin's licence (`License`), trimmed, as written. Required; any licence is accepted.
    pub license: String,
    /// Where the licence's text is (`License URI`).
    pub license_uri: Option<String>,
    /// The oldest plugin API version the plugin works with (`Requires at least`), compared with
    /// [`HOST_API_VERSION`]. Written `"0.1"`, `"0.1.2"` or as a full semantic version; missing
    /// parts are 0.
    pub requires: Version,
    /// A short description (`Description`), trimmed. It may have several lines.
    pub description: Option<String>,
    /// The plugin's web site (`Plugin URI`).
    pub plugin_uri: Option<String>,
    /// Who made the plugin (`Author`), trimmed.
    pub author: Option<String>,
    /// The author's web site (`Author URI`).
    pub author_uri: Option<String>,
    /// What the plugin may do, in the order written, each at most once.
    pub permissions: Vec<Permission>,
    /// The hosts the plugin may send HTTP requests to, in the order written. Not empty exactly
    /// when [`Permission::Http`] is granted.
    pub hosts: Vec<HostPattern>,
}

/// Something a plugin may do, from design section 4.2. The site owner sees the list when
/// activating the plugin, and the host links only the imports these allow.
///
/// The manifest spells each in kebab case, as `theme.toml`'s `supports` and Cargo's keys do;
/// browser extensions name theirs as flat words too (`storage`, `tabs`). The names are stable:
/// a plugin's manifest keeps working across releases.
///
/// Reading the plugin's own files from its zip needs no permission: every plugin may, as every
/// WordPress plugin reads its own folder and every browser extension its own package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    /// `hooks`: subscribe to actions and filters, in the plugin's `init`.
    ///
    /// A subscriber sees every payload and value of the hooks it subscribes to (post content,
    /// user data a hook carries) and may rewrite a filter's value: the new value is taken as
    /// given, not passed through kses or any other check. Hooks that decide who may do what or
    /// carry credentials (`user_has_cap`, `map_meta_cap`, `authenticate`, `auth_*`, ...) are
    /// refused to every plugin (Ruling R102; see the host's `DENIED_HOOKS`).
    Hooks,
    /// `read-content`: read posts of any type, terms, meta, users and options through the
    /// content API, with the acting user's capabilities applied.
    ReadContent,
    /// `write-content`: create, change and delete posts, terms, meta, users and options through
    /// the content API, with the acting user's capabilities applied. It does not imply
    /// `read-content`.
    WriteContent,
    /// `migrations`: ship database migrations, run on activation, with tables prefixed by the
    /// plugin's slug.
    Migrations,
    /// `register-types`: register custom post types, taxonomies and field groups.
    RegisterTypes,
    /// `admin-pages`: add admin pages and settings screens.
    AdminPages,
    /// `rest-routes`: register REST routes under `/wp-json/<slug>/v1`.
    RestRoutes,
    /// `shortcodes`: register shortcodes.
    Shortcodes,
    /// `http`: send HTTP requests to the hosts in `hosts`, and only to them.
    Http,
    /// `data-files`: store files in the plugin's own data folder.
    DataFiles,
}

impl Permission {
    /// Every permission, in the order of design section 4.2.
    pub const ALL: [Permission; 10] = [
        Permission::Hooks,
        Permission::ReadContent,
        Permission::WriteContent,
        Permission::Migrations,
        Permission::RegisterTypes,
        Permission::AdminPages,
        Permission::RestRoutes,
        Permission::Shortcodes,
        Permission::Http,
        Permission::DataFiles,
    ];

    /// The permission's name in the manifest, such as `read-content`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Permission::Hooks => "hooks",
            Permission::ReadContent => "read-content",
            Permission::WriteContent => "write-content",
            Permission::Migrations => "migrations",
            Permission::RegisterTypes => "register-types",
            Permission::AdminPages => "admin-pages",
            Permission::RestRoutes => "rest-routes",
            Permission::Shortcodes => "shortcodes",
            Permission::Http => "http",
            Permission::DataFiles => "data-files",
        }
    }

    /// What the permission grants, in a sentence for the site owner on the permissions screen.
    pub const fn description(self) -> &'static str {
        match self {
            Permission::Hooks => "Run code when the site does something, and change what it shows.",
            Permission::ReadContent => {
                "Read posts, pages, terms, users and settings, as the current user can."
            }
            Permission::WriteContent => {
                "Create, change and delete posts, pages, terms, users and settings, as the \
                 current user can."
            }
            Permission::Migrations => "Create its own database tables.",
            Permission::RegisterTypes => "Add post types, taxonomies and field groups.",
            Permission::AdminPages => "Add pages and settings screens to the admin.",
            Permission::RestRoutes => "Add REST API routes under its own namespace.",
            Permission::Shortcodes => "Add shortcodes.",
            Permission::Http => "Connect to the web sites listed below.",
            Permission::DataFiles => "Store files in its own data folder.",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Permission {
    type Err = ManifestError;

    /// Reads a permission by its manifest name; an unknown name is refused by name.
    fn from_str(name: &str) -> Result<Permission, ManifestError> {
        Permission::ALL
            .into_iter()
            .find(|permission| permission.as_str() == name)
            .ok_or_else(|| ManifestError::UnknownPermission { name: shown(name) })
    }
}

/// A host a plugin may send HTTP requests to, from the manifest's `hosts`.
///
/// Written as a host name with an optional port (`api.acme.example`, `api.acme.example:8443`),
/// optionally with a leading `*.` (`*.cdn.acme.example`) to allow every subdomain of the name
/// but not the name itself, as a Content Security Policy source does. No scheme, path, user,
/// IP address or other wildcard. The name is lowercased, has at least two labels (so neither
/// `localhost` nor `*.com`), each 1 to 63 letters, digits and `-` not at either end, and a last
/// label that is neither all digits nor `0x` and hex digits (no IPv4 address in any form); an
/// internationalized name is written in its `xn--` form.
///
/// Without a port, only the scheme's default port is allowed. A port is from 1 to 65535, and
/// `:80` and `:443` are refused: they are default ports, written as no port (so `https` on
/// port 80 or `http` on port 443 cannot be listed). The manifest names
/// only hosts and ports; which schemes a request may use (such as `https` only) is decided by
/// the HTTP import of the plugin host, not here.
///
/// A `*.` pattern on a public suffix (`*.co.uk`, `*.github.io`) allows every site under it:
/// there is no public suffix list here. The site owner sees the list on activation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostPattern {
    /// The host name, lowercase, without the `*.` of a wildcard.
    pub host: String,
    /// Whether the pattern was written `*.host`: it then allows the subdomains of `host`, at any
    /// depth, and not `host` itself.
    pub subdomains: bool,
    /// The port, or `None` for the scheme's default port.
    pub port: Option<u16>,
}

impl HostPattern {
    /// Whether a request to `host` on `port` is allowed. `host` is compared without regard to
    /// ASCII case; `port` is the URL's port, `None` for the scheme's default (callers turn an
    /// explicit default port, such as `:443` on `https`, into `None`).
    ///
    /// `host` must come from a URL parser (as the `url` crate's `Url::host_str`): with no
    /// trailing `.`, IDNs in their `xn--` form, and an IP address written as one. The labels are
    /// not checked again here, so raw text from a plugin must not be passed in.
    pub fn matches(&self, host: &str, port: Option<u16>) -> bool {
        if port != self.port {
            return false;
        }
        let host = host.to_ascii_lowercase();
        if self.subdomains {
            host.len() > self.host.len() + 1
                && host.ends_with(&self.host)
                && host.as_bytes()[host.len() - self.host.len() - 1] == b'.'
        } else {
            host == self.host
        }
    }

    /// Reads one `hosts` entry.
    fn parse(entry: &str) -> Result<HostPattern, ManifestError> {
        let invalid = |reason: &'static str| ManifestError::InvalidHost {
            host: shown(entry),
            reason,
        };
        let lower = entry.to_ascii_lowercase();
        let (name, port) = match lower.rsplit_once(':') {
            Some((name, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid(
                        "write a host name with an optional port, such as `api.example.com:8443`, \
                         without a scheme or path",
                    ));
                }
                match port.parse::<u16>() {
                    Ok(80 | 443) => {
                        return Err(invalid(
                            "`:80` and `:443` are default ports; leave the port out",
                        ));
                    }
                    Ok(port) if port != 0 => (name, Some(port)),
                    _ => return Err(invalid("the port must be from 1 to 65535")),
                }
            }
            None => (lower.as_str(), None),
        };
        let (name, subdomains) = match name.strip_prefix("*.") {
            Some(rest) => (rest, true),
            None => (name, false),
        };
        if name.contains('*') {
            return Err(invalid("only a leading `*.` is allowed as a wildcard"));
        }
        if name.len() > 253 {
            return Err(invalid("a host name is at most 253 characters"));
        }
        let labels: Vec<&str> = name.split('.').collect();
        let valid_label = |label: &&str| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        };
        if !labels.iter().all(valid_label) {
            return Err(invalid(
                "a host name is labels of 1 to 63 letters, digits and `-` (not at either end), \
                 separated by `.`, with no scheme, path or trailing `.`",
            ));
        }
        if labels.len() < 2 {
            return Err(invalid(
                "a host name needs at least two labels, such as `example.com`",
            ));
        }
        let numeric = |label: &&str| {
            label.bytes().all(|b| b.is_ascii_digit())
                || label
                    .strip_prefix("0x")
                    .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
        };
        if labels.last().is_some_and(numeric) {
            return Err(invalid("an IP address is not allowed; use a host name"));
        }
        Ok(HostPattern {
            host: name.to_owned(),
            subdomains,
            port,
        })
    }
}

impl fmt::Display for HostPattern {
    /// The pattern as written in the manifest, lowercased.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.subdomains {
            f.write_str("*.")?;
        }
        f.write_str(&self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// Why a manifest was refused, or why a plugin cannot be activated. Every value it holds that
/// came from the manifest is escaped and shortened to 64 characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The manifest is over [`MAX_MANIFEST_BYTES`].
    TooLarge {
        /// Its size, in bytes.
        len: usize,
    },
    /// The manifest's bytes are not UTF-8.
    NotUtf8,
    /// The manifest is not valid TOML, has an unknown key, or a value of the wrong type.
    InvalidToml {
        /// What is wrong and where (line and column). It names a key only when it is a short
        /// slug, and never echoes a value.
        message: String,
    },
    /// A required field is missing, or empty once trimmed.
    Missing {
        /// The key: `slug`, `name`, `version`, `license` or `requires`.
        field: &'static str,
    },
    /// The slug breaks the slug rules.
    InvalidSlug {
        /// The slug, escaped.
        slug: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// The slug is one of [`RESERVED_SLUGS`] or starts with [`RESERVED_SLUG_PREFIX`].
    ReservedSlug {
        /// The slug.
        slug: String,
    },
    /// A text field is longer than its limit.
    TooLong {
        /// The key.
        field: &'static str,
        /// The limit, in characters.
        max: usize,
    },
    /// A one-line text field has a line break or another control character, or `description`
    /// has a control character other than a line break or tab; or either has an invisible
    /// format character (zero-width spaces and joiners, direction marks and overrides, word
    /// joiners, the byte order mark), which could hide or reorder what the owner reads.
    ControlCharacter {
        /// The key.
        field: &'static str,
    },
    /// A URI is not an `http` or `https` URI, or has a space or control character in it.
    InvalidUri {
        /// The key.
        field: &'static str,
    },
    /// `version` is not a semantic version.
    InvalidVersion {
        /// The value, escaped.
        value: String,
    },
    /// `requires` is not a version (`0.1`, `0.1.2` or a full semantic version).
    InvalidRequires {
        /// The value, escaped.
        value: String,
    },
    /// `permissions` names a permission that does not exist.
    UnknownPermission {
        /// The name as written, escaped.
        name: String,
    },
    /// `permissions` names a permission twice.
    DuplicatePermission {
        /// The permission.
        permission: Permission,
    },
    /// A `hosts` entry is not a valid host pattern.
    InvalidHost {
        /// The entry, escaped.
        host: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// `hosts` names a host twice (compared lowercased).
    DuplicateHost {
        /// The host pattern, normalized.
        host: String,
    },
    /// `hosts` has more than [`MAX_HOSTS`] entries.
    TooManyHosts {
        /// How many it has.
        count: usize,
    },
    /// `hosts` is not empty, but `permissions` does not grant `http`.
    HostsWithoutHttp,
    /// `permissions` grants `http`, but `hosts` is empty.
    HttpWithoutHosts,
    /// The plugin's `requires` is newer than this host's plugin API version.
    Incompatible {
        /// The oldest plugin API version the plugin works with.
        requires: Version,
        /// The host's plugin API version, [`HOST_API_VERSION`].
        host: Version,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::TooLarge { len } => write!(
                f,
                "The plugin manifest is {len} bytes, over the {MAX_MANIFEST_BYTES}-byte limit. \
                 Keep only the plugin's details in {MANIFEST_FILE}."
            ),
            ManifestError::NotUtf8 => {
                write!(f, "{MANIFEST_FILE} is not UTF-8 text. Save it as UTF-8.")
            }
            ManifestError::InvalidToml { message } => {
                write!(f, "{MANIFEST_FILE} is not valid: {message}.")
            }
            ManifestError::Missing { field } => write!(
                f,
                "{MANIFEST_FILE} has no `{field}`, or it is empty. Add `{field} = \"...\"`."
            ),
            ManifestError::InvalidSlug { slug, reason } => {
                write!(f, "The plugin slug `{slug}` is not valid: {reason}.")
            }
            ManifestError::ReservedSlug { slug } => write!(
                f,
                "The plugin slug `{slug}` is reserved for WordPress's or Mission Control's own \
                 REST namespace. Choose another slug."
            ),
            ManifestError::TooLong { field, max } => {
                write!(f, "`{field}` is longer than {max} characters.")
            }
            ManifestError::ControlCharacter { field } => write!(
                f,
                "`{field}` has a line break, a control character or an invisible format \
                 character in it."
            ),
            ManifestError::InvalidUri { field } => write!(
                f,
                "`{field}` is not a web address. Write an `https://` or `http://` address with no \
                 spaces."
            ),
            ManifestError::InvalidVersion { value } => write!(
                f,
                "`version` `{value}` is not a semantic version. Write it as `1.0.0`."
            ),
            ManifestError::InvalidRequires { value } => write!(
                f,
                "`requires` `{value}` is not a version. Write the oldest plugin API version the \
                 plugin works with, such as `\"{}.{}\"`.",
                HOST_API_VERSION.major, HOST_API_VERSION.minor
            ),
            ManifestError::UnknownPermission { name } => {
                let known: Vec<&str> = Permission::ALL.iter().map(|p| p.as_str()).collect();
                write!(
                    f,
                    "The permission `{name}` does not exist. The permissions are: {}.",
                    known.join(", ")
                )
            }
            ManifestError::DuplicatePermission { permission } => {
                write!(f, "The permission `{permission}` is listed twice.")
            }
            ManifestError::InvalidHost { host, reason } => {
                write!(f, "The host `{host}` in `hosts` is not valid: {reason}.")
            }
            ManifestError::DuplicateHost { host } => {
                write!(f, "The host `{host}` is listed twice in `hosts`.")
            }
            ManifestError::TooManyHosts { count } => write!(
                f,
                "`hosts` has {count} entries, over the limit of {MAX_HOSTS}. Use a `*.` pattern \
                 for many subdomains."
            ),
            ManifestError::HostsWithoutHttp => f.write_str(
                "`hosts` lists hosts, but `permissions` does not have `http`. Add `http` to \
                 `permissions`, or remove `hosts`.",
            ),
            ManifestError::HttpWithoutHosts => f.write_str(
                "`permissions` has `http`, but `hosts` is empty. List the hosts the plugin \
                 connects to in `hosts`.",
            ),
            ManifestError::Incompatible { requires, host } => write!(
                f,
                "This plugin does not work with this version of Mission Control: it requires \
                 plugin API {requires} or newer, and this site provides plugin API {host}."
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// The manifest as written. Every field is optional here so a missing one gets its own error.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    slug: Option<String>,
    name: Option<String>,
    version: Option<String>,
    license: Option<String>,
    license_uri: Option<String>,
    requires: Option<String>,
    description: Option<String>,
    plugin_uri: Option<String>,
    author: Option<String>,
    author_uri: Option<String>,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    hosts: Vec<String>,
}

impl Manifest {
    /// Parses and checks a manifest read as bytes (from the plugin's zip): the size is checked
    /// against [`MAX_MANIFEST_BYTES`] before the bytes are decoded as UTF-8, then
    /// [`Manifest::parse`]. The caller still bounds how much it reads from the zip.
    pub fn parse_bytes(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge { len: bytes.len() });
        }
        let text = std::str::from_utf8(bytes).map_err(|_| ManifestError::NotUtf8)?;
        Manifest::parse(text)
    }

    /// Parses and checks a manifest's text. It does not check `requires` against the host: see
    /// [`Manifest::check_compatible`].
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge { len: text.len() });
        }
        let raw: RawManifest =
            toml::from_str(text).map_err(|error| ManifestError::InvalidToml {
                message: crate::theme::describe_toml_error(&error, text),
            })?;

        let slug = required(raw.slug, "slug")?;
        check_slug(&slug)?;
        let name = one_line(required(raw.name, "name")?, "name")?;
        let version = required(raw.version, "version")?;
        let version = (version.len() <= MAX_VERSION_LEN)
            .then(|| Version::parse(&version).ok())
            .flatten()
            .ok_or_else(|| ManifestError::InvalidVersion {
                value: shown(&version),
            })?;
        let license = one_line(required(raw.license, "license")?, "license")?;
        let requires = required(raw.requires, "requires")?;
        let requires = (requires.len() <= MAX_REQUIRES_LEN)
            .then(|| minimum_version(&requires))
            .flatten()
            .ok_or_else(|| ManifestError::InvalidRequires {
                value: shown(&requires),
            })?;
        let description = optional(raw.description)
            .map(|text| multi_line(text, "description"))
            .transpose()?;
        let author = optional(raw.author)
            .map(|text| one_line(text, "author"))
            .transpose()?;
        let license_uri = uri(raw.license_uri, "license_uri")?;
        let plugin_uri = uri(raw.plugin_uri, "plugin_uri")?;
        let author_uri = uri(raw.author_uri, "author_uri")?;

        let mut permissions = Vec::new();
        for name in &raw.permissions {
            let permission = name.parse::<Permission>()?;
            if permissions.contains(&permission) {
                return Err(ManifestError::DuplicatePermission { permission });
            }
            permissions.push(permission);
        }

        if raw.hosts.len() > MAX_HOSTS {
            return Err(ManifestError::TooManyHosts {
                count: raw.hosts.len(),
            });
        }
        let mut hosts: Vec<HostPattern> = Vec::new();
        for entry in &raw.hosts {
            let pattern = HostPattern::parse(entry)?;
            if hosts.contains(&pattern) {
                return Err(ManifestError::DuplicateHost {
                    host: pattern.to_string(),
                });
            }
            hosts.push(pattern);
        }
        match (permissions.contains(&Permission::Http), hosts.is_empty()) {
            (false, false) => return Err(ManifestError::HostsWithoutHttp),
            (true, true) => return Err(ManifestError::HttpWithoutHosts),
            _ => {}
        }

        Ok(Manifest {
            slug,
            name,
            version,
            license,
            license_uri,
            requires,
            description,
            plugin_uri,
            author,
            author_uri,
            permissions,
            hosts,
        })
    }

    /// Whether the manifest grants `permission`.
    pub fn allows(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }

    /// Whether the plugin may send an HTTP request to `host` on `port` (see
    /// [`HostPattern::matches`]). Always false without [`Permission::Http`].
    pub fn allows_host(&self, host: &str, port: Option<u16>) -> bool {
        self.allows(Permission::Http) && self.hosts.iter().any(|p| p.matches(host, port))
    }

    /// Whether the plugin works with this host's plugin API: [`HOST_API_VERSION`] is at least
    /// `requires`, as WordPress compares `Requires at least` with its own version. Build
    /// metadata (`0.1.0+build`) is ignored, as semver precedence ignores it.
    pub fn is_compatible(&self) -> bool {
        HOST_API_VERSION.cmp_precedence(&self.requires).is_ge()
    }

    /// [`Manifest::is_compatible`], as an error that names both versions, for activation.
    pub fn check_compatible(&self) -> Result<(), ManifestError> {
        if self.is_compatible() {
            Ok(())
        } else {
            Err(ManifestError::Incompatible {
                requires: self.requires.clone(),
                host: HOST_API_VERSION,
            })
        }
    }

    /// Whether `license` is known to be GPL-compatible, for the plugins screen to mark. False
    /// means "not known", not "incompatible": the licence is shown either way and never stops a
    /// plugin from being installed.
    ///
    /// Two forms are known: one of WordPress's common spellings ([`WORDPRESS_GPL_SPELLINGS`],
    /// such as `GPLv2 or later`, compared without regard to case, spaces, `-` and `.`), or an
    /// SPDX expression of ids joined by `OR` and `AND` (`AND` binds tighter), each compared
    /// without regard to case against [`GPL_COMPATIBLE_LICENSES`]. Parentheses, `WITH` and other
    /// free text are not read, so they give false.
    pub fn license_is_known_gpl_compatible(&self) -> bool {
        let spelling: String = self
            .license
            .chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '.'))
            .flat_map(char::to_lowercase)
            .collect();
        if WORDPRESS_GPL_SPELLINGS.contains(&spelling.as_str()) {
            return true;
        }
        let tokens: Vec<&str> = self.license.split_whitespace().collect();
        tokens.split(|token| *token == "OR").any(|alternative| {
            !alternative.is_empty()
                && alternative.split(|token| *token == "AND").all(|ids| {
                    matches!(ids, [id] if GPL_COMPATIBLE_LICENSES
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(id)))
                })
        })
    }

    /// The plugin's REST namespace, `<slug>/v1`, served under `/wp-json/`.
    pub fn rest_namespace(&self) -> String {
        format!("{}/v1", self.slug)
    }

    /// The prefix of the plugin's table names: the slug with `-` turned into `_`, then `__`
    /// (`acme-forms` gives `acme_forms__`), at most 34 bytes.
    ///
    /// A slug has no `_` and no `--`, and ends with a letter or digit, so the first `__` in a
    /// table name is where its prefix ends: two plugins can never make the same table name
    /// (`acme__forms_x` and `acme_forms__x` differ), and no core table, which has no `__`, can
    /// be made. Task 12.6 keeps the whole name within MySQL's 64 and PostgreSQL's 63 bytes.
    pub fn table_prefix(&self) -> String {
        format!("{}__", self.slug.replace('-', "_"))
    }
}

/// SPDX ids known to be GPL-compatible, from the Free Software Foundation's list. Some (such as
/// `Apache-2.0`, `LGPL-3.0-only` and the `AGPL-3.0` ids) are compatible with GPL version 3
/// only; Mission Control is `GPL-2.0-or-later`, so they combine with it under version 3.
pub const GPL_COMPATIBLE_LICENSES: [&str; 33] = [
    "GPL-2.0-only",
    "GPL-2.0-or-later",
    "GPL-2.0",
    "GPL-2.0+",
    "GPL-3.0-only",
    "GPL-3.0-or-later",
    "GPL-3.0",
    "GPL-3.0+",
    "LGPL-2.1-only",
    "LGPL-2.1-or-later",
    "LGPL-2.1",
    "LGPL-2.1+",
    "LGPL-3.0-only",
    "LGPL-3.0-or-later",
    "LGPL-3.0",
    "LGPL-3.0+",
    "AGPL-3.0-only",
    "AGPL-3.0-or-later",
    "MIT",
    "MIT-0",
    "X11",
    "ISC",
    "0BSD",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "Apache-2.0",
    "MPL-2.0",
    "Artistic-2.0",
    "BSL-1.0",
    "Zlib",
    "Unlicense",
    "CC0-1.0",
    "WTFPL",
];

/// WordPress plugins' common ways of writing a GPL licence in the `License` header (`GPLv2 or
/// later`, `GPL v2 or later`, `GPLv2`, `GPL2`, `GPL-2.0+`, `GPLv3`, ...), lowercased and without
/// spaces, `-` and `.`, as [`Manifest::license_is_known_gpl_compatible`] compares them.
pub const WORDPRESS_GPL_SPELLINGS: [&str; 34] = [
    "gpl",
    "gpl2",
    "gpl2+",
    "gplv2",
    "gplv2+",
    "gpl2orlater",
    "gplv2orlater",
    "gplversion2",
    "gplversion2orlater",
    "gpl20",
    "gpl20+",
    "gpl20orlater",
    "gpl3",
    "gpl3+",
    "gplv3",
    "gplv3+",
    "gpl3orlater",
    "gplv3orlater",
    "gplversion3",
    "gplversion3orlater",
    "gpl30",
    "gpl30+",
    "gpl30orlater",
    "gplv20",
    "gplv20orlater",
    "gplv30",
    "gplv30orlater",
    "gnugeneralpubliclicense",
    "gnugeneralpubliclicensev2",
    "gnugeneralpubliclicensev2orlater",
    "gnugeneralpubliclicensev3",
    "gnugeneralpubliclicensev3orlater",
    "gnugplv2",
    "gnugplv3",
];

/// `value` trimmed, or `Missing { field }` when it is absent or blank.
fn required(value: Option<String>, field: &'static str) -> Result<String, ManifestError> {
    optional(value).ok_or(ManifestError::Missing { field })
}

/// `value` trimmed, or `None` when it is absent or blank.
fn optional(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Whether `c` is an invisible format character (Unicode category Cf) that can hide or reorder
/// text: the soft hyphen, the Arabic letter mark, the Mongolian vowel separator, zero-width
/// spaces, joiners and direction marks (U+200B to U+200F), direction embeddings and overrides
/// (U+202A to U+202E), word joiners and invisible operators (U+2060 to U+2064), direction
/// isolates (U+2066 to U+2069), the byte order mark (U+FEFF), the interlinear annotation
/// characters (U+FFF9 to U+FFFB) and the tag characters (U+E0000 to U+E007F).
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{61C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// `text` if it is at most [`MAX_TEXT_LEN`] characters with no control or format character.
fn one_line(text: String, field: &'static str) -> Result<String, ManifestError> {
    if text.chars().count() > MAX_TEXT_LEN {
        return Err(ManifestError::TooLong {
            field,
            max: MAX_TEXT_LEN,
        });
    }
    // U+2028 and U+2029 are not control characters but render as line breaks.
    if text
        .chars()
        .any(|c| c.is_control() || is_format_char(c) || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err(ManifestError::ControlCharacter { field });
    }
    Ok(text)
}

/// `text` if it is at most [`MAX_DESCRIPTION_LEN`] characters with no control character but
/// line breaks and tabs, and no format character.
fn multi_line(text: String, field: &'static str) -> Result<String, ManifestError> {
    if text.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(ManifestError::TooLong {
            field,
            max: MAX_DESCRIPTION_LEN,
        });
    }
    if text
        .chars()
        .any(|c| (c.is_control() && !matches!(c, '\n' | '\r' | '\t')) || is_format_char(c))
    {
        return Err(ManifestError::ControlCharacter { field });
    }
    Ok(text)
}

/// The URI in `value`, trimmed, if it is an `http` or `https` URI with something after the
/// scheme, no space, control or format character, at most [`MAX_URI_LEN`] characters.
fn uri(value: Option<String>, field: &'static str) -> Result<Option<String>, ManifestError> {
    let Some(text) = optional(value) else {
        return Ok(None);
    };
    if text.chars().count() > MAX_URI_LEN {
        return Err(ManifestError::TooLong {
            field,
            max: MAX_URI_LEN,
        });
    }
    let lower = text.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    match rest {
        Some(rest)
            if !rest.is_empty()
                && !text
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || is_format_char(c)) =>
        {
            Ok(Some(text))
        }
        _ => Err(ManifestError::InvalidUri { field }),
    }
}

/// `requires` as a version: a full semantic version, or `MAJOR` or `MAJOR.MINOR` with the
/// missing parts 0, each part digits without a leading zero. `*`, ranges and operators are
/// refused.
fn minimum_version(text: &str) -> Option<Version> {
    if let Ok(version) = Version::parse(text) {
        return Some(version);
    }
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() > 2 {
        return None;
    }
    let mut numbers = [0u64; 2];
    for (number, part) in numbers.iter_mut().zip(&parts) {
        let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if !digits || (part.len() > 1 && part.starts_with('0')) {
            return None;
        }
        *number = part.parse().ok()?;
    }
    Some(Version::new(numbers[0], numbers[1], 0))
}

/// Checks the slug rules of [`Manifest::slug`].
pub(super) fn check_slug(slug: &str) -> Result<(), ManifestError> {
    let invalid = |reason: &'static str| ManifestError::InvalidSlug {
        slug: shown(slug),
        reason,
    };
    if slug.len() > MAX_SLUG_LEN {
        return Err(invalid("a slug is at most 32 characters"));
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid("use lowercase letters, digits and `-` only"));
    }
    if !slug.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(invalid("a slug starts with a letter"));
    }
    if slug.ends_with('-') || slug.contains("--") {
        return Err(invalid(
            "a slug does not end with `-` or have two `-` in a row",
        ));
    }
    if RESERVED_SLUGS.contains(&slug) || slug.starts_with(RESERVED_SLUG_PREFIX) {
        return Err(ManifestError::ReservedSlug {
            slug: slug.to_owned(),
        });
    }
    Ok(())
}

/// `value` safe to repeat in a message: escaped as a Rust string (no control characters) and cut
/// to [`MAX_SHOWN`] characters, with `...` when cut.
fn shown(value: &str) -> String {
    let escaped: String = value.escape_debug().collect();
    if escaped.chars().count() > MAX_SHOWN {
        let mut cut: String = escaped.chars().take(MAX_SHOWN).collect();
        cut.push_str("...");
        cut
    } else {
        escaped
    }
}
