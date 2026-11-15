use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mission_control_core::config::{self, Environment, TrustedProxies};
use mission_control_core::install::env_file::{
    EnvFile, EnvFileError, generate_secret_key, write_new,
};
use std::path::PathBuf;

fn tricky() -> EnvFile {
    let mut env = EnvFile::new(
        "mysql://us er:p#ss \"w'rd$HOME ${X} \\n\\\\@host/db name?x=1 #y",
        generate_secret_key().unwrap(),
    );
    env.debug = Some(true);
    env.upload_max_bytes = Some(1234);
    env.themes_dir = Some(PathBuf::from("my themes/#1 'x' \"y\" $Z"));
    env.uploads_dir = Some(PathBuf::from("up loads"));
    env.plugins_dir = Some(PathBuf::from("plug ins/'x'"));
    env.environment = Some(Environment::Staging);
    env.trusted_proxies = Some(TrustedProxies::Addresses(vec![
        "127.0.0.1".parse().unwrap(),
        "::1".parse().unwrap(),
    ]));
    env
}

#[cfg(unix)]
#[test]
fn file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = write_new(dir.path(), &tricky()).unwrap();
    assert_eq!(path, dir.path().join(".env"));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    // No temporary file is left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn key_is_32_random_bytes_in_standard_base64() {
    let a = generate_secret_key().unwrap();
    let b = generate_secret_key().unwrap();
    assert_ne!(a, b);
    assert_eq!(STANDARD.decode(&a).unwrap().len(), 32);
    assert_eq!(STANDARD.decode(&b).unwrap().len(), 32);
    assert_eq!(a.len(), 44);
}

#[test]
fn file_round_trips_through_config() {
    let dir = tempfile::tempdir().unwrap();
    let env = tricky();
    let path = write_new(dir.path(), &env).unwrap();
    let config = config::load_from(Some(&path), |_| None).unwrap();
    assert_eq!(config.database_url, env.database_url);
    assert_eq!(config.secret_key, env.secret_key);
    assert!(config.debug);
    assert_eq!(config.upload_max_bytes, 1234);
    assert_eq!(Some(config.themes_dir), env.themes_dir);
    assert_eq!(Some(config.uploads_dir), env.uploads_dir);
    assert_eq!(Some(config.plugins_dir), env.plugins_dir);
    assert_eq!(Some(config.environment), env.environment);
    assert_eq!(Some(config.trusted_proxies), env.trusted_proxies);
}

#[test]
fn minimal_file_leaves_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_new(dir.path(), &EnvFile::new("sqlite://a.db", "k")).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("MC_"));
    let config = config::load_from(Some(&path), |_| None).unwrap();
    assert!(!config.debug);
    assert_eq!(config.environment, Environment::Production);
}

#[test]
fn awkward_values_round_trip() {
    for value in [
        "'",
        "''",
        "\\",
        "$",
        "${HOME}",
        "a#b",
        " lead and trail ",
        "é€😀",
        "\\'$\"",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = write_new(dir.path(), &EnvFile::new(value, "k")).unwrap();
        let config = config::load_from(Some(&path), |_| None).unwrap();
        assert_eq!(config.database_url, value, "value {value:?}");
    }
}

#[test]
fn refuses_line_breaks_nul_and_empty_secrets() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["a\nb", "a\rb", "a\0b", ""] {
        let err = write_new(dir.path(), &EnvFile::new(bad, "k")).unwrap_err();
        assert!(matches!(err, EnvFileError::InvalidValue { .. }), "{bad:?}");
        let err = write_new(dir.path(), &EnvFile::new("u", bad)).unwrap_err();
        assert!(matches!(err, EnvFileError::InvalidValue { .. }), "{bad:?}");
    }
    let mut env = EnvFile::new("u", "k");
    env.uploads_dir = Some(PathBuf::from("a\nb"));
    assert!(write_new(dir.path(), &env).is_err());
    assert!(!dir.path().join(".env").exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn existing_env_is_refused_and_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "KEEP=me\n").unwrap();
    let err = write_new(dir.path(), &tricky()).unwrap_err();
    assert!(matches!(err, EnvFileError::AlreadyExists { .. }));
    let message = err.to_string();
    assert!(message.contains(".env") && message.contains("move or delete"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "KEEP=me\n");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn symlink_at_env_is_refused_and_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "original").unwrap();
    let link = dir.path().join(".env");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    let err = write_new(dir.path(), &tricky()).unwrap_err();
    assert!(matches!(err, EnvFileError::AlreadyExists { .. }));
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());

    // A dangling link is refused too, and nothing is created through it.
    std::fs::remove_file(&link).unwrap();
    let missing = dir.path().join("missing");
    std::os::unix::fs::symlink(&missing, &link).unwrap();
    let err = write_new(dir.path(), &tricky()).unwrap_err();
    assert!(matches!(err, EnvFileError::AlreadyExists { .. }));
    assert!(!missing.exists());
}

#[test]
fn debug_and_errors_hide_secrets() {
    let env = EnvFile::new("postgres://user:hunter2@host/db", "topsecretkey");
    let debug = format!("{env:?}");
    assert!(!debug.contains("hunter2") && !debug.contains("topsecretkey"));
    assert!(debug.contains("[redacted]"));

    let bad = EnvFile::new("postgres://user:hunter2@host/db\n", "topsecretkey");
    let err = write_new(std::path::Path::new("/nonexistent-mc-dir"), &bad).unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("hunter2") && !text.contains("topsecretkey"));

    let missing = write_new(
        std::path::Path::new("/nonexistent-mc-dir"),
        &EnvFile::new("postgres://user:hunter2@host/db", "topsecretkey"),
    )
    .unwrap_err();
    assert!(matches!(missing, EnvFileError::Io { .. }));
    let text = format!("{missing} {missing:?}");
    assert!(!text.contains("hunter2") && !text.contains("topsecretkey"));
}

#[test]
fn whitespace_only_secrets_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [" ", "\t ", "  "] {
        assert!(write_new(dir.path(), &EnvFile::new(bad, "k")).is_err());
        assert!(write_new(dir.path(), &EnvFile::new("u", bad)).is_err());
    }
    assert!(!dir.path().join(".env").exists());
}

#[test]
fn stale_temp_files_are_removed_but_only_the_exact_kind() {
    let dir = tempfile::tempdir().unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let make = |name: &str, age: Option<std::time::SystemTime>| {
        let path = dir.path().join(name);
        std::fs::write(&path, "x").unwrap();
        if let Some(t) = age {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        path
    };
    let stale = make(".env.mission-control-1-2.tmp", Some(old));
    let fresh = make(".env.mission-control-3-4.tmp", None);
    let other = make(".env.other.tmp", Some(old));
    let wrong_end = make(".env.mission-control-5-6.bak", Some(old));
    #[cfg(unix)]
    let (victim, link) = {
        let victim = make("victim", Some(old));
        let link = dir.path().join(".env.mission-control-7-8.tmp");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        (victim, link)
    };
    write_new(dir.path(), &EnvFile::new("u", "k")).unwrap();
    assert!(!stale.exists());
    assert!(fresh.exists() && other.exists() && wrong_end.exists());
    #[cfg(unix)]
    {
        assert!(victim.exists());
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }
}
