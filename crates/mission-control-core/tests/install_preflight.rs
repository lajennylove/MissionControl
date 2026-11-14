use mission_control_core::install::preflight::{IssueCode, preflight};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mc-preflight-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn codes(dir: &Path, port: u16) -> Vec<IssueCode> {
    preflight(dir, port).into_iter().map(|i| i.code).collect()
}

#[test]
fn healthy_directory_has_no_issues() {
    let dir = temp_root("healthy");
    assert!(preflight(&dir, free_port()).is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn read_only_directory_is_not_writable() {
    let dir = temp_root("readonly");
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    let original = perms.clone();
    perms.set_readonly(true);
    std::fs::set_permissions(&dir, perms).unwrap();
    // Permission bits are ignored for root: probe, and skip if so.
    let probe = dir.join("probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        std::fs::set_permissions(&dir, original).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        eprintln!("SKIPPED: permissions are ignored here (running as root)");
        return;
    }
    let issues = preflight(&dir, free_port());
    std::fs::set_permissions(&dir, original).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let issue = issues
        .iter()
        .find(|i| i.code == IssueCode::NotWritable)
        .expect("NotWritable expected");
    assert!(issue.message.contains("write permission"));
    assert!(issue.message.contains("mc-preflight-"));
}

#[test]
fn path_with_spaces_has_no_issue() {
    let root = temp_root("spaces");
    let dir = root.join("My Site Folder");
    std::fs::create_dir_all(&dir).unwrap();
    assert!(preflight(&dir, free_port()).is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn non_ascii_path_has_no_issue() {
    let root = temp_root("unicode");
    let dir = root.join("café-сайт-网站");
    std::fs::create_dir_all(&dir).unwrap();
    assert!(preflight(&dir, free_port()).is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn port_held_by_a_listener_is_in_use() {
    let dir = temp_root("port");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let found = codes(&dir, port);
    drop(listener);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(found, vec![IssueCode::PortInUse]);
}

#[test]
fn file_path_is_not_a_directory() {
    let dir = temp_root("file");
    let file = dir.join("not-a-folder");
    std::fs::write(&file, b"x").unwrap();
    let found = codes(&file, free_port());
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(found, vec![IssueCode::NotADirectory]);
}

#[test]
fn missing_directory_is_checked_through_its_parent() {
    let root = temp_root("missing");
    let dir = root.join("not").join("yet").join("created");
    assert!(preflight(&dir, free_port()).is_empty());
    assert!(!dir.exists(), "preflight must not create the folder");
    std::fs::remove_dir_all(&root).unwrap();
}
