//! Attachments: an uploaded file as a `posts` row of type `attachment`, with its title, alt
//! text, caption and description, as WordPress stores them. They run on the database in
//! `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use mission_control_core::content::{NewPost, Post, PostStatus, get_post_meta};
use mission_control_core::media::{
    AttachmentError, AttachmentMetadata, ImageSizes, LocalStorage, Storage, create_attachment,
    get_attachment, update_attachment_details, upload_subdir, validate,
};
use mission_control_core::migrations::migrator;
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Error, Model, raw};
use serde_json::json;

const MAX: u64 = 64 * 1024 * 1024;

/// A fresh temp folder, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mc-attachment-{name}-{}", std::process::id()));
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

/// Local storage in a temp folder, the registry of default sizes, and the temp folder (keep it
/// alive for the test).
fn media(name: &str) -> (Temp, Arc<dyn Storage>, Arc<ImageSizes>) {
    let temp = Temp::new(name);
    let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(temp.0.join("uploads")));
    (temp, storage, Arc::new(ImageSizes::new()))
}

/// Every file under the temp folder's `uploads`, as storage paths, sorted.
fn files(temp: &Temp) -> Vec<String> {
    let root = temp.0.join("uploads");
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(&root)
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

/// A gradient JPEG, so resized copies are real images.
fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&image)
        .unwrap();
    out
}

/// A minimal PDF.
const PDF: &[u8] =
    b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";

/// Runs the core migrations and returns the id of a new user to author attachments.
async fn setup(db: &Database) -> i64 {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .expect("inserts the author");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the author id")
}

/// The `YYYY/MM` folder for an upload made now.
fn this_month() -> String {
    upload_subdir(Utc::now().date_naive())
}

#[tokio::test]
async fn an_image_upload_becomes_an_attachment_with_its_sizes_and_meta() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (temp, storage, sizes) = media("image");

    let bytes = jpeg_bytes(1600, 1200);
    let upload = validate("Sunset Beach.jpg", &bytes, MAX).unwrap();
    let attachment = create_attachment(db, &storage, &sizes, &upload, None, author)
        .await
        .expect("creates the attachment");

    let folder = this_month();
    assert!(attachment.id > 0);
    // WordPress's default title: the file name without its extension.
    assert_eq!(attachment.title, "Sunset-Beach");
    assert_eq!(attachment.alt, "");
    assert_eq!(attachment.caption, "");
    assert_eq!(attachment.description, "");
    assert_eq!(attachment.file, format!("{folder}/Sunset-Beach.jpg"));
    assert_eq!(attachment.mime_type, "image/jpeg");
    assert_eq!(attachment.parent_id, None);
    assert_eq!(attachment.author_id, author);

    let metadata = attachment.metadata.clone().expect("an image has metadata");
    assert_eq!((metadata.width, metadata.height), (1600, 1200));
    assert_eq!(metadata.file, attachment.file);
    let mut names: Vec<&str> = metadata.sizes.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["1536x1536", "large", "medium", "medium_large", "thumbnail"]
    );
    let mut expected = vec![attachment.file.clone()];
    expected.extend(
        metadata
            .sizes
            .values()
            .map(|size| format!("{folder}/{}", size.file)),
    );
    expected.sort();
    assert_eq!(files(&temp), expected);

    // The row, as `wp_insert_attachment` writes it.
    let post = Post::query().find_or_fail(db, attachment.id).await.unwrap();
    assert_eq!(post.post_type, "attachment");
    assert_eq!(post.status, PostStatus::Inherit);
    assert_eq!(post.mime_type, "image/jpeg");
    assert_eq!(post.title, "Sunset-Beach");
    assert_eq!(post.slug, "sunset-beach");
    assert_eq!(post.author_id, author);
    assert!(post.published_at.is_some());

    // The meta WordPress keeps for an attachment.
    assert_eq!(
        get_post_meta(db, attachment.id, "_wp_attached_file")
            .await
            .unwrap(),
        Some(json!(attachment.file))
    );
    let stored = get_post_meta(db, attachment.id, "_wp_attachment_metadata")
        .await
        .unwrap()
        .expect("the metadata is stored");
    let stored: AttachmentMetadata = serde_json::from_value(stored).unwrap();
    assert_eq!(stored, metadata);

    let read = get_attachment(db, attachment.id)
        .await
        .unwrap()
        .expect("reads it back");
    assert_eq!(read, attachment);
}

