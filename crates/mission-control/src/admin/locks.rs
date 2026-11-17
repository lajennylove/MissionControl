//! Post locks on the admin's edit form (Task A6), as WordPress's `post.php`, the post lock
//! dialog and the heartbeat's `wp-refresh-post-lock` keep them (see
//! `mission_control_core::admin::locks`):
//!
//! - Opening a post's edit form (`GET /mc-admin/<post type>/<id>/edit`) locks it for the user
//!   and goes on to Ignite's form. When another user holds a fresh lock, the page says
//!   "<name> is currently editing this post. Do you want to take over?", with Go back and Take
//!   over, as WordPress's dialog does; nothing is changed.
//! - Take over (`POST /mc-admin/<post type>/<id>/lock`, with the session's CSRF token) locks the
//!   post for the user and opens the form, as `post.php?get-post-lock=1` does.
//! - The editor script renews the lock while the form is open and releases it when the form is
//!   left: `POST /mc-admin/admin-ajax.php?action=heartbeat` (the action also in the body, as
//!   WordPress sends it; other actions forward) answers
//!   `{"wp-refresh-post-lock": {"new_lock": ...}}`, or `{"lock_error": {"name", "text"}}` when
//!   someone took over; `action=wp-remove-post-lock` releases the user's lock.
//!
//! The lock is set only for a user Ignite will let edit the post (`edit_post`, Task A1); for
//! anyone else the request goes on to Ignite, which refuses it.

use std::sync::Arc;

