use std::io::{BufReader, Read, Seek};
use std::path::Path;

use crate::app::DecodedImage;

/// Timing data from the decode pipeline.
#[derive(Debug, Clone, Default)]
pub struct DecodeTimings {
    /// Time spent attempting EXIF thumbnail extraction (always measured).
    pub exif_ms: f64,
    /// Time spent on full decode + downscale (0 if EXIF succeeded).
    pub full_ms: f64,
}

/// Try to extract the EXIF embedded thumbnail from an image file.
/// This reads only the header (~64KB) and is very fast.
/// Returns None if no embedded thumbnail is found.
pub fn extract_exif_thumbnail(path: &Path) -> Option<DecodedImage> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);

    let exif_reader = exif::Reader::new();
    let exif = exif_reader.read_from_container(&mut reader).ok()?;

    // Look for JPEG thumbnail data in the EXIF
    for field in exif.fields() {
        if field.tag == exif::Tag::JPEGInterchangeFormat {
            // There's a thumbnail — re-read the file to get the raw bytes
            if let (Some(offset_field), Some(length_field)) = (
                exif.get_field(exif::Tag::JPEGInterchangeFormat, field.ifd_num),
                exif.get_field(exif::Tag::JPEGInterchangeFormatLength, field.ifd_num),
            ) {
                if let (Some(offset), Some(length)) = (
                    offset_field.value.get_uint(0),
                    length_field.value.get_uint(0),
                ) {
                    return decode_exif_jpeg_thumbnail(path, offset, length);
                }
            }
        }
    }

    None
}

/// Read and decode the JPEG thumbnail embedded at the given offset in the file.
fn decode_exif_jpeg_thumbnail(path: &Path, offset: u32, length: u32) -> Option<DecodedImage> {
    let mut file = std::fs::File::open(path).ok()?;

    // The offset is relative to the TIFF header, which starts after the EXIF marker.
    // For JPEG files, we need to find where the EXIF APP1 data starts.
    // The kamadak-exif library handles this internally, but for raw access we need
    // to scan for the EXIF data ourselves.
    //
    // Simpler approach: just try decoding the thumbnail data from the field values
    // that the exif reader already parsed. Let's re-read via the exif crate.

    // Actually, the simplest reliable approach: re-read the EXIF and get thumbnail
    // bytes directly if the reader supports it. kamadak-exif doesn't expose raw
    // thumbnail bytes easily, so let's use a different strategy:
    // Read the first 256KB of the file and look for the embedded JPEG.

    let mut buf = Vec::new();
    let file_len = file.metadata().ok()?.len();
    let read_len = file_len.min(256 * 1024) as usize;
    buf.resize(read_len, 0);
    file.seek(std::io::SeekFrom::Start(0)).ok()?;
    file.read_exact(&mut buf).ok()?;

    // Search for JPEG SOI marker (0xFF 0xD8) starting from the offset hint.
    // The offset from EXIF is relative to the TIFF header start, which we
    // approximate by searching near it.
    let search_start = (offset as usize).saturating_sub(20);
    let search_end = ((offset + length) as usize + 100).min(buf.len());

    // Find the embedded JPEG by looking for SOI marker
    for i in search_start..search_end.saturating_sub(1) {
        if buf[i] == 0xFF && buf[i + 1] == 0xD8 {
            // Found JPEG start, now find the end (EOI: 0xFF 0xD9)
            let jpeg_start = i;
            let max_end = (jpeg_start + length as usize + 1000).min(buf.len());
            for j in (jpeg_start + 2)..max_end.saturating_sub(1) {
                if buf[j] == 0xFF && buf[j + 1] == 0xD9 {
                    let jpeg_end = j + 2;
                    let jpeg_data = &buf[jpeg_start..jpeg_end];
                    return decode_jpeg_bytes(jpeg_data);
                }
            }
            // If we found SOI but no EOI, try using length hint
            let jpeg_end = (jpeg_start + length as usize).min(buf.len());
            let jpeg_data = &buf[jpeg_start..jpeg_end];
            return decode_jpeg_bytes(jpeg_data);
        }
    }

    None
}

/// Decode JPEG bytes into a DecodedImage using zune-jpeg.
fn decode_jpeg_bytes(data: &[u8]) -> Option<DecodedImage> {
    use zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let cursor = std::io::Cursor::new(data);
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

/// Decode an image file to a thumbnail of at most `max_size` x `max_size` pixels.
/// Uses image crate's integrated scale-on-decode for best performance.
pub fn decode_thumbnail(path: &Path, max_size: u32) -> Result<DecodedImage, String> {
    let img = image::open(path).map_err(|e| format!("Failed to load {}: {e}", path.display()))?;

    let thumb = img.thumbnail(max_size, max_size);
    let rgba = thumb.to_rgba8();

    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

/// Try EXIF thumbnail first (fast), then fall back to full decode (quality).
/// Returns the decoded thumbnail, whether it came from EXIF, and timing data.
pub fn decode_thumbnail_progressive(
    path: &Path,
    max_size: u32,
) -> Result<(DecodedImage, bool, DecodeTimings), String> {
    let mut timings = DecodeTimings::default();

    // Tier 0: Try EXIF embedded thumbnail (~1ms)
    let exif_start = std::time::Instant::now();
    let exif_result = extract_exif_thumbnail(path);
    timings.exif_ms = exif_start.elapsed().as_secs_f64() * 1000.0;

    if let Some(thumb) = exif_result {
        return Ok((thumb, true, timings));
    }

    // Tier 1: Full decode + downscale
    let full_start = std::time::Instant::now();
    let decoded = decode_thumbnail(path, max_size)?;
    timings.full_ms = full_start.elapsed().as_secs_f64() * 1000.0;

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
        // Our test JPEG helper creates images without EXIF data
        let path = create_test_jpeg(&dir, "no_exif.jpg", 200, 150);

        let result = extract_exif_thumbnail(&path);
        // Should return None — no EXIF data in our synthetic images
        assert!(result.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exif_extract_nonexistent_returns_none() {
        let result = extract_exif_thumbnail(Path::new("/no/such/file.jpg"));
        assert!(result.is_none());
    }
}
