//! The profile page (`wp-admin/profile.php`, Task 8.5) in both admin themes, at the panel's
//! profile address, `/mc-admin/profile` (the Users menu's "Your Profile", and the admin bar's
//! "Edit Profile"). Both themes show WordPress's fields, read and saved the same way (see
//! `mission_control_core::users::profile`, `edit_user`'s rules and messages), the picture from
//! the media library (the `avatars` submodule), the sessions and the application passwords.
//!
//! # The WordPress theme
//!
//! The fields are WordPress's, in its order: Personal Options (Visual Editor, Admin Theme in
//! place of WordPress's Admin Color Scheme, Keyboard Shortcuts, Toolbar), Name (Username, read
//! only, First Name, Last Name, Nickname, Display name publicly as), Contact Info (Email,
//! Website), About Yourself (Biographical Info, Profile Picture), Account Management (New
//! Password with "Set New Password" and "Generate Password", Sessions with "Log Out Everywhere
//! Else"), Application Passwords, then "Update Profile". WordPress's Language row shows only
//! when a language besides English is installed; Mission Control installs none, so the row is
//! left out.
//!
//! - **The page:** Ignite renders its profile route through the theme's `profile` view
//!   (`views/admin/wordpress/profile.propel`), which has the panel's chrome (menu, admin bar,
//!   Ignite's notices) and a marker in place of the form. [`ProfileView`], a response fairing,
//!   puts WordPress's form (`profile_form.propel`) there, built from the user's profile meta,
//!   picture, sessions and application passwords, which Ignite's page values do not hold.
//! - **The save:** `POST /mc-admin/profile` is answered here, before Ignite's route, when the
//!   user's page renders in this theme ([`WordPressPage`] forwards otherwise, so the Ignite
//!   theme's form reaches Ignite). The picture chosen in the picker is saved with it. A save
//!   goes back to `/mc-admin/profile?updated=1`, which shows "Profile updated." as WordPress
//!   does. A refused form goes back to the page with the errors on their fields and the values
//!   typed, as Laravel's `back()->withErrors()->withInput()` flashes them: the page cannot be
//!   rendered here, as Ignite holds its chrome.
//!
//! # The Ignite theme
//!
//! Ignite's profile page with the same fields ([`profile_fields`], Ignite's `ProfileFields`,
//! loaded and saved through the same `Profile` and `ProfileForm`), which [`IgniteProfileView`]
//! puts in WordPress's sections: Personal Options, Name (Ignite's name field is the username,
//! shown read only with "Usernames cannot be changed."; a name sent anyway is not kept), Contact
//! Info, About Yourself, then "Update Profile". Ignite's password card is Account Management,
//! with WordPress's Sessions under it, and the Application Passwords follow as a section of
//! their own, with the same forms as the WordPress theme's. The picture is Ignite's picture
//! card with the media library's picker.
//!
//! # The other forms
//!
//! They post to their own routes and come back to the page, in both themes: "Log Out
//! Everywhere Else" to `/profile/destroy-sessions` (WordPress's `destroy-sessions` action); the
//! application passwords to `/profile/application-passwords` and its `revoke` and `revoke-all`,
//! with WordPress's messages, the new password shown once.
//!
//! The flashed values are kept in memory, by the sign-in session, until the next page shows
//! them (at most [`NOTICE_TTL`]): one process's memory, as the panel's sessions are one
//! database's.

use std::collections::HashMap;
use std::io::{self, Cursor};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ignite_panel::ignite_core::{ActingUser, FieldDef, FieldKindDef, Row, ValidationError};
use ignite_panel::theme::AdminTheme;
use ignite_panel::{AdminUser, CsrfForm, CsrfToken, ProfileFields, users as ignite_users};
use mission_control_core::content::MetaError;
use mission_control_core::users::app_password::{self, AppPasswordError, PlainPassword};
use mission_control_core::users::avatar::{NOT_AN_IMAGE, avatar_attachment, set_avatar_attachment};
use mission_control_core::users::profile::{
    COMMENT_SHORTCUTS, DESCRIPTION, DISPLAY_NAME, EMAIL_EXISTS, FIRST_NAME, LAST_NAME, NICKNAME,
    PROFILE_UPDATED, Profile, ProfileForm, RICH_EDITING, ignite_session_id,
    log_out_everywhere_else, other_sessions,
};
use orbit_orm::{Database, Value as Sql, raw};
use propel_runtime::Loader;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, CookieJar, Method, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::Redirect;
use rocket::route::Route;
use rocket::{Request, Response, State};
use serde_json::{Value, json};

use super::ADMIN_PATH;
use super::avatars;
use super::wordpress_theme::{NAME as WORDPRESS, WordPressTheme};
use crate::editor::{editor_stylesheet_url, image_picker_script_url};

/// Where the theme's `profile` view puts the form.
pub(crate) const MARKER: &str = "<!-- mc-wordpress-profile -->";

/// The rank of the profile save, before Ignite's `POST /profile`.
const PROFILE_RANK: isize = -20;

/// How long a flashed notice waits for the page that shows it.
pub(crate) const NOTICE_TTL: Duration = Duration::from_secs(600);

/// Ignite's session cookie.
const SESSION_COOKIE: &str = "ignite_session";

