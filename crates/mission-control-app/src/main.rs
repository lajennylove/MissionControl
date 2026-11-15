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

use mission_control_app::restart::{Action, RestartHandle, RestartMethod, action, restart};
use mission_control_app::setup::{Mode, setup_rocket_with_restart};

fn main() {
    // Read (and remove) the test hook before the runtime starts: no other thread exists yet.
    let hook = test_hook::take();
    rocket::execute(run(hook));
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
