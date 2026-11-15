//! The installer wizard (design 5.1): the steps setup mode serves at `/mc-admin/install.php`.
//!
//! The steps follow WordPress's installer. WordPress's first step, the language, comes with
//! translations in phase 3; phase 1 is English only.
//!
//! 1. **Database** (`?step=1`), as WordPress's `setup-config.php` asks for it: SQLite (the
//!    default, the file `database.sqlite` in the install folder, beside `.env`, created if it is
//!    missing), MySQL, MariaDB or PostgreSQL (database name, username, password, host and port).
//!    The connection is tested with [`Database::connect`] before the wizard moves on. A
//!    failure shows a plain-language error on the same step, as WordPress's "Error establishing a
//!    database connection", and the wizard does not move on. A database that already holds a
//!    site is refused, as WordPress's "Already Installed".
//! 2. **Site and administrator** (`?step=2`), on one form as WordPress's `install.php` shows
//!    them: site title, administrator username, password and email, and search engine
//!    visibility.
//! 3. **Finished**: the wizard checks that `.env` is still absent, runs Ignite's panel migrations
//!    and the core migrations ([`run_migrations`]), stores the options WordPress's installer
//!    sets that Mission Control has, writes `.env` (`DATABASE_URL` and a generated
//!    `IGNITE_SECRET_KEY`, owner-only, never over an existing file), and asks for the restart into normal mode ([`RestartHandle::request`]).
//!
//! **The administrator account is pending (Ruling R71).** Ignite's user creation runs on
//! MariaDB and MySQL only, and creating the first user is a later task (Task 11.4b). The form
//! shows the administrator fields as WordPress does, with a note that the account is not created
//! yet. The username and password are never read into the wizard's state, stored, logged or
//! echoed back; the email is stored as the `admin_email` option, as WordPress stores it. The rest
//! of the install does not wait for the account.
//!
//! `.env` is written last, after the migrations and options. A failure before it (a migration
//! that fails, a database that goes away) leaves no `.env`, so the binary stays in setup mode and
//! the wizard can be run again. Written first, a failed migration would leave a configured site
//! with no tables, and the installer would never come back.
//!
//! Between the steps the wizard keeps the chosen database in memory, in the process: setup mode
//! has no sessions and no database, and the password is never echoed into a page. Each choice gets
//! a random id that the site form carries, so a finish installs only on the database its own
//! step 1 chose; if someone submits step 1 again in between, the finish is refused. While a
//! finish runs, other requests are told an installation is in progress instead of waiting. Every form
//! carries a random token made when the process starts (`_wpnonce`, as WordPress names its
//! nonce field); a `POST` without it, or with another one, is refused. Every value echoed back
//! into a form is escaped by the view, and no error message carries the password or the
//! database URL.

use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mission_control_core::content::{get_option, set_option};
use mission_control_core::install::env_file::{
    self, ENV_FILE_NAME, EnvFile, EnvFileError, generate_secret_key,
};
use mission_control_core::rest::settings::Settings;
use orbit_orm::Database;
use percent_encoding::{AsciiSet, CONTROLS, NON_ALPHANUMERIC, utf8_percent_encode};
use propel_runtime::Loader;
use rocket::form::{Form, FromForm};
use rocket::http::{ContentType, Status};
use rocket::request::{FromRequest, Outcome, Request};
use rocket::response::Redirect;
use rocket::{Responder, Route, State};
use serde::Serialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::install::run::{InstallError, run_migrations};
use crate::restart::RestartHandle;
use crate::setup::{INSTALL_PATH, SetupState};

/// The SQLite file the default choice creates, in the install folder beside `.env`.
pub const SQLITE_FILE_NAME: &str = "database.sqlite";

/// The name of the token field every wizard form carries, WordPress's nonce field.
pub const TOKEN_FIELD: &str = "_wpnonce";

/// The characters kept as they are in a URL's user name, password and database name: RFC 3986's
/// unreserved characters. Everything else is percent-encoded.
const URL_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The characters encoded in a SQLite path: sqlx percent-decodes the path and ends it at `?`.
const SQLITE_PATH: &AsciiSet = &CONTROLS.add(b'%').add(b'?').add(b'#');

/// The database the installer can set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseKind {
    /// A SQLite file in the install folder: no database server. The default.
    Sqlite,
    /// A MySQL server.
    MySql,
    /// A MariaDB server.
    MariaDb,
    /// A PostgreSQL server.
    Postgres,
}

impl DatabaseKind {
    /// Every choice, in the order the form lists them.
    pub const ALL: [Self; 4] = [Self::Sqlite, Self::MySql, Self::MariaDb, Self::Postgres];

    /// The value the form sends.
    pub fn value(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::MySql => "mysql",
            Self::MariaDb => "mariadb",
            Self::Postgres => "postgres",
        }
    }

    /// The name the form shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Sqlite => "SQLite (default, no database server needed)",
            Self::MySql => "MySQL",
            Self::MariaDb => "MariaDB",
            Self::Postgres => "PostgreSQL",
        }
    }

    /// The choice the form sent as `value`, or `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.value() == value)
    }

    /// The URL scheme of a server's `DATABASE_URL`. MariaDB speaks MySQL's protocol.
    fn scheme(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::MySql | Self::MariaDb => "mysql",
            Self::Postgres => "postgres",
        }
    }

    /// The server's usual port.
    fn default_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            _ => 3306,
        }
    }

    /// The short name in messages.
    fn name(self) -> &'static str {
        match self {
            Self::Sqlite => "SQLite",
            Self::MySql => "MySQL",
            Self::MariaDb => "MariaDB",
            Self::Postgres => "PostgreSQL",
        }
    }
}