/// WordPress's message after "Log Out Everywhere Else" (`wp_ajax_destroy_sessions`).
const LOGGED_OUT_ELSEWHERE: &str = "You are now logged out everywhere else.";
/// WordPress's messages after revoking application passwords (`application-passwords.js`).
const REVOKED: &str = "Application password revoked.";
const REVOKED_ALL: &str = "All application passwords revoked.";

/// The profile page's address.
fn profile_url() -> String {
    format!("{ADMIN_PATH}/profile")
}

/// What one page shows once: a message, a refused form's errors and values, a new application
/// password.
#[derive(Default)]
struct Notice {
    message: Option<String>,
    /// `(field, message)`.
    errors: Vec<(&'static str, String)>,
    /// The values the refused form held.
    old: Option<Profile>,
    /// A new application password's name and the password.
    new_app_password: Option<(String, PlainPassword)>,
    /// The application password name a refused create held, and why it was refused.
    app_password: Option<(String, String)>,
}

/// The notices waiting for their page, by sign-in session.
#[derive(Default)]
pub(crate) struct ProfileNotices(Mutex<HashMap<String, (Instant, Notice)>>);

impl ProfileNotices {
    fn put(&self, key: String, notice: Notice) {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, (at, _)| at.elapsed() < NOTICE_TTL);
        map.insert(key, (Instant::now(), notice));
    }

    fn take(&self, key: &str) -> Notice {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match map.remove(key) {
            Some((at, notice)) if at.elapsed() < NOTICE_TTL => notice,
            _ => Notice::default(),
        }
    }
}

/// The sign-in session of the request: the key of its notices, and the session "Log Out
/// Everywhere Else" keeps.
pub(crate) fn session_key(cookies: &CookieJar<'_>, user: &AdminUser) -> String {
    cookies
        .get(SESSION_COOKIE)
        .and_then(|cookie| ignite_session_id(cookie.value()))
        .unwrap_or_else(|| format!("user-{}", user.user_id()))
}

/// A signed-in user whose pages render in the WordPress theme. Any other request is forwarded,
/// to Ignite's own route.
pub(crate) struct WordPressPage(AdminUser);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WordPressPage {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        match request.guard::<AdminUser>().await {
            Outcome::Success(user) if user.admin_theme() == WORDPRESS => {
                Outcome::Success(WordPressPage(user))
            }
            _ => Outcome::Forward(Status::NotFound),
        }
    }
}

/// The profile's routes: the save (before Ignite's), sessions and application passwords.
pub(crate) fn routes() -> Vec<Route> {
    let mut routes = rocket::routes![update_profile];
    for route in &mut routes {
        route.rank = PROFILE_RANK;
    }
    routes.extend(rocket::routes![
        destroy_sessions,
        create_application_password,
        revoke_application_password,
        revoke_all_application_passwords
    ]);
    routes
}

/// A database error, logged, as a 500.
fn server_error(error: impl std::fmt::Display) -> Status {
    log::error!("The profile could not be saved: {error}");
    Status::InternalServerError
}

