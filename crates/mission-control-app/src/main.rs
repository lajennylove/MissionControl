//! The prebuilt Mission Control binary: setup mode and restart on top of the
//! `mission-control` library.
//!
//! It reads the configuration (`.env` and the environment). A configured site runs the app; a
//! site with no `.env` and no `DATABASE_URL` or `IGNITE_SECRET_KEY` in the environment starts in
//! setup mode, which serves only the installer and opens no database (see
//! [`mission_control_app::setup`]). Any other configuration error, including a `.env` that lacks
//! either variable, stops the binary with a plain-language message.
//!
//! When setup asks for a restart, Rocket shuts down gracefully and the binary starts again into
//! normal mode: by replacing itself, or, under systemd or a container, by exiting with status 0
//! for the supervisor to start again (see [`mission_control_app::restart`]).
//!
//! `mission-control-app ignite <command>` runs Ignite's command line instead of the site, as
//! Laravel's `artisan` is the app's own console (Ruling R122): `ignite user create --email ...
//! --name ...`, `ignite user role add <email> administrator` and the others `ignite` lists. It
//! reads `DATABASE_URL` from the environment or `.env`, and exits with the command's status.

use mission_control_app::restart::{Action, RestartHandle, RestartMethod, action, restart};
use mission_control_app::setup::{Mode, setup_rocket_with_restart};

fn main() {
    // The console runs on its own runtime, so it starts before Rocket's.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("ignite") {
        // Ignite stops with a panic when DATABASE_URL is missing; say it plainly instead.
        ignite_panel::load_env();
        if std::env::var_os("DATABASE_URL").is_none() {
            eprintln!("DATABASE_URL must be set, in the environment or in .env.");
            std::process::exit(1);
        }
        // A release build of Ignite stops with a panic without a valid key; a debug build uses a
        // random one, which the console does not need.
        if !cfg!(debug_assertions)
            && let Err(problem) = check_secret_key(std::env::var("IGNITE_SECRET_KEY").ok())
        {
            eprintln!("{problem}");
            std::process::exit(1);
        }
        std::process::exit(ignite_panel::Ignite::new().call_cli(args));
    }
    // Read (and remove) the test hook before the runtime starts: no other thread exists yet.
    let hook = test_hook::take();
    rocket::execute(run(hook));
}

/// The shortest signing key Ignite accepts, in bytes.
const MIN_SECRET_KEY_BYTES: usize = 32;

/// Checks `IGNITE_SECRET_KEY` as Ignite does: base64 of at least 32 bytes. The message says what
/// to do, and never repeats the key.
fn check_secret_key(key: Option<String>) -> Result<(), &'static str> {
    use base64::Engine;

    let Some(key) = key else {
        return Err(
            "IGNITE_SECRET_KEY must be set, in the environment or in .env \
                    (try `openssl rand -base64 32`).",
        );
    };
    match base64::engine::general_purpose::STANDARD.decode(key.trim()) {
        Ok(bytes) if bytes.len() >= MIN_SECRET_KEY_BYTES => Ok(()),
        Ok(_) => Err("IGNITE_SECRET_KEY must decode to at least 32 bytes \
                      (try `openssl rand -base64 32`)."),
        Err(_) => Err("IGNITE_SECRET_KEY must be base64 (try `openssl rand -base64 32`)."),
    }
}

async fn run(hook: test_hook::Hook) {
    let handle = RestartHandle::new();
    let rocket = match Mode::from_environment() {
        Ok(Mode::Normal(config)) => mission_control::rocket(config),
        Ok(Mode::Setup) => {
            test_hook::attach(hook, setup_rocket_with_restart(".", &handle), &handle)
        }
        Err(e) => {
            eprintln!("Mission Control cannot start: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = rocket.launch().await {
        eprintln!("Mission Control stopped with an error: {e}");
        std::process::exit(1);
    }
    // The method is only worked out when a restart was asked for.
    if handle.requested() {
        let (method, reason) = RestartMethod::detect();
        if action(true, method) != Action::Stop {
            restart(method, &reason);
        }
    }
}

/// The binary's own restart test hook, compiled only with the `test-hooks` feature (which the
/// integration test enables through a dev-dependency on this package): with
/// `MC_TEST_RESTART_ON_LIFTOFF` set, setup mode asks for a restart as soon as it is up. The
/// variable is removed at start so the process that replaces this one runs normally.
#[cfg(feature = "test-hooks")]
mod test_hook {
    use mission_control_app::restart::RestartHandle;
    use rocket::{Build, Rocket};

    /// Whether the hook was asked for.
    pub struct Hook(bool);

    /// Reads and removes the variable. Called from `main` before any thread starts.
    pub fn take() -> Hook {
        const HOOK: &str = "MC_TEST_RESTART_ON_LIFTOFF";
        let set = std::env::var_os(HOOK).is_some();
        if set {
            // SAFETY: called first in `main`, before the runtime or any other thread starts,
            // so nothing reads or writes the environment concurrently.
            unsafe { std::env::remove_var(HOOK) };
        }
        Hook(set)
    }

    /// Makes `rocket` ask for a restart at liftoff when the hook is set.
    pub fn attach(hook: Hook, rocket: Rocket<Build>, handle: &RestartHandle) -> Rocket<Build> {
        if !hook.0 {
            return rocket;
        }
        let handle = handle.clone();
        rocket.attach(rocket::fairing::AdHoc::on_liftoff(
            "Mission Control restart test",
            move |_| Box::pin(async move { handle.request() }),
        ))
    }
}

/// Without the feature there is no test hook.
#[cfg(not(feature = "test-hooks"))]
mod test_hook {
    use mission_control_app::restart::RestartHandle;
    use rocket::{Build, Rocket};

    pub struct Hook;

    pub fn take() -> Hook {
        Hook
    }

    pub fn attach(_: Hook, rocket: Rocket<Build>, _: &RestartHandle) -> Rocket<Build> {
        rocket
    }
}

#[cfg(test)]
mod tests {
    use super::check_secret_key;

    #[test]
    fn the_secret_key_is_checked_as_ignite_checks_it() {
        assert!(check_secret_key(None).is_err());
        assert!(check_secret_key(Some("not base64!".into())).is_err());
        assert!(check_secret_key(Some("c2hvcnQ=".into())).is_err());
        let key = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
        assert!(check_secret_key(Some(key.into())).is_ok());
        assert!(check_secret_key(Some(format!(" {key}\n"))).is_ok());
    }
}
