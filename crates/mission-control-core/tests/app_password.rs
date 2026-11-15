//! Tests for application passwords: creating, listing, revoking, and authenticating a request
//! by HTTP Basic auth. They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite
//! file when it is unset.
//!
//! The plain passwords are secrets even in tests: assertions never print them.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{TimeDelta, Utc};
use mission_control_core::migrations::migrator;
use mission_control_core::users::app_password::{
    self, AppPasswordError, BasicAuthError, ByEmail, check_password, chunk_password, hash_password,
};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, raw};

/// Runs the core migrations.
async fn setup(db: &Database) {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
}

/// Inserts a user and returns its id and email.
async fn new_user(db: &Database, name: &str) -> (i64, String) {
    let email = unique_email(name);
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &[name.into(), email.clone().into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    let id = raw::fetch_scalar(
        db,
        "SELECT id FROM users WHERE email = ?",
        &[email.clone().into()],
    )
    .await
    .expect("reads the user id");
    (id, email)
}

/// The `Authorization` header value for `user:password`.
fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

async fn count(db: &Database, user_id: i64) -> i64 {
    raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM application_passwords WHERE user_id = ?",
        &[user_id.into()],
    )
    .await
    .unwrap()
}

#[test]
fn hashes_as_wordpress_wp_fast_hash_does() {
    // Reference values from PHP: '$generic$' . sodium_bin2base64(sodium_crypto_generichash(
    // $m, 'wp_fast_hash_6.8+', 30), SODIUM_BASE64_VARIANT_URLSAFE_NO_PADDING).
    assert_eq!(
        hash_password("abcdEFGHijklMNOPqrstUVwx"),
        "$generic$ywVcWaGb44suUSsdgZC6wNp7GlvhmaV1hHQccGsy"
    );
    assert_eq!(
        hash_password(""),
        "$generic$XJQGK2eUryTa4vHbU0bDv1Ml3wGPwaC1ATczq0o_"
    );
    assert!(check_password(
        "abcdEFGHijklMNOPqrstUVwx",
        "$generic$ywVcWaGb44suUSsdgZC6wNp7GlvhmaV1hHQccGsy"
    ));
    assert!(!check_password(
        "abcdEFGHijklMNOPqrstUVwX",
        "$generic$ywVcWaGb44suUSsdgZC6wNp7GlvhmaV1hHQccGsy"
    ));
    // A hash from before WordPress 6.8 (phpass) is never accepted: fail closed.
    assert!(!check_password(
        "abcdEFGHijklMNOPqrstUVwx",
        "$P$B12345678901234567890123456789."
    ));
    assert!(!check_password("", ""));
}

#[test]
fn chunks_as_wordpress_does() {
    assert_eq!(
        chunk_password("abcdEFGHijklMNOPqrstUVwx"),
        "abcd EFGH ijkl MNOP qrst UVwx"
    );
    assert_eq!(chunk_password("ab-cd ef"), "abcd ef");
}

