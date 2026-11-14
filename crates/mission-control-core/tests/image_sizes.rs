//! Image sizes: the registry, WordPress's resize arithmetic, and sub-sizes made on upload.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, ImageReader, Rgb, RgbImage, Rgba, RgbaImage};
use mission_control_core::hooks::Owner;
use mission_control_core::media::{
    ImageSize, ImageSizes, LocalStorage, Storage, constrain_dimensions, create_image_subsizes,
    image_resize_dimensions,
};

/// The system allocator, recording the largest single request, so the
/// decompression-bomb test can tell a refused image from a lazily zeroed
/// gigabyte buffer.
struct Largest;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Largest = Largest;

/// A fresh temp folder, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mc-image-sizes-{name}-{}", std::process::id()));
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

/// A gradient, so resized copies are not trivially compressible.
fn rgb(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    })
}

fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&rgb(width, height))
        .unwrap();
    out
}

fn encoded(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// The dimensions of the image stored at `path`.
fn dimensions(storage: &LocalStorage, path: &str) -> (u32, u32) {
    let bytes = storage.get(path).unwrap();
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .unwrap()
        .into_dimensions()
        .unwrap()
}

fn format_of(storage: &LocalStorage, path: &str) -> ImageFormat {
    let bytes = storage.get(path).unwrap();
    image::guess_format(&bytes).unwrap()
}

// ---- the registry ----

#[test]
fn the_registry_starts_with_wordpress_default_sizes() {
    let sizes = ImageSizes::new();
    let names: Vec<String> = sizes.registered().into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "thumbnail",
            "medium",
            "medium_large",
            "large",
            "1536x1536",
            "2048x2048"
        ]
    );
    assert_eq!(
        sizes.get("thumbnail"),
        Some(ImageSize {
            width: 150,
            height: 150,
            crop: true
        })
    );
    assert_eq!(
        sizes.get("medium"),
        Some(ImageSize {
            width: 300,
            height: 300,
            crop: false
        })
    );
    assert_eq!(
        sizes.get("medium_large"),
        Some(ImageSize {
            width: 768,
            height: 0,
            crop: false
        })
    );
    assert_eq!(
        sizes.get("large"),
        Some(ImageSize {
            width: 1024,
            height: 1024,
            crop: false
        })
    );
    // WordPress 5.3 and later also register two larger sizes for srcset.
    assert_eq!(
        sizes.get("1536x1536"),
        Some(ImageSize {
            width: 1536,
            height: 1536,
            crop: false
        })
    );
    assert_eq!(
        sizes.get("2048x2048"),
        Some(ImageSize {
            width: 2048,
            height: 2048,
            crop: false
        })
    );
    assert!(ImageSizes::empty().registered().is_empty());
}

#[test]
fn add_image_size_replaces_in_place_and_remove_owner_drops_a_theme_sizes() {
    let sizes = ImageSizes::new();
    let theme = Owner::Theme("aurora".into());
    sizes.add_image_size("hero", 1200, 400, true, theme.clone());
    sizes.add_image_size("card", 400, 0, false, theme.clone());
    // Settings -> Media changes a default size: same name, same position.
    sizes.add_image_size("thumbnail", 100, 100, true, Owner::Core);
    assert!(sizes.has_image_size("hero"));
    let names: Vec<String> = sizes.registered().into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "thumbnail",
            "medium",
            "medium_large",
            "large",
            "1536x1536",
            "2048x2048",
            "hero",
            "card"
        ]
    );
    assert_eq!(sizes.get("thumbnail").unwrap().width, 100);

    assert!(sizes.remove_image_size("card"));
    assert!(!sizes.remove_image_size("card"));
    sizes.remove_owner(&theme);
    assert!(!sizes.has_image_size("hero"));
    assert!(sizes.has_image_size("medium"));
}

// ---- WordPress's arithmetic ----

#[test]
fn constrain_dimensions_matches_wordpress() {
    assert_eq!(constrain_dimensions(1600, 1200, 300, 300), (300, 225));
    assert_eq!(constrain_dimensions(1200, 1600, 300, 300), (225, 300));
    assert_eq!(constrain_dimensions(1600, 1200, 768, 0), (768, 576));
    assert_eq!(constrain_dimensions(200, 100, 300, 300), (200, 100));
    // WordPress's own example: 465x700 in a 177x177 box is 118x177, not 117x176.
    assert_eq!(constrain_dimensions(465, 700, 177, 177), (118, 177));
}