#[tokio::test]
async fn title_alt_caption_and_description_save_and_read_back() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (_temp, storage, sizes) = media("details");

    let bytes = jpeg_bytes(400, 300);
    let upload = validate("photo.jpg", &bytes, MAX).unwrap();
    let attachment = create_attachment(db, &storage, &sizes, &upload, None, author)
        .await
        .unwrap();

    let updated = update_attachment_details(
        db,
        attachment.id,
        "A red boat",
        "A red boat moored in the harbour",
        "Taken at dawn.",
        "<p>The harbour at Port Isaac.</p>",
    )
    .await
    .expect("updates the details");
    assert_eq!(updated.title, "A red boat");
    assert_eq!(updated.alt, "A red boat moored in the harbour");
    assert_eq!(updated.caption, "Taken at dawn.");
    assert_eq!(updated.description, "<p>The harbour at Port Isaac.</p>");
    // The file and its metadata are unchanged.
    assert_eq!(updated.file, attachment.file);
    assert_eq!(updated.metadata, attachment.metadata);

    let read = get_attachment(db, attachment.id).await.unwrap().unwrap();
    assert_eq!(read, updated);

    // Where WordPress keeps them: caption = excerpt, description = content, alt = meta.
    let post = Post::query().find_or_fail(db, attachment.id).await.unwrap();
    assert_eq!(post.title, "A red boat");
    assert_eq!(post.excerpt, "Taken at dawn.");
    assert_eq!(post.content, "<p>The harbour at Port Isaac.</p>");
    // The slug stays the one the upload gave it.
    assert_eq!(post.slug, "photo");
    assert_eq!(
        get_post_meta(db, attachment.id, "_wp_attachment_image_alt")
            .await
            .unwrap(),
        Some(json!("A red boat moored in the harbour"))
    );

    // Clearing the fields stores empty values.
    let cleared = update_attachment_details(db, attachment.id, "photo", "", "", "")
        .await
        .unwrap();
    assert_eq!(
        (
            cleared.alt.as_str(),
            cleared.caption.as_str(),
            cleared.description.as_str()
        ),
        ("", "", "")
    );
    assert_eq!(
        get_attachment(db, attachment.id).await.unwrap().unwrap(),
        cleared
    );
}

#[tokio::test]
async fn files_that_are_not_images_have_no_sizes() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (temp, storage, sizes) = media("documents");

    let pdf = validate("Annual Report.pdf", PDF, MAX).unwrap();
    let report = create_attachment(db, &storage, &sizes, &pdf, None, author)
        .await
        .unwrap();
    assert_eq!(report.mime_type, "application/pdf");
    assert_eq!(report.title, "Annual-Report");
    assert_eq!(report.metadata, None);
    assert_eq!(
        get_post_meta(db, report.id, "_wp_attachment_metadata")
            .await
            .unwrap(),
        None
    );

    let text = validate("notes.txt", b"Some notes.\n", MAX).unwrap();
    let notes = create_attachment(db, &storage, &sizes, &text, None, author)
        .await
        .unwrap();
    assert_eq!(notes.mime_type, "text/plain");
    assert_eq!(notes.metadata, None);

    let folder = this_month();
    assert_eq!(
        files(&temp),
        [
            format!("{folder}/Annual-Report.pdf"),
            format!("{folder}/notes.txt")
        ]
    );
    assert_eq!(
        get_attachment(db, notes.id).await.unwrap().unwrap().file,
        format!("{folder}/notes.txt")
    );
}

#[tokio::test]
async fn an_attachment_links_to_the_post_it_was_uploaded_to() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (temp, storage, sizes) = media("parent");

    let published = NaiveDate::from_ymd_opt(2025, 3, 7)
        .unwrap()
        .and_hms_opt(8, 0, 0)
        .unwrap();
    let post = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Harbour walk".into(),
            published_at: Some(published),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .unwrap();

    let bytes = jpeg_bytes(200, 100);
    let upload = validate("boat.jpg", &bytes, MAX).unwrap();
    let attachment = create_attachment(db, &storage, &sizes, &upload, Some(post.id), author)
        .await
        .unwrap();
    assert_eq!(attachment.parent_id, Some(post.id));
    // As `media_handle_upload` does, the file goes in the folder of the post's date.
    assert_eq!(attachment.file, "2025/03/boat.jpg");
    let row = Post::query().find_or_fail(db, attachment.id).await.unwrap();
    assert_eq!(row.parent_id, Some(post.id));

    // A page's uploads go in this month's folder, as in WordPress.
    let page = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "About".into(),
            published_at: Some(published),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .unwrap();
    let on_page = create_attachment(db, &storage, &sizes, &upload, Some(page.id), author)
        .await
        .unwrap();
    assert_eq!(on_page.file, format!("{}/boat.jpg", this_month()));

    // Attachments share the table but are not listed with posts.
    let posts = Post::of_type("post").get(db).await.unwrap();
    assert_eq!(posts.iter().map(|p| p.id).collect::<Vec<_>>(), [post.id]);
    let attachments = Post::of_type("attachment").get(db).await.unwrap();
    let mut ids: Vec<i64> = attachments.iter().map(|p| p.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [attachment.id, on_page.id]);

    // A parent that does not exist, or that is an attachment, is refused before any file is
    // written.
    let before = files(&temp);
    let missing =
        create_attachment(db, &storage, &sizes, &upload, Some(post.id + 1_000), author).await;
    assert!(
        matches!(missing, Err(AttachmentError::InvalidParent(id)) if id == post.id + 1_000),
        "{missing:?}"
    );
    let nested =
        create_attachment(db, &storage, &sizes, &upload, Some(attachment.id), author).await;
    assert!(
        matches!(nested, Err(AttachmentError::InvalidParent(_))),
        "{nested:?}"
    );
    assert_eq!(files(&temp), before);
}

