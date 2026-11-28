//! A user's own profile, as WordPress's `wp-admin/profile.php` (`user-edit.php`) shows it and
//! `edit_user` (`wp-admin/includes/user.php`) saves it through `wp_update_user`.
//!
//! - **Personal Options:** "Visual Editor" (the `rich_editing` user meta, `"false"` when the
//!   box "Disable the visual editor when writing" is ticked, else `"true"`), "Keyboard
//!   Shortcuts" (`comment_shortcuts`, `"true"` or `""`) and "Toolbar" (`show_admin_bar_front`,
//!   `"true"` or `"false"`). WordPress keeps these three as user meta (its "user options" are
//!   user meta too); Mission Control has no separate user options store, so they are user meta
//!   here as well.
//! - **Name:** the username (the `users.name` column, read only), then the `first_name`,
//!   `last_name` and `nickname` user meta, and "Display name publicly as" (`display_name` user
//!   meta), one of the names [`Profile::display_name_choices`] builds as WordPress builds them.
//! - **Contact Info:** the email (Ignite's `users.email` column, which signs the user in) and the
//!   website (the `user_url` user meta, where the Users screen keeps it too).
//! - **About Yourself:** the biographical info (the `description` user meta), filtered by
//!   `wp_filter_kses` ([`kses_data`]) as WordPress's `pre_user_description` filter does.
//!
//! Each value is stored as WordPress stores it, a JSON string in `user_meta` (see
//! [`set_user_meta`]). [`ProfileForm::read`] checks a submitted form as `edit_user` does and
//! gives WordPress's messages, each on its field: an empty or incorrect email, an empty
//! nickname, passwords that differ or hold a `\`, and a website that is no URL.
//!
//! Mission Control's own rules, beside WordPress's: a website must be an `http` or `https`
//! address (WordPress cleans any address with `esc_url_raw` and keeps what is left, and refuses
//! none; here an address it would change or empty is refused with the menu editor's "Please enter
//! a valid URL."), and a new password has at least [`MIN_PASSWORD_CHARS`] characters, the
//! panel's minimum (WordPress asks to confirm a weak password instead).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use orbit_orm::{Database, Value as Sql, raw};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::content::{MetaError, esc_url_raw, get_user_meta, is_email, kses_data, set_user_meta};
use crate::text::sanitize_text_field;

/// The user meta of the first name.
pub const FIRST_NAME: &str = "first_name";
/// The user meta of the last name.
pub const LAST_NAME: &str = "last_name";
/// The user meta of the nickname.
pub const NICKNAME: &str = "nickname";
/// The user meta of the name shown publicly (WordPress's `display_name` column).
pub const DISPLAY_NAME: &str = "display_name";
/// The user meta of the website (WordPress's `user_url` column).
pub const USER_URL: &str = "user_url";
/// The user meta of the biographical info.
pub const DESCRIPTION: &str = "description";
/// The user meta of the visual editor choice: `"false"` turns it off.
pub const RICH_EDITING: &str = "rich_editing";
/// The user meta of the comment moderation keyboard shortcuts: `"true"` turns them on.
pub const COMMENT_SHORTCUTS: &str = "comment_shortcuts";
/// The user meta of the toolbar on the site: `"false"` hides it.
pub const SHOW_ADMIN_BAR_FRONT: &str = "show_admin_bar_front";

/// The shortest new password: the panel's minimum (Ignite's `MIN_PASSWORD_CHARS`).
pub const MIN_PASSWORD_CHARS: usize = 8;

/// The longest website WordPress stores (`wp_insert_user`).
pub const MAX_URL_CHARS: usize = 100;