/// Saves the profile, as `edit_user` and `wp_update_user` do. See the [module](self).
#[rocket::post("/profile", data = "<form>")]
async fn update_profile(
    page: WordPressPage,
    cookies: &CookieJar<'_>,
    db: &State<Database>,
    notices: &State<ProfileNotices>,
    form: CsrfForm,
) -> Result<Redirect, Status> {
    let user = page.0;
    let id = user.user_id();
    let db = db.inner();
    let row = ignite_users::find_by_id(db, id)
        .await
        .map_err(server_error)?
        .ok_or(Status::NotFound)?;
    let current = Profile::load(db, id, &row.name, &row.email)
        .await
        .map_err(server_error)?;
    let read = ProfileForm::read(|name| form.get(name).map(str::to_owned), &current);
    let mut errors: Vec<(&'static str, String)> = read
        .errors
        .iter()
        .map(|(field, message)| (*field, (*message).to_owned()))
        .collect();
    let profile = read.profile;
    let email_changed = profile.email != row.email;
    if errors.is_empty()
        && email_changed
        && ignite_users::email_taken(db, &profile.email, id)
            .await
            .map_err(server_error)?
    {
        errors.push(("email", EMAIL_EXISTS.to_owned()));
    }
    // The picture, chosen in the media library's picker (the `avatars` submodule): saved with the
    // profile, as WordPress's avatar plugins save theirs with "Update Profile".
    // A blank field is not sent on (Ignite's form input): the row's marker says it was shown.
    let shown = form.get("mc_avatar_shown").is_some();
    let avatar = match form.get(avatars::FIELD).map(str::trim) {
        None | Some("") if shown => Some(None),
        None => None,
        Some("") => Some(None),
        Some(text) => match text.parse::<i64>().ok().filter(|id| *id > 0) {
            Some(chosen) => Some(Some(chosen)),
            None => {
                errors.push((avatars::FIELD, NOT_AN_IMAGE.to_owned()));
                None
            }
        },
    };
    let stored_avatar = avatar_attachment(db, id).await.map_err(server_error)?;
    let avatar = avatar.filter(|chosen| *chosen != stored_avatar);
    if errors.is_empty()
        && let Some(chosen) = avatar
        && let Some(message) = avatars::refusal(db, id, chosen)
            .await
            .map_err(server_error)?
    {
        errors.push((avatars::FIELD, message.to_owned()));
    }
    if errors.is_empty() && email_changed {
        match raw::execute(
            db,
            "UPDATE users SET email = ? WHERE id = ?",
            &[Sql::from(profile.email.as_str()), id.into()],
        )
        .await
        {
            Ok(_) => {}
            // Two saves can both pass the check above; the unique index refuses the second.
            Err(orbit_orm::Error::UniqueViolation { .. }) => {
                errors.push(("email", EMAIL_EXISTS.to_owned()));
            }
            Err(e) => return Err(server_error(e)),
        }
    }
    let key = session_key(cookies, &user);
    if !errors.is_empty() {
        notices.put(
            key,
            Notice {
                errors,
                old: Some(profile),
                ..Notice::default()
            },
        );
        return Ok(Redirect::to(profile_url()));
    }
    profile.save_meta(db, id).await.map_err(server_error)?;
    if let Some(chosen) = avatar {
        set_avatar_attachment(db, id, chosen)
            .await
            .map_err(server_error)?;
    }
    // The admin theme, Mission Control's Admin Color Scheme: blank is the site's default, and a
    // name no theme has is left as it was.
    let theme = form.get("admin_theme").map(str::trim);
    if theme.is_none_or(ignite_panel::theme::is_registered_admin_theme) {
        ignite_users::set_admin_theme(db, id, theme)
            .await
            .map_err(server_error)?;
    }
    if let Some(password) = read.new_password {
        ignite_users::update_password(db, id, &password)
            .await
            .map_err(server_error)?;
        // The other sign-ins end, as a new password ends them in WordPress and in Ignite.
        log_out_everywhere_else(db, id, &key)
            .await
            .map_err(server_error)?;
    }
    Ok(Redirect::to(format!("{}?updated=1", profile_url())))
}

/// "Log Out Everywhere Else": ends the user's other sign-in sessions.
#[rocket::post("/profile/destroy-sessions", data = "<form>")]
async fn destroy_sessions(
    user: AdminUser,
    cookies: &CookieJar<'_>,
    db: &State<Database>,
    notices: &State<ProfileNotices>,
    form: CsrfForm,
) -> Result<Redirect, Status> {
    let _ = form;
    let key = session_key(cookies, &user);
    log_out_everywhere_else(db.inner(), user.user_id(), &key)
        .await
        .map_err(server_error)?;
    notices.put(
        key,
        Notice {
            message: Some(LOGGED_OUT_ELSEWHERE.to_owned()),
            ..Notice::default()
        },
    );
    Ok(Redirect::to(profile_url()))
}

/// The address of the page's Application Passwords section.
fn application_passwords_url() -> String {
    format!("{}#application-passwords-section", profile_url())
}

/// WordPress's message for a refused application password (`WP_Application_Passwords`).
fn app_password_message(error: &AppPasswordError) -> Option<&'static str> {
    match error {
        AppPasswordError::EmptyName => {
            Some("An application name is required to create an application password.")
        }
        AppPasswordError::DuplicateName => Some("Each application name should be unique."),
        AppPasswordError::NameTooLong => {
            Some("The application name may not be longer than 255 characters.")
        }
        _ => None,
    }
}

/// "Add Application Password": creates one and shows it once.
#[rocket::post("/profile/application-passwords", data = "<form>")]
async fn create_application_password(
    user: AdminUser,
    cookies: &CookieJar<'_>,
    db: &State<Database>,
    notices: &State<ProfileNotices>,
    form: CsrfForm,
) -> Result<Redirect, Status> {
    let name = form
        .get("new_application_password_name")
        .unwrap_or_default()
        .to_owned();
    let key = session_key(cookies, &user);
    let notice = match app_password::create(db.inner(), user.user_id(), &name, None).await {
        Ok((plain, item)) => Notice {
            new_app_password: Some((item.name, plain)),
            ..Notice::default()
        },
        Err(error) => match app_password_message(&error) {
            Some(message) => Notice {
                app_password: Some((name, message.to_owned())),
                ..Notice::default()
            },
            None => return Err(server_error(error)),
        },
    };
    notices.put(key, notice);
    Ok(Redirect::to(application_passwords_url()))
}

/// "Revoke": revokes one of the user's application passwords, by its uuid.
#[rocket::post("/profile/application-passwords/revoke", data = "<form>")]
async fn revoke_application_password(
    user: AdminUser,
    cookies: &CookieJar<'_>,
    db: &State<Database>,
    notices: &State<ProfileNotices>,
    form: CsrfForm,
) -> Result<Redirect, Status> {
    let uuid = form.get("uuid").unwrap_or_default();
    let revoked = app_password::revoke(db.inner(), user.user_id(), uuid)
        .await
        .map_err(server_error)?;
    if revoked {
        notices.put(
            session_key(cookies, &user),
            Notice {
                message: Some(REVOKED.to_owned()),
                ..Notice::default()
            },
        );
    }
    Ok(Redirect::to(application_passwords_url()))
}

/// "Revoke all application passwords".
#[rocket::post("/profile/application-passwords/revoke-all", data = "<form>")]
async fn revoke_all_application_passwords(
    user: AdminUser,
    cookies: &CookieJar<'_>,
    db: &State<Database>,
    notices: &State<ProfileNotices>,
    form: CsrfForm,
) -> Result<Redirect, Status> {
    let _ = form;
    app_password::revoke_all(db.inner(), user.user_id())
        .await
        .map_err(server_error)?;
    notices.put(
        session_key(cookies, &user),
        Notice {
            message: Some(REVOKED_ALL.to_owned()),
            ..Notice::default()
        },
    );
    Ok(Redirect::to(application_passwords_url()))
}

