//! Post locks (Task A6), as WordPress's `wp_check_post_lock`, `wp_set_post_lock` and
//! `wp_refresh_post_lock` keep them: the post meta `_edit_lock` holds `"<timestamp>:<user id>"`;
//! a lock younger than [`LOCK_WINDOW`] seconds, held by another user who still exists, keeps the
//! post locked. The admin sets the lock when a user opens the edit form, the editor renews it
//! while the form is open (the heartbeat), a second user is shown who is editing and may take
//! over, and leaving the form releases it.

use orbit_orm::{Database, Error, Value as Sql, raw};
use serde_json::{Value, json};

use crate::content::{MetaError, get_post_meta, set_post_meta};

/// How long a lock holds without being renewed, in seconds (`wp_check_post_lock_window`).
pub const LOCK_WINDOW: i64 = 150;

/// The post meta that holds the lock.
pub const EDIT_LOCK: &str = "_edit_lock";

/// The post meta that holds the last editor (`_edit_last`), read when a lock has no user.
const EDIT_LAST: &str = "_edit_last";

/// Seconds since the Unix epoch, now.
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The text of a meta value: a JSON string or number.
fn meta_text(value: Option<Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A meta read where a value that is not JSON reads as none.
async fn meta(db: &Database, post_id: i64, key: &str) -> Result<Option<String>, Error> {
    match get_post_meta(db, post_id, key).await {
        Ok(value) => Ok(meta_text(value)),
        Err(MetaError::InvalidJson(_)) => Ok(None),
        Err(MetaError::Database(e)) => Err(e),
    }
}

/// The lock of post `post_id` as stored: `(timestamp, user id)`, the user from `_edit_last`
/// when the lock names none, as `wp_check_post_lock` reads it.
async fn stored_lock(db: &Database, post_id: i64) -> Result<Option<(i64, i64)>, Error> {
    let Some(lock) = meta(db, post_id, EDIT_LOCK).await? else {
        return Ok(None);
    };
    let mut parts = lock.splitn(2, ':');
    let time = parts
        .next()
        .and_then(|t| t.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let user = match parts.next() {
        Some(user) => user.trim().parse::<i64>().unwrap_or(0),
        None => meta(db, post_id, EDIT_LAST)
            .await?
            .and_then(|u| u.trim().parse().ok())
            .unwrap_or(0),
    };
    Ok(Some((time, user)))
}

/// The user who holds the lock of post `post_id`, when it is someone other than `user_id`
/// and the lock is younger than [`LOCK_WINDOW`] (`wp_check_post_lock`): their id and name.
///
/// # Errors
///
/// The database's error.
pub async fn check_post_lock(
    db: &Database,
    post_id: i64,
    user_id: i64,
) -> Result<Option<(i64, String)>, Error> {
    let Some((time, holder)) = stored_lock(db, post_id).await? else {
        return Ok(None);
    };
    if holder == user_id || time <= now() - LOCK_WINDOW {
        return Ok(None);
    }
    // A lock held by a user who no longer exists is free.
    let name: Option<(String,)> = raw::fetch_optional(
        db,
        "SELECT name FROM users WHERE id = ?",
        &[Sql::from(holder)],
    )
    .await?;
    Ok(name.map(|(name,)| (holder, name)))
}

/// Locks post `post_id` for `user_id` now (`wp_set_post_lock`); returns the lock's text.
///
/// # Errors
///
/// The database's error.
pub async fn set_post_lock(db: &Database, post_id: i64, user_id: i64) -> Result<String, Error> {
    let lock = format!("{}:{user_id}", now());
    set_post_meta(db, post_id, EDIT_LOCK, &json!(lock))
        .await
        .map_err(|e| match e {
            MetaError::Database(e) => e,
            MetaError::InvalidJson(e) => Error::Database(sqlx::Error::Decode(Box::new(e))),
        })?;
    Ok(lock)
}

/// Releases `user_id`'s lock on post `post_id`, as `wp_ajax_wp_remove_post_lock` does when the
/// editor is left: the lock is kept, dated so it ends in 5 seconds. Only the lock the leaving
/// form holds is released (`active`, the `"<timestamp>:<user id>"` it last set; any lock of the
/// user when `None`), so another tab of the same user keeps its lock. Returns whether a lock was
/// released.
///
/// # Errors
///
/// The database's error.
pub async fn release_post_lock(
    db: &Database,
    post_id: i64,
    user_id: i64,
    active: Option<&str>,
) -> Result<bool, Error> {
    let Some((time, holder)) = stored_lock(db, post_id).await? else {
        return Ok(false);
    };
    if holder != user_id || active.is_some_and(|active| active != format!("{time}:{holder}")) {
        return Ok(false);
    }
    let lock = format!("{}:{user_id}", now() - LOCK_WINDOW + 5);
    set_post_meta(db, post_id, EDIT_LOCK, &json!(lock))
        .await
        .map_err(|e| match e {
            MetaError::Database(e) => e,
            MetaError::InvalidJson(e) => Error::Database(sqlx::Error::Decode(Box::new(e))),
        })?;
    Ok(true)
}

/// The heartbeat's answer for `user_id` on post `post_id` (`wp_refresh_post_lock`): another
/// user's lock as `{"lock_error": {"name", "text"}}`, or the user's renewed lock as
/// `{"new_lock": "<timestamp>:<user id>"}`. The caller has checked `edit_post`.
///
/// # Errors
///
/// The database's error.
pub async fn refresh_post_lock(db: &Database, post_id: i64, user_id: i64) -> Result<Value, Error> {
    match check_post_lock(db, post_id, user_id).await? {
        Some((_, name)) => Ok(json!({
            "lock_error": {
                "text": format!("{name} has taken over and is currently editing."),
                "name": name,
            }
        })),
        None => Ok(json!({ "new_lock": set_post_lock(db, post_id, user_id).await? })),
    }
}
