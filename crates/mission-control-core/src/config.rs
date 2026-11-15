//! Configuration read from the environment and a git-ignored `.env` file.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// The application configuration, the equivalent of `wp-config.php`.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// The database connection URL (`DATABASE_URL`). A secret.
    pub database_url: String,
    /// The key that signs sessions and cookies (`IGNITE_SECRET_KEY`). A secret.
    pub secret_key: String,
    /// Whether debug mode is on (`MC_DEBUG`).
    pub debug: bool,
    /// The largest accepted upload, in bytes (`MC_UPLOAD_MAX_BYTES`).
    pub upload_max_bytes: u64,
    /// The folder that holds the installed themes, one folder per theme (`MC_THEMES_DIR`), as
    /// WordPress's `wp-content/themes`. Default `themes`, relative to the working directory.
    pub themes_dir: PathBuf,
    /// The folder uploads are stored in, in `YYYY/MM` folders (`MC_UPLOADS_DIR`), as WordPress's
    /// `wp-content/uploads`, and served at `/wp-content/uploads/`. Default `uploads`, relative to
    /// the working directory.
    pub uploads_dir: PathBuf,
    /// The kind of environment the site runs in (`MC_ENV`), as WordPress's
    /// `wp_get_environment_type`. Default `production`.
    pub environment: Environment,
    /// The proxies whose `X-Forwarded-Proto` header is believed (`MC_TRUSTED_PROXIES`), as
    /// Laravel's `TrustProxies`. Default none.
    pub trusted_proxies: TrustedProxies,
}

/// The kind of environment a site runs in, WordPress's environment types (`WP_ENVIRONMENT_TYPE`).
///
/// Only [`Environment::Local`] changes behaviour for now: application passwords work over plain
/// HTTP there, as WordPress's `wp_is_application_passwords_supported` allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Environment {
    /// A developer's own machine (`local`).
    Local,
    /// A shared development site (`development`).
    Development,
    /// A pre-release copy of the live site (`staging`).
    Staging,
    /// The live site (`production`), the default.
    #[default]
    Production,
}

impl Environment {
    /// The name, as `MC_ENV` and WordPress's `wp_get_environment_type` spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    /// The environment named `name`, ignoring ASCII case; `None` for any other name.
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::Local,
            Self::Development,
            Self::Staging,
            Self::Production,
        ]
        .into_iter()
        .find(|env| env.as_str().eq_ignore_ascii_case(name.trim()))
    }
}

/// The reverse proxies trusted to say how a client reached the site, as Laravel's
/// `TrustProxies` middleware lists them: a request is taken to have arrived over HTTPS when its
/// `X-Forwarded-Proto` says so and it comes straight from one of them. Untrusted by default, so
/// a client cannot claim HTTPS by sending the header itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TrustedProxies {
    /// No proxy is trusted (`MC_TRUSTED_PROXIES` unset or empty).
    #[default]
    None,
    /// Every peer is trusted (`*`): only when nothing reaches the app but through the proxy.
    All,
    /// The peers with these addresses (`127.0.0.1, ::1`).
    Addresses(Vec<IpAddr>),
}

impl TrustedProxies {
    /// Whether a request from the peer at `peer` is believed. An unknown peer is trusted only
    /// when every peer is.
    pub fn trusts(&self, peer: Option<IpAddr>) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Addresses(addresses) => {
                peer.is_some_and(|peer| addresses.contains(&peer.to_canonical()))
            }
        }
    }

    /// The proxies in `value`: empty for none, `*` for all, or IP addresses separated by commas.
    /// `None` when an entry is not an IP address.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Some(Self::None);
        }
        if value == "*" {
            return Some(Self::All);
        }
        value
            .split(',')
            .map(|entry| {
                entry
                    .trim()
                    .parse::<IpAddr>()
                    .ok()
                    .map(|ip| ip.to_canonical())
            })
            .collect::<Option<Vec<_>>>()
            .map(Self::Addresses)
    }
}

