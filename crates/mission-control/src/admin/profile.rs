//! The WordPress admin theme's profile page (`wp-admin/profile.php`, Task 8.5), at the panel's
//! profile address, `/mc-admin/profile` (the Users menu's "Your Profile", and the admin bar's
//! "Edit Profile"). The Ignite theme keeps Ignite's profile page.
//!
//! The fields are WordPress's, in its order, and saved as `edit_user` saves them (see
//! `mission_control_core::users::profile`): Personal Options (Visual Editor, Admin Theme in
//! place of WordPress's Admin Color Scheme, Keyboard Shortcuts, Toolbar), Name (Username, read
//! only, First Name, Last Name, Nickname, Display name publicly as), Contact Info (Email,
//! Website), About Yourself (Biographical Info, Profile Picture), Account Management (New
//! Password with "Set New Password" and "Generate Password", Sessions with "Log Out Everywhere
//! Else"), Application Passwords, then "Update Profile". WordPress's Language row shows only
//! when a language besides English is installed; Mission Control installs none, so the row is
//! left out.
//!
//! How it is built:
//!
//! - **The page:** Ignite renders its profile route through the theme's `profile` view
//!   (`views/admin/wordpress/profile.propel`), which has the panel's chrome (menu, admin bar,
//!   Ignite's notices) and a marker in place of the form. [`ProfileView`], a response fairing,
//!   puts WordPress's form (`profile_form.propel`) there, built from the user's profile meta,
//!   sessions and application passwords, which Ignite's page values do not hold.
//! - **The save:** `POST /mc-admin/profile` is answered here, before Ignite's route, when the
//!   user's page renders in this theme ([`WordPressPage`] forwards otherwise, so the Ignite
//!   theme's form reaches Ignite). A save goes back to `/mc-admin/profile?updated=1`, which shows
//!   "Profile updated." as WordPress does. A refused form goes back to the page with the errors
//!   on their fields and the values typed, as Laravel's `back()->withErrors()->withInput()`
//!   flashes them: the page cannot be rendered here, as Ignite holds its chrome.
//! - **The other forms** post to their own routes and come back to the page: the picture to
//!   Ignite's `/profile/avatar` and `/profile/avatar/remove`; "Log Out Everywhere Else" to
//!   `/profile/destroy-sessions` (WordPress's `destroy-sessions` action); the application
//!   passwords to `/profile/application-passwords` and its `revoke` and `revoke-all`, with
//!   WordPress's messages, the new password shown once.
//!
//! The flashed values are kept in memory, by the sign-in session, until the next page shows
//! them (at most [`NOTICE_TTL`]): one process's memory, as the panel's sessions are one
//! database's.

use std::collections::HashMap;
use std::io::{self, Cursor};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ignite_panel::theme::AdminTheme;
use ignite_panel::{AdminUser, CsrfForm, CsrfToken, users as ignite_users};
use mission_control_core::users::app_password::{self, AppPasswordError, PlainPassword};
use mission_control_core::users::profile::{
    EMAIL_EXISTS, PROFILE_UPDATED, Profile, ProfileForm, ignite_session_id,
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
use super::wordpress_theme::{NAME as WORDPRESS, WordPressTheme};

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
fn session_key(cookies: &CookieJar<'_>, user: &AdminUser) -> String {
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
        "has_avatar": user.has_avatar(),
        "avatar_url": format!(
            "{ADMIN_PATH}/profile/avatar?v={}",
            chrono_millis()
        ),
        "user_initials": initials(&row.name),
        "avatar_action": format!("{ADMIN_PATH}/profile/avatar"),
        "remove_avatar_action": format!("{ADMIN_PATH}/profile/avatar/remove"),
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
    for field in ["nickname", "email", "url", "pass1"] {
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
