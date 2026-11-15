//! Restart into normal mode (design 5.1): after the wizard writes `.env`, the binary starts again
//! so it runs the configured app.
//!
//! How it starts again depends on who started it:
//!
//! - **Started by hand** (a terminal, a script, `cargo run`): the process replaces itself. It
//!   shuts Rocket down gracefully, then runs the same executable again with the same arguments,
//!   working folder and environment (`exec` on Unix, so the process id stays the same). The new
//!   process reads the new `.env` and starts in normal mode. On Windows there is no `exec`: the
//!   new process is spawned and the old one exits with status 0, so the process id changes and
//!   the new process is no longer a child of the old one's parent job.
//! - **Under a supervisor** (systemd or a container): the process shuts Rocket down gracefully
//!   and exits with status 0. The supervisor starts it again, as it does after any exit, if its
//!   restart policy covers a clean exit: `Restart=always` in a systemd unit (`Restart=on-failure`
//!   does not restart a status-0 exit), `--restart unless-stopped` or `always` for Docker
//!   (`on-failure` does not), `restartPolicy: Always` for Kubernetes. Without such a policy the
//!   site stays stopped after setup until someone starts it.
//!
//! [`RestartMethod::detect`] chooses between them. A supervisor is recognised by systemd's
//! `INVOCATION_ID` or `NOTIFY_SOCKET` variables, or by a container's `/.dockerenv`,
//! `/run/.containerenv` or `container` variable. `MC_RESTART=exec` or `MC_RESTART=exit` overrides
//! the detection (for a container that is not recognised, or a supervisor that is). Any other
//! value is ignored and detection decides.
//!
//! The wizard asks for the restart through the [`RestartHandle`] in Rocket's state
//! ([`RestartHandle::request`]), which shuts the server down and records the request. `main`
//! reads the answer after Rocket returns: [`action`] decides, [`restart`] carries it out.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use rocket::Shutdown;
use rocket::fairing::AdHoc;

/// The variable that overrides the detection: `exec` or `exit`.
pub const OVERRIDE_VARIABLE: &str = "MC_RESTART";

/// The supervisor that starts the binary again after it exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supervisor {
    /// systemd: `INVOCATION_ID` or `NOTIFY_SOCKET` is set.
    Systemd,
    /// A container: `/.dockerenv` or `/run/.containerenv` exists, or `container` is set.
    Container,
}

impl Supervisor {
    /// The supervisor the process runs under, from `lookup` (the environment) and `exists` (a
    /// file-exists probe), or `None` when it was started by hand. A variable that is set but
    /// blank does not count.
    pub fn detect(
        lookup: impl Fn(&str) -> Option<String>,
        exists: impl Fn(&Path) -> bool,
    ) -> Option<Self> {
        let set = |name: &str| lookup(name).is_some_and(|value| !value.trim().is_empty());
        if set("INVOCATION_ID") || set("NOTIFY_SOCKET") {
            Some(Self::Systemd)
        } else if exists(Path::new("/.dockerenv"))
            || exists(Path::new("/run/.containerenv"))
            || set("container")
        {
            Some(Self::Container)
        } else {
            None
        }
    }
}

/// How the binary starts again after setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartMethod {
    /// Replace the process with a new one running the same executable.
    Exec,
    /// Exit with status 0 and let the supervisor start the binary again.
    Exit,
}

impl RestartMethod {
    /// The method for `lookup` and `exists`: `MC_RESTART` when it is `exec` or `exit` (any case),
    /// else `Exit` under a [`Supervisor`] and `Exec` when started by hand.
    pub fn choose(lookup: impl Fn(&str) -> Option<String>, exists: impl Fn(&Path) -> bool) -> Self {
        Self::choose_with_reason(lookup, exists).0
    }