#[test]
fn resize_dimensions_matches_wordpress() {
    // Hard crop: the exact box, from a centred source rectangle.
    let d = image_resize_dimensions(1600, 1200, 150, 150, true).unwrap();
    assert_eq!((d.width, d.height), (150, 150));
    assert_eq!(
        (d.src_x, d.src_y, d.src_width, d.src_height),
        (200, 0, 1200, 1200)
    );
    // Fit keeps proportions.
    let d = image_resize_dimensions(1600, 1200, 1024, 1024, false).unwrap();
    assert_eq!((d.width, d.height), (1024, 768));
    // Never upscale: a size as large as the original is no size.
    assert_eq!(image_resize_dimensions(100, 80, 300, 300, false), None);
    assert_eq!(image_resize_dimensions(100, 80, 150, 150, true), None);
    // Within a pixel of the original counts as the original.
    assert_eq!(image_resize_dimensions(301, 200, 300, 300, false), None);
    // Both unconstrained: nothing to do.
    assert_eq!(image_resize_dimensions(1600, 1200, 0, 0, false), None);
    // A crop box larger than the original in one side is cut to the original.
    let d = image_resize_dimensions(100, 400, 150, 150, true).unwrap();
    assert_eq!((d.width, d.height), (100, 150));
    // A sliver never gives an empty source rectangle.
    let d = image_resize_dimensions(1, 10_000, 0, 1, true).unwrap();
    assert!(d.src_width >= 1 && d.src_height >= 1, "{d:?}");
}

// ---- generation ----

#[test]
fn a_jpeg_upload_creates_each_registered_size_with_proportions_kept() {
    let (_temp, storage) = setup("jpeg");
    storage
        .put("2026/11/photo.jpg", &jpeg_bytes(1600, 1200))
        .unwrap();

    let meta = create_image_subsizes(&storage, "2026/11/photo.jpg", &ImageSizes::new())
        .unwrap()
        .expect("a JPEG is an image");
    assert_eq!((meta.width, meta.height), (1600, 1200));
    assert_eq!(meta.file, "2026/11/photo.jpg");
    assert_eq!(
        meta.filesize,
        storage.get("2026/11/photo.jpg").unwrap().len() as u64
    );

    let expected = [
        ("thumbnail", 150, 150, "photo-150x150.jpg"),
        ("medium", 300, 225, "photo-300x225.jpg"),
        ("medium_large", 768, 576, "photo-768x576.jpg"),
        ("large", 1024, 768, "photo-1024x768.jpg"),
        // 2048x2048 is not made: the image is not larger than it.
        ("1536x1536", 1536, 1152, "photo-1536x1152.jpg"),
    ];
    assert_eq!(meta.sizes.len(), expected.len());
    for (name, w, h, file) in expected {
        let size = &meta.sizes[name];
        assert_eq!((size.width, size.height), (w, h), "{name}");
        assert_eq!(size.file, file);
        assert_eq!(size.mime_type, "image/jpeg");
        let path = format!("2026/11/{file}");
        assert_eq!(dimensions(&storage, &path), (w, h), "{name} on disk");
        assert_eq!(format_of(&storage, &path), ImageFormat::Jpeg);
        assert_eq!(size.filesize, storage.get(&path).unwrap().len() as u64);
    }
}

#[test]
fn the_metadata_serialises_like_wp_attachment_metadata() {
    let (_temp, storage) = setup("json");
    storage.put("2026/11/a.jpg", &jpeg_bytes(400, 200)).unwrap();
    let meta = create_image_subsizes(&storage, "2026/11/a.jpg", &ImageSizes::new())
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(&meta).unwrap();
    assert_eq!(json["width"], 400);
    assert_eq!(json["height"], 200);
    assert_eq!(json["file"], "2026/11/a.jpg");
    assert_eq!(json["sizes"]["thumbnail"]["file"], "a-150x150.jpg");
    assert_eq!(json["sizes"]["thumbnail"]["mime-type"], "image/jpeg");
    assert_eq!(json["sizes"]["medium"]["width"], 300);
    assert_eq!(json["sizes"]["medium"]["height"], 150);
    assert!(json["sizes"]["medium"]["filesize"].as_u64().unwrap() > 0);
    let back: mission_control_core::media::AttachmentMetadata =
        serde_json::from_value(json).unwrap();
    assert_eq!(back, meta);
}

#[test]
fn a_small_image_creates_no_larger_sizes() {
    let (_temp, storage) = setup("small");
    storage
        .put("2026/11/tiny.jpg", &jpeg_bytes(120, 90))
        .unwrap();
    let meta = create_image_subsizes(&storage, "2026/11/tiny.jpg", &ImageSizes::new())
        .unwrap()
        .unwrap();
    assert_eq!((meta.width, meta.height), (120, 90));
    assert!(meta.sizes.is_empty(), "{:?}", meta.sizes);
    assert_eq!(
        fs::read_dir(storage.root().join("2026/11"))
            .unwrap()
            .count(),
        1,
        "only the original"
    );
}