/// WordPress's message after a save (`profile.php?updated=1`).
pub const PROFILE_UPDATED: &str = "Profile updated.";
/// `edit_user`: an empty email.
pub const EMPTY_EMAIL: &str = "Please enter an email address.";
/// `edit_user`: an email `is_email` refuses.
pub const INVALID_EMAIL: &str = "The email address is not correct.";
/// `edit_user`: an email another user has.
pub const EMAIL_EXISTS: &str = "This email is already registered. Please choose another one.";
/// `edit_user`: an empty nickname.
pub const EMPTY_NICKNAME: &str = "Please enter a nickname.";
/// `edit_user`: two passwords that differ.
pub const PASSWORDS_DIFFER: &str =
    "Passwords do not match. Please enter the same password in both password fields.";
/// `edit_user`: a password with a backslash.
pub const PASSWORD_BACKSLASH: &str = "Passwords may not contain the character \"\\\".";
/// `edit_user` and `user-new.php`: a username `validate_username` refuses.
pub const INVALID_USERNAME: &str =
    "This username is invalid because it uses illegal characters. Please enter a valid username.";
/// The menu editor's message for a link that is no URL (`nav-menu.js`).
pub const INVALID_URL: &str = "Please enter a valid URL.";
/// `wp_insert_user`: a website longer than [`MAX_URL_CHARS`].
pub const URL_TOO_LONG: &str = "User URL may not be longer than 100 characters.";

/// WordPress's `validate_username`: whether `username` is what `sanitize_user( $username, true )`
/// leaves of it, and not empty. The strict `sanitize_user` keeps only ASCII letters (either
/// case, as its pattern has the `i` flag), digits, spaces, `_`, `.`, `-` and `@`, after it takes
/// out tags, accents, `%` octets and entities (each of which has a character the strict pattern
/// refuses), trims the name and makes each run of whitespace one space. So a name is valid when
/// it has only those characters, does not start or end with a space and has no two spaces in a
/// row.
pub fn validate_username(username: &str) -> bool {
    !username.is_empty()
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-' | '@'))
        && !username.starts_with(' ')
        && !username.ends_with(' ')
        && !username.contains("  ")
}

/// The names users `ids` are shown by, as WordPress shows `display_name` wherever it names a
/// user (the admin bar's "Howdy", the posts list's Author column): each user's "Display name
/// publicly as" (the `display_name` user meta), or the username (`users.name`) when it is unset
/// or blank, as `wp_insert_user` sets a new user's display name to the login. A user that does
/// not exist is left out.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn display_names(
    db: &Database,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, String>, orbit_orm::Error> {
    let mut ids: Vec<i64> = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut names = std::collections::HashMap::new();
    for chunk in ids.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
        let users: Vec<(i64, String)> = raw::fetch_all(
            db,
            &format!("SELECT id, name FROM users WHERE id IN ({marks})"),
            &binds,
        )
        .await?;
        names.extend(users);
        let mut meta_binds = binds;
        meta_binds.push(DISPLAY_NAME.into());
        let shown: Vec<(i64, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT user_id, meta_value FROM user_meta WHERE user_id IN ({marks}) \
                 AND meta_key = ? ORDER BY id"
            ),
            &meta_binds,
        )
        .await?;
        for (id, value) in shown {
            if let Ok(Value::String(name)) = serde_json::from_str::<Value>(&value)
                && !name.trim().is_empty()
                && names.contains_key(&id)
            {
                names.insert(id, name);
            }
        }
    }
    Ok(names)
}

/// A user's profile: what the profile page shows and saves. See the [module](self).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    /// The login name (`users.name`), shown but not changed.
    pub username: String,
    /// The email (`users.email`).
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub nickname: String,
    /// The name shown publicly.
    pub display_name: String,
    /// The website.
    pub url: String,
    /// The biographical info.
    pub description: String,
    /// Whether the visual editor is on (WordPress's default).
    pub rich_editing: bool,
    /// Whether the comment moderation keyboard shortcuts are on (off by default).
    pub comment_shortcuts: bool,
    /// Whether the toolbar shows on the site (WordPress's default).
    pub admin_bar_front: bool,
}