/// An error raised while reading the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required variable is not set.
    Missing(&'static str),
    /// A variable is set to a value that cannot be used.
    Invalid {
        /// The variable name.
        name: &'static str,
        /// The offending value.
        value: String,
    },
    /// The `.env` file has a line that cannot be parsed (an unclosed quote, a bad escape). The
    /// line's text is not included: it may hold a secret.
    EnvSyntax {
        /// The `.env` file.
        path: PathBuf,
        /// The 1-based line number where the bad line starts, when it could be found.
        line: Option<usize>,
    },
    /// The `.env` file exists but cannot be read.
    EnvUnreadable {
        /// The `.env` file.
        path: PathBuf,
    },
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
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

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(name) => write!(f, "{name} is not set"),
            ConfigError::Invalid { name, value } => {
                write!(f, "{name} has an invalid value: {value:?}")
            }
            ConfigError::EnvSyntax {
                path,
                line: Some(line),
            } => write!(
                f,
                "{} has a line Mission Control cannot read (line {line}). Check it for an \
                 unclosed quote or a stray backslash.",
                path.display()
            ),
            ConfigError::EnvSyntax { path, line: None } => write!(
                f,
                "{} has a line Mission Control cannot read. Check it for an unclosed quote or \
                 a stray backslash.",
                path.display()
            ),
            ConfigError::EnvUnreadable { path } => write!(
                f,
                "{} cannot be read. Check that the user that runs Mission Control may read it.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Loads the configuration from `.env` in the working directory and the process environment.
///
/// Variables set in the process environment override the ones in `.env`. A missing `.env`
/// file is not an error.
pub fn load() -> Result<Config, ConfigError> {
    load_from(Some(Path::new(".env")), |name| std::env::var(name).ok())
}

/// Loads the configuration from an optional `.env` file and an environment lookup.
///
/// The lookup is asked first, so it overrides the file. The process environment is never
/// modified, which keeps tests independent of each other.
pub fn load_from(
    env_file: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let file = match env_file {
        Some(path) => read_env_file(path)?,
        None => HashMap::new(),
    };
    let get = |name: &str| lookup(name).or_else(|| file.get(name).cloned());
    let required = |name: &'static str| get(name).ok_or(ConfigError::Missing(name));

    let database_url = required("DATABASE_URL")?;
    let secret_key = required("IGNITE_SECRET_KEY")?;
    let debug = match get("MC_DEBUG") {
        None => false,
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => {
                return Err(ConfigError::Invalid {
                    name: "MC_DEBUG",
                    value,
                });
            }
        },
    };
    let upload_max_bytes = match get("MC_UPLOAD_MAX_BYTES") {
        None => 67_108_864,
        Some(value) => value.parse().map_err(|_| ConfigError::Invalid {
            name: "MC_UPLOAD_MAX_BYTES",
            value,
        })?,
    };
    let themes_dir = match get("MC_THEMES_DIR") {
        None => PathBuf::from("themes"),
        Some(value) if value.trim().is_empty() => {
            return Err(ConfigError::Invalid {
                name: "MC_THEMES_DIR",
                value,
            });
        }
        Some(value) => PathBuf::from(value),
    };
    let uploads_dir = match get("MC_UPLOADS_DIR") {
        None => PathBuf::from("uploads"),
        Some(value) if value.trim().is_empty() => {
            return Err(ConfigError::Invalid {
                name: "MC_UPLOADS_DIR",
                value,
            });
        }
        Some(value) => PathBuf::from(value),
    };
    let environment = match get("MC_ENV") {
        None => Environment::Production,
        Some(value) => Environment::parse(&value).ok_or(ConfigError::Invalid {
            name: "MC_ENV",
            value,
        })?,
    };
    let trusted_proxies = match get("MC_TRUSTED_PROXIES") {
        None => TrustedProxies::None,
        Some(value) => TrustedProxies::parse(&value).ok_or(ConfigError::Invalid {
            name: "MC_TRUSTED_PROXIES",
            value,
        })?,
    };

    Ok(Config {
        database_url,
        secret_key,
        debug,
        upload_max_bytes,
        themes_dir,
        uploads_dir,
        environment,
        trusted_proxies,
    })
}

/// Reads the variables of a `.env` file. A missing file is empty; an unreadable or unparseable
/// one is an error that names the file and the line, never the line's text.
fn read_env_file(path: &Path) -> Result<HashMap<String, String>, ConfigError> {
    let unreadable = || ConfigError::EnvUnreadable {
        path: path.to_path_buf(),
    };
    let items = match dotenvy::from_path_iter(path) {
        Ok(items) => items,
        Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::new());
        }
        Err(_) => return Err(unreadable()),
    };
    let mut vars = HashMap::new();
    for item in items {
        match item {
            Ok((name, value)) => {
                vars.insert(name, value);
            }
            Err(dotenvy::Error::LineParse(text, _)) => {
                // The error holds the text of the bad line, which the file's own text locates.
                let line = std::fs::read_to_string(path).ok().and_then(|all| {
                    all.find(&text)
                        .map(|at| all[..at].matches('\n').count() + 1)
                });
                return Err(ConfigError::EnvSyntax {
                    path: path.to_path_buf(),
                    line,
                });
            }
            Err(_) => return Err(unreadable()),
        }
    }
    Ok(vars)
}