    /// [`choose`](Self::choose), and a plain-language reason naming the variable or marker that
    /// decided it.
    pub fn choose_with_reason(
        lookup: impl Fn(&str) -> Option<String>,
        exists: impl Fn(&Path) -> bool,
    ) -> (Self, String) {
        match lookup(OVERRIDE_VARIABLE)
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("exec") => {
                return (Self::Exec, format!("{OVERRIDE_VARIABLE}=exec is set"));
            }
            Some("exit") => {
                return (Self::Exit, format!("{OVERRIDE_VARIABLE}=exit is set"));
            }
            _ => {}
        }
        let set = |name: &str| lookup(name).is_some_and(|value| !value.trim().is_empty());
        let marker = if set("INVOCATION_ID") {
            Some("systemd (INVOCATION_ID is set)")
        } else if set("NOTIFY_SOCKET") {
            Some("systemd (NOTIFY_SOCKET is set)")
        } else if exists(Path::new("/.dockerenv")) {
            Some("a container (/.dockerenv exists)")
        } else if exists(Path::new("/run/.containerenv")) {
            Some("a container (/run/.containerenv exists)")
        } else if set("container") {
            Some("a container (the container variable is set)")
        } else {
            None
        };
        match marker {
            Some(marker) => (Self::Exit, format!("it runs under {marker}")),
            None => (
                Self::Exec,
                "no systemd or container marker was found, so it was started by hand".to_owned(),
            ),
        }
    }

    /// The method for the process environment and the real file system, and why. A
    /// `MC_RESTART` value that is neither `exec` nor `exit` is logged and ignored.
    pub fn detect() -> (Self, String) {
        let lookup = |name: &str| std::env::var(name).ok();
        if let Some(value) = lookup(OVERRIDE_VARIABLE)
            && !matches!(value.trim().to_ascii_lowercase().as_str(), "exec" | "exit")
        {
            log::warn!(
                "{OVERRIDE_VARIABLE} is set to {value:?}, which is neither \"exec\" nor \"exit\", \
                 so Mission Control ignores it and detects how to restart."
            );
        }
        Self::choose_with_reason(lookup, |path| path.exists())
    }
}

/// What `main` does after Rocket returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nobody asked for a restart: Rocket stopped on its own (a signal), so the process ends.
    Stop,
    /// Exit with status 0; the supervisor starts the binary again.
    Exit,
    /// Replace the process with the same executable.
    Exec,
}

/// The action for a Rocket that returned: `requested` is whether the restart was asked for
/// ([`RestartHandle::requested`]) and `method` how to restart. Pure, so it is tested without
/// restarting.
pub fn action(requested: bool, method: RestartMethod) -> Action {
    match (requested, method) {
        (false, _) => Action::Stop,
        (true, RestartMethod::Exit) => Action::Exit,
        (true, RestartMethod::Exec) => Action::Exec,
    }
}

/// Carries out a restart for `method` and never returns. Call it after Rocket has returned, so
/// the shutdown was graceful and the port is free.
///
/// `Exit` ends the process with status 0. `Exec` runs the current executable again with the
/// same arguments, folder and environment; if that fails (the executable is gone, or not
/// allowed to run), it prints a plain-language message and exits with status 1.
pub fn restart(method: RestartMethod, reason: &str) -> ! {
    let (what, other) = match method {
        RestartMethod::Exit => (
            "exits with status 0 so its supervisor starts it again",
            "exec",
        ),
        RestartMethod::Exec => ("replaces its own process (exec)", "exit"),
    };
    log::warn!(
        "Mission Control restarts into normal mode: it {what}, because {reason}. Set \
         {OVERRIDE_VARIABLE}={other} to restart the other way."
    );
    match method {
        RestartMethod::Exit => std::process::exit(0),
        RestartMethod::Exec => {
            let error = exec_self();
            log::error!("Restart failed: {error}");
            eprintln!(
                "Mission Control is configured, but it could not restart itself: {error}. \
                 Start it again by hand."
            );
            std::process::exit(1)
        }
    }
}

/// The executables to try, in order: `current_exe`; then, when that fails, the same path without
/// the " (deleted)" suffix Linux adds after an in-place upgrade replaced the file; then
/// `arg0` resolved against `cwd`.
fn candidates(
    current: std::io::Result<PathBuf>,
    arg0: Option<&OsStr>,
    cwd: Option<&Path>,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(path) = current {
        if let Some(stripped) = path.to_str().and_then(|p| p.strip_suffix(" (deleted)")) {
            found.push(path.clone());
            found.push(PathBuf::from(stripped));
        } else {
            found.push(path);
        }
    }
    if let (Some(arg0), Some(cwd)) = (arg0, cwd) {
        found.push(cwd.join(arg0));
    }
    found
}

/// Replaces the process with the current executable (Unix), trying the [`candidates`] in turn.
/// Returns only the first error.
#[cfg(unix)]
fn exec_self() -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let mut first = None;
    let args = std::env::args_os().collect::<Vec<_>>();
    let cwd = std::env::current_dir().ok();
    for exe in candidates(
        std::env::current_exe(),
        args.first().map(|a| a.as_os_str()),
        cwd.as_deref(),
    ) {
        let error = std::process::Command::new(&exe)
            .args(args.iter().skip(1))
            .exec();
        log::warn!("Cannot run {}: {error}", exe.display());
        first.get_or_insert(error);
    }
    first.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "the executable is not known")
    })
}

