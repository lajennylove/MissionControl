//! Users' pictures through the media library (see `mission_control_core::users::avatar`), in
//! every admin theme:
//!
//! - **The picker:** the profile's picture is Ignite's picture field
//!   ([`ProfilePicture`], an image field named [`FIELD`]), which the media library's picker
//!   (`assets/editor/featured.js`, the featured image's frame with an "Upload Files" tab and the
//!   "Media Library" tab, searched and loaded 40 at a time) fills with the chosen attachment's
//!   id. A file uploaded there goes through the media library's uploader
//!   (`/mc-admin/async-upload.php`): the same checks, file store and kses as any upload. The
//!   save keeps the id in the user's meta, after checking the user may choose that image.
//! - **The address:** the attachment's thumbnail is the user's picture in the top bar
//!   (`ProfilePicture::avatar_url`, Filament's `getFilamentAvatarUrl`), on the profile and in
//!   the Users list ([`urls`]).
//! - **The pictures stored before** (Ignite's own upload, a file in its storage folder and the
//!   user's `avatar_mime`) become attachments on first use ([`migrate`]): the first time the
//!   user's picture is asked for, the file goes through the same upload checks into the media
//!   library, owned by the user, its id is stored as the picture, and Ignite's copy is removed.
//!   A picture the checks refuse is left where it is (logged), and Ignite still shows it.

use std::sync::{Arc, PoisonError, RwLock};

use ignite_panel::ignite_core::{
    ActingUser, Database, FieldDef, FieldKindDef, Row, RuntimeFuture, ValidationError,
};
use ignite_panel::{ProfilePicture, users as ignite_users};
use mission_control_core::media::{ImageSizes, Storage, create_attachment, validate};
use mission_control_core::users::UserRoles;
use mission_control_core::users::avatar::{
    avatar_attachment, avatar_urls, check_avatar, set_avatar_attachment,
};
use rocket::tokio::sync::Mutex;
use serde_json::{Value, json};

/// The picture's form field: the attachment's id.
pub(crate) const FIELD: &str = "mc_avatar";

/// The picture field's label, as WordPress's profile names the row.
pub(crate) const LABEL: &str = "Profile Picture";

/// The title of an attachment made from a picture stored before.
const MIGRATED_TITLE: &str = "Profile picture";

/// What the pictures need from the app: where uploads are stored, their sizes and limit, and
/// where users' roles come from.
#[derive(Clone)]
pub(crate) struct AvatarStore {
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) sizes: Arc<ImageSizes>,
    pub(crate) max_bytes: u64,
    pub(crate) user_roles: Arc<dyn UserRoles>,
}

/// The app's store, set when the panel mounts (the Users list reads it, as it has no Rocket
/// state).
static STORE: RwLock<Option<AvatarStore>> = RwLock::new(None);

/// One migration at a time, so a picture stored before becomes one attachment.
static MIGRATING: Mutex<()> = Mutex::const_new(());

/// Sets the app's store.
pub(crate) fn install(store: AvatarStore) {
    *STORE.write().unwrap_or_else(PoisonError::into_inner) = Some(store);
}

fn store() -> Option<AvatarStore> {
    STORE.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The picture field: an image of the media library.
pub(crate) fn field() -> FieldDef {
    FieldDef {
        name: FIELD.to_owned(),
        label: LABEL.to_owned(),
        kind: FieldKindDef::Image,
        required: false,
    }
}

/// The profile's picture: the field, its value (the attachment's id), its save and the
/// picture's address. See the [module](self).
pub(crate) fn profile_picture() -> ProfilePicture {
    ProfilePicture::make(field())
        .load(|db, user| Box::pin(load(db, user)))
        .save(|db, user, values| Box::pin(save(db, user, values)))
        .avatar_url(|db, user| Box::pin(avatar_url(db, user)))
}

async fn load<'a>(db: &'a Database, user: &'a ActingUser) -> Result<Row, orbit_orm::Error> {
    migrate(db, user.id()).await;
    let id = avatar_attachment(db, user.id()).await.map_err(meta_error)?;
    let mut row = Row::new();
    row.insert(FIELD.into(), id.map_or(Value::Null, |id| json!(id)));
    Ok(row)
}

async fn save<'a>(
    db: &'a Database,
    user: &'a ActingUser,
    values: Row,
) -> Result<Result<(), ValidationError>, orbit_orm::Error> {
    let chosen = values.get(FIELD).and_then(|value| match value {
        Value::Number(n) => n.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    });
    Ok(choose(db, user.id(), chosen)
        .await?
        .map_err(|message| ValidationError::with_messages([(FIELD, message)])))
}

