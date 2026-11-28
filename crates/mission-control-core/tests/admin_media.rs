//! The admin's media library (Task 6.4, Rulings R127 to R129): the `attachment` screen's data
//! layer (list, search, edit details, delete with the files), the picker's query, uploads with
//! their permissions, and the media library of Ignite's media fields. They run on the database
//! in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use ignite_core::{Direction, ListQuery, MediaLibrary, Row};
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use mission_control_core::admin::media::{
    AttachmentQuery, Attachments, UploadRefusal, attachment_for_js, attachment_post_type,
    attachment_resource, query_attachments, upload_to_library,
};
use mission_control_core::content::{
    NewPost, Post, PostStatus, get_post_meta, set_option, set_post_meta,
};
use mission_control_core::media::{ImageSizes, LocalStorage, Storage, validate};
use mission_control_core::migrations::migrator;
use mission_control_core::types::Access;
use mission_control_core::users::{AUTHOR, EDITOR, PostTypeCaps, SUBSCRIBER, UserCaps};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Model, raw};
use serde_json::{Value, json};

const MAX: u64 = 8 * 1024 * 1024;

/// A minimal PDF.
const PDF: &[u8] =
    b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";

/// A fresh temp folder, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mc-admin-media-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }

    fn file(&self, path: &str) -> PathBuf {
        self.0.join("uploads").join(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn media(name: &str) -> (Temp, Arc<dyn Storage>, Arc<ImageSizes>) {
    let temp = Temp::new(name);
    let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(temp.0.join("uploads")));
    (temp, storage, Arc::new(ImageSizes::new()))
}

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

async fn setup(db: &Database) {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    set_option(db, "home", &json!("https://example.test/"))
        .await
        .expect("sets home");
}

async fn add_user(db: &Database, name: &str) -> i64 {
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &[name.into(), unique_email(name).into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the user id")
}

fn caps(id: i64, role: &'static str) -> UserCaps {
    UserCaps::with_default_roles(id, &[role], &BTreeMap::new())
}

fn default_caps(post_type: &str) -> PostTypeCaps {
    PostTypeCaps::for_post_type(post_type)
}

async fn post(db: &Database, author: i64, post_type: &str, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: format!("A {post_type}"),
            ..NewPost::new(author, post_type)
        },
    )
    .await
    .expect("creates the post")
}

fn list_query(search: Option<&str>, sort: Option<(&str, Direction)>) -> ListQuery {
    ListQuery {
        page: 1,
        per_page: 20,
        search: search.map(str::to_owned),
        sort: sort.map(|(key, dir)| (key.to_owned(), dir)),
        filter: Default::default(),
    }
}