/// The text of user meta `key` of `user_id`, or `None` when it is unset or not a string.
async fn meta_text(db: &Database, user_id: i64, key: &str) -> Result<Option<String>, MetaError> {
    Ok(match get_user_meta(db, user_id, key).await? {
        Some(Value::String(text)) => Some(text),
        _ => None,
    })
}

impl Profile {
    /// The profile of user `user_id`, whose username and email are `username` and `email`.
    /// A nickname and a display name never set are the username, as `wp_insert_user` sets
    /// them for a new user.
    ///
    /// # Errors
    ///
    /// The database's error, or stored meta that is not JSON.
    pub async fn load(
        db: &Database,
        user_id: i64,
        username: &str,
        email: &str,
    ) -> Result<Self, MetaError> {
        let text = async |key: &str| -> Result<String, MetaError> {
            Ok(meta_text(db, user_id, key).await?.unwrap_or_default())
        };
        let or_username = |value: String| {
            if value.trim().is_empty() {
                username.to_owned()
            } else {
                value
            }
        };
        Ok(Profile {
            username: username.to_owned(),
            email: email.to_owned(),
            first_name: text(FIRST_NAME).await?,
            last_name: text(LAST_NAME).await?,
            nickname: or_username(text(NICKNAME).await?),
            display_name: or_username(text(DISPLAY_NAME).await?),
            url: text(USER_URL).await?,
            description: text(DESCRIPTION).await?,
            rich_editing: meta_text(db, user_id, RICH_EDITING).await?.as_deref() != Some("false"),
            comment_shortcuts: meta_text(db, user_id, COMMENT_SHORTCUTS).await?.as_deref()
                == Some("true"),
            admin_bar_front: meta_text(db, user_id, SHOW_ADMIN_BAR_FRONT)
                .await?
                .as_deref()
                != Some("false"),
        })
    }

    /// The choices of "Display name publicly as", in WordPress's order (`user-edit.php`): the
    /// stored display name first when it is none of the others, then the nickname, the
    /// username, the first name, the last name, "First Last" and "Last First" (the last two
    /// with both names), each trimmed, without blanks and without repeats.
    pub fn display_name_choices(&self) -> Vec<String> {
        let first = self.first_name.trim();
        let last = self.last_name.trim();
        let mut names = vec![self.nickname.clone(), self.username.clone()];
        if !first.is_empty() {
            names.push(first.to_owned());
        }
        if !last.is_empty() {
            names.push(last.to_owned());
        }
        if !first.is_empty() && !last.is_empty() {
            names.push(format!("{first} {last}"));
            names.push(format!("{last} {first}"));
        }
        if !names.contains(&self.display_name) {
            names.insert(0, self.display_name.clone());
        }
        let mut choices: Vec<String> = Vec::with_capacity(names.len());
        for name in names {
            let name = name.trim().to_owned();
            if !name.is_empty() && !choices.contains(&name) {
                choices.push(name);
            }
        }
        choices
    }

    /// Stores the user meta of the profile for user `user_id`. The username and the email are
    /// Ignite's columns, saved by the caller.
    ///
    /// # Errors
    ///
    /// The database's error.
    pub async fn save_meta(&self, db: &Database, user_id: i64) -> Result<(), MetaError> {
        let flag = |on: bool, yes: &str, no: &str| if on { yes } else { no }.to_owned();
        for (key, value) in [
            (FIRST_NAME, self.first_name.clone()),
            (LAST_NAME, self.last_name.clone()),
            (NICKNAME, self.nickname.clone()),
            (DISPLAY_NAME, self.display_name.clone()),
            (USER_URL, self.url.clone()),
            (DESCRIPTION, self.description.clone()),
            (RICH_EDITING, flag(self.rich_editing, "true", "false")),
            (COMMENT_SHORTCUTS, flag(self.comment_shortcuts, "true", "")),
            (
                SHOW_ADMIN_BAR_FRONT,
                flag(self.admin_bar_front, "true", "false"),
            ),
        ] {
            set_user_meta(db, user_id, key, &json!(value)).await?;
        }
        Ok(())
    }
}