/// Starts the current executable (the same [`candidates`]) and exits with status 0 (there is no
/// `exec` on Windows). Returns only the first error.
#[cfg(not(unix))]
fn exec_self() -> std::io::Error {
    let mut first = None;
    let args = std::env::args_os().collect::<Vec<_>>();
    let cwd = std::env::current_dir().ok();
    for exe in candidates(
        std::env::current_exe(),
        args.first().map(|a| a.as_os_str()),
        cwd.as_deref(),
    ) {
        match std::process::Command::new(&exe)
            .args(args.iter().skip(1))
            .spawn()
        {
            Ok(_) => std::process::exit(0),
            Err(error) => {
                log::warn!("Cannot run {}: {error}", exe.display());
                first.get_or_insert(error);
            }
        }
    }
    first.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "the executable is not known")
    })
}

/// Asks the server to restart. It lives in Rocket's state (a clone of the handle `main` holds),
/// so the wizard takes it as `&State<RestartHandle>` and calls [`request`](Self::request) after
/// it writes `.env`.
#[derive(Debug, Clone, Default)]
pub struct RestartHandle {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Whether a restart was asked for.
    requested: AtomicBool,
    /// Rocket's shutdown handle, stored at liftoff.
    shutdown: OnceLock<Shutdown>,
}

impl RestartHandle {
    /// A handle with no restart requested.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks for a restart: records the request and shuts Rocket down gracefully (in-flight
    /// requests, such as the one that calls this, finish first). Calling it again does nothing.
    /// A request before liftoff takes effect at liftoff.
    pub fn request(&self) {
        self.inner.requested.store(true, Ordering::SeqCst);
        if let Some(shutdown) = self.inner.shutdown.get() {
            shutdown.clone().notify();
        }
    }

    /// Whether a restart was asked for.
    pub fn requested(&self) -> bool {
        self.inner.requested.load(Ordering::SeqCst)
    }

