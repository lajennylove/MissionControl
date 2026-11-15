//! Setup mode: what the binary runs before the site is configured (design 5.1).
//!
//! At start the binary reads its configuration from `.env` and the environment. When
//! `DATABASE_URL` and `IGNITE_SECRET_KEY` are both there (and not blank), it runs the app
//! ([`Mode::Normal`]). When there is no `.env` and the environment lacks either, the site is not
//! configured yet and the binary starts in setup mode ([`Mode::Setup`]): only the installer is
//! mounted, at `/mc-admin/install.php` (WordPress's `/wp-admin/install.php`), every other path redirects there, and no
//! database is opened.
//!
//! A `.env` that exists but lacks either variable is an error, not setup mode
//! ([`ConfigError::Incomplete`]): the installer never writes over an existing `.env`, as
//! WordPress never installs over an existing `wp-config.php`. So is any other problem with the
//! configuration (a `.env` line that cannot be read, an unreadable `.env`, an invalid value):
//! it stops the binary with a plain-language error.
//!
//! As WordPress's installer, the setup page is open to whoever reaches the site first; the
//! binary logs that at start, so the owner knows to finish setup straight away. The wizard's
//! steps are in [`crate::install::wizard`]; its first page also lists the installer's preflight
//! issues.

use std::path::{Path, PathBuf};

use crate::install::wizard::{self, Wizard};
use crate::restart::RestartHandle;
use mission_control_core::config::{self, Config, ConfigError};
use mission_control_core::install::preflight::{IssueCode, PreflightIssue, preflight};
use rocket::fairing::AdHoc;
use rocket::http::Method;
use rocket::response::Redirect;
use rocket::route::{Handler, Outcome, Route};
use rocket::{Build, Data, Request, Rocket};

/// The installer's path, `/mc-admin/install.php`: WordPress's `wp-admin/install.php`, with
/// Mission Control's initials (owner decision).
pub const INSTALL_PATH: &str = "/mc-admin/install.php";

/// The variables a configured site has. Without both (and no `.env`), the binary starts in
/// setup mode.
pub const REQUIRED: [&str; 2] = ["DATABASE_URL", "IGNITE_SECRET_KEY"];

/// The rank of the route that redirects every other path to the installer: after the
/// installer's own route.
const REDIRECT_RANK: isize = 100;

/// How the binary starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The site is configured: run the app with this configuration.
    Normal(Config),
    /// The site is not configured yet: serve only the installer, open no database.
    Setup,
}

impl Mode {
    /// The mode for the configuration in `env_file` (when given) and `lookup`, which overrides
    /// the file, as [`config::load_from`] reads them.
    ///
    /// Setup mode when `DATABASE_URL` or `IGNITE_SECRET_KEY` is missing (or blank) and
    /// `env_file` does not exist. When the file exists and either is missing, the error is
    /// [`ConfigError::Incomplete`], naming the file and the missing variables. Every other
    /// configuration error is returned, also when a required variable is missing too: a `.env`
    /// that cannot be read, or an invalid optional value, is never mistaken for a site that is
    /// not configured yet.
    pub fn from_lookup(
        env_file: Option<&Path>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        match config::load_from(env_file, &lookup) {
            Ok(config) => Ok(Self::Normal(config)),
            Err(ConfigError::Missing(_)) => {
                // Check the rest of the configuration as if the required variables were set,
                // so an invalid optional value is still reported.
                config::load_from(env_file, stand_in(&lookup, None))?;
                // `symlink_metadata`: a link at `.env`, even a broken one, is a file the
                // installer would not write over.
                match env_file.filter(|path| path.symlink_metadata().is_ok()) {
                    None => Ok(Self::Setup),
                    Some(path) => Err(ConfigError::Incomplete {
                        path: path.to_path_buf(),
                        missing: REQUIRED
                            .into_iter()
                            .filter(|&name| {
                                matches!(
                                    config::load_from(Some(path), stand_in(&lookup, Some(name))),
                                    Err(ConfigError::Missing(missing)) if missing == name
                                )
                            })
                            .collect(),
                    }),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The mode for `.env` in the working directory and the process environment, as
    /// [`config::load`] reads them: a variable set to a value that is not valid UTF-8 is an
    /// [`ConfigError::Invalid`] error, not a missing one.
    pub fn from_environment() -> Result<Self, ConfigError> {
        Self::from_lookup(Some(Path::new(".env")), config::process_environment()?)
    }
}

/// `lookup`, with a stand-in value for every required variable it lacks (or has blank) except
/// `except`. The stand-in hides the `.env` file's value too, which is fine: it is only used to
/// check the other variables.
fn stand_in<'a>(
    lookup: &'a impl Fn(&str) -> Option<String>,
    except: Option<&'a str>,
) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        if Some(name) == except || !REQUIRED.contains(&name) {
            return lookup(name);
        }
        lookup(name)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| Some("setup-mode stand-in".to_owned()))
    }
}