/// The database step's fields, as WordPress's `setup-config.php` names them, plus the database
/// type and the port. The password is not kept here: it is never echoed back into the form.
#[derive(Debug, Clone, Default, Serialize)]
struct DatabaseFields {
    dbname: String,
    uname: String,
    dbhost: String,
    dbport: String,
}

/// The database the database step connected to, kept for the finish. `Debug` hides the URL,
/// which holds the password, and the choice id.
struct ChosenDatabase {
    /// A random id made when this database was chosen. The site form carries it, so a finish
    /// only installs on the database its own step 1 chose: when someone else submits step 1 in
    /// between, the id no longer matches and the finish is refused.
    id: String,
    kind: DatabaseKind,
    url: Zeroizing<String>,
    fields: DatabaseFields,
    /// What the site step shows: the type and where, never the password.
    summary: String,
}

impl fmt::Debug for ChosenDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChosenDatabase")
            .field("id", &"[redacted]")
            .field("kind", &self.kind)
            .field("url", &"[redacted]")
            .field("fields", &self.fields)
            .finish()
    }
}

/// Where the install is.
#[derive(Debug, Clone)]
enum Phase {
    /// The forms are open.
    Open,
    /// A finish is running: other requests are told so instead of waiting for it.
    Installing,
    /// The install finished: the site's address and the database it used.
    Installed { home: String, database: String },
}

/// The wizard's state in Rocket: the install folder, the form token, the database chosen at the
/// database step, and where the install is.
pub struct Wizard {
    install_dir: PathBuf,
    token: Option<String>,
    database: Mutex<Option<ChosenDatabase>>,
    phase: Mutex<Phase>,
    /// Whether the database step created the SQLite file, so it can remove it again (while it is
    /// still empty) when the owner switches to a database server.
    created_sqlite: Mutex<bool>,
}

impl fmt::Debug for Wizard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wizard")
            .field("install_dir", &self.install_dir)
            .field("token", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl Wizard {
    /// The wizard for an install in `install_dir`, where `.env` and the SQLite file go. The form
    /// token is made here, from the operating system's random bytes. When they are not available
    /// the wizard has no token and refuses every form, with a message that says why.
    pub fn new(install_dir: impl Into<PathBuf>) -> Self {
        let token = match generate_secret_key() {
            Ok(token) => Some(token),
            Err(e) => {
                log::error!("The installer cannot make its form token: {e}");
                None
            }
        };
        Self {
            install_dir: install_dir.into(),
            token,
            database: Mutex::new(None),
            phase: Mutex::new(Phase::Open),
            created_sqlite: Mutex::new(false),
        }
    }

    fn phase(&self) -> Phase {
        self.phase.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_phase(&self, phase: Phase) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
    }

    /// Moves from open to installing, or says why not.
    fn start_installing(&self) -> Result<InstallingGuard<'_>, Refusal> {
        let mut phase = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        match &*phase {
            Phase::Open => {
                *phase = Phase::Installing;
                Ok(InstallingGuard {
                    wizard: self,
                    done: false,
                })
            }
            other => Err(Refusal::not_open(other)),
        }
    }

    /// A refusal when the forms are not open (an install is running or finished).
    fn refuse_unless_open(&self) -> Option<Refusal> {
        match self.phase() {
            Phase::Open => None,
            other => Some(Refusal::not_open(&other)),
        }
    }

    /// Whether `given` is this process's form token. Compared in constant time.
    fn token_matches(&self, given: Option<&str>) -> bool {
        secret_matches(self.token.as_deref(), given)
    }

    /// The token for a form, or empty when there is none (the forms are then refused).
    fn form_token(&self) -> String {
        self.token.clone().unwrap_or_default()
    }

    fn chosen(&self) -> std::sync::MutexGuard<'_, Option<ChosenDatabase>> {
        self.database.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Puts the wizard back to open when a finish fails (or its request is dropped), unless it
/// finished.
struct InstallingGuard<'a> {
    wizard: &'a Wizard,
    done: bool,
}

impl InstallingGuard<'_> {
    /// Marks the install finished.
    fn finished(mut self, home: &str, database: &str) {
        self.done = true;
        self.wizard.set_phase(Phase::Installed {
            home: home.to_owned(),
            database: database.to_owned(),
        });
    }
}

impl Drop for InstallingGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.wizard.set_phase(Phase::Open);
        }
    }
}

/// Whether `given` is `expected`, compared in constant time. Never true when either is missing
/// or empty.
fn secret_matches(expected: Option<&str>, given: Option<&str>) -> bool {
    match (expected, given) {
        (Some(expected), Some(given)) if !expected.is_empty() => {
            expected.len() == given.len()
                && expected
                    .bytes()
                    .zip(given.bytes())
                    .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                    == 0
        }
        _ => false,
    }
}

/// The wizard's routes, all at [`INSTALL_PATH`].
pub fn routes() -> Vec<Route> {
    rocket::routes![show, database_step, site_step]
}

/// The views, built into the binary: setup mode runs before anything else is installed.
struct Views;

impl Loader for Views {
    fn source(&self, name: &str) -> io::Result<String> {
        let source = match name {
            "install.layout" => include_str!("../../views/install/layout.propel"),
            "install.database" => include_str!("../../views/install/database.propel"),
            "install.site" => include_str!("../../views/install/site.propel"),
            "install.finished" => include_str!("../../views/install/finished.propel"),
            "install.refused" => include_str!("../../views/install/refused.propel"),
            _ => return Err(io::Error::from(io::ErrorKind::NotFound)),
        };
        Ok(source.to_owned())
    }
}

