//! The prebuilt Mission Control binary: setup mode and restart on top of the
//! `mission-control` library.
//!
//! For now it reads the configuration (`.env` and the environment) and runs the app. Setup mode
//! (the installer) and restart come with their own tasks.

#[rocket::launch]
fn launch() -> _ {
    let config = match mission_control_core::config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Mission Control cannot start: {e}");
            std::process::exit(1);
        }
    };
    mission_control::rocket(config)
}