/// A submitted profile form, read and checked as `edit_user` reads it.
#[derive(Debug, Clone)]
pub struct ProfileForm {
    /// The profile to save: the form's values, cleaned.
    pub profile: Profile,
    /// The new password, when one was typed.
    pub new_password: Option<Zeroizing<String>>,
    /// The error of each refused field, `(field, message)`, in the order WordPress checks them.
    pub errors: Vec<(&'static str, &'static str)>,
}

impl ProfileForm {
    /// Reads the form's fields through `get` (a field's submitted value, `None` when it was not
    /// sent) for the user whose stored profile is `current`. The username is kept: it cannot be
    /// changed. A box not sent is unticked.
    pub fn read(get: impl Fn(&str) -> Option<String>, current: &Profile) -> Self {
        let text = |name: &str| sanitize_text_field(&get(name).unwrap_or_default());
        let mut errors = Vec::new();

        // The passwords: `pass1`, and `pass2` to repeat it.
        let pass1 = Zeroizing::new(get("pass1").unwrap_or_default());
        let pass2 = Zeroizing::new(get("pass2").unwrap_or_default());
        let new_password = if pass1.is_empty() && pass2.is_empty() {
            None
        } else {
            if pass1.contains('\\') {
                errors.push(("pass1", PASSWORD_BACKSLASH));
            } else if *pass1 != *pass2 {
                errors.push(("pass1", PASSWORDS_DIFFER));
            } else if pass1.chars().count() < MIN_PASSWORD_CHARS {
                errors.push(("pass1", short_password()));
            }
            Some(pass1)
        };

        let nickname = text("nickname");
        if nickname.is_empty() {
            errors.push(("nickname", EMPTY_NICKNAME));
        }
        let email = text("email");
        if email.is_empty() {
            errors.push(("email", EMPTY_EMAIL));
        } else if !is_email(&email) {
            errors.push(("email", INVALID_EMAIL));
        }
        let raw_url = get("url").unwrap_or_default();
        let url = match normalize_url(&raw_url) {
            Ok(url) => url,
            Err(message) => {
                errors.push(("url", message));
                raw_url.trim().to_owned()
            }
        };
        let display_name = match text("display_name") {
            name if name.is_empty() => current.username.clone(),
            name => name,
        };
        let description = kses_data(get("description").unwrap_or_default().trim())
            .trim()
            .to_owned();
        ProfileForm {
            profile: Profile {
                username: current.username.clone(),
                email,
                first_name: text("first_name"),
                last_name: text("last_name"),
                nickname,
                display_name,
                url,
                description,
                rich_editing: get("rich_editing").as_deref() != Some("false"),
                comment_shortcuts: get("comment_shortcuts").as_deref() == Some("true"),
                admin_bar_front: get("admin_bar_front").is_some(),
            },
            new_password,
            errors,
        }
    }
}

/// The message of a password shorter than [`MIN_PASSWORD_CHARS`].
fn short_password() -> &'static str {
    "The new password must be at least 8 characters."
}

/// The website to store for the form's `url`, as `edit_user` cleans it: empty (or WordPress's
/// placeholder `http://`) stores none; otherwise `esc_url_raw`'s address, with `http://` before
/// an address without a scheme. Refused (see the [module](self)) when it is longer than
/// [`MAX_URL_CHARS`], holds spaces or control characters, is not `http` or `https`, has no host,
/// or is changed by the cleaning.
///
/// # Errors
///
/// The message to show on the field.
pub fn normalize_url(raw: &str) -> Result<String, &'static str> {
    let url = raw.trim();
    if url.is_empty() || url == "http://" {
        return Ok(String::new());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(INVALID_URL);
    }
    let cleaned = esc_url_raw(url);
    let lower = cleaned.to_ascii_lowercase();
    let Some(rest) = ["http://", "https://"]
        .iter()
        .find_map(|scheme| lower.strip_prefix(scheme))
    else {
        return Err(INVALID_URL);
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_ok = !host.is_empty()
        && !host.starts_with(['.', '-', ':', '@'])
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || "-.:[]@".contains(c));
    if !host_ok || (cleaned != url && cleaned != format!("http://{url}")) {
        return Err(INVALID_URL);
    }
    if cleaned.chars().count() > MAX_URL_CHARS {
        return Err(URL_TOO_LONG);
    }
    Ok(cleaned)
}

