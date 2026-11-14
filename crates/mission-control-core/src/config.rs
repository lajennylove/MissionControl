//! Configuration read from the environment and a git-ignored `.env` file.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

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
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &"[redacted]")
            .field("secret_key", &"[redacted]")
            .field("debug", &self.debug)
            .field("upload_max_bytes", &self.upload_max_bytes)
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
    let file: HashMap<String, String> = match env_file {
        Some(path) => dotenvy::from_path_iter(path)
            .map(|items| items.filter_map(Result::ok).collect())
            .unwrap_or_default(),
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

    Ok(Config {
        database_url,
        secret_key,
        debug,
        upload_max_bytes,
    })
}