use ignite_panel::{AdminUser, CsrfToken};
use mission_control_core::admin::locks::{
    check_post_lock, refresh_post_lock, release_post_lock, set_post_lock,
};
use mission_control_core::admin::{PostScreen, authorize_post_screen};
use mission_control_core::content::Post;
use mission_control_core::types::{PostTypeDef, PostTypeResources};
use mission_control_core::users::UserRoles;
use orbit_orm::{Database, Model};
use rocket::form::{Form, FromForm};
use rocket::http::{Method, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::Redirect;
use rocket::response::content::{RawHtml, RawJson};
use rocket::{Request, Route, State};
use serde_json::json;

use super::media::{escape, token_matches};
use super::{ADMIN_PATH, error_page};
use crate::rest::auth::AuthSettings;

/// The rank of the lock routes: before Ignite's edit form and its `/<slug>` routes (ranks -12
/// to -1), and before the media library's `admin-ajax.php` (-21).
const LOCK_RANK: isize = -22;

/// The lock routes, at [`LOCK_RANK`].
pub(super) fn routes() -> Vec<Route> {
    let mut routes = rocket::routes![edit_locked, take_over, admin_ajax_post, lock_on_save];
    for route in &mut routes {
        route.rank = LOCK_RANK;
    }
    routes
}

/// The post a lock request is for, when the signed-in user may edit it.
struct Editable {
    db: Arc<Database>,
    def: PostTypeDef,
    post_id: i64,
    user_id: i64,
}

/// [`Editable`] for post `id` of the post type `slug`, or `None` when it is not a post type's,
/// the post is not of the type, nobody is signed in, or the user may not edit it (Ignite then
/// answers as it does).
async fn editable(app: &App, user: &AdminUser, slug: &str, id: &str) -> Option<Editable> {
    let def = app.resources.post_type(slug)?;
    let post_id = id.parse::<i64>().ok().filter(|id| *id > 0)?;
    let db = Arc::clone(&app.db);
    let caps = match app.user_roles.roles(&db, user.user_id()).await {
        Ok(assignment) => assignment.user_caps(user.user_id()),
        Err(e) => {
            log::error!("Could not read the roles of user {}: {e}", user.user_id());
            return None;
        }
    };
    match Post::query().find(&*db, post_id).await {
        Ok(Some(post)) if post.post_type == def.slug => {}
        Ok(_) => return None,
        Err(e) => {
            log::error!("Could not read post {post_id} for its lock: {e}");
            return None;
        }
    }
    match authorize_post_screen(&db, &caps, &def, PostScreen::Edit(post_id)).await {
        Ok(None) => Some(Editable {
            db,
            def,
            post_id,
            user_id: user.user_id(),
        }),
        Ok(Some(_)) => None,
        Err(e) => {
            log::error!("Could not check the edit lock of post {post_id}: {e}");
            None
        }
    }
}

/// What the lock routes read from the app.
struct App {
    resources: Arc<PostTypeResources>,
    db: Arc<Database>,
    user_roles: Arc<dyn UserRoles>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for App {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let rocket = request.rocket();
        match (
            rocket.state::<Arc<PostTypeResources>>(),
            rocket.state::<Arc<Database>>(),
            rocket.state::<AuthSettings>(),
        ) {
            (Some(resources), Some(db), Some(settings)) => Outcome::Success(Self {
                resources: Arc::clone(resources),
                db: Arc::clone(db),
                user_roles: Arc::clone(&settings.user_roles),
            }),
            _ => Outcome::Forward(Status::NotFound),
        }
    }
}

/// Another user's lock on the post an edit form is for. When the post is free, the guard locks
/// it for the user and forwards, so Ignite's form answers.
struct LockedBy {
    slug: String,
    post_id: i64,
    name: String,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for LockedBy {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let (Some(slug), Some(id)) = (request.routed_segment(0), request.routed_segment(1)) else {
            return Outcome::Forward(Status::NotFound);
        };
        let Outcome::Success(user) = request.guard::<AdminUser>().await else {
            return Outcome::Forward(Status::NotFound);
        };
        let Outcome::Success(app) = request.guard::<App>().await else {
            return Outcome::Forward(Status::NotFound);
        };
        let Some(post) = editable(&app, &user, slug, id).await else {
            return Outcome::Forward(Status::NotFound);
        };
        // `post.php`: someone else's fresh lock shows the dialog; otherwise the post is locked
        // for this user.
        match check_post_lock(&post.db, post.post_id, post.user_id).await {
            Ok(Some((_, name))) => Outcome::Success(Self {
                slug: post.def.slug,
                post_id: post.post_id,
                name,
            }),
            // `HEAD` looks without locking.
            Ok(None) if request.method() == Method::Head => Outcome::Forward(Status::NotFound),
            Ok(None) => {
                if let Err(e) = set_post_lock(&post.db, post.post_id, post.user_id).await {
                    log::error!("Could not lock post {}: {e}", post.post_id);
                }
                Outcome::Forward(Status::NotFound)
            }
            Err(e) => {
                log::error!("Could not read the lock of post {}: {e}", post.post_id);
                Outcome::Forward(Status::NotFound)
            }
        }
    }
}

/// A save of a post's edit form (`POST /<post type>/<id>`): the post is locked for the user,
/// as WordPress's `edit_post` calls `wp_set_post_lock`, and the request forwards to Ignite's
/// save. It never answers itself.
struct LockOnSave;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for LockOnSave {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        if let (Some(slug), Some(id), Outcome::Success(user), Outcome::Success(app)) = (
            request.routed_segment(0),
            request.routed_segment(1),
            request.guard::<AdminUser>().await,
            request.guard::<App>().await,
        ) && let Some(post) = editable(&app, &user, slug, id).await
            && let Err(e) = set_post_lock(&post.db, post.post_id, post.user_id).await
        {
            log::error!("Could not lock post {}: {e}", post.post_id);
        }
        Outcome::Forward(Status::NotFound)
    }
}

/// Locks the post on a save of its form, then forwards to Ignite's save ([`LockOnSave`]).
#[rocket::post("/<_>/<_>")]
fn lock_on_save(_lock: LockOnSave) -> Status {
    Status::NotFound
}

/// The edit form of a post another user is editing: WordPress's post lock dialog, as a page.
#[rocket::get("/<_>/<_>/edit")]
fn edit_locked(locked: LockedBy, token: CsrfToken) -> RawHtml<String> {
    let slug = &locked.slug;
    let id = locked.post_id;
    RawHtml(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>This content is currently locked.</title>
</head>
<body>
<main>
<div class="post-locked-message" role="alertdialog" aria-labelledby="mc-post-locked">
<p id="mc-post-locked">{name} is currently editing this post. Do you want to take over?</p>
<form method="post" action="{ADMIN_PATH}/{slug}/{id}/lock">
<input type="hidden" name="_token" value="{token}">
<p><a href="{ADMIN_PATH}/{slug}">Go back</a> <button type="submit">Take over</button></p>
</form>
</div>
</main>
</body>
</html>"#,
        name = escape(&locked.name),
        token = escape(token.value()),
    ))
}

/// The take-over form.
#[derive(FromForm)]
struct TakeOver {
    #[field(name = "_token")]
    token: Option<String>,
}