/// What a wizard route answers.
#[derive(Responder)]
enum Reply {
    /// A page.
    Page((Status, (ContentType, String))),
    /// A move to another step.
    Redirect(Redirect),
}

/// The page for `view` rendered with `context`, with `status`. A view that cannot render (a bug)
/// is logged and answered with a plain `500`.
fn page(status: Status, view: &str, context: &impl Serialize) -> Reply {
    match propel_runtime::render(&Views, view, context) {
        Ok(html) => Reply::Page((status, (ContentType::HTML, html))),
        Err(e) => {
            log::error!("The installer page `{view}` could not be rendered: {e}");
            Reply::Page((
                Status::InternalServerError,
                (
                    ContentType::Plain,
                    "The installer page could not be shown. Check the log.".to_owned(),
                ),
            ))
        }
    }
}

/// Why the wizard refused a request: shown on the refused page.
struct Refusal {
    status: Status,
    title: &'static str,
    message: String,
    /// Whether the page links back to the wizard.
    retry: bool,
}

impl Refusal {
    /// A form with no token, or a token from another process (WordPress's expired nonce).
    fn expired(has_token: bool) -> Self {
        Self {
            status: Status::Forbidden,
            title: "The link you followed has expired",
            message: if has_token {
                "This form is out of date or did not come from this installer, so nothing was \
                 saved. Open the installer again and submit the form from there."
                    .to_owned()
            } else {
                "The operating system could not provide random bytes, so the installer cannot \
                 protect its forms and accepts none. Check that the system's random number \
                 device is available, then restart Mission Control."
                    .to_owned()
            },
            retry: has_token,
        }
    }

    /// The forms are not open: an install is running, or finished, in this process.
    fn not_open(phase: &Phase) -> Self {
        match phase {
            Phase::Installing => Self {
                status: Status::ServiceUnavailable,
                title: "Installation in progress",
                message: "Mission Control is being installed right now. Wait a moment, then \
                          open the installer again."
                    .to_owned(),
                retry: true,
            },
            _ => Self {
                status: Status::Conflict,
                title: "Already installed",
                message: "Mission Control is installed and is restarting with its new \
                          configuration. Nothing was changed."
                    .to_owned(),
                retry: false,
            },
        }
    }

    /// The site form's database is no longer the chosen one: someone submitted step 1 since.
    fn database_changed() -> Self {
        Self {
            status: Status::Conflict,
            title: "The database was changed",
            message: "The database was changed after this form was opened, so nothing was \
                      installed. Review the database step again, then fill in this form."
                .to_owned(),
            retry: true,
        }
    }

    /// A failure while finishing that leaves no `.env`, so the wizard can run again.
    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: Status::InternalServerError,
            title: "The installation did not finish",
            message: message.into(),
            retry: true,
        }
    }

    fn reply(&self) -> Reply {
        page(
            self.status,
            "install.refused",
            &json!({
                "page_title": self.title,
                "message": self.message,
                "retry": self.retry,
            }),
        )
    }
}

/// The address of the site, from the request that finishes the install, as WordPress's
/// `wp_guess_url`: `https` when Rocket serves TLS, else `http`, and the `Host` header (Rocket's
/// own address and port when there is none).
pub struct SiteAddress(String);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SiteAddress {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let config = request.rocket().config();
        let scheme = if config.tls_enabled() {
            "https"
        } else {
            "http"
        };
        let host = request.host().map_or_else(
            || SocketAddr::new(config.address, config.port).to_string(),
            ToString::to_string,
        );
        Outcome::Success(Self(format!("{scheme}://{host}")))
    }
}

/// The installer page: the database step, the site step (`?step=2`, once a database is chosen),
/// or the finished page once the install is done.
#[rocket::get("/mc-admin/install.php?<step>")]
async fn show(step: Option<&str>, wizard: &State<Wizard>, setup: &State<SetupState>) -> Reply {
    match wizard.phase() {
        Phase::Open => {}
        Phase::Installed { home, database } => return finished_page(&home, &database),
        installing => return Refusal::not_open(&installing).reply(),
    }
    let chosen = wizard.chosen();
    match (step, chosen.as_ref()) {
        (Some("2"), Some(chosen)) => site_page(wizard, chosen, &SiteValues::default(), &[]),
        (Some("2"), None) => Reply::Redirect(Redirect::to(INSTALL_PATH)),
        (_, chosen) => {
            let (kind, fields) = chosen.map_or_else(
                || (DatabaseKind::Sqlite, DatabaseFields::default()),
                |c| (c.kind, c.fields.clone()),
            );
            database_page(wizard, setup, kind, &fields, None)
        }
    }
}

/// The finished page, also shown to a visit after the install finished: the site's address
/// and the database it was installed on (type and where, never the password).
fn finished_page(home: &str, database: &str) -> Reply {
    page(
        Status::Ok,
        "install.finished",
        &json!({ "page_title": "Installed", "home": home, "database": database }),
    )
}

/// The database form a step-1 `POST` carries.
#[derive(FromForm)]
struct DatabaseForm {
    #[field(name = "_wpnonce")]
    token: Option<String>,
    driver: Option<String>,
    dbname: Option<String>,
    uname: Option<String>,
    pwd: Option<String>,
    dbhost: Option<String>,
    dbport: Option<String>,
}

