use std::io::Cursor;
use std::path::Path;

use crate::app::DecodedImage;

/// Timing data from the decode pipeline.
#[derive(Debug, Clone, Default)]
pub struct DecodeTimings {
    /// Time spent attempting EXIF thumbnail extraction.
    pub exif_ms: f64,
    /// Time spent on full decode + downscale (0 if EXIF succeeded).
    pub full_ms: f64,
}

/// Maximum bytes to read for an EXIF-only check (256KB covers all EXIF headers).
const EXIF_READ_SIZE: usize = 256 * 1024;

/// Read the EXIF Orientation tag from file bytes.
/// Returns 1 (normal) if no orientation is found.
/// Values 1-8 per EXIF spec:
///   1=normal, 2=flip-h, 3=rotate180, 4=flip-v,
///   5=transpose, 6=rotate90, 7=transverse, 8=rotate270
pub fn read_exif_orientation(data: &[u8]) -> u32 {
    let cursor = Cursor::new(data);
    let exif_reader = exif::Reader::new();
    if let Ok(exif) = exif_reader.read_from_container(&mut std::io::BufReader::new(cursor)) {
        if let Some(field) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
            if let Some(v) = field.value.get_uint(0) {
                if (1..=8).contains(&v) {
                    return v;
                }
            }
        }
    }
    1
}

/// Apply EXIF orientation transform to an image.
pub fn apply_orientation(img: image::DynamicImage, orientation: u32) -> image::DynamicImage {
    match orientation {
        1 => img,                     // Normal
        2 => img.fliph(),             // Mirror horizontal
        3 => img.rotate180(),         // Rotate 180
        4 => img.flipv(),             // Mirror vertical
        5 => img.rotate90().fliph(),  // Transpose
        6 => img.rotate90(),          // Rotate 90 CW
        7 => img.rotate270().fliph(), // Transverse
        8 => img.rotate270(),         // Rotate 270 CW
        _ => img,
    }
}

/// Tier 0: Try EXIF thumbnail extraction only.
/// Reads at most 256KB from disk — very fast, especially on network shares.
/// Returns Some(image) if an embedded JPEG thumbnail was found.
pub fn try_exif_only(path: &Path) -> (Option<DecodedImage>, DecodeTimings) {
    let mut timings = DecodeTimings::default();
    let start = std::time::Instant::now();

    let result = (|| -> Option<DecodedImage> {
        let mut file = std::fs::File::open(path).ok()?;
        let file_len = file.metadata().ok()?.len() as usize;
        let read_len = file_len.min(EXIF_READ_SIZE);

        let mut buf = vec![0u8; read_len];
        std::io::Read::read_exact(&mut file, &mut buf).ok()?;

        let orientation = read_exif_orientation(&buf);
        let mut decoded = extract_exif_thumbnail(&buf)?;

        // Apply orientation if needed
        if orientation != 1 {
            let img = image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.pixels)?;
            let oriented = apply_orientation(image::DynamicImage::ImageRgba8(img), orientation);
            let rgba = oriented.to_rgba8();
            decoded = DecodedImage {
                width: rgba.width(),
                height: rgba.height(),
                pixels: rgba.into_raw(),
            };
        }

        Some(decoded)
    })();

    timings.exif_ms = start.elapsed().as_secs_f64() * 1000.0;
    (result, timings)
}

/// Tier 1: Full image decode + downscale to thumbnail.
/// Reads the entire file. Only call after EXIF has been tried and failed.
pub fn decode_full_thumbnail(
    path: &Path,
    max_size: u32,
) -> Result<(DecodedImage, DecodeTimings), String> {
    let mut timings = DecodeTimings::default();
    let start = std::time::Instant::now();

    let data =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let orientation = read_exif_orientation(&data);
    let img = image::load_from_memory(&data).map_err(|e| format!("Failed to decode: {e}"))?;
    let img = apply_orientation(img, orientation);
    let thumb = img.thumbnail(max_size, max_size);
    let rgba = thumb.to_rgba8();

    timings.full_ms = start.elapsed().as_secs_f64() * 1000.0;

    Ok((
        DecodedImage {
            width: rgba.width(),
            height: rgba.height(),
            pixels: rgba.into_raw(),
        },
        timings,
    ))
}

