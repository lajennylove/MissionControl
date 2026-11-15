//! Application passwords: passwords a user creates for REST API clients, sent by HTTP Basic auth,
//! after WordPress 6.8's `WP_Application_Passwords` and `wp_authenticate_application_password`.
//!
//! A password is 24 characters from `a-z`, `A-Z` and `0-9`, drawn from the operating system's
//! CSPRNG, as `wp_generate_password( 24, false )` draws them. It is shown once, when it is created
//! ([`PlainPassword`]), in WordPress's groups of four ([`chunk_password`]); only its hash is
//! stored. The hash is WordPress 6.8's `wp_fast_hash`: keyed BLAKE2b, since the password is a
//! random token with no need for a slow hash, so a hash WordPress made verifies here too.
//!
//! When a password is presented, every character but ASCII letters and digits is dropped, so the
//! spaces of the grouped form (and dashes) do not matter; case does, as in WordPress. Usage, the
//! time and the client's IP address, is recorded at most once a day, as WordPress records it.
//!
//! Each row of the `application_passwords` table is one password; WordPress keeps the same items
//! in the `_application_passwords` user meta.
//!
//! ```ignore
//! use mission_control_core::users::app_password::{self, ByEmail};
//!
//! let (plain, item) = app_password::create(&db, user_id, "Laptop").await?;
//! show_once(&plain.chunked()); // "abcd efgh ijkl mnop qrst uvwx"
//!
//! // On a REST request:
//! let item = app_password::authenticate_basic(&db, authorization, &ByEmail, Some(ip)).await?;
//! ```

use std::fmt;
use std::future::Future;
use std::net::IpAddr;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use blake2::Blake2bMac;
use blake2::digest::consts::U30;
use blake2::digest::{KeyInit, Mac};
use chrono::{NaiveDateTime, SubsecRound, TimeDelta, Utc};
use orbit_orm::{Database, Direction, Driver, Model, OrbitModel, Value, raw};
use subtle::ConstantTimeEq;

/// The length of a new password: WordPress's `WP_Application_Passwords::PW_LENGTH`.
pub const PASSWORD_LENGTH: usize = 24;

/// The longest name, in characters: the `name` column's length.
pub const MAX_NAME_LENGTH: usize = 255;

/// The characters of a password, as `wp_generate_password` without special characters.
const ALPHABET: &[u8; 62] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// The BLAKE2b key of WordPress 6.8's `wp_fast_hash`.
const FAST_HASH_KEY: &[u8] = b"wp_fast_hash_6.8+";

/// The prefix of a `wp_fast_hash` hash.
const FAST_HASH_PREFIX: &str = "$generic$";

/// One application password: a row of `application_passwords`. The hash is kept but never shown,
/// not even by `Debug`.
#[derive(Clone, PartialEq, Eq, OrbitModel)]
#[orbit(table = "application_passwords")]
pub struct ApplicationPassword {
    /// The id.
    pub id: i64,
    /// The password's public id, a version 4 UUID: what the REST API and revoking name it by.
    #[orbit(fillable)]
    pub uuid: String,
    /// The user it signs in as.
    #[orbit(fillable)]
    pub user_id: i64,
    /// The name the user gave it, such as the client's name. Unique per user, ignoring case.
    #[orbit(fillable)]
    pub name: String,
    /// The hash of the password, from [`hash_password`].
    #[orbit(fillable)]
    password: String,
    /// When it was created, in UTC.
    #[orbit(fillable)]
    pub created_at: NaiveDateTime,
    /// When it last authenticated a request, in UTC, to the day: see [`authenticate`].
    #[orbit(fillable)]
    pub last_used_at: Option<NaiveDateTime>,
    /// The IP address of the client it last authenticated, recorded with `last_used_at`.
    #[orbit(fillable)]
    pub last_ip: Option<String>,
}

impl fmt::Debug for ApplicationPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplicationPassword")
            .field("id", &self.id)
            .field("uuid", &self.uuid)
            .field("user_id", &self.user_id)
            .field("name", &self.name)
            .field("password", &"[hidden]")
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("last_ip", &self.last_ip)
            .finish()
    }
}

