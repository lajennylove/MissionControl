//! Image sizes: the registered sizes, and the resized copies made of an
//! uploaded image.
//!
//! [`ImageSizes`] is the registry WordPress keeps in its `thumbnail_size_w`
//! (and similar) options plus `$_wp_additional_image_sizes`. It starts with
//! WordPress's defaults, and themes add their own with
//! [`ImageSizes::add_image_size`]. The default sizes are plain entries too:
//! the Settings → Media screen changes them by registering the same name
//! again, so this module never reads options.
//!
//! [`create_image_subsizes`] does what WordPress's `wp_create_image_subsizes`
//! does on upload: it decodes the stored original and writes one resized copy
//! per registered size, named `name-{width}x{height}.ext` beside it, and
//! returns metadata shaped like `_wp_attachment_metadata`. The sizes follow
//! `image_resize_dimensions`: an image is never made larger, `fit` sizes keep
//! the proportions, and hard-crop sizes are cut from the centre.
//!
//! Decoding is limited ([`MAX_IMAGE_DIMENSION`], [`MAX_DECODE_BYTES`]), so a
//! small file that declares a huge image is skipped before its pixels are
//! allocated. Decoding and resizing are CPU work: async callers run
//! [`create_image_subsizes`] in `spawn_blocking`.
//!
//! ```
//! use mission_control_core::hooks::Owner;
//! use mission_control_core::media::{ImageSizes, image_resize_dimensions};
//!
//! let sizes = ImageSizes::new();
//! sizes.add_image_size("hero", 1200, 400, true, Owner::Theme("aurora".into()));
//! assert!(sizes.has_image_size("thumbnail"));
//!
//! let d = image_resize_dimensions(1600, 1200, 300, 300, false).unwrap();
//! assert_eq!((d.width, d.height), (300, 225));
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Cursor;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};

use super::storage::{Storage, StorageError, StorageResult};
use crate::hooks::Owner;

/// The widest or tallest image decoded, in pixels. Larger images are stored
/// but get no sub-sizes.
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;

/// The most memory one decoded image may take, in bytes (512 MiB, the
/// `image` crate's own default). An image needing more gets no sub-sizes.
pub const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;

/// The JPEG quality sub-sizes are saved with: WordPress's default
/// (`wp_editor_set_quality`, 82).
pub const JPEG_QUALITY: u8 = 82;

/// One registered size. A `width` or `height` of 0 leaves that side
/// unconstrained, as in WordPress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageSize {
    /// The largest width, in pixels; 0 for any width.
    pub width: u32,
    /// The largest height, in pixels; 0 for any height.
    pub height: u32,
    /// `true` cuts the image to exactly this box (from the centre); `false`
    /// fits the image inside it, keeping its proportions.
    pub crop: bool,
}

struct Entry {
    name: String,
    size: ImageSize,
    owner: Owner,
}

/// The image size registry. Methods take `&self`, like [`crate::hooks::Hooks`]:
/// themes register sizes while the site serves.
pub struct ImageSizes {
    inner: RwLock<Vec<Entry>>,
}

impl fmt::Debug for ImageSizes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.read().iter().map(|e| (&e.name, &e.size)))
            .finish()
    }
}

impl Default for ImageSizes {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageSizes {
    /// A registry with WordPress's default sizes, owned by
    /// [`Owner::Core`]: `thumbnail` (150×150, cropped), `medium` (300×300),
    /// `medium_large` (768 wide), `large` (1024×1024), and the two sizes
    /// WordPress 5.3 and later add for `srcset`, `1536x1536` and `2048x2048`
    /// (fitted, not cropped).
    pub fn new() -> Self {
        let sizes = Self::empty();
        for (name, width, height, crop) in [
            ("thumbnail", 150, 150, true),
            ("medium", 300, 300, false),
            ("medium_large", 768, 0, false),
            ("large", 1024, 1024, false),
            ("1536x1536", 1536, 1536, false),
            ("2048x2048", 2048, 2048, false),
        ] {
            sizes.add_image_size(name, width, height, crop, Owner::Core);
        }
        sizes
    }

