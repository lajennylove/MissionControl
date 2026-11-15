//! The prebuilt Mission Control binary: setup mode and restart on top of the
//! `mission-control` library.
//!
//! It reads the configuration (`.env` and the environment). A configured site runs the app; a
//! site with no `.env` and no `DATABASE_URL` or `IGNITE_SECRET_KEY` in the environment starts in
//! setup mode, which serves only the installer and opens no database (see
//! [`mission_control_app::setup`]). Any other configuration error, including a `.env` that lacks
//! either variable, stops the binary with a plain-language message. Restart after setup
//! comes with its own task.

use mission_control_app::setup::{Mode, setup_rocket};

#[rocket::launch]
fn launch() -> _ {
    match Mode::from_environment() {
        Ok(Mode::Normal(config)) => mission_control::rocket(config),
        Ok(Mode::Setup) => setup_rocket("."),
        Err(e) => {
            eprintln!("Mission Control cannot start: {e}");
            std::process::exit(1);
        }
    }
}