/// A new password in plain text, returned once by [`create`] to show the user. It is not stored
/// and cannot be read again. `Debug` hides it and there is no `Display`, so it is not logged by
/// accident.
#[derive(Clone, PartialEq, Eq)]
pub struct PlainPassword(String);

impl PlainPassword {
    /// The password: 24 ASCII letters and digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The password in groups of four, as WordPress shows it: `abcd efgh ijkl mnop qrst uvwx`.
    pub fn chunked(&self) -> String {
        chunk_password(&self.0)
    }
}

impl fmt::Debug for PlainPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlainPassword([hidden])")
    }
}

/// Why an application password was not created.
#[derive(Debug)]
pub enum AppPasswordError {
    /// The name is empty once sanitized: WordPress's `application_password_empty_name`.
    EmptyName,
    /// The name is longer than [`MAX_NAME_LENGTH`] characters.
    NameTooLong,
    /// The user has a password with this name, ignoring case: WordPress's
    /// `application_password_duplicate_name`.
    DuplicateName,
    /// No user has this id.
    UserNotFound,
    /// The random number generator failed, so no password was made.
    Random(getrandom::Error),
    /// The database refused a query.
    Database(orbit_orm::Error),
}

impl fmt::Display for AppPasswordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => {
                f.write_str("an application name is required to create an application password")
            }
            Self::NameTooLong => write!(
                f,
                "an application name has at most {MAX_NAME_LENGTH} characters"
            ),
            Self::DuplicateName => f.write_str("each application name should be unique"),
            Self::UserNotFound => f.write_str("the user does not exist"),
            Self::Random(e) => write!(f, "could not generate a password: {e}"),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for AppPasswordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Random(e) => Some(e),
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}

impl From<orbit_orm::Error> for AppPasswordError {
    fn from(e: orbit_orm::Error) -> Self {
        Self::Database(e)
    }
}

/// Why a request's HTTP Basic credentials were refused. [`code`](Self::code) gives WordPress's
/// error code for the REST response.
#[derive(Debug)]
pub enum BasicAuthError {
    /// The header is not `Basic` with base64 of `user:password` in UTF-8 and a user name. WordPress
    /// does not try application passwords then; the request is not authenticated by them.
    Malformed,
    /// No user has this login: WordPress's `invalid_username`.
    InvalidUsername,
    /// No user has this email address: WordPress's `invalid_email`.
    InvalidEmail,
    /// The password is none of the user's application passwords: WordPress's
    /// `incorrect_password`.
    IncorrectPassword,
    /// The database refused a query.
    Database(orbit_orm::Error),
}

impl BasicAuthError {
    /// WordPress's error code for this refusal, or `None` when WordPress reports none.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::InvalidUsername => Some("invalid_username"),
            Self::InvalidEmail => Some("invalid_email"),
            Self::IncorrectPassword => Some("incorrect_password"),
            Self::Malformed | Self::Database(_) => None,
        }
    }
}

impl fmt::Display for BasicAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => {
                f.write_str("the Authorization header is not valid Basic credentials")
            }
            Self::InvalidUsername => f.write_str("unknown username"),
            Self::InvalidEmail => f.write_str("unknown email address"),
            Self::IncorrectPassword => {
                f.write_str("the provided password is an invalid application password")
            }
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for BasicAuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}

impl From<orbit_orm::Error> for BasicAuthError {
    fn from(e: orbit_orm::Error) -> Self {
        Self::Database(e)
    }
}

/// Finds the user a Basic auth user name names. WordPress looks the name up as a login, then, when
/// it is an email address, as an email address.
pub trait UserLookup {
    /// The id of the user named `username`, or `None` when there is none.
    fn user_id(
        &self,
        db: &Database,
        username: &str,
    ) -> impl Future<Output = Result<Option<i64>, orbit_orm::Error>> + Send;
}

/// Looks a user up by the email address in `users.email`, the exact text. Logins arrive with the
/// user profiles (WordPress's `user_login`); until then a client signs in with the email address,
/// which WordPress also accepts.
#[derive(Debug, Clone, Copy, Default)]
pub struct ByEmail;

