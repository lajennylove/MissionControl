//! Local-disk storage: round trips, path safety, atomic writes.

use std::fs;
use std::path::PathBuf;

use chrono::NaiveDate;
use mission_control_core::media::{LocalStorage, Storage, StorageError, upload_subdir};

/// A fresh temp folder, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mc-media-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn setup(name: &str) -> (Temp, LocalStorage) {
    let temp = Temp::new(name);
    let storage = LocalStorage::new(temp.0.join("uploads"));
    (temp, storage)
}

#[test]
fn put_get_delete_round_trip() {
    let (_t, s) = setup("roundtrip");
    s.put("a.txt", b"one").unwrap();
    assert!(s.exists("a.txt").unwrap());
    assert_eq!(s.get("a.txt").unwrap(), b"one");
    s.put("a.txt", b"two").unwrap();
    assert_eq!(s.get("a.txt").unwrap(), b"two");
    s.delete("a.txt").unwrap();
    assert!(!s.exists("a.txt").unwrap());
    assert!(matches!(s.get("a.txt"), Err(StorageError::NotFound(_))));
}

#[test]
fn delete_of_a_missing_file_succeeds() {
    let (_t, s) = setup("delete-missing");
    s.delete("nope/none.txt").unwrap();
    s.put("x.txt", b"x").unwrap();
    s.delete("x.txt").unwrap();
    s.delete("x.txt").unwrap();
}

#[test]
fn year_month_folders_are_created() {
    let (_t, s) = setup("yearmonth");
    let date = NaiveDate::from_ymd_opt(2026, 3, 9).unwrap();
    assert_eq!(upload_subdir(date), "2026/03");
    let path = format!("{}/photo.jpg", upload_subdir(date));
    s.put(&path, b"jpg").unwrap();
    assert!(s.root().join("2026").join("03").join("photo.jpg").is_file());
    assert_eq!(s.get(&path).unwrap(), b"jpg");
}

#[test]
fn unsafe_paths_are_refused() {
    let (t, s) = setup("unsafe");
    fs::write(t.0.join("secret.txt"), b"secret").unwrap();
    let bad = [
        "../secret.txt",
        "a/../../secret.txt",
        "a/../b.txt",
        "./a.txt",
        "/etc/passwd",
        "",
        "a//b.txt",
        "a/",
        "a\\b.txt",
        "..\\secret.txt",
        "a\0b.txt",
    ];
    for path in bad {
        assert!(
            matches!(s.put(path, b"x"), Err(StorageError::InvalidPath(_))),
            "put {path:?}"
        );
        assert!(
            matches!(s.get(path), Err(StorageError::InvalidPath(_))),
            "get {path:?}"
        );
        assert!(
            matches!(s.delete(path), Err(StorageError::InvalidPath(_))),
            "delete {path:?}"
        );
        assert!(
            matches!(s.exists(path), Err(StorageError::InvalidPath(_))),
            "exists {path:?}"
        );
    }
    assert_eq!(fs::read(t.0.join("secret.txt")).unwrap(), b"secret");
}

#[cfg(unix)]
#[test]
fn symlinks_out_of_the_root_are_refused() {
    use std::os::unix::fs::symlink;

    let (t, s) = setup("symlink");
    let outside = t.0.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"secret").unwrap();
    s.put("keep.txt", b"keep").unwrap();
    symlink(&outside, s.root().join("escape")).unwrap();
    symlink(outside.join("secret.txt"), s.root().join("link.txt")).unwrap();

    // A folder link that leaves the root, with and without a new subfolder.
    for path in ["escape/new.txt", "escape/sub/new.txt"] {
        assert!(
            matches!(s.put(path, b"x"), Err(StorageError::InvalidPath(_))),
            "{path}"
        );
    }
    assert!(matches!(
        s.get("escape/secret.txt"),
        Err(StorageError::InvalidPath(_))
    ));
    assert!(matches!(
        s.delete("escape/secret.txt"),
        Err(StorageError::InvalidPath(_))
    ));
    // A file link that leaves the root.
    assert!(matches!(
        s.get("link.txt"),
        Err(StorageError::InvalidPath(_))
    ));
    assert!(matches!(
        s.exists("link.txt"),
        Err(StorageError::InvalidPath(_))
    ));

    assert!(!outside.join("new.txt").exists());
    assert!(!outside.join("sub").exists());
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"secret");
    assert_eq!(s.get("keep.txt").unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
fn symlinks_inside_the_root_still_work() {
    use std::os::unix::fs::symlink;

    let (_t, s) = setup("symlink-inside");
    s.put("real/a.txt", b"a").unwrap();
    symlink(s.root().join("real"), s.root().join("alias")).unwrap();
    assert_eq!(s.get("alias/a.txt").unwrap(), b"a");
}

#[test]
fn atomic_write_leaves_no_temp_file() {
    let (_t, s) = setup("atomic");
    s.put("2026/11/a.txt", b"first").unwrap();
    s.put("2026/11/a.txt", b"second").unwrap();
    let names: Vec<String> = fs::read_dir(s.root().join("2026/11"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["a.txt"]);
}

#[test]
fn a_failed_write_leaves_no_temp_file() {
    let (_t, s) = setup("atomic-fail");
    // A folder sits where the file should go, so the final rename fails.
    fs::create_dir_all(s.root().join("a.txt")).unwrap();
    assert!(s.put("a.txt", b"x").is_err());
    let names: Vec<String> = fs::read_dir(s.root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["a.txt"]);
}

#[test]
fn storage_is_object_safe_and_shareable() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LocalStorage>();
    let (_t, s) = setup("dyn");
    let boxed: std::sync::Arc<dyn Storage> = std::sync::Arc::new(s);
    boxed.put("x.txt", b"x").unwrap();
    assert_eq!(boxed.get("x.txt").unwrap(), b"x");
}