/// A database error the database step shows: a heading, what it means, and what to check.
#[derive(Debug, Serialize)]
struct StepError {
    title: String,
    message: String,
    hints: Vec<String>,
}

impl StepError {
    /// A field that is missing or not valid.
    fn field(message: impl Into<String>) -> Self {
        Self {
            title: "Check the database details".to_owned(),
            message: message.into(),
            hints: Vec::new(),
        }
    }
}

/// The database step's page, with `error` above the form when there is one.
fn database_page(
    wizard: &Wizard,
    setup: &SetupState,
    kind: DatabaseKind,
    fields: &DatabaseFields,
    error: Option<StepError>,
) -> Reply {
    let drivers: Vec<Value> = DatabaseKind::ALL
        .into_iter()
        .map(|k| json!({ "value": k.value(), "label": k.label(), "selected": k == kind }))
        .collect();
    let issues: Vec<&str> = setup.issues.iter().map(|i| i.message.as_str()).collect();
    let status = if error.is_some() {
        Status::UnprocessableEntity
    } else {
        Status::Ok
    };
    page(
        status,
        "install.database",
        &json!({
            "page_title": "Setup",
            "token": wizard.form_token(),
            "issues": issues,
            "error": error,
            "drivers": drivers,
            "fields": fields,
        }),
    )
}

/// Step 1: tests the connection to the chosen database. On success the database is kept for the
/// finish and the wizard moves to step 2; on failure the same step shows why.
#[rocket::post("/mc-admin/install.php?step=1", data = "<form>", rank = 1)]
async fn database_step(
    form: Form<DatabaseForm>,
    wizard: &State<Wizard>,
    setup: &State<SetupState>,
) -> Reply {
    if let Some(refusal) = wizard.refuse_unless_open() {
        return refusal.reply();
    }
    let form = form.into_inner();
    if !wizard.token_matches(form.token.as_deref()) {
        return Refusal::expired(wizard.token.is_some()).reply();
    }
    let kind = form
        .driver
        .as_deref()
        .and_then(DatabaseKind::parse)
        .unwrap_or(DatabaseKind::Sqlite);
    // WordPress trims every field, the password too.
    let trimmed = |value: Option<String>| value.map(|v| v.trim().to_owned()).unwrap_or_default();
    let fields = DatabaseFields {
        dbname: trimmed(form.dbname),
        uname: trimmed(form.uname),
        dbhost: trimmed(form.dbhost),
        dbport: trimmed(form.dbport),
    };
    let password = Zeroizing::new(trimmed(form.pwd));

    let target = match kind {
        DatabaseKind::Sqlite => sqlite_target(&wizard.install_dir),
        _ => server_target(kind, &fields, &password),
    };
    let (url, summary) = match target {
        Ok(target) => target,
        Err(error) => return database_page(wizard, setup, kind, &fields, Some(error)),
    };
    if kind == DatabaseKind::Sqlite {
        match create_sqlite_file(&wizard.install_dir.join(SQLITE_FILE_NAME)) {
            Ok(true) => {
                *wizard
                    .created_sqlite
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = true
            }
            Ok(false) => {}
            Err(e) => {
                log::warn!("The installer could not create the SQLite file: {e}");
                return database_page(wizard, setup, kind, &fields, Some(sqlite_error(wizard)));
            }
        }
    }

    let db = match connect(&url).await {
        Ok(db) => db,
        Err(e) => {
            let error = match kind {
                DatabaseKind::Sqlite => {
                    log::warn!("The installer could not open the SQLite file: {e}");
                    sqlite_error(wizard)
                }
                _ => connection_error(kind, &fields, &e),
            };
            return database_page(wizard, setup, kind, &fields, Some(error));
        }
    };
    let installed = holds_a_site(&db).await;
    db.close().await;
    if installed {
        return database_page(wizard, setup, kind, &fields, Some(already_installed()));
    }

    if kind != DatabaseKind::Sqlite {
        remove_unused_sqlite_file(wizard);
    }
    let id = match generate_secret_key() {
        Ok(id) => id,
        Err(e) => return Refusal::failed(e.to_string()).reply(),
    };
    *wizard.chosen() = Some(ChosenDatabase {
        id,
        kind,
        url,
        fields,
        summary,
    });
    Reply::Redirect(Redirect::to(format!("{INSTALL_PATH}?step=2")))
}

/// The URL and summary of the SQLite file in `install_dir`.
fn sqlite_target(install_dir: &Path) -> Result<(Zeroizing<String>, String), StepError> {
    let path = install_dir.join(SQLITE_FILE_NAME);
    let text = path.to_str().ok_or_else(|| {
        StepError::field(
            "The install folder's path is not valid UTF-8, so SQLite cannot be used there. Run \
             Mission Control from another folder, or choose a database server.",
        )
    })?;
    let url = format!("sqlite://{}", utf8_percent_encode(text, SQLITE_PATH));
    Ok((
        Zeroizing::new(url),
        format!("SQLite, the file {SQLITE_FILE_NAME} in the install folder"),
    ))
}