/// `POST /mc-admin/<post type>/<id>/lock`: Take over, as `post.php?get-post-lock=1`: the post is
/// locked for the user, and its form opens (303).
#[rocket::post("/<slug>/<id>/lock", data = "<form>")]
async fn take_over(
    slug: &str,
    id: &str,
    form: Form<TakeOver>,
    token: CsrfToken,
    user: AdminUser,
    app: App,
) -> Result<Redirect, (Status, RawHtml<String>)> {
    if !token_matches(&token, form.token.as_deref()) {
        return Err((
            Status::new(419),
            error_page(
                "Error",
                None,
                "The link you followed has expired. Please try again.",
            ),
        ));
    }
    let Some(post) = editable(&app, &user, slug, id).await else {
        return Err((
            Status::Forbidden,
            error_page(
                "Error",
                None,
                "Sorry, you are not allowed to edit this item.",
            ),
        ));
    };
    if let Err(e) = set_post_lock(&post.db, post.post_id, post.user_id).await {
        log::error!("Could not lock post {}: {e}", post.post_id);
        return Err((
            Status::InternalServerError,
            error_page(
                "Error",
                None,
                "Something went wrong. Try again in a moment.",
            ),
        ));
    }
    Ok(Redirect::to(format!(
        "{ADMIN_PATH}/{}/{}/edit",
        post.def.slug, post.post_id
    )))
}

/// The editor script's lock requests.
#[derive(FromForm)]
struct AjaxForm {
    action: Option<String>,
    /// The lock this tab holds (`active_post_lock`), for a release.
    active_post_lock: Option<String>,
    post_id: Option<String>,
    post_type: Option<String>,
    #[field(name = "_token")]
    token: Option<String>,
}

/// `POST /mc-admin/admin-ajax.php`: `action=heartbeat` renews the user's lock on `post_id`
/// (`wp_refresh_post_lock`), and `action=wp-remove-post-lock` releases it; see the
/// [module](self). Any other action forwards ([`LockAction`]).
/// The lock actions of `admin-ajax.php`, named in the query (`?action=heartbeat`): any other
/// action forwards, so another module can answer it.
struct LockAction;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for LockAction {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        match request.query_value::<&str>("action").and_then(Result::ok) {
            Some("heartbeat" | "wp-remove-post-lock") => Outcome::Success(Self),
            _ => Outcome::Forward(Status::NotFound),
        }
    }
}

#[rocket::post("/admin-ajax.php", data = "<form>")]
async fn admin_ajax_post(
    _action: LockAction,
    form: Form<AjaxForm>,
    user: Option<AdminUser>,
    token: CsrfToken,
    app: App,
    db: &State<Arc<Database>>,
) -> Result<RawJson<String>, (Status, RawJson<String>)> {
    let action = form.action.as_deref().unwrap_or_default();
    if !matches!(action, "heartbeat" | "wp-remove-post-lock") {
        return Err((Status::BadRequest, RawJson("0".to_owned())));
    }
    let Some(user) = user else {
        // Signed out: WordPress's heartbeat says so (`wp-auth-check`).
        return Err((
            Status::Unauthorized,
            RawJson(json!({"wp-auth-check": false}).to_string()),
        ));
    };
    if !token_matches(&token, form.token.as_deref()) {
        return Err((Status::new(419), RawJson("-1".to_owned())));
    }
    let (Some(post_type), Some(post_id)) = (form.post_type.as_deref(), form.post_id.as_deref())
    else {
        return Ok(RawJson("{}".to_owned()));
    };
    // A post the user may not edit gets no lock data, as `wp_refresh_post_lock` answers.
    let Some(post) = editable(&app, &user, post_type, post_id).await else {
        return Ok(RawJson("{}".to_owned()));
    };
    let failed = |e: orbit_orm::Error| {
        log::error!("Could not refresh the lock of post {}: {e}", post.post_id);
        (Status::InternalServerError, RawJson("0".to_owned()))
    };
    if action == "wp-remove-post-lock" {
        release_post_lock(
            db,
            post.post_id,
            post.user_id,
            form.active_post_lock.as_deref(),
        )
        .await
        .map_err(failed)?;
        return Ok(RawJson("1".to_owned()));
    }
    let lock = refresh_post_lock(db, post.post_id, post.user_id)
        .await
        .map_err(failed)?;
    Ok(RawJson(json!({ "wp-refresh-post-lock": lock }).to_string()))
}
