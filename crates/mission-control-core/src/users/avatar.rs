//! A user's profile picture: an image of the media library, as WordPress's avatar plugins keep
//! one ("WP User Avatar" stores the attachment's id in the user's meta), where WordPress itself
//! shows Gravatar's picture of the email.
//!
//! - The picture is an attachment, uploaded through the media library's uploader (the same
//!   checks, the same file store and the same kses as any upload), and chosen in the media
//!   library's picker. Its id is the user meta [`AVATAR_META`].
//! - A user may choose an image they may read in the media library ([`check_avatar`]: the
//!   picker's `get-attachment` rule, `upload_files` and an `inherit` attachment, or a `private`
//!   one with `read_private_posts`); anything else is refused with [`NOT_AN_IMAGE`].
//! - The picture's address is the attachment's thumbnail ([`avatar_urls`]), for the top bar,
//!   the profile and the Users list.
//! - The media library marks the attachments that are someone's picture with the label
//!   [`AVATAR_LABEL`] ([`avatar_attachments`]); they are listed with the other files.

use std::collections::{HashMap, HashSet};

use orbit_orm::{Database, Error, Value as Sql, raw};
use serde_json::{Value, json};

use crate::admin::media::{attachment_for_user, thumbnail_urls};
use crate::content::{MetaError, delete_user_meta, get_user_meta, set_user_meta};
use crate::users::UserCaps;

/// The user meta that holds the id of the user's picture.
pub const AVATAR_META: &str = "mc_avatar";

/// The label the media library shows on an attachment that is a user's picture.
pub const AVATAR_LABEL: &str = "Avatar";

/// The refusal of a picture that is not an image the user may choose.
pub const NOT_AN_IMAGE: &str = "Choose an image from the media library.";

/// The id `value` holds: a positive whole number, or its text.
fn id_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|id| *id > 0)
}

/// The attachment id of `user_id`'s picture, or `None`.
///
/// # Errors
///
/// The database's error, or stored meta that is not JSON.
pub async fn avatar_attachment(db: &Database, user_id: i64) -> Result<Option<i64>, MetaError> {
    Ok(get_user_meta(db, user_id, AVATAR_META)
        .await?
        .as_ref()
        .and_then(id_of))
}

/// Stores `attachment` as `user_id`'s picture, or removes the picture with `None`. The
/// attachment stays in the media library either way.
///
/// # Errors
///
/// The database's error.
pub async fn set_avatar_attachment(
    db: &Database,
    user_id: i64,
    attachment: Option<i64>,
) -> Result<(), MetaError> {
    match attachment {
        Some(id) => set_user_meta(db, user_id, AVATAR_META, &json!(id)).await,
        None => delete_user_meta(db, user_id, AVATAR_META).await.map(|_| ()),
    }
}

/// Whether `user` may choose attachment `id` as their picture: `None` when they may (an image
/// they may read in the media library, see the [module](self)), or the message of the refusal.
///
/// # Errors
///
/// The database's error.
pub async fn check_avatar(
    db: &Database,
    user: &UserCaps,
    id: i64,
) -> Result<Option<&'static str>, Error> {
    Ok(match attachment_for_user(db, user, id).await? {
        Some(item) if item.get("type").and_then(Value::as_str) == Some("image") => None,
        _ => Some(NOT_AN_IMAGE),
    })
}

/// The pictures' addresses of users `ids` that have one: each attachment's thumbnail. A user
/// whose picture is gone, or not an image, is left out.
///
/// # Errors
///
/// The database's error.
pub async fn avatar_urls(db: &Database, ids: &[i64]) -> Result<HashMap<i64, String>, Error> {
    let mut chosen: HashMap<i64, i64> = HashMap::new();
    for chunk in ids.chunks(500) {
        let mut binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
        binds.push(AVATAR_META.into());
        let rows: Vec<(i64, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT user_id, meta_value FROM user_meta WHERE user_id IN ({}) \
                 AND meta_key = ? ORDER BY id",
                vec!["?"; chunk.len()].join(", ")
            ),
            &binds,
        )
        .await?;
        for (user, value) in rows {
            if let Some(id) = serde_json::from_str::<Value>(&value)
                .ok()
                .as_ref()
                .and_then(id_of)
            {
                chosen.entry(user).or_insert(id);
            }
        }
    }
    let mut attachments: Vec<i64> = chosen.values().copied().collect();
    attachments.sort_unstable();
    attachments.dedup();
    let urls = thumbnail_urls(db, &attachments).await?;
    Ok(chosen
        .into_iter()
        .filter_map(|(user, id)| urls.get(&id).map(|url| (user, url.clone())))
        .collect())
}

/// Which of the attachments `ids` are a user's picture, for the media library's
/// [`AVATAR_LABEL`].
///
/// # Errors
///
/// The database's error.
pub async fn avatar_attachments(db: &Database, ids: &[i64]) -> Result<HashSet<i64>, Error> {
    let mut found = HashSet::new();
    for chunk in ids.chunks(500) {
        let mut binds: Vec<Sql> = vec![AVATAR_META.into()];
        binds.extend(chunk.iter().map(|id| Sql::from(id.to_string())));
        let rows: Vec<(String,)> = raw::fetch_all(
            db,
            &format!(
                "SELECT meta_value FROM user_meta WHERE meta_key = ? AND meta_value IN ({})",
                vec!["?"; chunk.len()].join(", ")
            ),
            &binds,
        )
        .await?;
        found.extend(
            rows.into_iter()
                .filter_map(|(value,)| value.trim().parse::<i64>().ok())
                .filter(|id| chunk.contains(id)),
        );
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_a_positive_number() {
        assert_eq!(id_of(&json!(12)), Some(12));
        assert_eq!(id_of(&json!("12")), Some(12));
        assert_eq!(id_of(&json!(0)), None);
        assert_eq!(id_of(&json!(-3)), None);
        assert_eq!(id_of(&json!("x")), None);
        assert_eq!(id_of(&Value::Null), None);
    }
}
