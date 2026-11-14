//! Upload validation and storing: what an uploaded file must pass before it
//! is kept, and how it gets its name on disk.
//!
//! Two steps, as WordPress's `wp_handle_upload` does them:
//!
//! 1. [`validate`] checks the size, cleans the client's file name
//!    ([`sanitize_file_name`], after WordPress's function of that name),
//!    checks the extension against an allow-list, and checks that the bytes
//!    really are that type (after `wp_check_filetype_and_ext`).
//! 2. [`store`] picks a free name in the `YYYY/MM` folder (`name.jpg`,
//!    `name-1.jpg`, ... as `wp_unique_filename` does) and writes it with
//!    [`Storage::put_new`], so two uploads of one name never overwrite each
//!    other.
//!
//! The client's file name is never used as a path: only its last component
//! is kept, and it is cleaned before use.
//!
//! ```
//! use mission_control_core::media::{LocalStorage, Storage, store, validate};
//! use chrono::NaiveDate;
//!
//! let root = std::env::temp_dir().join(format!("mc-upload-doc-{}", std::process::id()));
//! let storage = LocalStorage::new(&root);
//! let upload = validate("../My Notes.TXT", b"hello", 1024).unwrap();
//! assert_eq!(upload.file_name, "My-Notes.txt");
//! assert_eq!(upload.mime_type, "text/plain");
//! let day = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
//! assert_eq!(store(&storage, &upload, day).unwrap(), "2026/11/My-Notes.txt");
//! assert_eq!(store(&storage, &upload, day).unwrap(), "2026/11/My-Notes-1.txt");
//! # std::fs::remove_dir_all(&root).unwrap();
//! ```

use std::fmt;

use chrono::Datelike;

use super::storage::{Storage, StorageError, upload_subdir};

/// The longest file name [`sanitize_file_name`] returns, in bytes. Well under
/// the usual 255-byte limit, so a `-N` suffix from [`store`] always fits.
pub const MAX_FILE_NAME_BYTES: usize = 200;

/// The longest extension kept, in bytes. Longer ones are never allowed types
/// anyway; they are cut so the name stays short.
const MAX_EXTENSION_BYTES: usize = 16;

/// The name used when nothing usable is left of the client's name
/// (WordPress's `unnamed-file`).
const FALLBACK_NAME: &str = "unnamed-file";

/// How many numbered names [`store`] tries before giving up.
const MAX_UNIQUE_ATTEMPTS: u32 = 10_000;

/// Characters removed from file names: WordPress's `sanitize_file_name`
/// list, including its typographic quotes. NUL and other control
/// characters are removed separately.
const SPECIAL_CHARS: &[char] = &[
    '?', '[', ']', '/', '\\', '=', '<', '>', ':', ';', ',', '\'', '"', '&', '$', '#', '*', '(',
    ')', '|', '~', '`', '!', '{', '}', '%', '+', '\u{2019}', '\u{ab}', '\u{bb}', '\u{201d}',
    '\u{201c}',
];

/// How the bytes of a type are checked.
enum Check {
    /// The magic numbers must be detected as one of these MIME types.
    Magic(&'static [&'static str]),
    /// A legacy Office file: an OLE compound file holding one of these
    /// streams at its root (`WordDocument`, `Workbook`, ...). Checked by the
    /// stream rather than the root class id, which writers other than
    /// Microsoft Office often leave empty; an MSI installer, also an OLE
    /// file, has none of these streams.
    Ole(&'static [&'static str]),
    /// No reliable magic numbers: the bytes must be UTF-8 text without NUL
    /// that is not another text format (HTML, XML, a shell script).
    Text,
}

/// One allowed type: its extensions, the MIME type stored for it, and how
/// its bytes are checked.
struct FileType {
    extensions: &'static [&'static str],
    mime_type: &'static str,
    check: Check,
}

/// MIME types `infer` reports for ISO base media files (MP4, QuickTime,
/// 3GP, M4A), which share one container and are told apart only loosely.
const ISO_MEDIA: &[&str] = &["video/mp4", "video/x-m4v", "video/quicktime", "audio/m4a"];