/// The URL and summary of a database server from the form. The user name, password and
/// database name are percent-encoded; the host must be a name or an IP address.
fn server_target(
    kind: DatabaseKind,
    fields: &DatabaseFields,
    password: &str,
) -> Result<(Zeroizing<String>, String), StepError> {
    if fields.dbname.is_empty() {
        return Err(StepError::field(
            "Enter the name of the database you want to use with Mission Control.",
        ));
    }
    let (host, port_in_host) = url_host(&fields.dbhost).ok_or_else(|| {
        StepError::field(
            "The database host must be a host name or an IP address, such as localhost or \
             db.example.com, with no spaces or other characters.",
        )
    })?;
    let port = match (fields.dbport.as_str(), port_in_host) {
        ("", None) => kind.default_port(),
        ("", Some(port)) => port,
        (text, _) => text.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| {
            StepError::field("The port must be a number from 1 to 65535, or empty.")
        })?,
    };

    let mut url = Zeroizing::new(format!("{}://", kind.scheme()));
    if !fields.uname.is_empty() {
        url.push_str(&utf8_percent_encode(&fields.uname, URL_COMPONENT).to_string());
        if !password.is_empty() {
            url.push(':');
            url.push_str(&utf8_percent_encode(password, URL_COMPONENT).to_string());
        }
        url.push('@');
    }
    url.push_str(&format!(
        "{host}:{port}/{}",
        utf8_percent_encode(&fields.dbname, URL_COMPONENT)
    ));
    let summary = format!(
        "{}, the database {} on {host}:{port}",
        kind.name(),
        fields.dbname
    );
    Ok((url, summary))
}

/// The host for a URL, and a port written in it (`localhost:3307`, as WordPress accepts), from
/// the host field. Empty is `localhost`. An IPv6 address is bracketed. `None` for anything that
/// is not a host name or an IP address.
fn url_host(field: &str) -> Option<(String, Option<u16>)> {
    if field.is_empty() {
        return Some(("localhost".to_owned(), None));
    }
    if let Some(inner) = field.strip_prefix('[') {
        let (address, rest) = inner.split_once(']')?;
        address.parse::<Ipv6Addr>().ok()?;
        let port = match rest {
            "" => None,
            rest => Some(rest.strip_prefix(':')?.parse::<u16>().ok()?),
        };
        return Some((format!("[{address}]"), port));
    }
    if field.parse::<Ipv6Addr>().is_ok() {
        return Some((format!("[{field}]"), None));
    }
    let (name, port) = match field.split_once(':') {
        Some((name, port)) => (name, Some(port.parse::<u16>().ok()?)),
        None => (field, None),
    };
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    valid.then(|| (name.to_owned(), port))
}

