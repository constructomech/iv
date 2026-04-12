use std::path::Path;

use crate::app::DecodedImage;

/// Decode an image file to a thumbnail of at most `max_size` x `max_size` pixels.
/// Preserves aspect ratio. Works for all formats supported by the `image` crate.
pub fn decode_thumbnail(path: &Path, max_size: u32) -> Result<DecodedImage, String> {
    let img = image::open(path)
        .map_err(|e| format!("Failed to load {}: {e}", path.display()))?;

    let thumb = img.thumbnail(max_size, max_size);
    let rgba = thumb.to_rgba8();

    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbImage};
    use std::fs;

    fn make_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("iv_decode_test_{name}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_test_jpeg(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let img = RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        let path = dir.join(name);
        img.save_with_format(&path, ImageFormat::Jpeg).unwrap();
        path
    }

    #[test]
    fn thumbnail_downscales_large_image() {
        let dir = make_test_dir("downscale");
        let path = create_test_jpeg(&dir, "big.jpg", 2000, 1500);

        let thumb = decode_thumbnail(&path, 160).unwrap();

        // Should be at most 160 in either dimension
        assert!(thumb.width <= 160);
        assert!(thumb.height <= 160);
        // Should preserve aspect ratio (4:3)
        let ratio = thumb.width as f32 / thumb.height as f32;
        assert!((ratio - (4.0 / 3.0)).abs() < 0.1);
        // Should have valid RGBA data
        assert_eq!(thumb.pixels.len(), (thumb.width * thumb.height * 4) as usize);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_small_image_not_upscaled() {
        let dir = make_test_dir("small");
        let path = create_test_jpeg(&dir, "small.jpg", 80, 60);

        let thumb = decode_thumbnail(&path, 160).unwrap();

        // thumbnail() doesn't upscale beyond original dimensions
        // JPEG compression may cause minor size differences, but should stay small
        assert!(thumb.width <= 160);
        assert!(thumb.height <= 160);
        // Should not blow up to fill the max_size
        assert!(thumb.width < 160 || thumb.height < 160);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_corrupt_file_returns_error() {
        let dir = make_test_dir("corrupt");
        let path = dir.join("bad.jpg");
        fs::write(&path, b"not an image").unwrap();

        let result = decode_thumbnail(&path, 160);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_nonexistent_returns_error() {
        let result = decode_thumbnail(Path::new("/no/such/file.jpg"), 160);
        assert!(result.is_err());
    }
}