#[tokio::test]
async fn a_created_password_authenticates_by_basic_header() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, email) = new_user(db, "ann").await;

    let (plain, created) = app_password::create(db, ann, "Laptop", None).await.unwrap();
    assert_eq!(plain.as_str().len(), 24);
    assert!(plain.as_str().chars().all(|c| c.is_ascii_alphanumeric()));
    assert_eq!(plain.chunked().len(), 29);
    assert!(
        !format!("{plain:?}").contains(plain.as_str()),
        "Debug hides it"
    );
    assert_eq!(created.user_id, ann);
    assert_eq!(created.name, "Laptop");
    assert_eq!(created.uuid.len(), 36);
    assert_eq!(created.last_used_at, None);
    assert_eq!(created.last_ip, None);
    assert_eq!(created.app_id, None);

    let header = basic(&email, plain.as_str());
    let found = app_password::authenticate_basic(db, &header, &ByEmail, Some("203.0.113.5"))
        .await
        .expect("authenticates");
    assert_eq!(found.uuid, created.uuid);
    assert_eq!(found.user_id, ann);
    assert!(found.last_used_at.is_some());
    assert_eq!(found.last_ip.as_deref(), Some("203.0.113.5"));

    // Usage is stored, and listed.
    let listed = app_password::list(db, ann).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].last_ip.as_deref(), Some("203.0.113.5"));
    assert!(listed[0].last_used_at.is_some());

    // The password is everything after the first `:`, kept whole: a `:` inside it reaches the
    // password check (where, like spaces, it is dropped), not the user name.
    let (head, tail) = plain.as_str().split_at(12);
    let header = basic(&email, &format!("{head}:{tail}"));
    assert!(
        app_password::authenticate_basic(db, &header, &ByEmail, None)
            .await
            .is_ok(),
        "a password with a colon passes through whole"
    );
    // Any number of spaces may separate the scheme from the credentials.
    let header = basic(&email, plain.as_str()).replacen("Basic ", "Basic   ", 1);
    assert!(
        app_password::authenticate_basic(db, &header, &ByEmail, None)
            .await
            .is_ok(),
        "extra spaces after Basic are accepted"
    );

    // The chunked form, as shown to the user, authenticates too.
    let header = basic(&email, &plain.chunked());
    assert!(
        app_password::authenticate_basic(db, &header, &ByEmail, None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn wrong_revoked_and_other_users_passwords_and_malformed_headers_are_refused() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, ann_email) = new_user(db, "ann").await;
    let (bo, _) = new_user(db, "bo").await;
    let (plain, item) = app_password::create(db, ann, "Laptop", None).await.unwrap();
    let (bo_plain, _) = app_password::create(db, bo, "Phone", None).await.unwrap();

    let refused = async |header: &str| {
        app_password::authenticate_basic(db, header, &ByEmail, None)
            .await
            .err()
    };

    // A wrong password.
    assert!(matches!(
        refused(&basic(&ann_email, "abcdEFGHijklMNOPqrstUVwx")).await,
        Some(BasicAuthError::IncorrectPassword)
    ));
    assert!(matches!(
        refused(&basic(&ann_email, "")).await,
        Some(BasicAuthError::IncorrectPassword)
    ));
    // Another user's password.
    assert!(matches!(
        refused(&basic(&ann_email, bo_plain.as_str())).await,
        Some(BasicAuthError::IncorrectPassword)
    ));
    // An unknown user.
    assert!(matches!(
        refused(&basic("nobody@example.test", plain.as_str())).await,
        Some(BasicAuthError::InvalidEmail)
    ));
    assert!(matches!(
        refused(&basic("nobody", plain.as_str())).await,
        Some(BasicAuthError::InvalidUsername)
    ));
    // Malformed headers.
    let not_utf8 = format!("Basic {}", STANDARD.encode([0xff, b':', b'x']));
    let no_colon = format!("Basic {}", STANDARD.encode(ann_email.as_bytes()));
    let empty_user = basic("", plain.as_str());
    for header in [
        "",
        "Basic",
        "Basic ",
        "Bearer abc",
        "Basic !!!not-base64",
        not_utf8.as_str(),
        no_colon.as_str(),
        empty_user.as_str(),
    ] {
        assert!(
            matches!(refused(header).await, Some(BasicAuthError::Malformed)),
            "refuses a malformed header"
        );
    }
    // The scheme is case-insensitive.
    let lower = basic(&ann_email, plain.as_str()).replacen("Basic", "basic", 1);
    assert!(refused(&lower).await.is_none());

    // A revoked password.
    assert!(app_password::revoke(db, ann, &item.uuid).await.unwrap());
    assert!(matches!(
        refused(&basic(&ann_email, plain.as_str())).await,
        Some(BasicAuthError::IncorrectPassword)
    ));
    assert!(!app_password::revoke(db, ann, &item.uuid).await.unwrap());
    // One user cannot revoke another's.
    let bo_item = &app_password::list(db, bo).await.unwrap()[0];
    assert!(!app_password::revoke(db, ann, &bo_item.uuid).await.unwrap());
    assert_eq!(count(db, bo).await, 1);
}

#[tokio::test]
async fn the_stored_value_is_a_hash_of_the_password() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, _) = new_user(db, "ann").await;
    let (plain, item) = app_password::create(db, ann, "Laptop", None).await.unwrap();

    let stored: String = raw::fetch_scalar(
        db,
        "SELECT password FROM application_passwords WHERE uuid = ?",
        &[item.uuid.clone().into()],
    )
    .await
    .unwrap();
    assert!(stored.starts_with("$generic$"));
    assert!(!stored.contains(plain.as_str()), "not the plain text");
    assert!(check_password(plain.as_str(), &stored), "verifies");
    assert!(
        !format!("{item:?}").contains(&stored),
        "Debug hides the hash"
    );
}

#[tokio::test]
async fn spaces_and_dashes_are_ignored_but_case_is_kept() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, _) = new_user(db, "ann").await;
    let (plain, item) = app_password::create(db, ann, "Laptop", None).await.unwrap();

    let spaced = plain.chunked();
    let dashed = spaced.replace(' ', "-");
    for presented in [spaced.as_str(), dashed.as_str()] {
        let found = app_password::authenticate(db, ann, presented, None)
            .await
            .unwrap();
        assert_eq!(found.map(|f| f.uuid), Some(item.uuid.clone()));
    }
    // WordPress keeps case: a password with its case swapped is refused, unless it has no
    // letters, which 24 random characters from 62 almost never are.
    let swapped: String = plain
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    if swapped != plain.as_str() {
        assert!(
            app_password::authenticate(db, ann, &swapped, None)
                .await
                .unwrap()
                .is_none(),
            "case matters"
        );
    }
}