impl UserLookup for ByEmail {
    async fn user_id(
        &self,
        db: &Database,
        username: &str,
    ) -> Result<Option<i64>, orbit_orm::Error> {
        let row: Option<(i64,)> = raw::fetch_optional(
            db,
            "SELECT id FROM users WHERE email = ?",
            &[username.into()],
        )
        .await?;
        Ok(row.map(|(id,)| id))
    }
}

/// Creates an application password named `name` for the user, as WordPress's
/// `create_new_application_password`, and returns the password, shown this once, with its row.
///
/// The name is sanitized (runs of spaces, tabs and line breaks become one space, and the ends are
/// trimmed) and must not be empty, nor the name of another of the user's passwords, ignoring
/// ASCII case, as WordPress's `application_name_exists_for_user` compares with `strtolower`.
///
/// # Errors
///
/// [`AppPasswordError::EmptyName`], [`NameTooLong`](AppPasswordError::NameTooLong),
/// [`DuplicateName`](AppPasswordError::DuplicateName),
/// [`UserNotFound`](AppPasswordError::UserNotFound), or the random number generator's or the
/// database's error. Nothing is stored then.
pub async fn create(
    db: &Database,
    user_id: i64,
    name: &str,
) -> Result<(PlainPassword, ApplicationPassword), AppPasswordError> {
    let name = sanitize_name(name);
    if name.is_empty() {
        return Err(AppPasswordError::EmptyName);
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        return Err(AppPasswordError::NameTooLong);
    }
    let plain = generate_password()?;
    let mut item = ApplicationPassword {
        id: 0,
        uuid: uuid::Uuid::new_v4().to_string(),
        user_id,
        name,
        password: hash_password(plain.as_str()),
        created_at: now(),
        last_used_at: None,
        last_ip: None,
    };
    let driver = db.driver();
    item.id = db
        .transaction(async |tx| {
            // Locks the user's row on MySQL, MariaDB and PostgreSQL, so two creates for one user
            // take turns and the name check below holds. SQLite allows one writer at a time.
            let lock = match driver {
                Driver::Sqlite => "",
                Driver::MySql | Driver::Postgres => " FOR UPDATE",
            };
            let user: Option<(i64,)> = raw::fetch_optional(
                &mut *tx,
                &format!("SELECT id FROM users WHERE id = ?{lock}"),
                &[user_id.into()],
            )
            .await?;
            if user.is_none() {
                return Err(AppPasswordError::UserNotFound);
            }
            let taken = ApplicationPassword::query()
                .where_eq("user_id", user_id)
                .get(&mut *tx)
                .await?
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&item.name));
            if taken {
                return Err(AppPasswordError::DuplicateName);
            }
            let id = ApplicationPassword::query()
                .insert(&mut *tx, &item.columns())
                .await?;
            Ok(i64::try_from(id).expect("an id from a BIGINT column fits in i64"))
        })
        .await?;
    Ok((plain, item))
}

/// The user's application passwords, oldest first, as WordPress's
/// `get_user_application_passwords`.
///
/// # Errors
///
/// The database's error.
pub async fn list(
    db: &Database,
    user_id: i64,
) -> Result<Vec<ApplicationPassword>, orbit_orm::Error> {
    ApplicationPassword::query()
        .where_eq("user_id", user_id)
        .order_by("id", Direction::Asc)
        .get(db)
        .await
}

/// The user's application password with this uuid, as WordPress's
/// `get_user_application_password`. Another user's password is `None`.
///
/// # Errors
///
/// The database's error.
pub async fn find(
    db: &Database,
    user_id: i64,
    uuid: &str,
) -> Result<Option<ApplicationPassword>, orbit_orm::Error> {
    ApplicationPassword::query()
        .where_eq("user_id", user_id)
        .where_eq("uuid", uuid)
        .first(db)
        .await
}