/// MIME types `infer` reports for Matroska files (MKV, WebM, MKA).
const MATROSKA: &[&str] = &["video/x-matroska", "video/webm"];

/// The allowed types: WordPress's default `get_allowed_mime_types` for
/// images, documents, spreadsheets, text, audio and video, limited to the
/// types whose bytes can be checked. No executables, archives, code, SVG,
/// HTML, PHP or JavaScript.
const ALLOWED_TYPES: &[FileType] = &[
    // Images.
    FileType {
        extensions: &["jpg", "jpeg", "jpe"],
        mime_type: "image/jpeg",
        check: Check::Magic(&["image/jpeg"]),
    },
    FileType {
        extensions: &["gif"],
        mime_type: "image/gif",
        check: Check::Magic(&["image/gif"]),
    },
    FileType {
        extensions: &["png"],
        mime_type: "image/png",
        check: Check::Magic(&["image/png"]),
    },
    FileType {
        extensions: &["bmp"],
        mime_type: "image/bmp",
        check: Check::Magic(&["image/bmp"]),
    },
    FileType {
        extensions: &["tiff", "tif"],
        mime_type: "image/tiff",
        check: Check::Magic(&["image/tiff"]),
    },
    FileType {
        extensions: &["webp"],
        mime_type: "image/webp",
        check: Check::Magic(&["image/webp"]),
    },
    FileType {
        extensions: &["avif"],
        mime_type: "image/avif",
        check: Check::Magic(&["image/avif"]),
    },
    FileType {
        extensions: &["ico"],
        mime_type: "image/x-icon",
        check: Check::Magic(&["image/vnd.microsoft.icon"]),
    },
    FileType {
        extensions: &["heic"],
        mime_type: "image/heic",
        check: Check::Magic(&["image/heif"]),
    },
    FileType {
        extensions: &["heif"],
        mime_type: "image/heif",
        check: Check::Magic(&["image/heif"]),
    },
    FileType {
        extensions: &["psd"],
        mime_type: "image/vnd.adobe.photoshop",
        check: Check::Magic(&["image/vnd.adobe.photoshop"]),
    },
    // Video.
    FileType {
        extensions: &["asf", "asx"],
        mime_type: "video/x-ms-asf",
        check: Check::Magic(&["video/x-ms-wmv"]),
    },
    FileType {
        extensions: &["wmv"],
        mime_type: "video/x-ms-wmv",
        check: Check::Magic(&["video/x-ms-wmv"]),
    },
    FileType {
        extensions: &["avi"],
        mime_type: "video/avi",
        check: Check::Magic(&["video/x-msvideo"]),
    },
    FileType {
        extensions: &["flv"],
        mime_type: "video/x-flv",
        check: Check::Magic(&["video/x-flv"]),
    },
    FileType {
        extensions: &["mov", "qt"],
        mime_type: "video/quicktime",
        check: Check::Magic(ISO_MEDIA),
    },
    FileType {
        extensions: &["mpeg", "mpg", "mpe"],
        mime_type: "video/mpeg",
        check: Check::Magic(&["video/mpeg"]),
    },
    FileType {
        extensions: &["mp4", "m4v"],
        mime_type: "video/mp4",
        check: Check::Magic(ISO_MEDIA),
    },
    FileType {
        extensions: &["ogv"],
        mime_type: "video/ogg",
        check: Check::Magic(&["audio/ogg"]),
    },
    FileType {
        extensions: &["webm"],
        mime_type: "video/webm",
        check: Check::Magic(MATROSKA),
    },
    FileType {
        extensions: &["mkv"],
        mime_type: "video/x-matroska",
        check: Check::Magic(MATROSKA),
    },
    FileType {
        extensions: &["3gp", "3gpp"],
        mime_type: "video/3gpp",
        check: Check::Magic(ISO_MEDIA),
    },
    FileType {
        extensions: &["3g2", "3gp2"],
        mime_type: "video/3gpp2",
        check: Check::Magic(ISO_MEDIA),
    },
    // Audio.
    FileType {
        extensions: &["mp3"],
        mime_type: "audio/mpeg",
        check: Check::Magic(&["audio/mpeg"]),
    },
    FileType {
        extensions: &["m4a", "m4b"],
        mime_type: "audio/mpeg",
        check: Check::Magic(ISO_MEDIA),
    },
    FileType {
        extensions: &["aac"],
        mime_type: "audio/aac",
        check: Check::Magic(&["audio/aac"]),
    },
    FileType {
        extensions: &["wav"],
        mime_type: "audio/wav",
        check: Check::Magic(&["audio/x-wav"]),
    },
    FileType {
        extensions: &["ogg", "oga"],
        mime_type: "audio/ogg",
        check: Check::Magic(&["audio/ogg", "audio/opus"]),
    },
    FileType {
        extensions: &["flac"],
        mime_type: "audio/flac",
        check: Check::Magic(&["audio/x-flac"]),
    },
    FileType {
        extensions: &["mid", "midi"],
        mime_type: "audio/midi",
        check: Check::Magic(&["audio/midi"]),
    },
    FileType {
        extensions: &["wma"],
        mime_type: "audio/x-ms-wma",
        check: Check::Magic(&["video/x-ms-wmv"]),
    },
    FileType {
        extensions: &["mka"],
        mime_type: "audio/x-matroska",
        check: Check::Magic(MATROSKA),
    },
    // Documents and spreadsheets.
    FileType {
        extensions: &["pdf"],
        mime_type: "application/pdf",
        check: Check::Magic(&["application/pdf"]),
    },
    FileType {
        extensions: &["rtf"],
        mime_type: "application/rtf",
        check: Check::Magic(&["application/rtf"]),
    },
    FileType {
        extensions: &["doc"],
        mime_type: "application/msword",
        check: Check::Ole(&["WordDocument"]),
    },
    FileType {
        extensions: &["pot", "pps", "ppt"],
        mime_type: "application/vnd.ms-powerpoint",
        check: Check::Ole(&["PowerPoint Document"]),
    },
    FileType {
        extensions: &["xla", "xls", "xlt", "xlw"],
        mime_type: "application/vnd.ms-excel",
        check: Check::Ole(&["Workbook", "Book"]),
    },
    FileType {
        extensions: &["docx"],
        mime_type: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        check: Check::Magic(&[
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ]),
    },
    FileType {
        extensions: &["xlsx"],
        mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        check: Check::Magic(&["application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"]),
    },
    FileType {
        extensions: &["pptx"],
        mime_type: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        check: Check::Magic(&[
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ]),
    },
    FileType {
        extensions: &["odt"],
        mime_type: "application/vnd.oasis.opendocument.text",
        check: Check::Magic(&["application/vnd.oasis.opendocument.text"]),
    },
    FileType {
        extensions: &["odp"],
        mime_type: "application/vnd.oasis.opendocument.presentation",
        check: Check::Magic(&["application/vnd.oasis.opendocument.presentation"]),
    },
    FileType {
        extensions: &["ods"],
        mime_type: "application/vnd.oasis.opendocument.spreadsheet",
        check: Check::Magic(&["application/vnd.oasis.opendocument.spreadsheet"]),
    },
    // Text.
    FileType {
        extensions: &["txt", "asc", "srt"],
        mime_type: "text/plain",
        check: Check::Text,
    },
    FileType {
        extensions: &["csv"],
        mime_type: "text/csv",
        check: Check::Text,
    },
    FileType {
        extensions: &["tsv"],
        mime_type: "text/tab-separated-values",
        check: Check::Text,
    },
    FileType {
        extensions: &["ics"],
        mime_type: "text/calendar",
        check: Check::Text,
    },
    FileType {
        extensions: &["vtt"],
        mime_type: "text/vtt",
        check: Check::Text,
    },
];

