//! Preflight checks for the installer.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Free space the default install needs: 100 MiB.
pub const MIN_FREE_BYTES: u64 = 104_857_600;

/// The kind of problem a preflight check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueCode {
    /// Mission Control cannot create files in the install folder.
    NotWritable,
    /// Another program is already using the port.
    PortInUse,
    /// The disk holding the install folder has too little free space.
    LowDiskSpace,
    /// The install path exists but is a file, not a folder.
    NotADirectory,
    /// The checks themselves could not run, so nothing is known to be ready.
    ChecksFailed,
}

/// A problem the site owner can fix, with a plain-language explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightIssue {
    /// Machine-readable kind of the problem.
    pub code: IssueCode,
    /// Plain-language explanation that names the fix.
    pub message: String,
}

/// Runs every preflight check and returns the problems found (empty means ready).
///
/// `install_dir` may not exist yet: it is checked through its nearest existing
/// parent, because the installer can create it. Nothing is left behind.
pub fn preflight(install_dir: &Path, port: u16) -> Vec<PreflightIssue> {
    let mut issues = Vec::new();
    let shown = install_dir.display();

    match nearest_existing(install_dir) {
        Some(existing) if !existing.is_dir() => issues.push(PreflightIssue {
            code: IssueCode::NotADirectory,
            message: format!(
                "{} is a file, not a folder. Choose a folder for Mission Control, \
                 or remove or rename that file.",
                existing.display()
            ),
        }),
        Some(existing) => {
            if !can_write(&existing) {
                issues.push(PreflightIssue {
                    code: IssueCode::NotWritable,
                    message: format!(
                        "Mission Control cannot write to {shown}. Give the user that runs \
                         Mission Control write permission on that folder, or choose another folder."
                    ),
                });
            }
            if let Some(issue) = check_disk(&existing, MIN_FREE_BYTES) {
                issues.push(issue);
            }
        }
        None => issues.push(PreflightIssue {
            code: IssueCode::NotWritable,
            message: format!(
                "Mission Control cannot find {shown} or any folder above it. \
                 Check the path, or choose another folder."
            ),
        }),
    }

    if port_in_use(port) {
        issues.push(PreflightIssue {
            code: IssueCode::PortInUse,
            message: format!(
                "Port {port} is already in use by another program. Stop that program, \
                 or choose a different port for Mission Control."
            ),
        });
    }

    issues
}

/// True when something already listens on `port`, on loopback or on all interfaces.
///
/// A bind to 127.0.0.1 alone can succeed while another program holds 0.0.0.0
/// (SO_REUSEADDR on macOS), so a connect probe and a wildcard bind are tried too.
fn port_in_use(port: u16) -> bool {
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&loopback, Duration::from_millis(500)).is_ok() {
        return true;
    }
    [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED]
        .iter()
        .any(|ip| {
            TcpListener::bind((*ip, port)).is_err_and(|e| e.kind() == io::ErrorKind::AddrInUse)
        })
}

/// The path itself, or its closest ancestor that exists.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        })
        .find(|p| p.exists())
        .map(Path::to_path_buf)
}

/// Proves write access by creating and removing a uniquely named file.
fn can_write(dir: &Path) -> bool {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let probe = dir.join(format!(
        ".mission-control-preflight-{}-{nanos}",
        std::process::id()
    ));
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|mut f| f.write_all(b"ok"));
    let _ = std::fs::remove_file(&probe);
    created.is_ok()
}

/// Reports `LowDiskSpace` when the filesystem holding `dir` has under `min` bytes free.
fn check_disk(dir: &Path, min: u64) -> Option<PreflightIssue> {
    let free = fs4::available_space(dir).ok()?;
    (free < min).then(|| PreflightIssue {
        code: IssueCode::LowDiskSpace,
        message: format!(
            "Only {} MiB of disk space is free where Mission Control will be installed, \
             and it needs at least {} MiB. Free up space on that disk, or choose a folder on another disk.",
            free / 1_048_576,
            min / 1_048_576
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_disk_space_is_reported() {
        let issue = check_disk(&std::env::temp_dir(), u64::MAX).expect("issue expected");
        assert_eq!(issue.code, IssueCode::LowDiskSpace);
        assert!(issue.message.contains("Free up space"));
        assert!(check_disk(&std::env::temp_dir(), 1).is_none());
    }
}