/// Revokes the user's application password with this uuid, as WordPress's
/// `delete_application_password`, and returns whether there was one. Another user's password is
/// left alone.
///
/// # Errors
///
/// The database's error.
pub async fn revoke(db: &Database, user_id: i64, uuid: &str) -> Result<bool, orbit_orm::Error> {
    let deleted = ApplicationPassword::query()
        .where_eq("user_id", user_id)
        .where_eq("uuid", uuid)
        .delete(db)
        .await?;
    Ok(deleted > 0)
}

/// Revokes every application password of the user, as WordPress's
/// `delete_all_application_passwords`, and returns how many there were.
///
/// # Errors
///
/// The database's error.
pub async fn revoke_all(db: &Database, user_id: i64) -> Result<u64, orbit_orm::Error> {
    ApplicationPassword::query()
        .where_eq("user_id", user_id)
        .delete(db)
        .await
}

/// Checks `password` against the user's application passwords, as WordPress's
/// `wp_authenticate_application_password` does once it has the user, and returns the one it
/// matches, with its usage recorded.
///
/// Every character but ASCII letters and digits is dropped from `password` first, so spaces and
/// dashes do not matter; case does. Usage is recorded as WordPress's
/// `record_application_password_usage` records it: the time and `ip`, unless the password was
/// last used less than a day ago. `ip` is stored only when it parses as an IP address; with none,
/// the previous one stays.
///
/// # Errors
///
/// The database's error. A password that matches none is `Ok(None)`.
pub async fn authenticate(
    db: &Database,
    user_id: i64,
    password: &str,
    ip: Option<&str>,
) -> Result<Option<ApplicationPassword>, orbit_orm::Error> {
    let password = normalize_password(password);
    if password.is_empty() {
        return Ok(None);
    }
    let hash = hash_password(&password);
    // Every stored hash is compared, so the time taken does not tell which one matched.
    let mut matched = None;
    for item in list(db, user_id).await? {
        if bool::from(hash.as_bytes().ct_eq(item.password.as_bytes())) && matched.is_none() {
            matched = Some(item);
        }
    }
    let Some(mut item) = matched else {
        return Ok(None);
    };
    record_usage(db, &mut item, ip).await?;
    Ok(Some(item))
}

/// Authenticates a request by its `Authorization` header: `Basic` and the base64 of
/// `user:password`. The user name is resolved with `lookup` and the password checked with
/// [`authenticate`], which records the usage with `ip`. Returns the matching password, whose
/// `user_id` is the user the request runs as.
///
/// # Errors
///
/// [`BasicAuthError::Malformed`] for a header that is not Basic credentials,
/// [`InvalidEmail`](BasicAuthError::InvalidEmail) or
/// [`InvalidUsername`](BasicAuthError::InvalidUsername) for an unknown user (the name is taken
/// as an email address when it holds an `@`),
/// [`IncorrectPassword`](BasicAuthError::IncorrectPassword), or the database's error.
pub async fn authenticate_basic<L: UserLookup>(
    db: &Database,
    authorization: &str,
    lookup: &L,
    ip: Option<&str>,
) -> Result<ApplicationPassword, BasicAuthError> {
    let (username, password) = parse_basic(authorization).ok_or(BasicAuthError::Malformed)?;
    let Some(user_id) = lookup.user_id(db, &username).await? else {
        return Err(if username.contains('@') {
            BasicAuthError::InvalidEmail
        } else {
            BasicAuthError::InvalidUsername
        });
    };
    authenticate(db, user_id, &password, ip)
        .await?
        .ok_or(BasicAuthError::IncorrectPassword)
}

/// Hashes an application password as WordPress 6.8's `wp_fast_hash`: `$generic$` and the
/// URL-safe base64, without padding, of the 30-byte BLAKE2b of the password keyed with
/// `wp_fast_hash_6.8+`.
pub fn hash_password(password: &str) -> String {
    let mut mac = <Blake2bMac<U30> as KeyInit>::new_from_slice(FAST_HASH_KEY)
        .expect("the key is shorter than BLAKE2b's 64-byte limit");
    mac.update(password.as_bytes());
    let digest = mac.finalize().into_bytes();
    format!("{FAST_HASH_PREFIX}{}", URL_SAFE_NO_PAD.encode(digest))
}

