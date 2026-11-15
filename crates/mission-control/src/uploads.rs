//! The uploads, served at `/wp-content/uploads/YYYY/MM/name.ext` as WordPress serves
//! `wp-content/uploads`, from the app's [`Storage`].
//!
//! Uploads are files people send, so they are served as data and never run (design note G4):
//!
//! - The `Content-Type` is the type the upload checks stored for the file's extension
//!   ([`mime_type_for`]), never sniffed from the bytes; a file whose extension is not an allowed
//!   upload type is not served.
//! - Every file is sent with `X-Content-Type-Options: nosniff`, so a browser keeps that type,
//!   and with `Content-Security-Policy: sandbox`, so a file opened on its own runs no script and
//!   gets a unique origin. PDFs are the exception: browsers' PDF viewers refuse to open a
//!   sandboxed document, and a PDF's own scripts run in the viewer, not on the site.
//! - A segment that starts with `.` (a dot file, `..`, the storage's `.mc-tmp.*` temp files),
//!   or that holds `/`, `\` or NUL once decoded, is refused; [`Storage`] refuses a path that
//!   would leave the uploads folder, through a symlink too.
//!
//! A refused path or a missing file forwards, so the public site answers with the theme's 404
//! page, as WordPress's rewrite rules send a missing upload to `index.php`.
//!
//! A file on the local disk is streamed from an open handle ([`Storage::open`]) with its
//! `Content-Length`, never read whole into memory. There are no range requests or caching
//! headers yet.

use std::io::Cursor;
use std::sync::Arc;

use mission_control_core::media::{OpenedFile, Storage, StorageError, mime_type_for};
use rocket::http::{ContentType, Header, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::{self, Responder};
use rocket::{Request, Response, Route};

/// A stored upload, read for the request's path.
pub(crate) struct UploadFile {
    opened: OpenedFile,
    mime_type: &'static str,
}

/// The storage path a request under `/wp-content/uploads/` names, when every segment is a
/// plain name: not empty, not starting with `.`, with no `/`, `\` or NUL. The segments are
/// percent-decoded already.
fn storage_path<'a>(segments: impl Iterator<Item = &'a str>) -> Option<String> {
    let parts: Vec<&str> = segments.collect();
    let plain = |part: &&str| {
        !part.is_empty() && !part.starts_with('.') && !part.contains(['/', '\\', '\0'])
    };
    if parts.is_empty() || !parts.iter().all(plain) {
        return None;
    }
    Some(parts.join("/"))
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for UploadFile {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        let Some(storage) = request.rocket().state::<Arc<dyn Storage>>() else {
            log::error!("The uploads route ran before the app was ready");
            return Outcome::Error((Status::InternalServerError, ()));
        };
        // The route's own `/wp-content/uploads`, then the file's path.
        let Some(path) = storage_path(request.uri().path().segments().skip(2)) else {
            return Outcome::Forward(Status::NotFound);
        };
        let Some(mime_type) = path
            .rsplit_once('.')
            .and_then(|(_, extension)| mime_type_for(extension))
        else {
            return Outcome::Forward(Status::NotFound);
        };
        let storage = Arc::clone(storage);
        let read = rocket::tokio::task::spawn_blocking(move || storage.open(&path)).await;
        match read {
            Ok(Ok(opened)) => Outcome::Success(Self { opened, mime_type }),
            Ok(Err(StorageError::NotFound(_) | StorageError::InvalidPath(_))) => {
                Outcome::Forward(Status::NotFound)
            }
            // A folder at that path reads as an I/O error; it is no file either.
            Ok(Err(StorageError::Io(e))) if e.kind() == std::io::ErrorKind::IsADirectory => {
                Outcome::Forward(Status::NotFound)
            }
            Ok(Err(e)) => {
                log::error!("Could not read an upload: {e}");
                Outcome::Error((Status::InternalServerError, ()))
            }
            Err(e) => {
                log::error!("The upload read failed: {e}");
                Outcome::Error((Status::InternalServerError, ()))
            }
        }
    }
}

impl<'r> Responder<'r, 'static> for UploadFile {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let content_type =
            ContentType::parse_flexible(self.mime_type).unwrap_or(ContentType::Binary);
        let mut builder = Response::build();
        builder
            .status(Status::Ok)
            .header(content_type)
            .header(Header::new("X-Content-Type-Options", "nosniff"));
        if self.mime_type != "application/pdf" {
            builder.header(Header::new("Content-Security-Policy", "sandbox"));
        }
        match self.opened {
            OpenedFile::File(file, length) => {
                let length = usize::try_from(length).unwrap_or(usize::MAX);
                builder.sized_body(length, rocket::tokio::fs::File::from_std(file));
            }
            OpenedFile::Bytes(bytes) => {
                builder.sized_body(bytes.len(), Cursor::new(bytes));
            }
        }
        Ok(builder.finalize())
    }
}

/// The uploads route.
pub(crate) fn routes() -> Vec<Route> {
    rocket::routes![upload]
}

/// A stored upload.
#[rocket::get("/wp-content/uploads/<_..>")]
fn upload(file: UploadFile) -> UploadFile {
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_make_a_path() {
        let path = |parts: &[&str]| storage_path(parts.iter().copied());
        assert_eq!(
            path(&["2026", "11", "a.jpg"]).as_deref(),
            Some("2026/11/a.jpg")
        );
        for refused in [
            &[][..],
            &["2026", "11", ".mc-tmp.1.2"],
            &["..", "secret.txt"],
            &["2026", "", "a.jpg"],
            &["2026/11", "a.jpg"],
            &["2026", "a\\b.jpg"],
            &["2026", "a\0.jpg"],
        ] {
            assert_eq!(path(refused), None, "{refused:?}");
        }
    }
}