/// The view of the form, built into the binary.
struct Views;

impl Loader for Views {
    fn source(&self, name: &str) -> io::Result<String> {
        match name {
            "profile.form" => {
                Ok(include_str!("../../views/admin/wordpress/profile_form.propel").to_owned())
            }
            "profile.sessions" => {
                Ok(include_str!("../../views/admin/ignite/profile_sessions.propel").to_owned())
            }
            "profile.app_passwords" => Ok(include_str!(
                "../../views/admin/ignite/profile_app_passwords.propel"
            )
            .to_owned()),
            _ => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}

/// The initials Ignite shows without a picture: the first letter of the first two words.
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

/// The admin theme select: the site's default first, then each registered theme, by label.
fn admin_theme_options(choice: Option<&str>) -> Vec<Value> {
    let names = ignite_panel::registered_admin_themes();
    let label = |name: &str| match name {
        WORDPRESS => WordPressTheme.label(),
        "ignite" => "Ignite".to_owned(),
        other => other.to_owned(),
    };
    let chosen = choice.filter(|name| names.iter().any(|known| known == name));
    let default = ignite_panel::active_admin_theme();
    let mut options = vec![json!({
        "value": "",
        "label": format!("Site default ({})", label(&default)),
        "selected": chosen.is_none(),
    })];
    options.extend(names.iter().map(|name| {
        json!({
            "value": name,
            "label": label(name),
            "selected": chosen == Some(name.as_str()),
        })
    }));
    options
}

/// The form's values for `user`, signed in with session `key`, showing `notice`.
async fn form_context(
    db: &Database,
    user: &AdminUser,
    key: &str,
    csrf: &str,
    updated: bool,
    notice: Notice,
) -> Result<Value, String> {
    let id = user.user_id();
    let row = ignite_users::find_by_id(db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the user is gone")?;
    let stored = Profile::load(db, id, &row.name, &row.email)
        .await
        .map_err(|e| e.to_string())?;
    let profile = notice.old.unwrap_or(stored);
    let error = |field: &str| {
        notice
            .errors
            .iter()
            .find(|(name, _)| *name == field)
            .map(|(_, message)| message.clone())
    };
    let others = other_sessions(db, id, key)
        .await
        .map_err(|e| e.to_string())?;
    let avatar_id = avatar_attachment(db, id).await.map_err(|e| e.to_string())?;
    let passwords = app_password::list(db, id)
        .await
        .map_err(|e| e.to_string())?;
    let dash = "\u{2014}".to_owned();
    let app_passwords: Vec<Value> = passwords
        .iter()
        .map(|item| {
            json!({
                "uuid": item.uuid,
                "form_id": format!("mc-revoke-{}", item.uuid),
                "name": item.name,
                "created": item.created_at.format("%B %-d, %Y").to_string(),
                "last_used": item
                    .last_used_at
                    .map_or_else(|| dash.clone(), |at| at.format("%B %-d, %Y").to_string()),
                "last_ip": item.last_ip.clone().unwrap_or_else(|| dash.clone()),
            })
        })
        .collect();
    let display_names: Vec<Value> = profile
        .display_name_choices()
        .into_iter()
        .map(|name| json!({ "selected": name == profile.display_name, "name": name }))
        .collect();
    let message = notice
        .message
        .or_else(|| updated.then(|| PROFILE_UPDATED.to_owned()));
    let (app_password_name, app_password_error) = notice.app_password.unwrap_or_default();
    let (new_app_password_name, new_app_password) = notice
        .new_app_password
        .map(|(name, password)| (name, password.chunked().to_string()))
        .unwrap_or_default();
    let mut context = json!({
        "csrf_token": csrf,
        "action": profile_url(),
        "has_message": message.is_some(),
        "message": message.unwrap_or_default(),
        "has_errors": !notice.errors.is_empty(),
        "errors": notice
            .errors
            .iter()
            .map(|(_, text)| json!({ "text": text }))
            .collect::<Vec<_>>(),
        "rich_editing_disabled": !profile.rich_editing,
        "comment_shortcuts": profile.comment_shortcuts,
        "admin_bar_front": profile.admin_bar_front,
        "has_admin_themes": ignite_panel::registered_admin_themes().len() > 1,
        "admin_themes": admin_theme_options(user.admin_theme_choice()),
        "username": profile.username,
        "first_name": profile.first_name,
        "last_name": profile.last_name,
        "nickname": profile.nickname,
        "display_names": display_names,
        "email": profile.email,
        "url": profile.url,
        "description": profile.description,
        // The picture: an image of the media library (the `avatars` submodule); a picture
        // Ignite kept that could not be moved there still shows.
        "has_avatar": user.avatar_url().is_some() || user.has_avatar(),
        "avatar_url": user.avatar_url().map_or_else(
            || format!("{ADMIN_PATH}/profile/avatar?v={}", chrono_millis()),
            str::to_owned,
        ),
        "user_initials": initials(&row.name),
        "avatar_id": avatar_id.map_or_else(String::new, |id| id.to_string()),
        "picker_stylesheet": editor_stylesheet_url(),
        "picker_script": image_picker_script_url(),
        "has_other_sessions": others > 0,
        "destroy_sessions_action": format!("{ADMIN_PATH}/profile/destroy-sessions"),
        "has_new_app_password": !new_app_password.is_empty(),
        "new_app_password_name": new_app_password_name,
        "new_app_password": new_app_password,
        "app_password_action": format!("{ADMIN_PATH}/profile/application-passwords"),
        "app_password_name": app_password_name,
        "has_app_password_error": !app_password_error.is_empty(),
        "app_password_error": app_password_error,
        "has_app_passwords": !app_passwords.is_empty(),
        "app_passwords": app_passwords,
        "revoke_action": format!("{ADMIN_PATH}/profile/application-passwords/revoke"),
        "revoke_all_action": format!("{ADMIN_PATH}/profile/application-passwords/revoke-all"),
    });
    for field in ["nickname", "email", "url", "pass1", avatars::FIELD] {
        let message = error(field);
        context[format!("has_{field}_error")] = json!(message.is_some());
        context[format!("{field}_error")] = json!(message.unwrap_or_default());
    }
    Ok(context)
}

/// Milliseconds since the epoch: a picture URL that changes, so the browser fetches the
/// picture again after a change, as Ignite's layout does.
fn chrono_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

/// Puts WordPress's profile form in the WordPress theme's profile page. See the [module](self).
pub(crate) struct ProfileView;

#[rocket::async_trait]
impl Fairing for ProfileView {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control WordPress profile",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if request.method() != Method::Get
            || request.uri().path().as_str() != profile_url()
            || response.status() != Status::Ok
            || response.content_type() != Some(ContentType::HTML)
        {
            return;
        }
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        let html = match fill(request, &html).await {
            Some(filled) => filled,
            None => html,
        };
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}

/// The page `html` with the form in place of the marker, or `None` when it has no marker (the
/// Ignite theme's page) or the form cannot be built (logged).
async fn fill(request: &Request<'_>, html: &str) -> Option<String> {
    if !html.contains(MARKER) {
        return None;
    }
    let Outcome::Success(user) = request.guard::<AdminUser>().await else {
        return None;
    };
    let Outcome::Success(csrf) = request.guard::<CsrfToken>().await else {
        return None;
    };
    let db = request.rocket().state::<Database>()?;
    let notices = request.rocket().state::<ProfileNotices>()?;
    let key = session_key(request.cookies(), &user);
    let notice = notices.take(&key);
    let updated = request
        .uri()
        .query()
        .is_some_and(|query| query.segments().any(|(k, v)| k == "updated" && v == "1"));
    let context = match form_context(db, &user, &key, csrf.value(), updated, notice).await {
        Ok(context) => context,
        Err(e) => {
            log::error!("The profile form could not be built: {e}");
            return None;
        }
    };
    match propel_runtime::render(&Views, "profile.form", &context) {
        Ok(form) => Some(html.replacen(MARKER, &form, 1)),
        Err(e) => {
            log::error!("The profile form could not be rendered: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The Ignite theme

/// The form field of "Visual Editor" (ticked turns the editor off), "Keyboard Shortcuts" and
/// "Toolbar": WordPress's names, as its profile form posts them.
const ADMIN_BAR_FRONT: &str = "admin_bar_front";

/// The website's form field (the `user_url` user meta).
const URL_FIELD: &str = "url";

fn field(name: &str, label: &str, kind: FieldKindDef) -> FieldDef {
    FieldDef {
        name: name.to_owned(),
        label: label.to_owned(),
        kind,
        required: false,
    }
}

/// WordPress's profile fields that are not Ignite's (Ignite's are the name, the username here,
/// and the email), in WordPress's order, loaded and saved as the WordPress theme's form loads
/// and saves them: through `Profile::load`, `ProfileForm::read` (its checks and messages) and
/// `Profile::save_meta`. "Display name publicly as" is a lookup of the names the user can
/// display ([`Profile::display_name_choices`]). See the [module](self).
pub(crate) fn profile_fields() -> ProfileFields {
    ProfileFields::make()
        .field(field(
            RICH_EDITING,
            "Disable the visual editor when writing",
            FieldKindDef::Checkbox,
        ))
        .field(field(
            COMMENT_SHORTCUTS,
            "Enable keyboard shortcuts for comment moderation.",
            FieldKindDef::Checkbox,
        ))
        .field(field(
            ADMIN_BAR_FRONT,
            "Show Toolbar when viewing site",
            FieldKindDef::Checkbox,
        ))
        .field(field(FIRST_NAME, "First Name", FieldKindDef::Text))
        .field(field(LAST_NAME, "Last Name", FieldKindDef::Text))
        // Required: `ProfileForm` refuses a blank one with WordPress's "Please enter a nickname.".
        .field(field(NICKNAME, "Nickname (required)", FieldKindDef::Text))
        .field(field(
            DISPLAY_NAME,
            "Display name publicly as",
            FieldKindDef::Lookup,
        ))
        // Text, not Ignite's URL field: `ProfileForm` checks it as `edit_user` does, so
        // `example.com` is `http://example.com`, as WordPress's form (`novalidate`) takes it.
        .field(field(URL_FIELD, "Website", FieldKindDef::Text))
        .field(field(
            DESCRIPTION,
            "Biographical Info",
            FieldKindDef::Textarea,
        ))
        .load(|db, user| Box::pin(load_fields(db, user)))
        .save(|db, user, values| Box::pin(save_fields(db, user, values)))
        .options(|db, user, name| Box::pin(field_options(db, user, name)))
}

/// A meta error as the database's (see `avatars::meta_error`).
fn meta_error(error: MetaError) -> orbit_orm::Error {
    avatars::meta_error(error)
}

/// The stored profile of the acting user.
async fn stored_profile(db: &Database, user: &ActingUser) -> Result<Profile, orbit_orm::Error> {
    let row = ignite_users::find_by_id(db, user.id())
        .await?
        .ok_or(orbit_orm::Error::NotFound { table: "users" })?;
    Profile::load(db, user.id(), &row.name, &row.email)
        .await
        .map_err(meta_error)
}

async fn load_fields<'a>(db: &'a Database, user: &'a ActingUser) -> Result<Row, orbit_orm::Error> {
    let profile = stored_profile(db, user).await?;
    let mut row = Row::new();
    row.insert(RICH_EDITING.into(), json!(!profile.rich_editing));
    row.insert(COMMENT_SHORTCUTS.into(), json!(profile.comment_shortcuts));
    row.insert(ADMIN_BAR_FRONT.into(), json!(profile.admin_bar_front));
    row.insert(FIRST_NAME.into(), json!(profile.first_name));
    row.insert(LAST_NAME.into(), json!(profile.last_name));
    row.insert(NICKNAME.into(), json!(profile.nickname));
    row.insert(DISPLAY_NAME.into(), json!(profile.display_name));
    row.insert(URL_FIELD.into(), json!(profile.url));
    row.insert(DESCRIPTION.into(), json!(profile.description));
    Ok(row)
}

/// The values of `values` as the WordPress form posts them, for `ProfileForm::read`: a ticked
/// "Disable the visual editor" is `rich_editing=false`, a ticked "Keyboard Shortcuts"
/// `comment_shortcuts=true`, a ticked "Toolbar" `admin_bar_front=1`; the email is the stored
/// one (Ignite saves the email field), and no password is set here (Ignite's password card). A
/// field the form did not send keeps its stored value in `current`, and one sent blank is empty.
fn posted(values: &Row, name: &str, current: &Profile) -> Option<String> {
    let ticked = |key: &str, stored: bool| {
        values
            .get(key)
            .map_or(stored, |value| value.as_bool().unwrap_or(false))
    };
    let text = |key: &str, stored: &str| match values.get(key) {
        None => stored.to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    };
    match name {
        RICH_EDITING => Some(
            if ticked(RICH_EDITING, !current.rich_editing) {
                "false"
            } else {
                "true"
            }
            .to_owned(),
        ),
        COMMENT_SHORTCUTS => {
            ticked(COMMENT_SHORTCUTS, current.comment_shortcuts).then(|| "true".to_owned())
        }
        ADMIN_BAR_FRONT => ticked(ADMIN_BAR_FRONT, current.admin_bar_front).then(|| "1".to_owned()),
        "email" => Some(current.email.clone()),
        "pass1" | "pass2" => None,
        FIRST_NAME => Some(text(FIRST_NAME, &current.first_name)),
        LAST_NAME => Some(text(LAST_NAME, &current.last_name)),
        NICKNAME => Some(text(NICKNAME, &current.nickname)),
        DISPLAY_NAME => Some(text(DISPLAY_NAME, &current.display_name)),
        URL_FIELD => Some(text(URL_FIELD, &current.url)),
        DESCRIPTION => Some(text(DESCRIPTION, &current.description)),
        other => values.get(other).and_then(Value::as_str).map(str::to_owned),
    }
}

async fn save_fields<'a>(
    db: &'a Database,
    user: &'a ActingUser,
    values: Row,
) -> Result<Result<(), ValidationError>, orbit_orm::Error> {
    let current = stored_profile(db, user).await?;
    let read = ProfileForm::read(|name| posted(&values, name, &current), &current);
    if !read.errors.is_empty() {
        return Ok(Err(ValidationError::with_messages(read.errors)));
    }
    let mut profile = read.profile;
    profile.email = current.email;
    profile.save_meta(db, user.id()).await.map_err(meta_error)?;
    Ok(Ok(()))
}

async fn field_options<'a>(
    db: &'a Database,
    user: &'a ActingUser,
    name: &'a str,
) -> Result<Vec<(String, String)>, orbit_orm::Error> {
    if name != DISPLAY_NAME {
        return Ok(Vec::new());
    }
    let profile = stored_profile(db, user).await?;
    Ok(profile
        .display_name_choices()
        .into_iter()
        .map(|name| (name.clone(), name))
        .collect())
}

/// The heading of the Ignite theme's profile form, which marks its page.
const DETAILS_HEADING: &str = "<h2 class=\"mb-5\">Your details</h2>";

/// WordPress's sections of the profile form, each with its fields, in its order.
const SECTIONS: &[(&str, &[&str])] = &[
    (
        "Personal Options",
        &[RICH_EDITING, COMMENT_SHORTCUTS, ADMIN_BAR_FRONT],
    ),
    (
        "Name",
        &["name", FIRST_NAME, LAST_NAME, NICKNAME, DISPLAY_NAME],
    ),
    ("Contact Info", &["email", URL_FIELD]),
    ("About Yourself", &[DESCRIPTION]),
];

/// The note under the username, as `user-edit.php` writes it.
const USERNAME_NOTE: &str = "Usernames cannot be changed.";

/// The username a profile save started with, so a name sent anyway is not kept.
#[derive(Default)]
struct KeptUsername(std::sync::OnceLock<(i64, String)>);

/// The Ignite theme's profile page with WordPress's sections, the username read only, the
/// sessions, the application passwords and the picture's picker; see the [module](self).
pub(crate) struct IgniteProfileView;

#[rocket::async_trait]
impl Fairing for IgniteProfileView {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control Ignite profile",
            kind: Kind::Request | Kind::Response,
        }
    }

    async fn on_request(&self, request: &mut Request<'_>, _: &mut rocket::Data<'_>) {
        if request.method() != Method::Post || request.uri().path().as_str() != profile_url() {
            return;
        }
        if let Outcome::Success(user) = request.guard::<AdminUser>().await
            && user.admin_theme() != WORDPRESS
        {
            let _ = request
                .local_cache(KeptUsername::default)
                .0
                .set((user.user_id(), user.name().to_owned()));
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if request.method() == Method::Post
            && request.uri().path().as_str() == profile_url()
            && let Some((id, username)) = request.local_cache(KeptUsername::default).0.get()
            && let Some(db) = request.rocket().state::<Database>()
        {
            keep_username(db, *id, username).await;
        }
        if !matches!(request.method(), Method::Get | Method::Post)
            || !request.uri().path().as_str().starts_with(&profile_url())
            || response.content_type() != Some(ContentType::HTML)
        {
            return;
        }
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        let html = if html.contains(DETAILS_HEADING) {
            fill_ignite(request, &html).await.unwrap_or(html)
        } else {
            html
        };
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}

/// Puts back `username` when a save changed the user's name: the username is not changed from
/// the profile (`edit_user` ignores a `user_login` sent with it).
async fn keep_username(db: &Database, id: i64, username: &str) {
    match ignite_users::find_by_id(db, id).await {
        Ok(Some(row)) if row.name != username => {
            if let Err(e) = raw::execute(
                db,
                "UPDATE users SET name = ? WHERE id = ?",
                &[Sql::from(username), id.into()],
            )
            .await
            {
                log::error!("The username of user {id} could not be kept: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => log::error!("Could not read user {id} after a profile save: {e}"),
    }
}

/// The Ignite theme's profile page `html` with Mission Control's parts, or `None` when they
/// cannot be built (logged).
async fn fill_ignite(request: &Request<'_>, html: &str) -> Option<String> {
    let Outcome::Success(user) = request.guard::<AdminUser>().await else {
        return None;
    };
    let Outcome::Success(csrf) = request.guard::<CsrfToken>().await else {
        return None;
    };
    let db = request.rocket().state::<Database>()?;
    let notices = request.rocket().state::<ProfileNotices>()?;
    let key = session_key(request.cookies(), &user);
    let notice = notices.take(&key);
    let context = match form_context(db, &user, &key, csrf.value(), false, notice).await {
        Ok(context) => context,
        Err(e) => {
            log::error!("The profile's sections could not be built: {e}");
            return None;
        }
    };
    let render = |name: &str| match propel_runtime::render(&Views, name, &context) {
        Ok(html) => Some(html),
        Err(e) => {
            log::error!("The profile's {name} could not be rendered: {e}");
            None
        }
    };
    let sessions = render("profile.sessions")?;
    let app_passwords = render("profile.app_passwords")?;
    let mut html = with_sections(html);
    // "Display name publicly as" has no blank choice, as `user-edit.php`'s select.
    html = super::forms::edit_select(&html, DISPLAY_NAME, |options| {
        options.retain(|option| !option.value.is_empty());
    });
    html = with_account_management(&html, &sessions);
    html = with_section_after_picture(&html, &app_passwords);
    if let Some(message) = context["message"].as_str().filter(|m| !m.is_empty()) {
        html = with_notice(&html, message);
    }
    Some(with_picker(&html))
}

/// Where the field block that starts at `start` (`<div data-ignite-field="...">`) ends.
fn block_end(html: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = start;
    loop {
        let open = html[at..].find("<div").map(|i| at + i);
        let close = html[at..].find("</div>").map(|i| at + i)?;
        match open {
            Some(open) if open < close => {
                depth += 1;
                at = open + "<div".len();
            }
            _ => {
                depth = depth.checked_sub(1)?;
                at = close + "</div>".len();
                if depth == 0 {
                    return Some(at);
                }
            }
        }
    }
}

/// The username's block: read only, named "Username", with WordPress's note.
fn username_block(block: &str) -> String {
    let mut block = block
        .replace(">Name *</label>", ">Username</label>")
        .replace(
            "id=\"name\" name=\"name\"",
            "id=\"name\" name=\"name\" readonly aria-describedby=\"name-description\"",
        );
    if let Some(end) = block.rfind("</div>") {
        block.insert_str(
            end,
            &format!(
                "<p class=\"mt-1.5 mb-0 text-sm\" id=\"name-description\">{USERNAME_NOTE}</p>\n"
            ),
        );
    }
    block
}

/// `html` with the fields of the profile form in WordPress's sections ([`SECTIONS`]), each under
/// its heading; a field of no section follows them. The form's button is "Update Profile".
fn with_sections(html: &str) -> String {
    let marker = format!("action=\"{}\">", profile_url());
    let Some(form) = html.find(&marker) else {
        return html.to_owned();
    };
    let Some(form_end) = html[form..].find("</form>").map(|end| form + end) else {
        return html.to_owned();
    };
    let inner = &html[form..form_end];
    const FIELD: &str = "<div data-ignite-field=\"";
    let mut rest = String::with_capacity(inner.len());
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut first: Option<usize> = None;
    let mut at = 0;
    while let Some(start) = inner[at..].find(FIELD).map(|i| at + i) {
        let Some(end) = block_end(inner, start) else {
            break;
        };
        let name_start = start + FIELD.len();
        let name = inner[name_start..]
            .split('"')
            .next()
            .unwrap_or_default()
            .to_owned();
        rest.push_str(inner[at..start].trim_end());
        first.get_or_insert(rest.len());
        blocks.push((name, inner[start..end].to_owned()));
        at = end;
    }
    rest.push_str(&inner[at..]);
    let Some(first) = first else {
        return html.to_owned();
    };
    let mut sections = String::new();
    let mut take = |name: &str| {
        blocks
            .iter()
            .position(|(n, _)| n == name)
            .map(|i| blocks.remove(i).1)
    };
    for (heading, names) in SECTIONS {
        let fields: Vec<String> = names
            .iter()
            .filter_map(|name| {
                take(name).map(|block| {
                    if *name == "name" {
                        username_block(&block)
                    } else {
                        block
                    }
                })
            })
            .collect();
        if fields.is_empty() {
            continue;
        }
        sections.push_str(&format!(
            "\n<h3 class=\"m-0 text-base font-semibold text-heading\">{heading}</h3>"
        ));
        for block in fields {
            sections.push('\n');
            sections.push_str(&block);
        }
    }
    for (_, block) in blocks {
        sections.push('\n');
        sections.push_str(&block);
    }
    rest.insert_str(first, &sections);
    let rest = rest.replace(">Save details</button>", ">Update Profile</button>");
    format!("{}{rest}{}", &html[..form], &html[form_end..])
}

/// `html` with Ignite's password card as WordPress's Account Management: its heading, "New
/// Password" over the password fields, "Set New Password", then `sessions`.
fn with_account_management(html: &str, sessions: &str) -> String {
    let mut html = html.replacen(
        "<h2 class=\"mb-5\">Password</h2>",
        "<h2 class=\"mb-5\">Account Management</h2>\n<h3 class=\"mb-3 text-base font-semibold \
         text-heading\">New Password</h3>",
        1,
    );
    html = html.replacen(">Change password</button>", ">Set New Password</button>", 1);
    let marker = format!("action=\"{}/password\"", profile_url());
    if let Some(form) = html.find(&marker)
        && let Some(end) = html[form..]
            .find("</form>")
            .map(|end| form + end + "</form>".len())
    {
        html.insert_str(end, &format!("\n{sessions}"));
    }
    html
}

/// `html` with `section` after the picture card, or at the end of the page's content when there
/// is no picture card.
fn with_section_after_picture(html: &str, section: &str) -> String {
    let mut html = html.to_owned();
    let marker = format!("action=\"{}/picture\"", profile_url());
    let at = html
        .find(&marker)
        .and_then(|form| {
            html[form..]
                .find("</section>")
                .map(|end| form + end + "</section>".len())
        })
        .or_else(|| {
            let start = html.find(DETAILS_HEADING)?;
            html[start..]
                .find("</section>")
                .map(|end| start + end + "</section>".len())
        });
    if let Some(at) = at {
        html.insert_str(at, &format!("\n{section}"));
    }
    html
}

/// The Ignite theme's success notice, as its flash partial renders it.
const IGNITE_NOTICE: &str = "<p class=\"mb-5 rounded-md border border-primary/30 bg-lightprimary \
     px-4 py-3 text-heading\" role=\"status\">";

/// `html` with `message` as a notice under the page's heading.
fn with_notice(html: &str, message: &str) -> String {
    let heading = "<h1 class=\"mb-6\">Profile</h1>";
    let Some(at) = html.find(heading).map(|at| at + heading.len()) else {
        return html.to_owned();
    };
    let mut html = html.to_owned();
    html.insert_str(
        at,
        &format!("\n{IGNITE_NOTICE}{}</p>", super::media::escape(message)),
    );
    html
}

/// `html` with the media library picker's stylesheet and script, which turn the picture field
/// into the picker.
fn with_picker(html: &str) -> String {
    let assets = format!(
        "<link rel=\"stylesheet\" href=\"{}\">\n<script src=\"{}\" defer></script>\n",
        editor_stylesheet_url(),
        image_picker_script_url()
    );
    match html.rfind("</body>") {
        Some(at) => format!("{}{assets}{}", &html[..at], &html[at..]),
        None => format!("{html}{assets}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_are_ignites() {
        assert_eq!(initials("ada lovelace"), "AL");
        assert_eq!(initials("ada"), "A");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn notices_are_shown_once() {
        let notices = ProfileNotices::default();
        notices.put(
            "s".into(),
            Notice {
                message: Some("hi".into()),
                ..Notice::default()
            },
        );
        assert_eq!(notices.take("s").message.as_deref(), Some("hi"));
        assert!(notices.take("s").message.is_none());
    }

    #[test]
    fn the_form_view_renders() {
        let source = Views.source("profile.form").unwrap();
        assert!(source.contains("value=\"Update Profile\""));
    }
}