#[test]
fn a_mid_sized_image_gets_only_the_sizes_it_is_larger_than() {
    let (_temp, storage) = setup("mid");
    storage
        .put("2026/11/mid.jpg", &jpeg_bytes(800, 600))
        .unwrap();
    let meta = create_image_subsizes(&storage, "2026/11/mid.jpg", &ImageSizes::new())
        .unwrap()
        .unwrap();
    let names: Vec<&str> = meta.sizes.keys().map(String::as_str).collect();
    assert_eq!(names, ["medium", "medium_large", "thumbnail"]);
    assert_eq!(
        (
            meta.sizes["medium_large"].width,
            meta.sizes["medium_large"].height
        ),
        (768, 576)
    );
}

#[test]
fn hard_crop_gives_the_exact_box_from_the_centre() {
    let (_temp, storage) = setup("crop");
    // Left half red, middle green, right half blue: a centre crop is all green.
    let image = RgbImage::from_fn(900, 300, |x, _| match x {
        0..300 => Rgb([255, 0, 0]),
        300..600 => Rgb([0, 255, 0]),
        _ => Rgb([0, 0, 255]),
    });
    let png = encoded(DynamicImage::ImageRgb8(image), ImageFormat::Png);
    storage.put("2026/11/bands.png", &png).unwrap();

    let sizes = ImageSizes::empty();
    sizes.add_image_size("square", 200, 200, true, Owner::Core);
    let meta = create_image_subsizes(&storage, "2026/11/bands.png", &sizes)
        .unwrap()
        .unwrap();
    let square = &meta.sizes["square"];
    assert_eq!((square.width, square.height), (200, 200));
    assert_eq!(square.file, "bands-200x200.png");
    assert_eq!(square.mime_type, "image/png");

    let bytes = storage.get("2026/11/bands-200x200.png").unwrap();
    let out = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert_eq!(out.dimensions(), (200, 200));
    for (x, y) in [(5, 5), (100, 100), (194, 194)] {
        let px = out.get_pixel(x, y);
        assert!(px[1] > 200 && px[0] < 40 && px[2] < 40, "{x},{y}: {px:?}");
    }
}

#[test]
fn a_theme_registered_size_is_generated() {
    let (_temp, storage) = setup("theme");
    storage
        .put("2026/11/wide.jpg", &jpeg_bytes(2000, 1000))
        .unwrap();
    let sizes = ImageSizes::new();
    sizes.add_image_size("hero", 1200, 400, true, Owner::Theme("aurora".into()));
    sizes.add_image_size("card", 500, 0, false, Owner::Theme("aurora".into()));
    let meta = create_image_subsizes(&storage, "2026/11/wide.jpg", &sizes)
        .unwrap()
        .unwrap();
    assert_eq!(meta.sizes["hero"].file, "wide-1200x400.jpg");
    assert_eq!(
        dimensions(&storage, "2026/11/wide-1200x400.jpg"),
        (1200, 400)
    );
    assert_eq!(meta.sizes["card"].file, "wide-500x250.jpg");
    assert_eq!(dimensions(&storage, "2026/11/wide-500x250.jpg"), (500, 250));
}

#[test]
fn png_gif_and_webp_keep_their_format() {
    let (_temp, storage) = setup("formats");
    let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(400, 400, |x, y| {
        Rgba([x as u8, y as u8, 0, if x < 200 { 255 } else { 128 }])
    }));
    let sizes = ImageSizes::empty();
    sizes.add_image_size("small", 100, 100, false, Owner::Core);
    for (ext, format, mime) in [
        ("png", ImageFormat::Png, "image/png"),
        ("gif", ImageFormat::Gif, "image/gif"),
        ("webp", ImageFormat::WebP, "image/webp"),
    ] {
        let path = format!("2026/11/pic.{ext}");
        storage.put(&path, &encoded(image.clone(), format)).unwrap();
        let meta = create_image_subsizes(&storage, &path, &sizes)
            .unwrap()
            .unwrap_or_else(|| panic!("{ext} is an image"));
        let small = &meta.sizes["small"];
        assert_eq!(small.file, format!("pic-100x100.{ext}"));
        assert_eq!(small.mime_type, mime);
        let out = format!("2026/11/pic-100x100.{ext}");
        assert_eq!(format_of(&storage, &out), format, "{ext}");
        assert_eq!(dimensions(&storage, &out), (100, 100));
    }
}

#[test]
fn an_existing_file_is_never_overwritten() {
    let (_temp, storage) = setup("existing");
    storage
        .put("2026/11/photo.jpg", &jpeg_bytes(1600, 1200))
        .unwrap();
    storage
        .put("2026/11/photo-150x150.jpg", b"someone else's")
        .unwrap();
    let meta = create_image_subsizes(&storage, "2026/11/photo.jpg", &ImageSizes::new())
        .unwrap()
        .unwrap();
    assert_eq!(
        storage.get("2026/11/photo-150x150.jpg").unwrap(),
        b"someone else's"
    );
    assert!(!meta.sizes.contains_key("thumbnail"));
    assert!(meta.sizes.contains_key("medium"));
}