#[tokio::test]
async fn names_are_required_and_unique_per_user() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, _) = new_user(db, "ann").await;
    let (bo, _) = new_user(db, "bo").await;

    let (_, item) = app_password::create(db, ann, "  My   Phone ", None)
        .await
        .unwrap();
    assert_eq!(
        item.name, "My Phone",
        "sanitized as sanitize_text_field does"
    );
    // WordPress compares names with `strtolower`.
    assert!(matches!(
        app_password::create(db, ann, "my phone", None).await,
        Err(AppPasswordError::DuplicateName)
    ));
    // Another user may use the same name.
    assert!(app_password::create(db, bo, "My Phone", None).await.is_ok());
    for empty in ["", "   ", "\t\n"] {
        assert!(matches!(
            app_password::create(db, ann, empty, None).await,
            Err(AppPasswordError::EmptyName)
        ));
    }
    assert!(matches!(
        app_password::create(db, ann, &"x".repeat(256), None).await,
        Err(AppPasswordError::NameTooLong)
    ));
    assert!(matches!(
        app_password::create(db, i64::MAX, "Laptop", None).await,
        Err(AppPasswordError::UserNotFound)
    ));
    assert_eq!(count(db, ann).await, 1);
}

#[tokio::test]
async fn an_app_id_is_stored_when_it_is_a_uuid() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, _) = new_user(db, "ann").await;
    let app_id = "0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0";

    let (_, item) = app_password::create(db, ann, "Client", Some(app_id))
        .await
        .unwrap();
    assert_eq!(item.app_id.as_deref(), Some(app_id));
    let stored = app_password::find(db, ann, &item.uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.app_id.as_deref(), Some(app_id));
    // An empty one is none, as WordPress stores `''`.
    let (_, empty) = app_password::create(db, ann, "Other", Some(""))
        .await
        .unwrap();
    assert_eq!(empty.app_id, None);
    // Anything but a hyphenated UUID is refused.
    for bad in [
        "not-a-uuid",
        "0f1e2d3c4b5a49688776a5b4c3d2e1f0",
        "{0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0}",
    ] {
        assert!(matches!(
            app_password::create(db, ann, "Bad", Some(bad)).await,
            Err(AppPasswordError::InvalidAppId)
        ));
    }
}

#[tokio::test]
async fn list_revoke_all_and_deleting_the_user() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, _) = new_user(db, "ann").await;
    let (bo, _) = new_user(db, "bo").await;
    let (a, _) = app_password::create(db, ann, "One", None).await.unwrap();
    let (b, _) = app_password::create(db, ann, "Two", None).await.unwrap();
    app_password::create(db, bo, "One", None).await.unwrap();
    assert!(a.as_str() != b.as_str(), "each password is new");

    let names: Vec<String> = app_password::list(db, ann)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["One", "Two"]);
    assert_eq!(app_password::revoke_all(db, ann).await.unwrap(), 2);
    assert!(app_password::list(db, ann).await.unwrap().is_empty());
    assert_eq!(count(db, bo).await, 1);

    raw::execute(db, "DELETE FROM users WHERE id = ?", &[bo.into()])
        .await
        .unwrap();
    assert_eq!(count(db, bo).await, 0);
}

#[tokio::test]
async fn usage_is_recorded_at_most_once_a_day() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let (ann, _) = new_user(db, "ann").await;
    let (plain, item) = app_password::create(db, ann, "Laptop", None).await.unwrap();

    let first = app_password::authenticate(db, ann, plain.as_str(), Some("203.0.113.5"))
        .await
        .unwrap()
        .unwrap();
    // Within a day, nothing is written, as WordPress's record_application_password_usage.
    let second = app_password::authenticate(db, ann, plain.as_str(), Some("2001:db8::1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.last_used_at, first.last_used_at);
    assert_eq!(second.last_ip.as_deref(), Some("203.0.113.5"));

    // A day later, it is.
    let two_days_ago = (Utc::now() - TimeDelta::days(2)).naive_utc();
    raw::execute(
        db,
        "UPDATE application_passwords SET last_used_at = ? WHERE uuid = ?",
        &[two_days_ago.into(), item.uuid.clone().into()],
    )
    .await
    .unwrap();
    let third = app_password::authenticate(db, ann, plain.as_str(), Some("2001:db8::1"))
        .await
        .unwrap()
        .unwrap();
    assert!(third.last_used_at > Some(two_days_ago));
    assert_eq!(third.last_ip.as_deref(), Some("2001:db8::1"));

    // Something that is not an IP address is not stored.
    raw::execute(
        db,
        "UPDATE application_passwords SET last_used_at = ? WHERE uuid = ?",
        &[two_days_ago.into(), item.uuid.clone().into()],
    )
    .await
    .unwrap();
    let fourth = app_password::authenticate(db, ann, plain.as_str(), Some("not an ip"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fourth.last_ip.as_deref(), Some("2001:db8::1"));
}
