//! Upload validation: size, file names, real types, unique names (Review Focus 2).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::NaiveDate;
use mission_control_core::media::{
    LocalStorage, Storage, UploadError, sanitize_file_name, store, validate,
};

/// A fresh temp folder, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mc-upload-{name}-{}", std::process::id()));
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

const MAX: u64 = 1024 * 1024;

/// The start of a PNG file.
fn png() -> &'static [u8] {
    b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0"
}

/// The start of a JPEG file.
fn jpeg() -> &'static [u8] {
    b"\xFF\xD8\xFF\xE0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0"
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()
}

/// Every file under `dir`, as paths relative to it.
fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    out.sort();
    out
}

// ---- size ----

#[test]
fn a_file_over_the_limit_is_refused_with_the_limit() {
    let mut bytes = jpeg().to_vec();
    bytes.resize(101, 0);
    match validate("big.jpg", &bytes, 100) {
        Err(UploadError::TooLarge { size, limit }) => {
            assert_eq!(size, 101);
            assert_eq!(limit, 100);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    bytes.truncate(100);
    assert!(validate("big.jpg", &bytes, 100).is_ok());
}

#[test]
fn an_empty_file_is_refused() {
    assert!(matches!(
        validate("a.txt", b"", MAX),
        Err(UploadError::Empty)
    ));
}

// ---- types ----

#[test]
fn allowed_types_pass_with_their_mime_type() {
    let ok = validate("Photo.JPG", jpeg(), MAX).unwrap();
    assert_eq!(ok.file_name, "Photo.jpg");
    assert_eq!(ok.extension, "jpg");
    assert_eq!(ok.mime_type, "image/jpeg");

    let ok = validate("shot.png", png(), MAX).unwrap();
    assert_eq!(ok.mime_type, "image/png");

    let ok = validate("paper.pdf", b"%PDF-1.7\n%...", MAX).unwrap();
    assert_eq!(ok.mime_type, "application/pdf");

    let ok = validate("notes.txt", "plain text, caf\u{e9}\n".as_bytes(), MAX).unwrap();
    assert_eq!(ok.mime_type, "text/plain");

    let ok = validate("data.csv", b"a,b\n1,2\n", MAX).unwrap();
    assert_eq!(ok.mime_type, "text/csv");
}

#[test]
fn bytes_that_do_not_match_the_extension_are_refused() {
    // PNG bytes named .jpg.
    assert!(matches!(
        validate("photo.jpg", png(), MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
    // A PHP script named .jpg.
    assert!(matches!(
        validate("shell.jpg", b"<?php system($_GET['c']); ?>", MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
    // A JPEG named .pdf.
    assert!(matches!(
        validate("doc.pdf", jpeg(), MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
}

/// An OLE compound file (legacy Office, MSI) holding one stream.
fn ole_file(stream: &str) -> Vec<u8> {
    use std::io::Write;
    let mut file = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    file.create_stream(format!("/{stream}"))
        .unwrap()
        .write_all(b"data")
        .unwrap();
    file.flush().unwrap();
    file.into_inner().into_inner()
}

#[test]
fn legacy_office_files_are_checked_by_their_stream() {
    let doc = ole_file("WordDocument");
    assert_eq!(
        validate("a.doc", &doc, MAX).unwrap().mime_type,
        "application/msword"
    );
    let xls = ole_file("Workbook");
    assert_eq!(
        validate("a.xls", &xls, MAX).unwrap().mime_type,
        "application/vnd.ms-excel"
    );
    let ppt = ole_file("PowerPoint Document");
    assert!(validate("a.ppt", &ppt, MAX).is_ok());
    // The wrong stream for the extension, or an installer's tables.
    assert!(matches!(
        validate("a.xls", &doc, MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
    let msi = ole_file("\u{4840}\u{3f3f}\u{4577}");
    assert!(matches!(
        validate("setup.doc", &msi, MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
}

#[test]
fn text_types_need_utf8_without_nul() {
    assert!(matches!(
        validate("a.txt", b"abc\0def", MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
    assert!(matches!(
        validate("a.csv", b"\xFF\xFEa\0,\0", MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
    // A binary file named .txt.
    assert!(matches!(
        validate("a.txt", png(), MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
    // Text that happens to start like a binary signature is still text.
    for text in ["BMW,Audi\n1,2\n", "MZ notes\n", "ID3 tags\n"] {
        assert!(validate("a.csv", text.as_bytes(), MAX).is_ok(), "{text:?}");
    }
    // HTML named .txt, which a browser could sniff.
    assert!(matches!(
        validate("a.txt", b"<html><script>alert(1)</script></html>", MAX),
        Err(UploadError::ContentMismatch { .. })
    ));
}

#[test]
fn types_outside_the_allow_list_are_refused() {
    let cases: [(&str, &[u8]); 7] = [
        ("setup.exe", b"MZ\x90\0\x03\0\0\0\x04\0"),
        ("shell.php", b"<?php echo 1;"),
        ("logo.svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
        ("page.html", b"<html></html>"),
        ("app.js", b"alert(1)"),
        ("noextension", b"hello"),
        (".htaccess", b"Options +ExecCGI"),
    ];
    for (name, bytes) in cases {
        assert!(
            matches!(
                validate(name, bytes, MAX),
                Err(UploadError::TypeNotAllowed { .. })
            ),
            "{name}: {:?}",
            validate(name, bytes, MAX)
        );
    }
}

// ---- file names ----

#[test]
fn path_parts_are_stripped() {
    assert_eq!(sanitize_file_name("../../etc/x"), "x");
    assert_eq!(sanitize_file_name("../../etc/passwd.txt"), "passwd.txt");
    assert_eq!(sanitize_file_name("C:\\x\\y.png"), "y.png");
    assert_eq!(sanitize_file_name("..\\..\\boot.ini"), "boot.ini");
    assert_eq!(sanitize_file_name("/abs/olute.jpg"), "olute.jpg");
    assert_eq!(sanitize_file_name("dir/"), "unnamed-file");
}

#[test]
fn special_and_control_characters_are_removed() {
    assert_eq!(sanitize_file_name("a\0b.jpg"), "ab.jpg");
    assert_eq!(sanitize_file_name("a\u{1}b\u{7f}c.jpg"), "abc.jpg");
    assert_eq!(
        sanitize_file_name("we?ird[]=<>:;,'\"&$#*()|~`!{}%+.png"),
        "weird.png"
    );
    assert_eq!(sanitize_file_name("“quoted” «name».png"), "quoted-name.png");
    // A right-to-left override that would show `exe.jpg` as `gpj.exe`.
    assert_eq!(sanitize_file_name("photo\u{202e}gpj.exe"), "photogpj.exe");
}

#[test]
fn whitespace_dots_and_dashes_collapse() {
    assert_eq!(
        sanitize_file_name("My  Summer\tPhoto.jpg"),
        "My-Summer-Photo.jpg"
    );
    assert_eq!(sanitize_file_name("a - b.jpg"), "a-b.jpg");
    assert_eq!(sanitize_file_name("a..b...jpg"), "a.b.jpg");
    assert_eq!(sanitize_file_name("a\u{a0}b.jpg"), "a-b.jpg");
    assert_eq!(sanitize_file_name("--_.name.jpg._-"), "name.jpg");
}

#[test]
fn dot_files_lose_their_leading_dot() {
    assert_eq!(sanitize_file_name(".htaccess"), "htaccess");
    assert_eq!(sanitize_file_name("...env"), "env");
}

#[test]
fn extensions_are_lowercased_and_inner_extensions_defused() {
    assert_eq!(sanitize_file_name("IMG.JPEG"), "IMG.jpeg");
    assert_eq!(sanitize_file_name("file.php.jpg"), "file.php_.jpg");
    assert_eq!(sanitize_file_name("file.PHTML.jpg"), "file.PHTML_.jpg");
    // An allowed inner extension and a non-alphabetic part are kept as they are.
    assert_eq!(sanitize_file_name("archive.pdf.jpg"), "archive.pdf.jpg");
    assert_eq!(sanitize_file_name("v1.2.3.txt"), "v1.2.3.txt");
}

#[test]
fn a_name_with_nothing_usable_gets_a_generated_name() {
    assert_eq!(sanitize_file_name("?#*.jpg"), "unnamed-file.jpg");
    assert_eq!(sanitize_file_name("jpg"), "unnamed-file.jpg");
    assert_eq!(sanitize_file_name("?#*"), "unnamed-file");
    assert_eq!(sanitize_file_name(""), "unnamed-file");
    assert_eq!(sanitize_file_name("..."), "unnamed-file");
}

#[test]
fn very_long_names_are_shortened_keeping_the_extension() {
    let long = format!("{}.png", "a".repeat(1000));
    let name = sanitize_file_name(&long);
    assert!(name.len() <= 200, "{}", name.len());
    assert!(name.ends_with(".png"));

    // Multi-byte characters are cut on a character boundary.
    let long = format!("{}.png", "\u{e9}".repeat(400));
    let name = sanitize_file_name(&long);
    assert!(name.len() <= 200);
    assert!(name.ends_with(".png"));

    // A long junk extension is shortened too.
    let name = sanitize_file_name(&format!("a.{}", "x".repeat(500)));
    assert!(name.len() <= 200);
}

#[test]
fn hostile_names_validate_to_safe_names() {
    let ok = validate("../../etc/x.jpg", jpeg(), MAX).unwrap();
    assert_eq!(ok.file_name, "x.jpg");
    let ok = validate("C:\\x\\y.png", png(), MAX).unwrap();
    assert_eq!(ok.file_name, "y.png");
    let ok = validate("file.php.jpg", jpeg(), MAX).unwrap();
    assert_eq!(ok.file_name, "file.php_.jpg");
    assert_eq!(ok.extension, "jpg");
    let ok = validate("?#*.jpg", jpeg(), MAX).unwrap();
    assert_eq!(ok.file_name, "unnamed-file.jpg");
}

// ---- storing ----

#[test]
fn store_writes_into_the_year_month_folder() {
    let (_t, s) = setup("store");
    let upload = validate("Photo.jpg", jpeg(), MAX).unwrap();
    let path = store(&s, &upload, day()).unwrap();
    assert_eq!(path, "2026/11/Photo.jpg");
    assert_eq!(s.get(&path).unwrap(), jpeg());
}

#[test]
fn the_same_name_twice_gets_distinct_names() {
    let (_t, s) = setup("unique");
    let upload = validate("photo.jpg", jpeg(), MAX).unwrap();
    let first = store(&s, &upload, day()).unwrap();
    let second = store(&s, &upload, day()).unwrap();
    let third = store(&s, &upload, day()).unwrap();
    assert_eq!(first, "2026/11/photo.jpg");
    assert_eq!(second, "2026/11/photo-1.jpg");
    assert_eq!(third, "2026/11/photo-2.jpg");
}

#[test]
fn names_that_look_like_image_sizes_always_get_a_number() {
    let (_t, s) = setup("subsize");
    for (name, want) in [
        ("photo-150x150.jpg", "2026/11/photo-150x150-1.jpg"),
        ("photo-scaled.jpg", "2026/11/photo-scaled-1.jpg"),
        ("photo-rotated.jpg", "2026/11/photo-rotated-1.jpg"),
    ] {
        let upload = validate(name, jpeg(), MAX).unwrap();
        assert_eq!(store(&s, &upload, day()).unwrap(), want);
    }
}

#[test]
fn concurrent_uploads_of_one_name_never_overwrite() {
    let (_t, s) = setup("race");
    let s = Arc::new(s);
    let threads: Vec<_> = (0..16u8)
        .map(|i| {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                let mut bytes = jpeg().to_vec();
                bytes.push(i);
                let upload = validate("same.jpg", &bytes, MAX).unwrap();
                (store(s.as_ref(), &upload, day()).unwrap(), bytes)
            })
        })
        .collect();
    let mut paths = Vec::new();
    for thread in threads {
        let (path, bytes) = thread.join().unwrap();
        assert_eq!(s.get(&path).unwrap(), bytes, "{path} was overwritten");
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 16);
}

#[test]
fn nothing_is_written_outside_the_root() {
    let (t, s) = setup("escape");
    let hostile = [
        "../../etc/x.jpg",
        "..\\..\\x.jpg",
        "/etc/passwd.jpg",
        "a\0/../b.jpg",
        "C:\\x\\y.jpg",
        "....jpg",
        ".htaccess.jpg",
    ];
    for name in hostile {
        let upload = validate(name, jpeg(), MAX).unwrap();
        let path = store(&s, &upload, day()).unwrap();
        assert!(path.starts_with("2026/11/"), "{name} -> {path}");
        assert!(!path["2026/11/".len()..].contains(['/', '\\']), "{path}");
    }
    let long = format!("{}.jpg", "\u{e9}".repeat(300));
    let upload = validate(&long, jpeg(), MAX).unwrap();
    store(&s, &upload, day()).unwrap();

    // Only the uploads folder exists beside the test's own files, and every
    // file sits in 2026/11.
    let top: Vec<String> = fs::read_dir(&t.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(top, ["uploads"]);
    let files = files_under(s.root());
    assert_eq!(files.len(), hostile.len() + 1);
    for file in files {
        assert!(file.starts_with("2026/11/"), "{file}");
        assert!(!file["2026/11/".len()..].starts_with('.'), "{file}");
    }
}