/// Whether `password` is the one `hash` was made from, compared in constant time, as WordPress's
/// `wp_verify_fast_hash`. A hash from before WordPress 6.8 (phpass), which WordPress still checks,
/// is refused: Mission Control never made one.
pub fn check_password(password: &str, hash: &str) -> bool {
    if !hash.starts_with(FAST_HASH_PREFIX) {
        return false;
    }
    hash_password(password)
        .as_bytes()
        .ct_eq(hash.as_bytes())
        .into()
}

/// A password in groups of four separated by spaces, as WordPress's `chunk_password`: every
/// character but ASCII letters and digits is dropped first.
pub fn chunk_password(password: &str) -> String {
    normalize_password(password)
        .as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).expect("ASCII"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `password` with every character but ASCII letters and digits dropped, as WordPress's
/// `preg_replace( '/[^a-z\d]/i', '', $password )`.
fn normalize_password(password: &str) -> String {
    password
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// The name as WordPress's `sanitize_text_field` leaves it for plain text: runs of spaces, tabs
/// and line breaks become one space, and the ends are trimmed.
fn sanitize_name(name: &str) -> String {
    name.split([' ', '\t', '\r', '\n'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A new random password of [`PASSWORD_LENGTH`] characters from [`ALPHABET`], each equally likely.
fn generate_password() -> Result<PlainPassword, AppPasswordError> {
    // Bytes from 248 up are dropped, so `byte % 62` is uniform (248 = 4 × 62).
    const LIMIT: u8 = 248;
    let mut password = String::with_capacity(PASSWORD_LENGTH);
    let mut bytes = [0u8; 64];
    while password.len() < PASSWORD_LENGTH {
        getrandom::fill(&mut bytes).map_err(AppPasswordError::Random)?;
        for &byte in bytes.iter().filter(|&&b| b < LIMIT) {
            if password.len() == PASSWORD_LENGTH {
                break;
            }
            password.push(char::from(ALPHABET[usize::from(byte % 62)]));
        }
    }
    bytes.fill(0);
    Ok(PlainPassword(password))
}

/// The user name and password of `Basic` credentials, or `None` when the header is not that. The
/// scheme is matched without case (RFC 7617); the user name ends at the first `:` and must not be
/// empty.
fn parse_basic(authorization: &str) -> Option<(String, String)> {
    let (scheme, credentials) = authorization.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = STANDARD.decode(credentials.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    if username.is_empty() {
        return None;
    }
    Some((username.to_owned(), password.to_owned()))
}

/// Records that `item` was used now from `ip`, unless it was used less than a day ago.
async fn record_usage(
    db: &Database,
    item: &mut ApplicationPassword,
    ip: Option<&str>,
) -> Result<(), orbit_orm::Error> {
    let now = now();
    if item
        .last_used_at
        .is_some_and(|last| last + TimeDelta::days(1) > now)
    {
        return Ok(());
    }
    item.last_used_at = Some(now);
    if let Some(ip) = ip.and_then(|ip| ip.trim().parse::<IpAddr>().ok()) {
        item.last_ip = Some(ip.to_string());
    }
    ApplicationPassword::query()
        .where_eq(ApplicationPassword::PRIMARY_KEY, item.id)
        .update(
            db,
            &[
                ("last_used_at", item.last_used_at.into()),
                ("last_ip", item.last_ip.clone().into()),
            ],
        )
        .await?;
    Ok(())
}

impl ApplicationPassword {
    /// Every column but the id, to insert.
    fn columns(&self) -> [(&'static str, Value); 7] {
        [
            ("uuid", self.uuid.clone().into()),
            ("user_id", self.user_id.into()),
            ("name", self.name.clone().into()),
            ("password", self.password.clone().into()),
            ("created_at", self.created_at.into()),
            ("last_used_at", self.last_used_at.into()),
            ("last_ip", self.last_ip.clone().into()),
        ]
    }
}

/// The current time in UTC, to the second, as the database stores it.
fn now() -> NaiveDateTime {
    Utc::now().naive_utc().trunc_subsecs(0)
}
