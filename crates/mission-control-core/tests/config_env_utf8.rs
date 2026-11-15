//! A variable in the process environment that is not valid UTF-8 is an invalid value, not a
//! missing one. In a test binary of its own: it sets a variable of the process environment.

#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use mission_control_core::config::{ConfigError, NOT_UTF8, load, process_environment};

#[test]
fn non_utf8_secret_is_invalid_not_missing() {
    // SAFETY: this binary runs this one test only, so no other thread reads the environment.
    unsafe { std::env::set_var("DATABASE_URL", OsStr::from_bytes(b"sqlite://\xff.db")) };
    let expected = ConfigError::Invalid {
        name: "DATABASE_URL",
        value: NOT_UTF8.to_owned(),
    };
    assert_eq!(process_environment().err(), Some(expected.clone()));
    assert_eq!(load().err(), Some(expected.clone()));
    assert!(!expected.to_string().contains("sqlite"));
}
