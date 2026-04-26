use std::path::Path;

// ---------------------------------------------------------------------------
// Pure (testable) image-loading logic — no GPU context needed
// ---------------------------------------------------------------------------

/// Decoded image data ready for GPU upload.
#[derive(Debug)]
pub struct DecodedImage {
    /// RGBA pixels, row-major.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Load an image file from disk and decode it to RGBA, applying EXIF orientation.
/// For HEIC/HEIF, libheif applies orientation internally, so we skip it.
/// For raw files (DNG, CR2, NEF, etc.), uses LibRaw for full-resolution demosaicing,
/// falling back to the embedded JPEG preview.
pub fn load_image(path: &Path) -> Result<DecodedImage, String> {
    load_image_with_develop(path, true)
}

pub(crate) fn load_image_with_develop(
    path: &Path,
    apply_develop: bool,
) -> Result<DecodedImage, String> {
    let data =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

    // Raw files: full-resolution decode via LibRaw (demosaic + white balance).
    // Falls back to the largest embedded JPEG preview if LibRaw fails.
    if crate::decode::is_raw_extension(path) {
        if let Some(decoded) = crate::decode::decode_raw_libraw(&data) {
            return Ok(apply_develop_settings(decoded, path, &data, apply_develop));
        }
        if let Some(decoded) = crate::decode::load_raw_preview(&data) {
            return Ok(apply_develop_settings(decoded, path, &data, apply_develop));
        }
    }

    load_image_standard(&data, path)
        .map(|decoded| apply_develop_settings(decoded, path, &data, apply_develop))
}

/// Fast preview for raw files: extract the embedded JPEG preview (~8ms)
/// without doing full demosaicing. Used for progressive loading.
pub fn load_raw_preview_image(path: &Path) -> Result<DecodedImage, String> {
    load_raw_preview_image_with_develop(path, true)
}

pub(crate) fn load_raw_preview_image_with_develop(
    path: &Path,
    apply_develop: bool,
) -> Result<DecodedImage, String> {
    let data =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    if let Some(decoded) = crate::decode::load_raw_preview(&data) {
        return Ok(apply_develop_settings(decoded, path, &data, apply_develop));
    }
    load_image_standard(&data, path)
        .map(|decoded| apply_develop_settings(decoded, path, &data, apply_develop))
}

/// Full-resolution raw decode via LibRaw. Returns None for non-raw formats.
pub fn load_raw_full(path: &Path) -> Option<DecodedImage> {
    load_raw_full_with_develop(path, true)
}

pub(crate) fn load_raw_full_with_develop(path: &Path, apply_develop: bool) -> Option<DecodedImage> {
    let data = std::fs::read(path).ok()?;
    crate::decode::decode_raw_libraw(&data)
        .map(|decoded| apply_develop_settings(decoded, path, &data, apply_develop))
}

fn apply_develop_settings(
    mut decoded: DecodedImage,
    path: &Path,
    data: &[u8],
    apply_develop: bool,
) -> DecodedImage {
    if !apply_develop {
        return decoded;
    }
    let settings = crate::develop::read_xmp_develop_settings_for_image(path, Some(data));
    crate::develop::apply_xmp_develop_settings(&mut decoded, &settings);
    decoded
}

/// Standard image decode path (non-raw or raw fallback).
fn load_image_standard(data: &[u8], path: &Path) -> Result<DecodedImage, String> {
    let img = image::load_from_memory(data)
        .map_err(|e| format!("Failed to decode {}: {e}", path.display()))?;
    // libheif applies orientation during decode for HEIC/HEIF,
    // so only apply manual orientation for other formats.
    let img = if !crate::decode::is_heif_extension(path) {
        let orientation = crate::decode::read_exif_orientation(data);
        crate::decode::apply_orientation(img, orientation)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

/// Compute the display size that fits `image_size` into `available_size`,
/// preserving aspect ratio, without upscaling beyond 1:1.
/// Returns `(width, height)`.
pub fn fit_size(image_size: (f32, f32), available_size: (f32, f32)) -> (f32, f32) {
    let (iw, ih) = image_size;
    let (aw, ah) = available_size;
    if iw <= 0.0 || ih <= 0.0 || aw <= 0.0 || ah <= 0.0 {
        return (0.0, 0.0);
    }
    let scale = (aw / iw).min(ah / ih).min(1.0);
    (iw * scale, ih * scale)
}

/// Recognized image file extensions (lowercase, without dot).
pub const IMAGE_EXTENSIONS: &[&str] = &[
    // Common raster
    "jpg", "jpeg", "png", "webp", "tiff", "tif", "bmp", "gif", // HEIF/HEIC
    "heic", "heif", // RAW (first-class — we extract embedded JPEG previews)
    "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf",
];

/// Check whether a path has a recognized image extension.
pub fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_size_smaller_image_no_upscale() {
        let (w, h) = fit_size((100.0, 100.0), (800.0, 600.0));
        assert!((w - 100.0).abs() < 0.01);
        assert!((h - 100.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_landscape_image() {
        let (w, h) = fit_size((1600.0, 1200.0), (800.0, 600.0));
        assert!((w - 800.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_portrait_image() {
        let (w, h) = fit_size((600.0, 800.0), (800.0, 600.0));
        assert!((w - 450.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_exact_match() {
        let (w, h) = fit_size((800.0, 600.0), (800.0, 600.0));
        assert!((w - 800.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_zero_viewport() {
        let (w, h) = fit_size((100.0, 100.0), (0.0, 0.0));
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn fit_size_zero_image() {
        let (w, h) = fit_size((0.0, 0.0), (800.0, 600.0));
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn is_image_file_jpeg() {
        assert!(is_image_file(Path::new("photo.jpg")));
        assert!(is_image_file(Path::new("photo.JPEG")));
        assert!(is_image_file(Path::new("photo.Jpg")));
    }

    #[test]
    fn is_image_file_negative() {
        assert!(!is_image_file(Path::new("readme.txt")));
        assert!(!is_image_file(Path::new("photo.xmp")));
        assert!(!is_image_file(Path::new("noext")));
    }

    #[test]
    fn is_image_file_other_formats() {
        assert!(is_image_file(Path::new("image.png")));
        assert!(is_image_file(Path::new("image.webp")));
        assert!(is_image_file(Path::new("image.tiff")));
        assert!(is_image_file(Path::new("image.bmp")));
        assert!(is_image_file(Path::new("image.gif")));
        assert!(is_image_file(Path::new("image.heic")));
    }

    #[test]
    fn is_image_file_raw() {
        assert!(is_image_file(Path::new("photo.dng")));
        assert!(is_image_file(Path::new("photo.CR2")));
        assert!(is_image_file(Path::new("photo.nef")));
        assert!(is_image_file(Path::new("photo.arw")));
    }

    #[test]
    fn load_image_nonexistent_file() {
        let result = load_image(Path::new("/nonexistent/file.jpg"));
        assert!(result.is_err());
    }

    #[test]
    fn load_image_not_an_image() {
        let dir = std::env::temp_dir().join(format!("iv_test_notimg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not_an_image.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let result = load_image(&path);
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_image_with_develop_can_disable_sidecar_edits() {
        let dir = std::env::temp_dir().join(format!("iv_test_develop_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.png");
        let image = image::RgbaImage::from_raw(1, 1, vec![96, 96, 96, 255]).unwrap();
        image.save(&path).unwrap();
        std::fs::write(
            path.with_extension("xmp"),
            r#"<rdf:RDF><rdf:Description crs:Exposure="1.00" /></rdf:RDF>"#,
        )
        .unwrap();

        let edited = load_image_with_develop(&path, true).unwrap();
        let unedited = load_image_with_develop(&path, false).unwrap();

        assert!(edited.pixels[0] > unedited.pixels[0]);
        assert_eq!(&unedited.pixels[..4], &[96, 96, 96, 255]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