/// The allowed type for a (lowercase or not) extension.
fn allowed_type(extension: &str) -> Option<&'static FileType> {
    ALLOWED_TYPES.iter().find(|file_type| {
        file_type
            .extensions
            .iter()
            .any(|ext| ext.eq_ignore_ascii_case(extension))
    })
}

/// The MIME type an allowed extension is stored with, or `None` when the
/// extension is not allowed.
pub fn mime_type_for(extension: &str) -> Option<&'static str> {
    allowed_type(extension).map(|file_type| file_type.mime_type)
}

/// Why an upload was refused.
#[derive(Debug)]
pub enum UploadError {
    /// The file is empty.
    Empty,
    /// The file is larger than the limit (`MC_UPLOAD_MAX_BYTES`).
    TooLarge {
        /// The file's size, in bytes.
        size: u64,
        /// The largest accepted size, in bytes.
        limit: u64,
    },
    /// The extension is not on the allow-list (or there is none).
    TypeNotAllowed {
        /// The sanitised extension, if the name had one.
        extension: Option<String>,
    },
    /// The bytes are not the type the extension says.
    ContentMismatch {
        /// The sanitised extension.
        extension: String,
        /// The type detected from the bytes, if any.
        detected: Option<&'static str>,
    },
    /// No free numbered name was found in the folder.
    NoUniqueName(String),
    /// Writing the file failed.
    Storage(StorageError),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UploadError::Empty => write!(f, "the file is empty"),
            UploadError::TooLarge { size, limit } => write!(
                f,
                "the file is {size} bytes, larger than the {limit}-byte upload limit"
            ),
            UploadError::TypeNotAllowed {
                extension: Some(ext),
            } => write!(f, "files of type .{ext} are not allowed"),
            UploadError::TypeNotAllowed { extension: None } => {
                write!(f, "files without an extension are not allowed")
            }
            UploadError::ContentMismatch {
                extension,
                detected,
            } => match detected {
                Some(found) => write!(f, "the file is {found}, not .{extension}"),
                None => write!(f, "the file's content is not .{extension}"),
            },
            UploadError::NoUniqueName(name) => write!(f, "no free name for {name:?}"),
            UploadError::Storage(err) => write!(f, "could not store the upload: {err}"),
        }
    }
}