fn titles(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|row| row["col_title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn the_screen_lists_edits_and_deletes_attachments() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let editor_id = add_user(db, "Edna").await;
    let editor = caps(editor_id, EDITOR);
    let (temp, storage, sizes) = media("screen");

    let jpeg = jpeg_bytes(800, 600);
    let photo = validate("Harbour Boats.jpg", &jpeg, MAX).unwrap();
    let photo = upload_to_library(db, &storage, &sizes, &editor, &photo, None, &default_caps)
        .await
        .expect("uploads the photo");
    let report = validate("Annual Report.pdf", PDF, MAX).unwrap();
    let report = upload_to_library(db, &storage, &sizes, &editor, &report, None, &default_caps)
        .await
        .expect("uploads the report");
    // A private attachment is listed; one in the trash (no MEDIA_TRASH) and other posts are not.
    let private = post(db, editor_id, "attachment", PostStatus::Private).await;
    let trashed = post(db, editor_id, "attachment", PostStatus::Trash).await;
    let page = post(db, editor_id, "page", PostStatus::Publish).await;

    let resource = attachment_resource(Arc::clone(&storage), Access::default());
    assert_eq!(resource.slug, "attachment");
    let fields: Vec<&str> = resource.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fields, ["title", "alt", "caption", "description"]);
    let columns: Vec<&str> = resource.columns.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(
        columns,
        [
            "col_thumb",
            "col_title",
            "col_file",
            "col_mime",
            "col_author",
            "col_date"
        ]
    );
    let data = Arc::clone(&resource.data);

    // Newest first, every listed status, with the file name, type and author.
    let listed = data.list(db, &list_query(None, None)).await.unwrap();
    assert_eq!(listed.total, 3);
    let ids: Vec<i64> = listed
        .rows
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [private.id, report.id, photo.id]);
    let photo_row = &listed.rows[2];
    assert_eq!(photo_row["col_title"], "Harbour Boats");
    assert_eq!(photo_row["col_file"], "Harbour-Boats.jpg");
    assert_eq!(photo_row["col_mime"], "image/jpeg");
    assert_eq!(photo_row["col_author"], "Edna");
    assert!(
        photo_row["col_date"]
            .as_str()
            .is_some_and(|d| d.len() == 19)
    );

    // Sorted by title; searched on the title and on the file name.
    let sorted = data
        .list(db, &list_query(None, Some(("col_title", Direction::Asc))))
        .await
        .unwrap();
    assert_eq!(
        titles(&sorted.rows),
        ["A attachment", "Annual Report", "Harbour Boats"]
    );
    let found = data
        .list(db, &list_query(Some("harbour"), None))
        .await
        .unwrap();
    assert_eq!(titles(&found.rows), ["Harbour Boats"]);
    let row = data.find(db, report.id).await.unwrap().unwrap();
    let mut input = Row::new();
    input.insert("title".into(), json!("Figures"));
    data.update(db, report.id, input).await.unwrap();
    assert_eq!(row["title"], "Annual Report");
    let by_file = data
        .list(db, &list_query(Some("Annual-Report.pdf"), None))
        .await
        .unwrap();
    assert_eq!(titles(&by_file.rows), ["Figures"]);
    let excluded = data
        .list(db, &list_query(Some("-harbour"), None))
        .await
        .unwrap();
    assert_eq!(excluded.total, 2);
    let beyond = data
        .list(
            db,
            &ListQuery {
                page: 9,
                per_page: 500,
                search: None,
                sort: None,
                filter: Default::default(),
            },
        )
        .await
        .unwrap();
    assert!(beyond.rows.is_empty());
    assert_eq!(beyond.total, 3);

    // The form: no row for a post that is not a listed attachment.
    assert!(data.find(db, page.id).await.unwrap().is_none());
    assert!(data.find(db, trashed.id).await.unwrap().is_none());
    assert!(data.find(db, i64::MAX).await.unwrap().is_none());

    // Edit the details: the alt text without tags, the caption and description as sent; a key
    // not sent keeps its value, a cleared one is empty.
    let mut input = Row::new();
    input.insert("title".into(), json!("Boats in the harbour"));
    input.insert(
        "alt".into(),
        json!("  <b>Boats</b> at dawn <script>x()</script> "),
    );
    input.insert("caption".into(), json!("Taken <em>early</em>"));
    input.insert(
        "description".into(),
        json!("<p onclick=\"x()\">Long description</p>"),
    );
    let saved = data.update(db, photo.id, input).await.unwrap();
    assert_eq!(saved["title"], "Boats in the harbour");
    assert_eq!(saved["alt"], "Boats at dawn");
    assert_eq!(saved["caption"], "Taken <em>early</em>");
    assert_eq!(
        saved["description"],
        "<p onclick=\"x()\">Long description</p>"
    );
    assert_eq!(
        get_post_meta(db, photo.id, "_wp_attachment_image_alt")
            .await
            .unwrap(),
        Some(json!("Boats at dawn"))
    );
    let stored = Post::query().find(db, photo.id).await.unwrap().unwrap();
    assert_eq!(stored.excerpt, "Taken <em>early</em>");
    assert_eq!(stored.content, "<p onclick=\"x()\">Long description</p>");
    let mut input = Row::new();
    input.insert("caption".into(), Value::Null);
    let saved = data.update(db, photo.id, input).await.unwrap();
    assert_eq!(saved["caption"], "");
    assert_eq!(saved["title"], "Boats in the harbour");
    assert_eq!(saved["alt"], "Boats at dawn");
    assert!(data.update(db, page.id, Row::new()).await.is_err());

    // Ignite's create is refused: files are uploaded.
    assert!(data.create(db, Row::new()).await.is_err());

    // Delete removes the row, its file and its sub-sizes, and the featured images that point
    // at it.
    let file = photo.file.clone();
    let metadata = photo.metadata.clone().expect("the photo has sizes");
    assert!(metadata.sizes.contains_key("thumbnail"));
    assert!(metadata.sizes.contains_key("medium"));
    let folder = file.rsplit_once('/').unwrap().0.to_owned();
    let mut paths = vec![file.clone()];
    paths.extend(
        metadata
            .sizes
            .values()
            .map(|s| format!("{folder}/{}", s.file)),
    );
    for path in &paths {
        assert!(temp.file(path).exists(), "{path}");
    }
    let featured = post(db, editor_id, "post", PostStatus::Publish).await;
    set_post_meta(db, featured.id, "_thumbnail_id", &json!(photo.id))
        .await
        .unwrap();
    assert_eq!(data.delete(db, photo.id).await.unwrap(), 1);
    for path in &paths {
        assert!(!temp.file(path).exists(), "{path} is deleted");
    }
    assert!(Post::query().find(db, photo.id).await.unwrap().is_none());
    assert_eq!(
        get_post_meta(db, featured.id, "_thumbnail_id")
            .await
            .unwrap(),
        None
    );
    assert_eq!(data.delete(db, photo.id).await.unwrap(), 0);
    assert_eq!(data.delete(db, page.id).await.unwrap(), 0);
    assert!(Post::query().find(db, page.id).await.unwrap().is_some());
    assert_eq!(
        data.list(db, &list_query(None, None)).await.unwrap().total,
        2
    );
}

