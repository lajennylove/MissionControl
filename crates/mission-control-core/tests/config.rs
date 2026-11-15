use mission_control_core::config::{ConfigError, load_from};
use std::collections::HashMap;
use std::path::PathBuf;

fn env_file(name: &str, contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mc-config-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".env");
    std::fs::write(&path, contents).unwrap();
    path
}

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name| map.get(name).cloned()
}

const REQUIRED: &str = "DATABASE_URL=sqlite://mc.db\nIGNITE_SECRET_KEY=abc\n";

#[test]
fn reads_values_from_env_file() {
    let path = env_file(
        "values",
        &format!("{REQUIRED}MC_DEBUG=true\nMC_UPLOAD_MAX_BYTES=1024\n"),
    );
    let config = load_from(Some(&path), lookup(&[])).unwrap();
    assert_eq!(config.database_url, "sqlite://mc.db");
    assert_eq!(config.secret_key, "abc");
    assert!(config.debug);
    assert_eq!(config.upload_max_bytes, 1024);
}

#[test]
fn applies_defaults() {
    let path = env_file("defaults", REQUIRED);
    let config = load_from(Some(&path), lookup(&[])).unwrap();
    assert!(!config.debug);
    assert_eq!(config.upload_max_bytes, 67_108_864);
}

#[test]
fn environment_overrides_env_file() {
    let path = env_file("override", REQUIRED);
    let config = load_from(Some(&path), lookup(&[("DATABASE_URL", "postgres://real")])).unwrap();
    assert_eq!(config.database_url, "postgres://real");
    assert_eq!(config.secret_key, "abc");
}

#[test]
fn missing_database_url() {
    let path = env_file("missing", "IGNITE_SECRET_KEY=abc\n");
    let error = load_from(Some(&path), lookup(&[])).err().unwrap();
    assert_eq!(error, ConfigError::Missing("DATABASE_URL"));
}

#[test]
fn missing_secret_key() {
    let path = env_file("nosecret", "DATABASE_URL=x\n");
    let error = load_from(Some(&path), lookup(&[])).err().unwrap();
    assert_eq!(error, ConfigError::Missing("IGNITE_SECRET_KEY"));
}

#[test]
fn works_without_an_env_file() {
    let config = load_from(
        None,
        lookup(&[("DATABASE_URL", "x"), ("IGNITE_SECRET_KEY", "y")]),
    )
    .unwrap();
    assert_eq!(config.secret_key, "y");
}

#[test]
fn non_numeric_upload_max_bytes_is_invalid() {
    let path = env_file("badbytes", &format!("{REQUIRED}MC_UPLOAD_MAX_BYTES=lots\n"));
    let error = load_from(Some(&path), lookup(&[])).err().unwrap();
    assert_eq!(
        error,
        ConfigError::Invalid {
            name: "MC_UPLOAD_MAX_BYTES",
            value: "lots".into()
        }
    );
}

#[test]
fn debug_accepts_boolean_spellings_case_insensitively() {
    for (text, expected) in [("TRUE", true), ("1", true), ("False", false), ("0", false)] {
        let path = env_file("debug", &format!("{REQUIRED}MC_DEBUG={text}\n"));
        assert_eq!(load_from(Some(&path), lookup(&[])).unwrap().debug, expected);
    }
}

#[test]
fn invalid_debug_is_rejected() {
    let path = env_file("baddebug", &format!("{REQUIRED}MC_DEBUG=maybe\n"));
    let error = load_from(Some(&path), lookup(&[])).err().unwrap();
    assert_eq!(
        error,
        ConfigError::Invalid {
            name: "MC_DEBUG",
            value: "maybe".into()
        }
    );
}

#[test]
fn debug_output_and_errors_hide_secrets() {
    let path = env_file(
        "redact",
        "DATABASE_URL=sqlite://topsecret\nIGNITE_SECRET_KEY=hunter2\n",
    );
    let config = load_from(Some(&path), lookup(&[])).unwrap();
    let shown = format!("{config:?}");
    assert!(!shown.contains("topsecret") && !shown.contains("hunter2"));
    let error = ConfigError::Missing("DATABASE_URL");
    assert!(error.to_string().contains("DATABASE_URL"));
    let invalid = ConfigError::Invalid {
        name: "MC_DEBUG",
        value: "maybe".into(),
    };
    assert!(invalid.to_string().contains("MC_DEBUG"));
}

#[test]
fn themes_dir_defaults_to_themes_and_can_be_set() {
    let path = env_file("themesdefault", REQUIRED);
    let config = load_from(Some(&path), lookup(&[])).unwrap();
    assert_eq!(config.themes_dir, PathBuf::from("themes"));

    let path = env_file(
        "themesset",
        &format!("{REQUIRED}MC_THEMES_DIR=/srv/site/themes\n"),
    );
    let config = load_from(Some(&path), lookup(&[])).unwrap();
    assert_eq!(config.themes_dir, PathBuf::from("/srv/site/themes"));
}

#[test]
fn empty_themes_dir_is_invalid() {
    let path = env_file("themesempty", &format!("{REQUIRED}MC_THEMES_DIR=\n"));
    let error = load_from(Some(&path), lookup(&[])).err().unwrap();
    assert_eq!(
        error,
        ConfigError::Invalid {
            name: "MC_THEMES_DIR",
            value: String::new()
        }
    );
}