impl std::error::Error for UploadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UploadError::Storage(err) => Some(err),
            _ => None,
        }
    }
}

impl From<StorageError> for UploadError {
    fn from(err: StorageError) -> Self {
        UploadError::Storage(err)
    }
}

/// An upload that passed [`validate`], ready for [`store`].
#[derive(Debug, Clone)]
pub struct ValidatedUpload<'a> {
    /// The sanitised file name, for example `My-Photo.jpg`.
    pub file_name: String,
    /// The lowercase extension, for example `jpg`.
    pub extension: String,
    /// The MIME type stored for the extension, for example `image/jpeg`.
    pub mime_type: &'static str,
    /// The file's bytes.
    pub bytes: &'a [u8],
}

/// Refuses a size over `max_bytes`, or an empty file. Callers that know the
/// size before reading the body (a `Content-Length`) can refuse early with
/// this; [`validate`] runs it again on the bytes.
pub fn check_size(size: u64, max_bytes: u64) -> Result<(), UploadError> {
    if size == 0 {
        return Err(UploadError::Empty);
    }
    if size > max_bytes {
        return Err(UploadError::TooLarge {
            size,
            limit: max_bytes,
        });
    }
    Ok(())
}

/// Checks an upload: its size against `max_bytes`, its name (cleaned with
/// [`sanitize_file_name`]), its extension against the allow-list, and its
/// bytes against the extension.
///
/// Unlike WordPress, which renames an image whose bytes are another image
/// type, a mismatch is refused.
pub fn validate<'a>(
    client_name: &str,
    bytes: &'a [u8],
    max_bytes: u64,
) -> Result<ValidatedUpload<'a>, UploadError> {
    check_size(bytes.len() as u64, max_bytes)?;

    let file_name = sanitize_file_name(client_name);
    let extension = match file_name.rsplit_once('.') {
        Some((_, ext)) => ext.to_string(),
        None => return Err(UploadError::TypeNotAllowed { extension: None }),
    };
    let Some(file_type) = allowed_type(&extension) else {
        return Err(UploadError::TypeNotAllowed {
            extension: Some(extension),
        });
    };

    let kind = infer::get(bytes);
    let detected = kind.map(|kind| kind.mime_type());
    let matches = match file_type.check {
        Check::Magic(accepted) => detected.is_some_and(|found| accepted.contains(&found)),
        Check::Ole(streams) => has_ole_stream(bytes, streams),
        // Short binary signatures (`BM`, `MZ`, `ID3`) also start ordinary
        // text, so only a text format (HTML, XML, a script) is refused here;
        // real binary files fail the UTF-8 and NUL checks.
        Check::Text => {
            !kind.is_some_and(|kind| kind.matcher_type() == infer::MatcherType::Text)
                && !bytes.contains(&0)
                && std::str::from_utf8(bytes).is_ok()
        }
    };
    if !matches {
        return Err(UploadError::ContentMismatch {
            extension,
            detected,
        });
    }

    Ok(ValidatedUpload {
        file_name,
        extension,
        mime_type: file_type.mime_type,
        bytes,
    })
}