#[tokio::test]
async fn the_picker_lists_what_the_user_may_read_as_wordpress_shapes_it() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let editor_id = add_user(db, "Edna").await;
    let author_id = add_user(db, "Abe").await;
    let subscriber_id = add_user(db, "Sue").await;
    let editor = caps(editor_id, EDITOR);
    let (_temp, storage, sizes) = media("picker");

    let jpeg = jpeg_bytes(1000, 750);
    let photo = validate("Sunset.jpg", &jpeg, MAX).unwrap();
    let photo = upload_to_library(db, &storage, &sizes, &editor, &photo, None, &default_caps)
        .await
        .unwrap();
    let report = validate("Report.pdf", PDF, MAX).unwrap();
    let report = upload_to_library(db, &storage, &sizes, &editor, &report, None, &default_caps)
        .await
        .unwrap();
    let private = post(db, editor_id, "attachment", PostStatus::Private).await;
    post(db, editor_id, "attachment", PostStatus::Trash).await;

    let query = AttachmentQuery {
        page: 1,
        ..AttachmentQuery::default()
    };
    // An editor reads private attachments too; an author does not; a subscriber gets nothing.
    let all = query_attachments(db, &editor, &query).await.unwrap();
    let ids: Vec<i64> = all
        .items
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [private.id, report.id, photo.id]);
    assert_eq!((all.total, all.total_pages), (3, 1));
    let by_author = query_attachments(db, &caps(author_id, AUTHOR), &query)
        .await
        .unwrap();
    let ids: Vec<i64> = by_author
        .items
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [report.id, photo.id]);
    let nothing = query_attachments(db, &caps(subscriber_id, SUBSCRIBER), &query)
        .await
        .unwrap();
    assert!(nothing.items.is_empty());
    assert_eq!(nothing.total, 0);

    // `wp_prepare_attachment_for_js`'s keys, with the sizes and their addresses.
    let image = all.items.iter().find(|i| i["id"] == photo.id).unwrap();
    let base = "https://example.test/mc-content/uploads";
    assert_eq!(image["title"], "Sunset");
    assert_eq!(image["filename"], "Sunset.jpg");
    assert_eq!(image["url"], format!("{base}/{}", photo.file));
    assert_eq!(image["type"], "image");
    assert_eq!(image["subtype"], "jpeg");
    assert_eq!(image["mime"], "image/jpeg");
    assert_eq!(image["authorName"], "Edna");
    assert_eq!(image["author"], editor_id.to_string());
    assert_eq!(image["alt"], "");
    assert_eq!(
        (image["width"].as_u64(), image["height"].as_u64()),
        (Some(1000), Some(750))
    );
    assert_eq!(image["orientation"], "landscape");
    assert_eq!(image["sizes"]["full"]["url"], image["url"]);
    // Only the sizes the media modal offers (`image_size_names_choose`).
    let names: Vec<&String> = image["sizes"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["full", "medium", "thumbnail"]);
    let stored = photo.metadata.as_ref().unwrap();
    assert!(stored.sizes.contains_key("medium_large"), "{stored:?}");
    let medium = &image["sizes"]["medium"];
    assert_eq!(
        (medium["width"].as_u64(), medium["height"].as_u64()),
        (Some(300), Some(225))
    );
    let folder = photo.file.rsplit_once('/').unwrap().0;
    assert_eq!(medium["url"], format!("{base}/{folder}/Sunset-300x225.jpg"));
    assert!(
        image["date"]
            .as_i64()
            .is_some_and(|ms| ms > 1_700_000_000_000)
    );
    let pdf = all.items.iter().find(|i| i["id"] == report.id).unwrap();
    assert_eq!(pdf["type"], "application");
    assert_eq!(pdf["subtype"], "pdf");
    assert!(pdf.get("sizes").is_none());
    assert_eq!(
        attachment_for_js(db, report.id).await.unwrap().as_ref(),
        Some(pdf)
    );
    assert!(attachment_for_js(db, i64::MAX).await.unwrap().is_none());

    // Searched, filtered by type, paged and bounded.
    let search = |s: &str| AttachmentQuery {
        search: Some(s.to_owned()),
        page: 1,
        ..AttachmentQuery::default()
    };
    let found = query_attachments(db, &editor, &search("sunset"))
        .await
        .unwrap();
    assert_eq!(found.items.len(), 1);
    let long = "x".repeat(10_000);
    assert!(
        query_attachments(db, &editor, &search(&long))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let images = AttachmentQuery {
        media_type: Some("image".to_owned()),
        page: 1,
        ..AttachmentQuery::default()
    };
    let images = query_attachments(db, &editor, &images).await.unwrap();
    assert_eq!(images.total, 1);
    let odd = AttachmentQuery {
        media_type: Some("image/%".to_owned()),
        page: 1,
        ..AttachmentQuery::default()
    };
    assert_eq!(query_attachments(db, &editor, &odd).await.unwrap().total, 3);
    let paged = AttachmentQuery {
        page: 2,
        per_page: 2,
        ..AttachmentQuery::default()
    };
    let second = query_attachments(db, &editor, &paged).await.unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!((second.total, second.total_pages), (3, 2));
    let huge = AttachmentQuery {
        page: i64::MAX,
        per_page: 1_000_000,
        ..AttachmentQuery::default()
    };
    let huge = query_attachments(db, &editor, &huge).await.unwrap();
    assert!(huge.items.is_empty());
    assert_eq!(huge.total_pages, 1);
}