    /// A registry with no sizes.
    pub fn empty() -> Self {
        Self {
            inner: RwLock::new(Vec::new()),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Vec<Entry>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Vec<Entry>> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers a size (WordPress's `add_image_size`). Registering a name
    /// again replaces it in place, keeping its position. 0 leaves a side
    /// unconstrained.
    pub fn add_image_size(&self, name: &str, width: u32, height: u32, crop: bool, owner: Owner) {
        let size = ImageSize {
            width,
            height,
            crop,
        };
        let mut entries = self.write();
        match entries.iter_mut().find(|e| e.name == name) {
            Some(entry) => {
                entry.size = size;
                entry.owner = owner;
            }
            None => entries.push(Entry {
                name: name.to_owned(),
                size,
                owner,
            }),
        }
    }

    /// Removes a size (WordPress's `remove_image_size`). Returns whether it
    /// was registered.
    pub fn remove_image_size(&self, name: &str) -> bool {
        let mut entries = self.write();
        let before = entries.len();
        entries.retain(|e| e.name != name);
        entries.len() != before
    }

    /// Whether a size is registered (WordPress's `has_image_size`).
    pub fn has_image_size(&self, name: &str) -> bool {
        self.read().iter().any(|e| e.name == name)
    }

    /// The size registered under `name`.
    pub fn get(&self, name: &str) -> Option<ImageSize> {
        self.read().iter().find(|e| e.name == name).map(|e| e.size)
    }

    /// Every registered size, in registration order
    /// (WordPress's `wp_get_registered_image_subsizes`).
    pub fn registered(&self) -> Vec<(String, ImageSize)> {
        self.read()
            .iter()
            .map(|e| (e.name.clone(), e.size))
            .collect()
    }

    /// Removes every size `owner` registered, for when a theme or plugin is
    /// deactivated.
    pub fn remove_owner(&self, owner: &Owner) {
        self.write().retain(|e| &e.owner != owner);
    }
}

/// `width` and `height` scaled to fit inside `max_width` × `max_height`,
/// keeping the proportions (WordPress's `wp_constrain_dimensions`). A max of
/// 0 leaves that side free. Never larger than the input; a side one pixel
/// short of its max is rounded up to it, as WordPress does.
pub fn constrain_dimensions(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    if max_width == 0 && max_height == 0 {
        return (width, height);
    }
    let (w, h) = (f64::from(width), f64::from(height));
    let did_width = max_width > 0 && width > 0 && width > max_width;
    let did_height = max_height > 0 && height > 0 && height > max_height;
    let width_ratio = if did_width {
        f64::from(max_width) / w
    } else {
        1.0
    };
    let height_ratio = if did_height {
        f64::from(max_height) / h
    } else {
        1.0
    };
    let smaller = width_ratio.min(height_ratio);
    let larger = width_ratio.max(height_ratio);
    // PHP compares against a max of 0 too, so a free side always takes the smaller ratio.
    let ratio = if (w * larger).round() > f64::from(max_width)
        || (h * larger).round() > f64::from(max_height)
    {
        smaller
    } else {
        larger
    };
    let mut new_w = ((w * ratio).round() as u32).max(1);
    let mut new_h = ((h * ratio).round() as u32).max(1);
    if did_width && new_w + 1 == max_width {
        new_w = max_width;
    }
    if did_height && new_h + 1 == max_height {
        new_h = max_height;
    }
    (new_w, new_h)
}

/// Where a sub-size comes from in the original, and its final size: the
/// result of [`image_resize_dimensions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeDimensions {
    /// Left edge of the source rectangle.
    pub src_x: u32,
    /// Top edge of the source rectangle.
    pub src_y: u32,
    /// Width of the source rectangle.
    pub src_width: u32,
    /// Height of the source rectangle.
    pub src_height: u32,
    /// Width of the sub-size.
    pub width: u32,
    /// Height of the sub-size.
    pub height: u32,
}

/// The source rectangle and size for resizing an `orig_width` ×
/// `orig_height` image to a registered size (WordPress's
/// `image_resize_dimensions`, with crops centred). `None` when there is
/// nothing to make: both sides unconstrained, or the result within a pixel
/// of the original (so an image is never made larger).
pub fn image_resize_dimensions(
    orig_width: u32,
    orig_height: u32,
    dest_width: u32,
    dest_height: u32,
    crop: bool,
) -> Option<ResizeDimensions> {
    if orig_width == 0 || orig_height == 0 || (dest_width == 0 && dest_height == 0) {
        return None;
    }
    let dims = if crop {
        let (ow, oh) = (f64::from(orig_width), f64::from(orig_height));
        let aspect = ow / oh;
        let mut new_w = dest_width.min(orig_width);
        let mut new_h = dest_height.min(orig_height);
        if new_w == 0 {
            new_w = (f64::from(new_h) * aspect).round() as u32;
        }
        if new_h == 0 {
            new_h = (f64::from(new_w) / aspect).round() as u32;
        }
        let size_ratio = (f64::from(new_w) / ow).max(f64::from(new_h) / oh);
        let crop_w = ((f64::from(new_w) / size_ratio).round() as u32).clamp(1, orig_width);
        let crop_h = ((f64::from(new_h) / size_ratio).round() as u32).clamp(1, orig_height);
        ResizeDimensions {
            src_x: (orig_width - crop_w) / 2,
            src_y: (orig_height - crop_h) / 2,
            src_width: crop_w,
            src_height: crop_h,
            width: new_w.max(1),
            height: new_h.max(1),
        }
    } else {
        let (width, height) =
            constrain_dimensions(orig_width, orig_height, dest_width, dest_height);
        ResizeDimensions {
            src_x: 0,
            src_y: 0,
            src_width: orig_width,
            src_height: orig_height,
            width,
            height,
        }
    };
    // WordPress's `wp_fuzzy_number_match`: within one pixel is the same size.
    if dims.width.abs_diff(orig_width) <= 1 && dims.height.abs_diff(orig_height) <= 1 {
        return None;
    }
    Some(dims)
}

/// One generated sub-size, as stored under `sizes` in
/// `_wp_attachment_metadata`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubSize {
    /// The file name, without the folder (for example `photo-300x225.jpg`).
    pub file: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The MIME type, for example `image/jpeg`.
    #[serde(rename = "mime-type")]
    pub mime_type: String,
    /// The file's size in bytes.
    pub filesize: u64,
}

/// An image's metadata, shaped like WordPress's `_wp_attachment_metadata`
/// (without `image_meta`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMetadata {
    /// Width in pixels, as displayed (after the EXIF orientation).
    pub width: u32,
    /// Height in pixels, as displayed (after the EXIF orientation).
    pub height: u32,
    /// The original's storage path, for example `2026/11/photo.jpg`.
    pub file: String,
    /// The original's size in bytes.
    pub filesize: u64,
    /// The generated sub-sizes, by size name.
    pub sizes: BTreeMap<String, SubSize>,
}

/// The formats sub-sizes are made for: the ones WordPress resizes by
/// default, kept in the original's format.
fn supported(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP
    )
}

fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

/// Decodes `bytes` within the limits and turns it upright by its EXIF
/// orientation. `None` for anything that is not a supported image, or is
/// over the limits.
fn decode(bytes: &[u8], path: &str) -> Option<(DynamicImage, ImageFormat)> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format().filter(|f| supported(*f))?;
    reader.limits(decode_limits());
    let result = reader.into_decoder().and_then(|mut decoder| {
        // `into_decoder` checks the dimensions but not the pixel buffer
        // `from_decoder` allocates (only `ImageReader::decode` does, and it
        // ignores the orientation), so reserve it against the cap here.
        decode_limits().reserve(decoder.total_bytes())?;
        let orientation = decoder.orientation()?;
        let mut image = DynamicImage::from_decoder(decoder)?;
        image.apply_orientation(orientation);
        Ok(image)
    });
    match result {
        Ok(image) => Some((image, format)),
        Err(err) => {
            log::warn!("no image sizes for {path:?}: {err}");
            None
        }
    }
}

/// Converts to a pixel type the format's encoder accepts.
fn encodable(image: DynamicImage, format: ImageFormat) -> DynamicImage {
    let color = image.color();
    match format {
        ImageFormat::Jpeg => match color {
            ColorType::L8 | ColorType::Rgb8 => image,
            _ => DynamicImage::ImageRgb8(image.to_rgb8()),
        },
        ImageFormat::Gif => match color {
            ColorType::Rgba8 => image,
            _ => DynamicImage::ImageRgba8(image.to_rgba8()),
        },
        ImageFormat::WebP => match color {
            ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8 => image,
            _ if color.has_alpha() => DynamicImage::ImageRgba8(image.to_rgba8()),
            _ => DynamicImage::ImageRgb8(image.to_rgb8()),
        },
        _ => match color {
            ColorType::Rgb32F | ColorType::Rgba32F => DynamicImage::ImageRgba16(image.to_rgba16()),
            _ => image,
        },
    }
}