/// The first bytes of every OLE compound file.
const OLE_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Whether `bytes` is an OLE compound file with one of `streams` at its
/// root. Some writers (macOS's TextEdit, for one) produce files that the
/// strict `cfb` reader refuses but Office opens; for those, the stream's
/// directory entry name (UTF-16LE) must appear after the OLE header.
fn has_ole_stream(bytes: &[u8], streams: &[&str]) -> bool {
    if !bytes.starts_with(OLE_MAGIC) {
        return false;
    }
    match cfb::CompoundFile::open(std::io::Cursor::new(bytes)) {
        Ok(file) => streams
            .iter()
            .any(|stream| file.is_stream(format!("/{stream}"))),
        Err(_) => streams.iter().any(|stream| {
            let name: Vec<u8> = stream
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect();
            bytes.windows(name.len()).any(|window| window == name)
        }),
    }
}

/// Whether `c` is an invisible formatting character: Unicode general
/// category Cf (Unicode 15.1: bidirectional controls, which can show
/// `exe.jpg` as `gpj.exe`, zero-width characters, the soft hyphen, the byte
/// order mark, tag characters, ...), plus the combining grapheme joiner
/// U+034F. `std` has no category lookup, so the Cf ranges are listed here.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{34f}'
            | '\u{600}'..='\u{605}'
            | '\u{61c}'
            | '\u{6dd}'
            | '\u{70f}'
            | '\u{890}'..='\u{891}'
            | '\u{8e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// Cuts `s` to at most `max` bytes, on a character boundary.