/// Try to extract the EXIF embedded thumbnail from file bytes.
pub fn extract_exif_thumbnail(data: &[u8]) -> Option<DecodedImage> {
    let cursor = Cursor::new(data);
    let exif_reader = exif::Reader::new();
    let exif = exif_reader
        .read_from_container(&mut std::io::BufReader::new(cursor))
        .ok()?;

    for field in exif.fields() {
        if field.tag == exif::Tag::JPEGInterchangeFormat {
            if let (Some(offset_field), Some(length_field)) = (
                exif.get_field(exif::Tag::JPEGInterchangeFormat, field.ifd_num),
                exif.get_field(exif::Tag::JPEGInterchangeFormatLength, field.ifd_num),
            ) {
                if let (Some(offset), Some(length)) = (
                    offset_field.value.get_uint(0),
                    length_field.value.get_uint(0),
                ) {
                    return find_and_decode_exif_jpeg(data, offset, length);
                }
            }
        }
    }

    None
}

/// Search for and decode the embedded JPEG thumbnail in the file data.
fn find_and_decode_exif_jpeg(data: &[u8], offset: u32, length: u32) -> Option<DecodedImage> {
    let search_start = (offset as usize).saturating_sub(20);
    let search_end = ((offset + length) as usize + 100).min(data.len());

    for i in search_start..search_end.saturating_sub(1) {
        if data[i] == 0xFF && data[i + 1] == 0xD8 {
            let jpeg_start = i;
            let max_end = (jpeg_start + length as usize + 1000).min(data.len());
            // Look for EOI marker
            for j in (jpeg_start + 2)..max_end.saturating_sub(1) {
                if data[j] == 0xFF && data[j + 1] == 0xD9 {
                    return decode_jpeg_bytes(&data[jpeg_start..j + 2]);
                }
            }
            // No EOI found, use length hint
            let jpeg_end = (jpeg_start + length as usize).min(data.len());
            return decode_jpeg_bytes(&data[jpeg_start..jpeg_end]);
        }
    }

    None
}

/// Decode JPEG bytes into a DecodedImage using zune-jpeg.
fn decode_jpeg_bytes(data: &[u8]) -> Option<DecodedImage> {
    use zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let cursor = Cursor::new(data);
    let opts =
        DecoderOptions::default().jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(cursor, opts);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;

    Some(DecodedImage {
        width: info.width as u32,
        height: info.height as u32,
        pixels,
    })
}

/// Convenience for tests: decode a thumbnail from a path.
pub fn decode_thumbnail(path: &Path, max_size: u32) -> Result<DecodedImage, String> {
    let (img, _timings) = decode_full_thumbnail(path, max_size)?;
    Ok(img)
}

/// Convenience: progressive decode matching old API for tests/examples.
pub fn decode_thumbnail_progressive(
    path: &Path,
    max_size: u32,
) -> Result<(DecodedImage, bool, DecodeTimings), String> {
    let (exif_result, mut timings) = try_exif_only(path);

    if let Some(thumb) = exif_result {
        return Ok((thumb, true, timings));
    }

    let (decoded, full_timings) = decode_full_thumbnail(path, max_size)?;
    timings.full_ms = full_timings.full_ms;
    Ok((decoded, false, timings))
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
        let dir =
            std::env::temp_dir().join(format!("iv_decode_test_{name}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_test_jpeg(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let img = RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
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
        assert!(thumb.width <= 160, "width {} should be <= 160", thumb.width);
        assert!(
            thumb.height <= 160,
            "height {} should be <= 160",
            thumb.height
        );
        // Should have valid RGBA data
        assert_eq!(
            thumb.pixels.len(),
            (thumb.width * thumb.height * 4) as usize
        );

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

    #[test]
    fn progressive_falls_back_to_full_decode() {
        let dir = make_test_dir("progressive");
        // PNG files don't have EXIF thumbnails, so it should fall back
        let img = RgbImage::from_fn(400, 300, |_, _| image::Rgb([100, 150, 200]));
        let path = dir.join("test.png");
        img.save_with_format(&path, ImageFormat::Png).unwrap();

        let (thumb, is_exif, timings) = decode_thumbnail_progressive(&path, 160).unwrap();
        assert!(!is_exif, "PNG should not have EXIF thumbnail");
        assert!(thumb.width <= 160);
        assert!(thumb.height <= 160);
        assert!(timings.full_ms > 0.0, "full decode should have been timed");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn progressive_corrupt_returns_error() {
        let dir = make_test_dir("prog_corrupt");
        let path = dir.join("bad.jpg");
        fs::write(&path, b"garbage").unwrap();

        let result = decode_thumbnail_progressive(&path, 160);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exif_extract_from_jpeg_without_exif() {
        let dir = make_test_dir("no_exif");
        let path = create_test_jpeg(&dir, "no_exif.jpg", 200, 150);

        let data = fs::read(&path).unwrap();
        let result = extract_exif_thumbnail(&data);
        // Should return None — no EXIF data in our synthetic images
        assert!(result.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exif_extract_nonexistent_returns_none() {
        let result = extract_exif_thumbnail(&[]);
        assert!(result.is_none());
    }
}