/// What the setup page shows: the installer's preflight issues, found at start.
#[derive(Debug, Clone, Default)]
pub struct SetupState {
    /// The problems the preflight checks found in the install folder and the port. Empty means
    /// ready.
    pub issues: Vec<PreflightIssue>,
}

/// The setup-mode app: the installer at [`INSTALL_PATH`], a redirect there from every other
/// path, and no database. At ignition it runs the installer's preflight checks on `install_dir`
/// (where `.env` will be written) and the configured port, and shows their issues on the page.
pub fn setup_rocket(install_dir: impl Into<PathBuf>) -> Rocket<Build> {
    setup_rocket_with_restart(install_dir, &RestartHandle::new())
}

/// [`setup_rocket`] with `restart` in Rocket's state, so the wizard can restart the binary into
/// normal mode once it has written `.env` (see [`crate::restart`]). `main` keeps a clone of the
/// handle to see whether the restart was asked for when Rocket returns.
pub fn setup_rocket_with_restart(
    install_dir: impl Into<PathBuf>,
    restart: &RestartHandle,
) -> Rocket<Build> {
    let install_dir = install_dir.into();
    let redirects = [
        Method::Get,
        Method::Post,
        Method::Put,
        Method::Patch,
        Method::Delete,
        Method::Options,
    ]
    .into_iter()
    .map(|method| Route::ranked(REDIRECT_RANK, method, "/<_..>", ToInstaller));

    let rocket = rocket::build()
        .manage(Wizard::new(install_dir.clone()))
        .mount("/", wizard::routes())
        .mount("/", redirects.collect::<Vec<_>>())
        .attach(AdHoc::on_ignite("Mission Control preflight", |rocket| {
            Box::pin(async move {
                let port = rocket
                    .figment()
                    .extract_inner::<u16>("port")
                    .unwrap_or(rocket::Config::default().port);
                let issues = run_checks(move || preflight(&install_dir, port)).await;
                rocket.manage(SetupState { issues })
            })
        }))
        .attach(AdHoc::on_liftoff("Mission Control setup mode", |rocket| {
            Box::pin(async move {
                let config = rocket.config();
                let scheme = if config.tls_enabled() {
                    "https"
                } else {
                    "http"
                };
                log::warn!(
                    "Mission Control is not configured yet, so it runs in setup mode. The \
                     installer at {scheme}://{}:{}{INSTALL_PATH} is open to anyone who reaches \
                     this site: the first visitor completes setup. Finish setup now.",
                    config.address,
                    config.port
                );
            })
        }));
    restart.attach(rocket)
}

/// Runs the preflight `checks` on a blocking thread and logs the issues they find. When the
/// checks cannot finish (they panicked), the failure is logged and reported as an issue of its
/// own, so the page never looks ready when nothing was checked.
async fn run_checks(
    checks: impl FnOnce() -> Vec<PreflightIssue> + Send + 'static,
) -> Vec<PreflightIssue> {
    let issues = match rocket::tokio::task::spawn_blocking(checks).await {
        Ok(issues) => issues,
        Err(e) => {
            log::error!("The installer checks could not run: {e}");
            vec![PreflightIssue {
                code: IssueCode::ChecksFailed,
                message: "The installer checks could not run, so Mission Control does not know \
                          whether this folder and port are ready. Check the log, fix the \
                          problem it names, and restart Mission Control."
                    .to_owned(),
            }]
        }
    };
    for issue in &issues {
        log::warn!("Preflight: {}", issue.message);
    }
    issues
}

/// Redirects a request to the installer, as WordPress does for every page of a site that is
/// not installed (a `302 Found`).
#[derive(Clone, Copy)]
struct ToInstaller;

#[rocket::async_trait]
impl Handler for ToInstaller {
    async fn handle<'r>(&self, request: &'r Request<'_>, _data: Data<'r>) -> Outcome<'r> {
        Outcome::from(request, Redirect::found(INSTALL_PATH))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rocket::async_test]
    async fn failed_checks_are_an_issue() {
        let issues = run_checks(|| panic!("a check failed")).await;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, IssueCode::ChecksFailed);
        assert!(issues[0].message.contains("could not run"));
    }

    #[rocket::async_test]
    async fn passing_checks_are_kept() {
        assert!(run_checks(Vec::new).await.is_empty());
    }
}