/// The id of the sign-in session in the value of Ignite's session cookie (`ignite_session`):
/// the `sid` of its signed payload, `<base64url JSON>.<signature>`. The signature is not
/// checked here: call it only on a request Ignite's `AdminUser` guard has accepted, which checks
/// it.
pub fn ignite_session_id(cookie: &str) -> Option<String> {
    let (body, _) = cookie.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(body).ok()?;
    match serde_json::from_slice::<Value>(&payload).ok()?.get("sid")? {
        Value::String(sid) if !sid.is_empty() => Some(sid.clone()),
        _ => None,
    }
}

/// The number of `user_id`'s live sign-in sessions other than `keep`.
///
/// # Errors
///
/// The database's error.
pub async fn other_sessions(
    db: &Database,
    user_id: i64,
    keep: &str,
) -> Result<i64, orbit_orm::Error> {
    raw::fetch_scalar(
        db,
        "SELECT COUNT(*) FROM sessions WHERE user_id = ? AND id <> ? AND expires_at > ?",
        &[
            user_id.into(),
            keep.into(),
            Sql::from(Utc::now().naive_utc()),
        ],
    )
    .await
}

/// Ends every sign-in session of `user_id` but `keep`, the one in use, as WordPress's "Log Out
/// Everywhere Else" (`wp_ajax_destroy_sessions`, `WP_Session_Tokens::destroy_others`), and
/// replaces the user's "Remember me" token, so a remembered browser does not sign in again.
/// Returns the number of sessions ended.
///
/// # Errors
///
/// The database's error, or no random bytes for the new token.
pub async fn log_out_everywhere_else(
    db: &Database,
    user_id: i64,
    keep: &str,
) -> Result<u64, orbit_orm::Error> {
    let ended = raw::execute(
        db,
        "DELETE FROM sessions WHERE user_id = ? AND id <> ?",
        &[user_id.into(), keep.into()],
    )
    .await?;
    let mut bytes = Zeroizing::new([0u8; 45]);
    getrandom::fill(&mut bytes[..]).map_err(|e| {
        orbit_orm::Error::Database(orbit_orm::__sqlx::Error::Protocol(e.to_string()))
    })?;
    let token = Zeroizing::new(URL_SAFE_NO_PAD.encode(&bytes[..]));
    raw::execute(
        db,
        "UPDATE users SET remember_token = ? WHERE id = ? AND remember_token IS NOT NULL",
        &[token.as_str().into(), user_id.into()],
    )
    .await?;
    Ok(ended)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn profile() -> Profile {
        Profile {
            username: "ada".into(),
            email: "ada@example.test".into(),
            nickname: "ada".into(),
            display_name: "ada".into(),
            rich_editing: true,
            admin_bar_front: true,
            ..Profile::default()
        }
    }

    fn read(fields: &[(&str, &str)]) -> ProfileForm {
        let map: HashMap<String, String> = fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ProfileForm::read(|name| map.get(name).cloned(), &profile())
    }

    #[test]
    fn usernames_are_validated_as_validate_username() {
        for valid in ["ada", "Ada Lovelace", "qa.tester-1_x@example", "a b"] {
            assert!(validate_username(valid), "{valid}");
        }
        for invalid in [
            "",
            "qa tester 1!",
            " ada",
            "ada ",
            "a  b",
            "ad\u{e9}",
            "a%20b",
            "a&amp;b",
            "<b>ada</b>",
            "a\tb",
        ] {
            assert!(!validate_username(invalid), "{invalid}");
        }
    }

    #[test]
    fn the_display_name_choices_are_wordpresss() {
        let mut p = profile();
        assert_eq!(p.display_name_choices(), ["ada"]);
        p.nickname = "Countess".into();
        p.first_name = "Ada".into();
        p.last_name = "Lovelace".into();
        assert_eq!(
            p.display_name_choices(),
            [
                "Countess",
                "ada",
                "Ada",
                "Lovelace",
                "Ada Lovelace",
                "Lovelace Ada"
            ]
        );
        p.display_name = "A. L.".into();
        assert_eq!(p.display_name_choices()[0], "A. L.");
        p.last_name = String::new();
        assert_eq!(
            p.display_name_choices(),
            ["A. L.", "Countess", "ada", "Ada"]
        );
    }

    #[test]
    fn a_form_is_read_as_edit_user_reads_it() {
        let form = read(&[
            ("email", " ada@example.test "),
            ("nickname", "Countess"),
            ("first_name", "<b>Ada</b>"),
            ("url", "example.com/me"),
            ("description", "Hi <script>x</script><em>there</em>"),
            ("display_name", "Ada"),
            ("rich_editing", "false"),
            ("comment_shortcuts", "true"),
        ]);
        assert!(form.errors.is_empty(), "{:?}", form.errors);
        let p = form.profile;
        assert_eq!(p.email, "ada@example.test");
        assert_eq!(p.first_name, "Ada");
        assert_eq!(p.url, "http://example.com/me");
        assert_eq!(p.description, "Hi <em>there</em>");
        assert!(!p.rich_editing && p.comment_shortcuts && !p.admin_bar_front);
        assert!(form.new_password.is_none());
    }

    #[test]
    fn wordpresss_errors_are_on_their_fields() {
        let form = read(&[("email", "not-an-email"), ("url", "javascript:alert(1)")]);
        assert_eq!(
            form.errors,
            [
                ("nickname", EMPTY_NICKNAME),
                ("email", INVALID_EMAIL),
                ("url", INVALID_URL)
            ]
        );
        let form = read(&[("nickname", "n"), ("pass1", "abcdefgh"), ("pass2", "x")]);
        assert_eq!(
            form.errors,
            [("pass1", PASSWORDS_DIFFER), ("email", EMPTY_EMAIL)]
        );
        let form = read(&[
            ("nickname", "n"),
            ("email", "a@example.test"),
            ("pass1", "a\\bcdefgh"),
            ("pass2", "a\\bcdefgh"),
        ]);
        assert_eq!(form.errors, [("pass1", PASSWORD_BACKSLASH)]);
    }

    #[test]
    fn a_website_is_an_http_address() {
        assert_eq!(normalize_url("").unwrap(), "");
        assert_eq!(normalize_url("http://").unwrap(), "");
        assert_eq!(
            normalize_url("https://example.com/a?b=c").unwrap(),
            "https://example.com/a?b=c"
        );
        assert_eq!(normalize_url("example.com").unwrap(), "http://example.com");
        for bad in [
            "not a url",
            "javascript:alert(1)",
            "ftp://example.com",
            "https://",
            "http://<x>",
        ] {
            assert_eq!(normalize_url(bad), Err(INVALID_URL), "{bad}");
        }
        let long = format!("https://example.com/{}", "a".repeat(100));
        assert_eq!(normalize_url(&long), Err(URL_TOO_LONG));
    }

    #[test]
    fn the_session_id_is_read_from_ignites_cookie() {
        let body = URL_SAFE_NO_PAD.encode(br#"{"sid":"abc123"}"#);
        assert_eq!(
            ignite_session_id(&format!("{body}.signature")).as_deref(),
            Some("abc123")
        );
        assert_eq!(ignite_session_id("garbage"), None);
    }
}
