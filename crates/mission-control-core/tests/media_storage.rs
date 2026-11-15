//! Local-disk storage: round trips, path safety, atomic and create-only writes.

use std::fs;
use std::path::PathBuf;

use chrono::NaiveDate;
use mission_control_core::media::{LocalStorage, OpenedFile, Storage, StorageError, upload_subdir};

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

#[test]
fn put_new_refuses_an_existing_file() {
    let (_t, s) = setup("put-new");
    s.put_new("a/b.txt", b"one").unwrap();
    assert!(matches!(
        s.put_new("a/b.txt", b"two"),
        Err(StorageError::AlreadyExists(_))
    ));
    assert_eq!(s.get("a/b.txt").unwrap(), b"one");
    // No temp file is left behind.
    let names: Vec<String> = fs::read_dir(s.root().join("a"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["b.txt"]);
    assert!(matches!(
        s.put_new("../x.txt", b"x"),
        Err(StorageError::InvalidPath(_))
    ));
}

#[test]
fn names_near_the_file_system_limit_can_be_written() {
    let (_t, s) = setup("long");
    let name = format!("{}.txt", "a".repeat(250));
    s.put(&name, b"x").unwrap();
    s.put(&name, b"y").unwrap();
    assert_eq!(s.get(&name).unwrap(), b"y");
    let other = format!("{}.txt", "b".repeat(250));
    s.put_new(&other, b"z").unwrap();
    assert_eq!(s.get(&other).unwrap(), b"z");
}

#[cfg(unix)]
#[test]
fn exists_reports_io_errors() {
    use std::os::unix::fs::PermissionsExt;

    let (_t, s) = setup("exists-err");
    s.put("locked/a.txt", b"a").unwrap();
    let locked = s.root().join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = s.exists("locked/a.txt");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(result, Err(StorageError::Io(_))), "{result:?}");
    assert!(!s.exists("missing.txt").unwrap());
    assert!(!s.exists("locked/a.txt/below").unwrap());
}

#[test]
fn put_new_first_takes_the_first_free_path() {
    let (_t, s) = setup("put-new-first");
    s.put("d/a.txt", b"old").unwrap();
    s.put("d/b.txt", b"old").unwrap();
    let mut paths = ["d/a.txt", "d/b.txt", "d/c.txt", "d/e.txt"]
        .into_iter()
        .map(String::from);
    assert_eq!(s.put_new_first(&mut paths, b"new").unwrap(), "d/c.txt");
    assert_eq!(s.get("d/a.txt").unwrap(), b"old");
    assert_eq!(s.get("d/c.txt").unwrap(), b"new");
    assert!(!s.exists("d/e.txt").unwrap());

    // Every path taken: the last one is named, nothing is replaced.
    let mut paths = ["d/a.txt", "d/b.txt"].into_iter().map(String::from);
    assert!(matches!(
        s.put_new_first(&mut paths, b"x"),
        Err(StorageError::AlreadyExists(path)) if path == "d/b.txt"
    ));
    // Paths in another folder, unsafe paths and no paths are refused.
    let mut paths = ["d/a.txt", "other/x.txt"].into_iter().map(String::from);
    assert!(matches!(
        s.put_new_first(&mut paths, b"x"),
        Err(StorageError::InvalidPath(_))
    ));
    let mut paths = ["d/a.txt", "d/../x.txt"].into_iter().map(String::from);
    assert!(matches!(
        s.put_new_first(&mut paths, b"x"),
        Err(StorageError::InvalidPath(_))
    ));
    assert!(matches!(
        s.put_new_first(&mut std::iter::empty(), b"x"),
        Err(StorageError::InvalidPath(_))
    ));

    // No temp file is left behind.
    let mut names: Vec<String> = fs::read_dir(s.root().join("d"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "b.txt", "c.txt"]);
    assert!(!s.root().join("other").exists());
}

#[test]
fn list_names_what_a_folder_holds() {
    let (_t, s) = setup("list");
    // A folder that does not exist yet holds nothing.
    assert!(s.list("2026/11").unwrap().is_empty());
    s.put("2026/11/b.jpg", b"b").unwrap();
    s.put("2026/11/a.jpg", b"a").unwrap();
    s.put("2026/11/sub/c.jpg", b"c").unwrap();
    s.put("2026/12/d.jpg", b"d").unwrap();
    // The storage's own temp files (dot names) are not listed.
    fs::write(s.root().join("2026/11/.mc-tmp.1.2"), b"t").unwrap();
    let mut names = s.list("2026/11").unwrap();
    names.sort();
    assert_eq!(names, ["a.jpg", "b.jpg", "sub"]);
    for path in ["", "../x", "/abs", "2026//11", "2026\\11"] {
        assert!(
            matches!(s.list(path), Err(StorageError::InvalidPath(_))),
            "{path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn list_refuses_a_folder_that_leaves_the_root() {
    use std::os::unix::fs::symlink;

    let (t, s) = setup("list-symlink");
    let outside = t.0.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"secret").unwrap();
    s.put("keep.txt", b"keep").unwrap();
    symlink(&outside, s.root().join("escape")).unwrap();
    assert!(matches!(
        s.list("escape"),
        Err(StorageError::InvalidPath(_))
    ));
}

#[test]
fn open_hands_out_a_file_with_its_length() {
    use std::io::Read;

    let (temp, s) = setup("open");
    s.put("2026/11/a.txt", b"hello").unwrap();
    match s.open("2026/11/a.txt").unwrap() {
        OpenedFile::File(mut file, length) => {
            assert_eq!(length, 5);
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            assert_eq!(text, "hello");
        }
        OpenedFile::Bytes(_) => panic!("the local disk opens a file"),
    }
    assert!(matches!(s.open("2026/11"), Err(StorageError::NotFound(_))));
    assert!(matches!(
        s.open("2026/11/missing.txt"),
        Err(StorageError::NotFound(_))
    ));
    assert!(matches!(
        s.open("../a.txt"),
        Err(StorageError::InvalidPath(_))
    ));
    #[cfg(unix)]
    {
        fs::write(temp.0.join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(temp.0.join("outside.txt"), temp.0.join("uploads/link.txt"))
            .unwrap();
        assert!(matches!(
            s.open("link.txt"),
            Err(StorageError::InvalidPath(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn a_fifo_is_no_file_and_never_blocks() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (_t, s) = setup("fifo");
    s.put("2026/11/a.txt", b"a").unwrap();
    let fifo = s.root().join("2026/11/pipe.txt");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let (send, receive) = mpsc::channel();
    let storage = s.clone();
    std::thread::spawn(move || {
        let opened = storage.open("2026/11/pipe.txt").map(|_| ());
        let read = storage.get("2026/11/pipe.txt").map(|_| ());
        let _ = send.send((opened, read));
    });
    let (opened, read) = receive
        .recv_timeout(Duration::from_secs(10))
        .expect("opening a FIFO does not block");
    assert!(
        matches!(opened, Err(StorageError::NotFound(_))),
        "{opened:?}"
    );
    assert!(matches!(read, Err(StorageError::NotFound(_))), "{read:?}");
}