#[tokio::test]
async fn uploads_need_upload_files_and_edit_post_on_the_parent() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let editor_id = add_user(db, "Edna").await;
    let author_id = add_user(db, "Abe").await;
    let subscriber_id = add_user(db, "Sue").await;
    let editor = caps(editor_id, EDITOR);
    let author = caps(author_id, AUTHOR);
    let (_temp, storage, sizes) = media("uploads");
    let jpeg = jpeg_bytes(200, 100);
    let upload = validate("Photo.jpg", &jpeg, MAX).unwrap();

    let refused = upload_to_library(
        db,
        &storage,
        &sizes,
        &caps(subscriber_id, SUBSCRIBER),
        &upload,
        None,
        &default_caps,
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, UploadRefusal::NotAllowed), "{refused:?}");
    assert_eq!(refused.status(), 403);

    // An author may upload, but not attach to an editor's post.
    let editors_post = post(db, editor_id, "post", PostStatus::Publish).await;
    let own = upload_to_library(db, &storage, &sizes, &author, &upload, None, &default_caps)
        .await
        .expect("an author uploads");
    assert_eq!(own.author_id, author_id);
    let refused = upload_to_library(
        db,
        &storage,
        &sizes,
        &author,
        &upload,
        Some(editors_post.id),
        &default_caps,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, UploadRefusal::CannotAttach),
        "{refused:?}"
    );
    let refused = upload_to_library(
        db,
        &storage,
        &sizes,
        &editor,
        &upload,
        Some(i64::MAX),
        &default_caps,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, UploadRefusal::CannotAttach),
        "{refused:?}"
    );

    // An editor attaches to the post; not to an attachment.
    let attached = upload_to_library(
        db,
        &storage,
        &sizes,
        &editor,
        &upload,
        Some(editors_post.id),
        &default_caps,
    )
    .await
    .expect("attaches to the post");
    assert_eq!(attached.parent_id, Some(editors_post.id));
    assert_eq!(attached.title, "Photo");
    let refused = upload_to_library(
        db,
        &storage,
        &sizes,
        &editor,
        &upload,
        Some(attached.id),
        &default_caps,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, UploadRefusal::InvalidParent),
        "{refused:?}"
    );

    // The type's capabilities come from the caller: a type only administrators edit.
    let book = post(db, editor_id, "book", PostStatus::Publish).await;
    let admins_only = |post_type: &str| {
        if post_type == "book" {
            PostTypeCaps::for_capability_type("manage_option")
        } else {
            default_caps(post_type)
        }
    };
    let refused = upload_to_library(
        db,
        &storage,
        &sizes,
        &editor,
        &upload,
        Some(book.id),
        &admins_only,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, UploadRefusal::CannotAttach),
        "{refused:?}"
    );

    // The attachment type's screens need the posts' capabilities.
    assert_eq!(
        attachment_post_type().caps().edit_others_posts,
        "edit_others_posts"
    );
}

#[tokio::test]
async fn ignite_media_fields_find_attachments_only() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let editor_id = add_user(db, "Edna").await;
    let editor = caps(editor_id, EDITOR);
    let (_temp, storage, sizes) = media("library");
    let jpeg = jpeg_bytes(200, 100);
    let upload = validate("Field Photo.jpg", &jpeg, MAX).unwrap();
    let photo = upload_to_library(db, &storage, &sizes, &editor, &upload, None, &default_caps)
        .await
        .unwrap();
    let page = post(db, editor_id, "page", PostStatus::Publish).await;
    let trashed = post(db, editor_id, "attachment", PostStatus::Trash).await;

    let library = Attachments::new(Some("/mc-admin/attachment".to_owned()));
    assert_eq!(
        library.browse_url().as_deref(),
        Some("/mc-admin/attachment")
    );
    let found = library
        .find(db, &[photo.id, page.id, trashed.id, i64::MAX, -1, photo.id])
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, photo.id);
    assert_eq!(found[0].title, "Field Photo");
    assert_eq!(
        found[0].url,
        format!("https://example.test/mc-content/uploads/{}", photo.file)
    );
    assert!(library.find(db, &[]).await.unwrap().is_empty());
    assert!(Attachments::default().browse_url().is_none());
}