/// Why `user_id` may not choose attachment `chosen` as their picture, or `None` when they may
/// (no picture is always allowed). See `mission_control_core::users::avatar::check_avatar`.
pub(crate) async fn refusal(
    db: &Database,
    user_id: i64,
    chosen: Option<i64>,
) -> Result<Option<&'static str>, orbit_orm::Error> {
    let Some(id) = chosen else {
        return Ok(None);
    };
    let Some(store) = store() else {
        log::error!("A profile picture was chosen before the app was ready");
        return Err(orbit_orm::Error::NotFound { table: "users" });
    };
    let caps = store
        .user_roles
        .roles(db, user_id)
        .await?
        .user_caps(user_id);
    check_avatar(db, &caps, id).await
}

/// Stores attachment `chosen` as `user_id`'s picture (or none), when the user may choose it;
/// otherwise the refusal's message. The WordPress theme's profile saves the picture with it too.
pub(crate) async fn choose(
    db: &Database,
    user_id: i64,
    chosen: Option<i64>,
) -> Result<Result<(), &'static str>, orbit_orm::Error> {
    if let Some(message) = refusal(db, user_id, chosen).await? {
        return Ok(Err(message));
    }
    set_avatar_attachment(db, user_id, chosen)
        .await
        .map_err(meta_error)?;
    Ok(Ok(()))
}

async fn avatar_url<'a>(
    db: &'a Database,
    user: &'a ActingUser,
) -> Result<Option<String>, orbit_orm::Error> {
    migrate(db, user.id()).await;
    Ok(avatar_urls(db, &[user.id()]).await?.remove(&user.id()))
}

/// A meta error as the database's: stored meta that is not JSON is a server error.
pub(crate) fn meta_error(error: mission_control_core::content::MetaError) -> orbit_orm::Error {
    match error {
        mission_control_core::content::MetaError::Database(e) => e,
        other => orbit_orm::Error::Database(orbit_orm::__sqlx::Error::Protocol(other.to_string())),
    }
}

/// The picture Ignite stored for `user_id` before (its upload), as an attachment of the media
/// library: once, the first time the user's picture is asked for. See the [module](self).
/// Failures are logged, and the picture stays Ignite's.
pub(crate) async fn migrate(db: &Database, user_id: i64) {
    let Some(store) = store() else {
        return;
    };
    // Without a lock, the usual answers: a picture in the media library, or none stored by
    // Ignite (its `avatar_mime`).
    if !needs_migration(db, user_id).await {
        return;
    }
    let _one = MIGRATING.lock().await;
    // Again under the lock: another request may have moved it.
    if !needs_migration(db, user_id).await {
        return;
    }
    let (mime, bytes) = match ignite_users::avatar(db, user_id).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return,
        Err(e) => {
            log::error!("Could not read the stored picture of user {user_id}: {e}");
            return;
        }
    };
    let extension = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        other => {
            log::error!("The stored picture of user {user_id} is kept: its type is {other}");
            return;
        }
    };
    let name = format!("avatar-{user_id}.{extension}");
    let validated = match validate(&name, &bytes, store.max_bytes) {
        Ok(validated) => validated,
        Err(e) => {
            log::error!(
                "The stored picture of user {user_id} is kept: the upload refused it ({e})"
            );
            return;
        }
    };
    let attachment = match create_attachment(
        db,
        &store.storage,
        &store.sizes,
        &validated,
        None,
        user_id,
        Some(MIGRATED_TITLE),
    )
    .await
    {
        Ok(attachment) => attachment,
        Err(e) => {
            log::error!("The stored picture of user {user_id} could not be moved: {e}");
            return;
        }
    };
    if let Err(e) = set_avatar_attachment(db, user_id, Some(attachment.id)).await {
        log::error!(
            "The picture of user {user_id} is attachment {}, which could not be stored: {e}",
            attachment.id
        );
        return;
    }
    if let Err(e) = ignite_users::remove_avatar(db, user_id).await {
        log::error!("Ignite's copy of the picture of user {user_id} was not removed: {e}");
    }
}

/// Whether `user_id` has no picture in the media library but one Ignite stored. A failed read
/// is logged and answers no.
async fn needs_migration(db: &Database, user_id: i64) -> bool {
    match avatar_attachment(db, user_id).await {
        Ok(None) => {}
        Ok(Some(_)) => return false,
        Err(e) => {
            log::error!("Could not read the picture of user {user_id}: {e}");
            return false;
        }
    }
    match ignite_users::find_by_id(db, user_id).await {
        Ok(user) => user.is_some_and(|user| user.avatar_mime.is_some()),
        Err(e) => {
            log::error!("Could not read user {user_id}: {e}");
            false
        }
    }
}

/// The pictures' addresses of users `ids` (the Users list), after moving the pictures stored
/// before ([`migrate`]).
pub(crate) fn urls<'a>(
    db: &'a Database,
    ids: &'a [i64],
) -> RuntimeFuture<'a, std::collections::HashMap<i64, String>> {
    Box::pin(async move {
        for id in ids {
            migrate(db, *id).await;
        }
        avatar_urls(db, ids).await
    })
}
