//! Restart into normal mode (Task 11.5), end to end: the built binary in setup mode asks for a
//! restart as soon as it is up (the debug-build test hook), and either exits with status 0 (for a
//! supervisor) or replaces itself (`exec`) and keeps serving.
//!
//! The hook is the `test-hooks` feature, which this package's dev-dependency on itself enables.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Starts the binary in an empty folder (so: setup mode) with the restart hook and `method`.
/// Rocket's output arrives line by line on the receiver.
fn start(method: &str, dir: &std::path::Path) -> (Child, mpsc::Receiver<String>) {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("finds a free port")
        .local_addr()
        .expect("reads the port")
        .port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mission-control-app"))
        .current_dir(dir)
        .env_remove("DATABASE_URL")
        .env_remove("IGNITE_SECRET_KEY")
        .env("MC_RESTART", method)
        .env("MC_TEST_RESTART_ON_LIFTOFF", "1")
        .env("ROCKET_ADDRESS", "127.0.0.1")
        .env("ROCKET_PORT", port.to_string())
        .env("ROCKET_CLI_COLORS", "false")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts the binary");
    let stdout = child.stdout.take().expect("has stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

#[test]
fn exit_method_exits_with_status_zero() {
    let dir = tempfile::tempdir().expect("creates a folder");
    let (mut child, _output) = start("exit", dir.path());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("waits") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the binary did not exit after the restart request");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));
}

#[test]
fn exec_method_replaces_the_process_and_keeps_serving() {
    let dir = tempfile::tempdir().expect("creates a folder");
    let (mut child, output) = start("exec", dir.path());
    // Rocket launches twice: before the restart and in the process that replaced it.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut launches = 0;
    while launches < 2 && Instant::now() < deadline {
        if let Ok(line) = output.recv_timeout(Duration::from_millis(100))
            && line.contains("has launched")
        {
            launches += 1;
        }
    }
    let still_running = child.try_wait().expect("waits").is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(launches, 2, "Rocket launched again after the restart");
    assert!(
        still_running,
        "the same process is still running after exec"
    );
}