fn encode(image: DynamicImage, format: ImageFormat) -> image::ImageResult<Vec<u8>> {
    let image = encodable(image, format);
    let mut out = Cursor::new(Vec::new());
    if format == ImageFormat::Jpeg {
        image.write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))?;
    } else {
        image.write_to(&mut out, format)?;
    }
    Ok(out.into_inner())
}

/// Makes a resized copy of the stored image at `path` for each size in
/// `sizes`, and returns the image's metadata (WordPress's
/// `wp_create_image_subsizes`).
///
/// - Sizes the image is not larger than are skipped ([`image_resize_dimensions`]).
/// - Each copy is saved beside the original as `name-{w}x{h}.ext`, in the
///   original's format (JPEG at quality [`JPEG_QUALITY`]), through
///   [`Storage::put_new`]: an existing file is never replaced; that size is
///   left out and a warning is logged. Sizes with the same dimensions share
///   one file.
/// - The EXIF orientation is applied, so copies are upright and the
///   metadata gives the displayed width and height.
///
/// Returns `Ok(None)` for a file that is not a JPEG, PNG, GIF or WebP image,
/// one that does not decode, or one over [`MAX_IMAGE_DIMENSION`] or
/// [`MAX_DECODE_BYTES`]. Fails only when storage does (the original cannot
/// be read, or a copy cannot be written).
pub fn create_image_subsizes(
    storage: &dyn Storage,
    path: &str,
    sizes: &ImageSizes,
) -> StorageResult<Option<AttachmentMetadata>> {
    let bytes = storage.get(path)?;
    let Some((image, format)) = decode(&bytes, path) else {
        return Ok(None);
    };
    let (orig_width, orig_height) = (image.width(), image.height());
    let (folder, file_name) = match path.rsplit_once('/') {
        Some((folder, name)) => (Some(folder), name),
        None => (None, path),
    };
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (file_name, None),
    };
    let mime_type = format.to_mime_type();

    let mut made: HashMap<(u32, u32), SubSize> = HashMap::new();
    let mut out = BTreeMap::new();
    for (name, size) in sizes.registered() {
        let Some(dims) =
            image_resize_dimensions(orig_width, orig_height, size.width, size.height, size.crop)
        else {
            continue;
        };
        let key = (dims.width, dims.height);
        if let Some(sub) = made.get(&key) {
            out.insert(name, sub.clone());
            continue;
        }
        let sub_name = match extension {
            Some(ext) => format!("{stem}-{}x{}.{ext}", dims.width, dims.height),
            None => format!("{stem}-{}x{}", dims.width, dims.height),
        };
        let sub_path = match folder {
            Some(folder) => format!("{folder}/{sub_name}"),
            None => sub_name.clone(),
        };
        let resized = if size.crop {
            image
                .crop_imm(dims.src_x, dims.src_y, dims.src_width, dims.src_height)
                .resize_exact(dims.width, dims.height, FilterType::Lanczos3)
        } else {
            image.resize_exact(dims.width, dims.height, FilterType::Lanczos3)
        };
        let encoded = match encode(resized, format) {
            Ok(encoded) => encoded,
            Err(err) => {
                log::warn!("image size {name:?} for {path:?} not made: {err}");
                continue;
            }
        };
        match storage.put_new(&sub_path, &encoded) {
            Ok(()) => {}
            Err(StorageError::AlreadyExists(_)) => {
                log::warn!("image size {name:?} for {path:?} not made: {sub_path:?} exists");
                continue;
            }
            Err(err) => return Err(err),
        }
        let sub = SubSize {
            file: sub_name,
            width: dims.width,
            height: dims.height,
            mime_type: mime_type.to_owned(),
            filesize: encoded.len() as u64,
        };
        made.insert(key, sub.clone());
        out.insert(name, sub);
    }

    Ok(Some(AttachmentMetadata {
        width: orig_width,
        height: orig_height,
        file: path.to_owned(),
        filesize: bytes.len() as u64,
        sizes: out,
    }))
}