#[tokio::test]
async fn an_upload_named_like_a_sub_size_never_blocks_or_overwrites_sub_sizes() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (temp, storage, sizes) = media("clash");
    let folder = this_month();

    // `photo.jpg`, then an upload named like its thumbnail.
    let first_bytes = jpeg_bytes(1600, 1200);
    let first = create_attachment(
        db,
        &storage,
        &sizes,
        &validate("photo.jpg", &first_bytes, MAX).unwrap(),
        None,
        author,
    )
    .await
    .unwrap();
    let thumbnail_path = format!("{folder}/photo-150x150.jpg");
    let thumbnail = storage.get(&thumbnail_path).unwrap();
    let second_bytes = jpeg_bytes(800, 600);
    let second = create_attachment(
        db,
        &storage,
        &sizes,
        &validate("photo-150x150.jpg", &second_bytes, MAX).unwrap(),
        None,
        author,
    )
    .await
    .unwrap();
    assert_eq!(first.file, format!("{folder}/photo.jpg"));
    assert_eq!(second.file, format!("{folder}/photo-150x150-1.jpg"));
    assert_eq!(storage.get(&thumbnail_path).unwrap(), thumbnail);
    let second_sizes = &second.metadata.as_ref().unwrap().sizes;
    // 800x600: thumbnail, medium and medium_large; the larger sizes are not made.
    assert_eq!(second_sizes.len(), 3, "{second_sizes:?}");
    assert_eq!(
        second_sizes["thumbnail"].file,
        "photo-150x150-1-150x150.jpg"
    );

    // The other way round: a file named like `picture.jpg`'s thumbnail is already there.
    storage
        .put(&format!("{folder}/picture-150x150.jpg"), b"someone else's")
        .unwrap();
    let picture = create_attachment(
        db,
        &storage,
        &sizes,
        &validate("picture.jpg", &first_bytes, MAX).unwrap(),
        None,
        author,
    )
    .await
    .unwrap();
    assert_eq!(picture.file, format!("{folder}/picture-1.jpg"));
    let picture_sizes = &picture.metadata.as_ref().unwrap().sizes;
    assert_eq!(
        picture_sizes.len(),
        5,
        "every size is made: {picture_sizes:?}"
    );
    assert_eq!(picture_sizes["thumbnail"].file, "picture-1-150x150.jpg");
    assert_eq!(
        storage
            .get(&format!("{folder}/picture-150x150.jpg"))
            .unwrap(),
        b"someone else's"
    );
    assert!(files(&temp).contains(&format!("{folder}/picture-1-150x150.jpg")));
}

#[tokio::test]
async fn a_database_failure_removes_the_files_written() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (temp, storage, sizes) = media("rollback");

    let bytes = jpeg_bytes(800, 600);
    let upload = validate("photo.jpg", &bytes, MAX).unwrap();
    let missing_author = author + 1_000_000;
    let result = create_attachment(db, &storage, &sizes, &upload, None, missing_author).await;
    assert!(
        matches!(
            result,
            Err(AttachmentError::Database(Error::ForeignKeyViolation { .. }))
        ),
        "{result:?}"
    );
    assert!(files(&temp).is_empty(), "{:?}", files(&temp));
    let count: i64 = raw::fetch_scalar(db, "SELECT COUNT(*) FROM posts", &[])
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn other_post_types_are_not_attachments() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let post = Post::create(db, NewPost::new(author, "post"))
        .await
        .unwrap();

    assert_eq!(get_attachment(db, post.id).await.unwrap(), None);
    assert_eq!(get_attachment(db, post.id + 1_000).await.unwrap(), None);
    let err = update_attachment_details(db, post.id, "t", "a", "c", "d")
        .await
        .unwrap_err();
    assert!(
        matches!(err, AttachmentError::NotFound(id) if id == post.id),
        "{err:?}"
    );
    // The post is untouched.
    assert_eq!(Post::query().find_or_fail(db, post.id).await.unwrap(), post);
}