/// Creates the SQLite file, readable by its owner only on Unix, when nothing is at `path`. An
/// existing file is kept. True when the file was created here.
fn create_sqlite_file(path: &Path) -> io::Result<bool> {
    if path.symlink_metadata().is_ok() {
        return Ok(false);
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Removes the SQLite file the database step created, when the owner chose a database server
/// afterwards and the file is still empty (a regular file of 0 bytes). A file that was there
/// before, or that holds anything, is kept.
fn remove_unused_sqlite_file(wizard: &Wizard) {
    let mut created = wizard
        .created_sqlite
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !*created {
        return;
    }
    let path = wizard.install_dir.join(SQLITE_FILE_NAME);
    let empty = path
        .symlink_metadata()
        .is_ok_and(|m| m.is_file() && m.len() == 0);
    if empty && let Err(e) = std::fs::remove_file(&path) {
        log::warn!("The installer could not remove the unused SQLite file: {e}");
        return;
    }
    *created = false;
}

/// Connects to `url` with Orbit's default pool, which opens one connection to check it and
/// waits at most ten seconds for the server. The migrator needs more than one connection.
async fn connect(url: &str) -> Result<Database, orbit_orm::Error> {
    Database::connect(url).await
}

/// Whether the database already holds a site: its `siteurl` option is set, as WordPress's
/// `is_blog_installed` checks. A database without the options table holds none.
async fn holds_a_site(db: &Database) -> bool {
    matches!(get_option(db, "siteurl").await, Ok(Some(_)))
}

/// The error for a database that already holds a site, as WordPress's "Already Installed".
fn already_installed() -> StepError {
    StepError {
        title: "Already installed".to_owned(),
        message: "This database already holds a Mission Control site. To install again, clear \
                  its tables first, or choose another database."
            .to_owned(),
        hints: Vec::new(),
    }
}

/// The error for a SQLite file that cannot be created or opened.
fn sqlite_error(wizard: &Wizard) -> StepError {
    StepError {
        title: "Error establishing a database connection".to_owned(),
        message: format!(
            "Mission Control could not create or open the SQLite file {}.",
            wizard.install_dir.join(SQLITE_FILE_NAME).display()
        ),
        hints: vec![
            "Can the user that runs Mission Control write to the install folder?".to_owned(),
            "Is there free space on the disk?".to_owned(),
            "If the file is there already, is it a SQLite database?".to_owned(),
        ],
    }
}

/// The plain-language error for a server connection that failed, as WordPress's "Error
/// establishing a database connection" and "Cannot select database". It names the host, the
/// database and the user, never the password.
fn connection_error(
    kind: DatabaseKind,
    fields: &DatabaseFields,
    error: &orbit_orm::Error,
) -> StepError {
    let code = match error {
        orbit_orm::Error::Database(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()),
        _ => None,
    };
    let host = if fields.dbhost.is_empty() {
        "localhost"
    } else {
        fields.dbhost.as_str()
    };
    // Orbit's error names the server's complaint and at most the user, never the password.
    log::warn!(
        "The installer could not connect to the {} database: {error}",
        kind.name()
    );
    match code.as_deref() {
        // MySQL's 1045 (access denied) is SQLSTATE 28000; PostgreSQL's wrong password is 28P01
        // (28000 for a refused role).
        Some("28000" | "28P01") => StepError {
            title: "Error establishing a database connection".to_owned(),
            message: format!(
                "The {} database server at {host} refused the username and password you \
                 entered.",
                kind.name()
            ),
            hints: vec![
                "Are you sure you have the correct username and password?".to_owned(),
                "Does this user have permission to connect from this computer?".to_owned(),
            ],
        },
        // MySQL's 1044 and 1049 (no such database, or no access to it) are SQLSTATE 42000;
        // PostgreSQL's missing database is 3D000.
        Some("42000" | "3D000") => StepError {
            title: "Cannot select database".to_owned(),
            message: format!(
                "Mission Control was able to connect to the database server (which means your \
                 username and password are okay) but not able to select the {} database.",
                fields.dbname
            ),
            hints: vec![
                "Are you sure it exists?".to_owned(),
                format!(
                    "Does the user {} have permission to use the {} database?",
                    fields.uname, fields.dbname
                ),
                "On some systems the name of your database is prefixed with your username. \
                 Could that be the problem?"
                    .to_owned(),
            ],
        },
        _ => StepError {
            title: "Error establishing a database connection".to_owned(),
            message: format!(
                "This either means that the username and password you entered are incorrect, \
                 or that contact with the {} database server at {host} could not be \
                 established. This could mean the database server is down.",
                kind.name()
            ),
            hints: vec![
                "Are you sure you have the correct username and password?".to_owned(),
                "Are you sure you have typed the correct hostname and port?".to_owned(),
                "Are you sure the database server is running?".to_owned(),
            ],
        },
    }
}

/// The site form a step-2 `POST` carries. The administrator's password fields
/// (`admin_password`, `admin_password2`) are deliberately not read: the account is not created
/// yet (Ruling R71), so the password is never kept.
#[derive(FromForm)]
struct SiteForm {
    #[field(name = "_wpnonce")]
    token: Option<String>,
    /// The id of the database choice this form was opened for.
    database_choice: Option<String>,
    weblog_title: Option<String>,
    user_name: Option<String>,
    admin_email: Option<String>,
    blog_public: Option<String>,
}

/// The site step's values echoed back into the form. Never a password.
#[derive(Debug, Clone, Default, Serialize)]
struct SiteValues {
    weblog_title: String,
    user_name: String,
    admin_email: String,
    #[serde(skip)]
    discourage: bool,
}

/// The site step's page, with `errors` above the form.
fn site_page(
    wizard: &Wizard,
    chosen: &ChosenDatabase,
    values: &SiteValues,
    errors: &[String],
) -> Reply {
    let status = if errors.is_empty() {
        Status::Ok
    } else {
        Status::UnprocessableEntity
    };
    page(
        status,
        "install.site",
        &json!({
            "page_title": "Installation",
            "token": wizard.form_token(),
            "choice": chosen.id,
            "database": chosen.summary,
            "errors": errors,
            "fields": values,
            "discourage": values.discourage,
        }),
    )
}

/// The value WordPress's `sanitize_option` gives option `name` (the registered setting's
/// sanitizer), or `None` when it refuses it.
fn sanitize(settings: &Settings, name: &str, value: &str) -> Option<Value> {
    let value = Value::String(value.to_owned());
    match settings
        .shown_in_rest()
        .into_iter()
        .find(|s| s.option_name == name)
        .and_then(|s| s.sanitize)
    {
        Some(sanitize) => sanitize(value),
        None => Some(value),
    }
}

/// Step 2 and the finish: checks the site form, then installs (see [`finish`]) and asks for the
/// restart into normal mode.
#[rocket::post("/mc-admin/install.php?step=2", data = "<form>", rank = 2)]
async fn site_step(
    form: Form<SiteForm>,
    address: SiteAddress,
    wizard: &State<Wizard>,
    restart: &State<RestartHandle>,
) -> Reply {
    if let Some(refusal) = wizard.refuse_unless_open() {
        return refusal.reply();
    }
    let form = form.into_inner();
    if !wizard.token_matches(form.token.as_deref()) {
        return Refusal::expired(wizard.token.is_some()).reply();
    }
    let values = SiteValues {
        weblog_title: form.weblog_title.unwrap_or_default().trim().to_owned(),
        user_name: form.user_name.unwrap_or_default().trim().to_owned(),
        admin_email: form.admin_email.unwrap_or_default().trim().to_owned(),
        discourage: form.blog_public.is_some(),
    };

    // A snapshot of the chosen database, taken only when it is the one this form was opened
    // for: the finish installs on this URL, even if step 1 is submitted again meanwhile.
    let (id, url, summary) = {
        let chosen = wizard.chosen();
        match chosen.as_ref() {
            Some(c) if secret_matches(Some(&c.id), form.database_choice.as_deref()) => {
                (c.id.clone(), c.url.clone(), c.summary.clone())
            }
            Some(_) => return Refusal::database_changed().reply(),
            None => return Reply::Redirect(Redirect::to(INSTALL_PATH)),
        }
    };

    let settings = Settings::new();
    let email = if values.admin_email.is_empty() {
        Err("You must provide an email address.")
    } else {
        sanitize(&settings, "admin_email", &values.admin_email).ok_or(
            "Sorry, that is not a valid email address. Email addresses look like \
             username@example.com.",
        )
    };
    let email = match email {
        Ok(email) => email,
        Err(message) => {
            let chosen = wizard.chosen();
            return match chosen.as_ref() {
                Some(c) if c.id == id => site_page(wizard, c, &values, &[message.to_owned()]),
                _ => Refusal::database_changed().reply(),
            };
        }
    };
    let Some(site_url) = sanitize(&settings, "siteurl", &address.0) else {
        return Refusal::failed(
            "Mission Control could not work out this site's address from the request. Open \
             the installer at the address the site will use, and try again.",
        )
        .reply();
    };
    let blogname =
        sanitize(&settings, "blogname", &values.weblog_title).unwrap_or_else(|| json!(""));
    let options = site_options(site_url.clone(), blogname, email, values.discourage);

    let home = site_url.as_str().unwrap_or("/").to_owned();
    match finish(wizard, &url, &options, &home, &summary).await {
        Ok(()) => {
            log::info!("Mission Control is installed; restarting into normal mode.");
            restart.request();
            finished_page(&home, &summary)
        }
        Err(refusal) => refusal.reply(),
    }
}

/// The options the finish stores, in the order it stores them. `siteurl` is the marker of an
/// installed site (see [`holds_a_site`]), so it comes last: when storing an earlier option
/// fails, the database does not look installed and the wizard can run again.
fn site_options(
    site_url: Value,
    blogname: Value,
    admin_email: Value,
    discourage: bool,
) -> Vec<(&'static str, Value)> {
    vec![
        ("home", site_url.clone()),
        ("blogname", blogname),
        ("blogdescription", json!("")),
        ("admin_email", admin_email),
        ("blog_public", json!(if discourage { 0 } else { 1 })),
        ("posts_per_page", json!(10)),
        ("show_on_front", json!("posts")),
        ("permalink_structure", json!("")),
        ("date_format", json!("F j, Y")),
        ("time_format", json!("g:i a")),
        ("start_of_week", json!(1)),
        ("timezone_string", json!("")),
        ("gmt_offset", json!(0)),
        ("siteurl", site_url),
    ]
}

/// Installs on the database at `url`: checks `.env` is absent, runs the panel and core
/// migrations, stores `options`, and writes `.env` with the URL and a new secret key. `.env` is
/// written last, so a failure before it leaves the wizard able to run again. Two finishes never
/// run at once (the second is told an install is in progress); once one succeeds, the wizard
/// remembers `home` and `database` and refuses every later form.
async fn finish(
    wizard: &Wizard,
    url: &str,
    options: &[(&str, Value)],
    home: &str,
    database: &str,
) -> Result<(), Refusal> {
    let installing = wizard.start_installing()?;
    let env_path = wizard.install_dir.join(ENV_FILE_NAME);
    if env_path.symlink_metadata().is_ok() {
        return Err(env_exists(&env_path));
    }
    let secret = generate_secret_key().map_err(|e| Refusal::failed(e.to_string()))?;
    let env = EnvFile::new(url, secret);
    // Checked before the database is touched: a value `.env` cannot hold stops here.
    env.render().map_err(|e| Refusal::failed(e.to_string()))?;

    let db = connect(url).await.map_err(|e| {
        log::error!("The installer could not reconnect to the database: {e}");
        Refusal::failed(
            "Mission Control could not connect to the database it tested a moment ago. Check \
             that the database server is running, then submit the form again. Nothing was \
             saved.",
        )
    })?;
    let result = install_database(&db, url, options).await;
    db.close().await;
    result?;

    match env_file::write_new(&wizard.install_dir, &env) {
        Ok(_) => {
            installing.finished(home, database);
            Ok(())
        }
        Err(EnvFileError::AlreadyExists { path }) => Err(env_exists(&path)),
        Err(e) => {
            log::error!("The installer could not write .env: {e}");
            Err(Refusal::failed(format!(
                "{e} The database is set up already: to run the installer again, clear its \
                 tables first."
            )))
        }
    }
}

/// The refusal for an existing `.env`, with the `.env` writer's message.
fn env_exists(path: &Path) -> Refusal {
    Refusal {
        status: Status::Conflict,
        title: "Mission Control is already configured",
        message: EnvFileError::AlreadyExists {
            path: path.to_path_buf(),
        }
        .to_string(),
        retry: false,
    }
}

/// Runs the panel and core migrations on the database at `url` (each on its own connection, see
/// [`run_migrations`]) and stores `options` on `db`, its connection, unless it already holds a
/// site.
async fn install_database(
    db: &Database,
    url: &str,
    options: &[(&str, Value)],
) -> Result<(), Refusal> {
    if holds_a_site(db).await {
        return Err(Refusal {
            status: Status::Conflict,
            title: "Already installed",
            message: already_installed().message,
            retry: false,
        });
    }
    match run_migrations(url).await {
        Ok(summary) => log::info!(
            "The installer ran {} panel and {} core migrations.",
            summary.panel.len(),
            summary.core.len()
        ),
        Err(e @ (InstallError::CoreInvalid(_) | InstallError::VersionCollision(_))) => {
            log::error!("The installer did not run the migrations: {e}");
            return Err(Refusal::failed(
                "Mission Control's database migrations are not valid, so nothing was changed. \
                 Check the log.",
            ));
        }
        Err(e) => {
            log::error!("The installer could not run the migrations: {e}");
            return Err(Refusal::failed(
                "Mission Control could not create its tables in the database. Nothing was \
                 written to .env, so you can fix the problem (the log has the details) and \
                 submit the form again.",
            ));
        }
    }
    for (name, value) in options {
        if let Err(e) = set_option(db, name, value).await {
            log::error!("The installer could not store the {name} option: {e}");
            return Err(Refusal::failed(
                "Mission Control created its tables but could not store the site's settings. \
                 Nothing was written to .env, so you can fix the problem (the log has the \
                 details) and submit the form again.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(name: &str, user: &str, host: &str, port: &str) -> DatabaseFields {
        DatabaseFields {
            dbname: name.to_owned(),
            uname: user.to_owned(),
            dbhost: host.to_owned(),
            dbport: port.to_owned(),
        }
    }

    fn url(kind: DatabaseKind, fields: &DatabaseFields, password: &str) -> String {
        match server_target(kind, fields, password) {
            Ok((url, _)) => url.to_string(),
            Err(e) => panic!("{}", e.message),
        }
    }

    #[test]
    fn server_urls_are_percent_encoded() {
        assert_eq!(
            url(
                DatabaseKind::MySql,
                &fields("my db", "us@er:x", "localhost", ""),
                "p@ss:w/rd?#%"
            ),
            "mysql://us%40er%3Ax:p%40ss%3Aw%2Frd%3F%23%25@localhost:3306/my%20db"
        );
        assert_eq!(
            url(
                DatabaseKind::MariaDb,
                &fields("site", "u", "db.example.com", "3398"),
                ""
            ),
            "mysql://u@db.example.com:3398/site"
        );
        assert_eq!(
            url(DatabaseKind::Postgres, &fields("site", "", "", ""), "x"),
            "postgres://localhost:5432/site"
        );
        assert_eq!(
            url(
                DatabaseKind::Postgres,
                &fields("site", "u", "127.0.0.1:6543", ""),
                ""
            ),
            "postgres://u@127.0.0.1:6543/site"
        );
        assert_eq!(
            url(DatabaseKind::MySql, &fields("s", "u", "::1", ""), ""),
            "mysql://u@[::1]:3306/s"
        );
        assert_eq!(
            url(DatabaseKind::MySql, &fields("s", "u", "[::1]:3307", ""), ""),
            "mysql://u@[::1]:3307/s"
        );
    }

    #[test]
    fn bad_server_fields_are_refused_without_the_password() {
        for (f, password) in [
            (fields("", "u", "localhost", ""), "secret-pw"),
            (fields("s", "u", "local host", ""), "secret-pw"),
            (fields("s", "u", "host/x", ""), "secret-pw"),
            (fields("s", "u", "localhost", "0"), "secret-pw"),
            (fields("s", "u", "localhost", "70000"), "secret-pw"),
            (fields("s", "u", "localhost:abc", ""), "secret-pw"),
        ] {
            let error = server_target(DatabaseKind::MySql, &f, password)
                .err()
                .unwrap_or_else(|| panic!("{f:?} is refused"));
            assert!(!error.message.contains(password));
        }
    }

    #[test]
    fn sqlite_paths_are_encoded() {
        let (url, _) = sqlite_target(Path::new("/tmp/a b?c#d%e")).unwrap();
        assert_eq!(&*url, "sqlite:///tmp/a b%3Fc%23d%25e/database.sqlite");
        let (url, _) = sqlite_target(Path::new(".")).unwrap();
        assert_eq!(&*url, "sqlite://./database.sqlite");
    }

    #[test]
    fn kinds_round_trip() {
        for kind in DatabaseKind::ALL {
            assert_eq!(DatabaseKind::parse(kind.value()), Some(kind));
        }
        assert_eq!(DatabaseKind::parse("oracle"), None);
    }

    #[test]
    fn tokens_are_compared_exactly() {
        let wizard = Wizard::new(".");
        let token = wizard.token.clone().expect("random bytes are available");
        assert!(wizard.token_matches(Some(&token)));
        assert!(!wizard.token_matches(None));
        assert!(!wizard.token_matches(Some("")));
        assert!(!wizard.token_matches(Some(&token[1..])));
        let mut other = token.clone();
        other.replace_range(0..1, if token.starts_with('A') { "B" } else { "A" });
        assert!(!wizard.token_matches(Some(&other)));
    }

    #[test]
    fn debug_hides_the_url_and_token() {
        let chosen = ChosenDatabase {
            id: "choice-id-secret".to_owned(),
            kind: DatabaseKind::MySql,
            url: Zeroizing::new("mysql://u:hunter2@h/d".to_owned()),
            fields: DatabaseFields::default(),
            summary: String::new(),
        };
        assert!(!format!("{chosen:?}").contains("hunter2"));
        assert!(!format!("{chosen:?}").contains("choice-id-secret"));
        let wizard = Wizard::new(".");
        let token = wizard.token.clone().unwrap();
        assert!(!format!("{wizard:?}").contains(&token));
    }

    #[test]
    fn siteurl_is_stored_last() {
        let options = site_options(json!("http://x.test"), json!("T"), json!("a@b.test"), true);
        assert_eq!(options.last().map(|(name, _)| *name), Some("siteurl"));
        assert_eq!(options.iter().filter(|(n, _)| *n == "siteurl").count(), 1);
        assert!(
            options
                .iter()
                .any(|(n, v)| *n == "blog_public" && *v == json!(0))
        );
    }

    #[test]
    fn secrets_never_match_when_missing_or_empty() {
        assert!(secret_matches(Some("abc"), Some("abc")));
        assert!(!secret_matches(Some(""), Some("")));
        assert!(!secret_matches(None, None));
        assert!(!secret_matches(Some("abc"), None));
        assert!(!secret_matches(Some("abc"), Some("abd")));
    }
}