    /// Puts the handle in `rocket`'s state and attaches the fairing that gives it Rocket's
    /// shutdown handle at liftoff.
    pub fn attach(&self, rocket: rocket::Rocket<rocket::Build>) -> rocket::Rocket<rocket::Build> {
        let handle = self.clone();
        rocket.manage(self.clone()).attach(AdHoc::on_liftoff(
            "Mission Control restart",
            move |rocket| {
                Box::pin(async move {
                    let _ = handle.inner.shutdown.set(rocket.shutdown());
                    // Stored first, then checked, so a request racing with liftoff is not lost.
                    if handle.requested() {
                        rocket.shutdown().notify();
                    }
                })
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    fn no_files(_: &Path) -> bool {
        false
    }

    #[test]
    fn by_hand_has_no_supervisor() {
        assert_eq!(Supervisor::detect(env(&[]), no_files), None);
        assert_eq!(
            RestartMethod::choose(env(&[]), no_files),
            RestartMethod::Exec
        );
    }

    #[test]
    fn systemd_is_detected_from_either_variable() {
        for name in ["INVOCATION_ID", "NOTIFY_SOCKET"] {
            let vars = [(name, "x")];
            assert_eq!(
                Supervisor::detect(env(&vars), no_files),
                Some(Supervisor::Systemd)
            );
            assert_eq!(
                RestartMethod::choose(env(&vars), no_files),
                RestartMethod::Exit
            );
        }
    }

    #[test]
    fn a_container_is_detected_from_a_file_or_the_variable() {
        for file in ["/.dockerenv", "/run/.containerenv"] {
            let exists = |path: &Path| path == Path::new(file);
            assert_eq!(
                Supervisor::detect(env(&[]), exists),
                Some(Supervisor::Container)
            );
        }
        assert_eq!(
            Supervisor::detect(env(&[("container", "podman")]), no_files),
            Some(Supervisor::Container)
        );
    }

    #[test]
    fn blank_variables_do_not_count() {
        let vars = [
            ("INVOCATION_ID", " "),
            ("NOTIFY_SOCKET", ""),
            ("container", ""),
        ];
        assert_eq!(Supervisor::detect(env(&vars), no_files), None);
    }

    #[test]
    fn systemd_wins_over_a_container() {
        let vars = [("INVOCATION_ID", "x")];
        assert_eq!(
            Supervisor::detect(env(&vars), |_| true),
            Some(Supervisor::Systemd)
        );
    }

    #[test]
    fn the_override_wins_either_way() {
        let supervised = |value| [("INVOCATION_ID", "x"), (OVERRIDE_VARIABLE, value)];
        assert_eq!(
            RestartMethod::choose(env(&supervised("exec")), no_files),
            RestartMethod::Exec
        );
        assert_eq!(
            RestartMethod::choose(env(&supervised(" EXEC ")), no_files),
            RestartMethod::Exec
        );
        assert_eq!(
            RestartMethod::choose(env(&[(OVERRIDE_VARIABLE, "exit")]), no_files),
            RestartMethod::Exit
        );
    }

    #[test]
    fn an_unknown_override_is_ignored() {
        assert_eq!(
            RestartMethod::choose(env(&[(OVERRIDE_VARIABLE, "reboot")]), no_files),
            RestartMethod::Exec
        );
        assert_eq!(
            RestartMethod::choose(
                env(&[(OVERRIDE_VARIABLE, ""), ("INVOCATION_ID", "x")]),
                no_files
            ),
            RestartMethod::Exit
        );
    }

    #[test]
    fn the_action_follows_the_request_and_the_method() {
        assert_eq!(action(false, RestartMethod::Exec), Action::Stop);
        assert_eq!(action(false, RestartMethod::Exit), Action::Stop);
        assert_eq!(action(true, RestartMethod::Exec), Action::Exec);
        assert_eq!(action(true, RestartMethod::Exit), Action::Exit);
    }

    #[rocket::async_test]
    async fn a_request_makes_rocket_return() {
        let handle = RestartHandle::new();
        let rocket = handle.attach(rocket::custom(
            rocket::Config::figment()
                .merge(("port", 0))
                .merge(("log_level", "off")),
        ));
        let state = rocket.state::<RestartHandle>().cloned();
        assert!(state.is_some(), "the handle is in Rocket's state");
        let requester = handle.clone();
        // The request comes while Rocket is running.
        rocket::tokio::spawn(async move {
            rocket::tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            requester.request();
        });
        assert!(!handle.requested());
        rocket::tokio::time::timeout(std::time::Duration::from_secs(20), rocket.launch())
            .await
            .expect("Rocket returns after the request")
            .expect("Rocket launches");
        assert!(handle.requested());
        assert_eq!(
            action(handle.requested(), RestartMethod::Exit),
            Action::Exit
        );
    }

    #[rocket::async_test]
    async fn a_request_before_liftoff_still_stops_rocket() {
        let handle = RestartHandle::new();
        handle.request();
        let rocket = handle.attach(rocket::custom(
            rocket::Config::figment()
                .merge(("port", 0))
                .merge(("log_level", "off")),
        ));
        rocket::tokio::time::timeout(std::time::Duration::from_secs(20), rocket.launch())
            .await
            .expect("Rocket returns")
            .expect("Rocket launches");
    }

    #[test]
    fn the_reason_names_the_marker() {
        let reason =
            |vars: &[(&str, &str)]| RestartMethod::choose_with_reason(env(vars), no_files).1;
        assert!(reason(&[("INVOCATION_ID", "x")]).contains("INVOCATION_ID"));
        assert!(reason(&[("NOTIFY_SOCKET", "x")]).contains("NOTIFY_SOCKET"));
        assert!(reason(&[("container", "x")]).contains("container"));
        assert!(reason(&[(OVERRIDE_VARIABLE, "exit")]).contains("MC_RESTART=exit"));
        assert!(reason(&[]).contains("by hand"));
    }

    #[test]
    fn candidates_strip_the_deleted_suffix_and_resolve_arg0() {
        let found = candidates(
            Ok(PathBuf::from("/opt/mc/mission-control-app (deleted)")),
            Some(OsStr::new("./mc")),
            Some(Path::new("/srv")),
        );
        assert_eq!(
            found,
            [
                PathBuf::from("/opt/mc/mission-control-app (deleted)"),
                PathBuf::from("/opt/mc/mission-control-app"),
                PathBuf::from("/srv/./mc"),
            ]
        );
        let failed = candidates(
            Err(std::io::Error::other("no")),
            Some(OsStr::new("mc")),
            Some(Path::new("/srv")),
        );
        assert_eq!(failed, [PathBuf::from("/srv/mc")]);
    }
}