#[test]
fn two_sizes_with_the_same_dimensions_share_one_file() {
    let (_temp, storage) = setup("shared");
    storage
        .put("2026/11/photo.jpg", &jpeg_bytes(1600, 1200))
        .unwrap();
    let sizes = ImageSizes::empty();
    sizes.add_image_size("a", 300, 300, false, Owner::Core);
    sizes.add_image_size("b", 300, 0, false, Owner::Core);
    let meta = create_image_subsizes(&storage, "2026/11/photo.jpg", &sizes)
        .unwrap()
        .unwrap();
    assert_eq!(meta.sizes["a"], meta.sizes["b"]);
    assert_eq!(meta.sizes["a"].file, "photo-300x225.jpg");
}

#[test]
fn files_that_are_not_images_get_no_sizes_and_no_error() {
    let (_temp, storage) = setup("not-images");
    let pdf = b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";
    storage.put("2026/11/doc.pdf", pdf).unwrap();
    storage.put("2026/11/notes.txt", b"hello").unwrap();
    // Says it is a JPEG, but is cut short after the header.
    storage
        .put("2026/11/broken.jpg", &jpeg_bytes(400, 400)[..40])
        .unwrap();
    for path in ["2026/11/doc.pdf", "2026/11/notes.txt", "2026/11/broken.jpg"] {
        assert_eq!(
            create_image_subsizes(&storage, path, &ImageSizes::new()).unwrap(),
            None,
            "{path}"
        );
    }
    assert_eq!(
        fs::read_dir(storage.root().join("2026/11"))
            .unwrap()
            .count(),
        3
    );
}

#[test]
fn a_missing_original_is_a_storage_error() {
    let (_temp, storage) = setup("missing");
    assert!(create_image_subsizes(&storage, "2026/11/nope.jpg", &ImageSizes::new()).is_err());
}

/// CRC-32 (IEEE), for hand-built PNG chunks.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A PNG that declares `width`x`height` RGBA pixels and carries one tiny
/// compressed row: the shape of a decompression bomb.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    // An empty zlib stream (stored block): the decoder must refuse before reading pixels.
    chunk(
        b"IDAT",
        &[0x78, 0x01, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0, 0, 0, 1],
    );
    chunk(b"IEND", &[]);
    out
}

#[test]
fn a_decompression_bomb_is_skipped_without_a_huge_allocation() {
    let (_temp, storage) = setup("bomb");
    // Past the width and height caps.
    storage
        .put("2026/11/huge.png", &png_header(100_000, 100_000))
        .unwrap();
    // Within the caps, but 16000 x 16000 RGBA is about 1 GB: past the allocation cap.
    storage
        .put("2026/11/big.png", &png_header(16_000, 16_000))
        .unwrap();
    let start = Instant::now();
    for path in ["2026/11/huge.png", "2026/11/big.png"] {
        assert_eq!(
            create_image_subsizes(&storage, path, &ImageSizes::new()).unwrap(),
            None,
            "{path}"
        );
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "refused before decoding"
    );
    // No other test here allocates anywhere near this much at once.
    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(
        largest < 64 * 1024 * 1024,
        "largest allocation {largest} bytes"
    );
}

#[test]
fn exif_orientation_is_applied() {
    let (_temp, storage) = setup("exif");
    // A 400x200 JPEG with an EXIF APP1 saying "rotate 90 degrees clockwise" (orientation 6).
    let plain = jpeg_bytes(400, 200);
    let tiff: &[u8] = &[
        b'M', b'M', 0, 42, 0, 0, 0, 8, // big-endian TIFF header, IFD at 8
        0, 1, // one entry
        0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, // Orientation, SHORT, 1, value 6
        0, 0, 0, 0, // no next IFD
    ];
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(tiff);
    let mut bytes = plain[..2].to_vec(); // SOI
    bytes.extend_from_slice(&[0xFF, 0xE1]);
    bytes.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    bytes.extend_from_slice(&app1);
    bytes.extend_from_slice(&plain[2..]);
    storage.put("2026/11/turned.jpg", &bytes).unwrap();

    let sizes = ImageSizes::empty();
    sizes.add_image_size("medium", 100, 100, false, Owner::Core);
    let meta = create_image_subsizes(&storage, "2026/11/turned.jpg", &sizes)
        .unwrap()
        .unwrap();
    assert_eq!((meta.width, meta.height), (200, 400), "as displayed");
    assert_eq!(meta.sizes["medium"].file, "turned-50x100.jpg");
    assert_eq!(dimensions(&storage, "2026/11/turned-50x100.jpg"), (50, 100));
}