fn truncate_bytes(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Postfixes inner extensions of `stem` with `_` when they look like an
/// extension (2 to 5 letters and an optional digit) that is not allowed, so
/// `file.php` in `file.php.jpg` cannot be taken for the real extension.
fn defuse_inner_extensions(stem: &str) -> String {
    let mut parts = stem.split('.');
    let mut out = parts.next().unwrap_or_default().to_string();
    for part in parts {
        out.push('.');
        out.push_str(part);
        let letters = part.trim_end_matches(|c: char| c.is_ascii_digit());
        let looks_like_extension = (2..=5).contains(&letters.len())
            && letters.chars().all(|c| c.is_ascii_alphabetic())
            && part.len() - letters.len() <= 1;
        if looks_like_extension && allowed_type(part).is_none() {
            out.push('_');
        }
    }
    out
}

/// Cleans a client's file name, after WordPress's `sanitize_file_name`.
///
/// - Only the last component after `/` or `\` is kept, so the name is never
///   a path.
/// - NUL, control characters, invisible formatting characters (category
///   Cf) and
///   WordPress's special characters (`? [ ] / \ = < > : ; , ' " & $ # * ( )
///   | ~ ` ! { } % +` and typographic quotes) are removed.
/// - Whitespace becomes `-`; runs of `-` and of `.` collapse to one.
/// - Leading and trailing `.`, `-` and `_` are trimmed, so there are no dot
///   files such as `.htaccess`.
/// - A name that is only an allowed extension becomes `unnamed-file.<ext>`;
///   a name with nothing left becomes `unnamed-file`.
/// - The extension is lowercased; inner extensions that are not allowed get
///   a `_` (`file.php.jpg` becomes `file.php_.jpg`).
/// - The name is cut to [`MAX_FILE_NAME_BYTES`], keeping the extension.
pub fn sanitize_file_name(client_name: &str) -> String {
    let last = client_name.rsplit(['/', '\\']).next().unwrap_or_default();

    let mut name = String::with_capacity(last.len());
    for c in last.chars() {
        let c = if c.is_whitespace() { '-' } else { c };
        if c.is_control() || is_invisible_format(c) || SPECIAL_CHARS.contains(&c) {
            continue;
        }
        if (c == '-' || c == '.') && name.ends_with(c) {
            continue;
        }
        name.push(c);
    }
    let name = name.trim_matches(['.', '-', '_']);

    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), ext.to_lowercase()),
        None if !name.is_empty() && allowed_type(name).is_some() => {
            (FALLBACK_NAME.to_string(), name.to_lowercase())
        }
        None if name.is_empty() => return FALLBACK_NAME.to_string(),
        None => {
            let mut name = name.to_string();
            truncate_bytes(&mut name, MAX_FILE_NAME_BYTES);
            return name;
        }
    };
    let mut extension = extension;
    truncate_bytes(&mut extension, MAX_EXTENSION_BYTES);

    let budget = MAX_FILE_NAME_BYTES - 1 - extension.len();
    let mut stem = defuse_inner_extensions(&stem);
    if stem.len() > budget {
        // Leave a byte for a `_` the cut may need, then defuse the cut end.
        truncate_bytes(&mut stem, budget - 1);
        stem = defuse_inner_extensions(stem.trim_end_matches(['.', '-']));
    }
    if stem.is_empty() {
        stem = FALLBACK_NAME.to_string();
    }
    format!("{stem}.{extension}")
}

/// Whether a file name stem could be taken for an image sub-size
/// (`-150x150`, `-scaled`, `-rotated`), which WordPress always numbers.
fn looks_like_sub_size(stem: &str) -> bool {
    let Some((_, tail)) = stem.rsplit_once('-') else {
        return false;
    };
    if tail == "scaled" || tail == "rotated" {
        return true;
    }
    match tail.split_once('x') {
        Some((w, h)) => {
            !w.is_empty()
                && !h.is_empty()
                && w.bytes().all(|b| b.is_ascii_digit())
                && h.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Writes a validated upload into the `YYYY/MM` folder for `date` under a
/// free name, and returns its storage path (for example
/// `2026/11/photo-1.jpg`).
///
/// The name is the upload's name, then `name-1.ext`, `name-2.ext`, ... as
/// WordPress's `wp_unique_filename` does; names that look like an image
/// sub-size start at `-1`, and at most 10,000 numbers are tried
/// ([`UploadError::NoUniqueName`] after that). The names are claimed with
/// [`Storage::put_new_first`]: the bytes are written once, a taken name is
/// never replaced, and a concurrent upload of the same name takes the next
/// number instead of overwriting.
pub fn store(
    storage: &dyn Storage,
    upload: &ValidatedUpload<'_>,
    date: impl Datelike,
) -> Result<String, UploadError> {
    let folder = upload_subdir(date);
    let (stem, extension) = upload
        .file_name
        .rsplit_once('.')
        .unwrap_or((upload.file_name.as_str(), upload.extension.as_str()));
    let first = u32::from(looks_like_sub_size(stem));
    let mut paths = (first..=MAX_UNIQUE_ATTEMPTS).map(|number| {
        if number == 0 {
            format!("{folder}/{stem}.{extension}")
        } else {
            format!("{folder}/{stem}-{number}.{extension}")
        }
    });
    match storage.put_new_first(&mut paths, upload.bytes) {
        Ok(path) => Ok(path),
        Err(StorageError::AlreadyExists(_)) => {
            Err(UploadError::NoUniqueName(upload.file_name.clone()))
        }
        Err(err) => Err(err.into()),
    }
}
