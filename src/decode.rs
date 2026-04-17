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

// ---------------------------------------------------------------------------
// Smart I/O probe helpers
// ---------------------------------------------------------------------------

/// Result of probing a file's header to determine where (if anywhere) an
/// embedded thumbnail lives.
pub enum ProbeResult {
    /// Thumbnail data is fully contained in the provided buffer at `[offset..offset+length]`.
    /// No further I/O needed — decode directly from the buffer.
    ContainedInProbe {
        /// The probe buffer (may be the full APP1 or full meta box).
        data: Vec<u8>,
    },
    /// Thumbnail exists but requires a targeted read at the given file offset.
    NeedsRead { offset: u64, length: usize },
    /// No embedded thumbnail exists. Skip to full decode.
    NoThumbnail,
    /// Probe was inconclusive (file too small, unrecognized format).
    /// Fall back to full prefix read.
    Inconclusive,
}

/// JPEG: scan for APP1 marker and return the APP1 length.
/// Call with the first ~64 bytes of the file.
/// Returns `Some((app1_data_offset, app1_length))` — the offset in the file
/// where the APP1 data starts (after marker+length) and the total length
/// including the EXIF payload.
pub fn jpeg_probe_app1(header: &[u8]) -> Option<(u64, usize)> {
    if header.len() < 4 || header[0] != 0xFF || header[1] != 0xD8 {
        return None;
    }
    let mut pos = 2;
    while pos + 4 <= header.len() {
        if header[pos] != 0xFF {
            return None;
        }
        let marker = header[pos + 1];
        let seg_len = u16::from_be_bytes([header[pos + 2], header[pos + 3]]) as usize;
        if marker == 0xE1 {
            // APP1 found. seg_len includes the 2-byte length field itself.
            // Total segment: marker(2) + length_field(2) + payload(seg_len - 2).
            // We want to read the entire segment starting at `pos`.
            let total = 2 + seg_len; // marker(2) + length_field(2) + payload(seg_len-2) = 2 + seg_len
            return Some((pos as u64, total));
        }
        // Skip this segment: marker(2) + seg_len bytes
        pos += 2 + seg_len;
    }
    None
}

/// HEIC: probe the meta box from the first ~8KB of the file.
/// Returns the iloc offset and length of the thumbnail item, if found.
pub fn heif_probe_thumbnail_location(header: &[u8]) -> ProbeResult {
    // Find ftyp box
    let Some((_ftyp_start, _ftyp_end)) = find_top_box(header, b"ftyp") else {
        return ProbeResult::Inconclusive;
    };
    // Find meta box (should be right after ftyp)
    let Some((meta_start, meta_end)) = find_top_box(header, b"meta") else {
        return ProbeResult::Inconclusive;
    };
    if meta_end > header.len() {
        // Meta box extends beyond our probe — need a larger read
        return ProbeResult::Inconclusive;
    }

    // meta is a FullBox: [size:4][type:4][version:1][flags:3][children...]
    let meta_body = &header[meta_start + 12..meta_end];

    // Find pitm → primary item ID
    let Some(primary_id) = find_pitm_item_id(meta_body) else {
        return ProbeResult::Inconclusive;
    };

    // Find iref → look for thmb reference to find the thumbnail item ID
    let thumb_id = find_thmb_item_id(meta_body, primary_id);
    let Some(thumb_id) = thumb_id else {
        return ProbeResult::NoThumbnail;
    };

    // Find iloc → get offset and length for the thumbnail item
    let Some((offset, length)) = find_iloc_item_location(meta_body, thumb_id) else {
        return ProbeResult::Inconclusive;
    };

    // Check if the thumbnail data is within our probe buffer
    if (offset as usize) + (length as usize) <= header.len() {
        // Thumbnail is fully in the probe — extract just that region
        // But we need to pass enough context for libheif to parse it.
        // Actually, for HEVC thumbnails we need the full meta + the data.
        // Easier: pass the whole probe buffer, libheif will find it.
        ProbeResult::ContainedInProbe {
            data: header.to_vec(),
        }
    } else {
        ProbeResult::NeedsRead {
            offset,
            length: length as usize,
        }
    }
}

// --- ISOBMFF helpers for probing (read-only, no mutation) ---

fn find_top_box(data: &[u8], fourcc: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as u64;
        let box_type = &data[pos + 4..pos + 8];
        let box_size = if size == 1 {
            if pos + 16 > data.len() {
                break;
            }
            u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?)
        } else if size == 0 {
            (data.len() - pos) as u64
        } else {
            size
        };
        if box_size < 8 {
            break;
        }
        if box_type == fourcc {
            return Some((pos, (pos as u64 + box_size) as usize));
        }
        pos += box_size as usize;
    }
    None
}

fn find_sub_box(data: &[u8], fourcc: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        if size < 8 || pos + size > data.len() {
            break;
        }
        if &data[pos + 4..pos + 8] == fourcc {
            return Some((pos, pos + size));
        }
        pos += size;
    }
    None
}

fn find_pitm_item_id(meta_body: &[u8]) -> Option<u32> {
    let (start, _end) = find_sub_box(meta_body, b"pitm")?;
    let version = meta_body[start + 8];
    if version == 0 {
        Some(u16::from_be_bytes(meta_body[start + 12..start + 14].try_into().ok()?) as u32)
    } else {
        Some(u32::from_be_bytes(
            meta_body[start + 12..start + 16].try_into().ok()?,
        ))
    }
}

fn find_thmb_item_id(meta_body: &[u8], primary_id: u32) -> Option<u32> {
    let (start, end) = find_sub_box(meta_body, b"iref")?;
    let version = meta_body[start + 8];
    let mut pos = start + 12; // skip fullbox header
    while pos + 8 < end {
        let entry_size = u32::from_be_bytes(meta_body[pos..pos + 4].try_into().ok()?) as usize;
        if entry_size < 8 || pos + entry_size > end {
            break;
        }
        let ref_type = &meta_body[pos + 4..pos + 8];
        if ref_type == b"thmb" {
            // Parse the reference entry
            let (from_id, ref_count, refs_start) = if version == 0 {
                let from = u16::from_be_bytes(meta_body[pos + 8..pos + 10].try_into().ok()?) as u32;
                let count =
                    u16::from_be_bytes(meta_body[pos + 10..pos + 12].try_into().ok()?) as usize;
                (from, count, pos + 12)
            } else {
                let from = u32::from_be_bytes(meta_body[pos + 8..pos + 12].try_into().ok()?);
                let count =
                    u16::from_be_bytes(meta_body[pos + 12..pos + 14].try_into().ok()?) as usize;
                (from, count, pos + 14)
            };
            // thmb: from_id is the thumbnail item, references point to the primary
            for i in 0..ref_count {
                let to_id = if version == 0 {
                    u16::from_be_bytes(
                        meta_body[refs_start + i * 2..refs_start + i * 2 + 2]
                            .try_into()
                            .ok()?,
                    ) as u32
                } else {
                    u32::from_be_bytes(
                        meta_body[refs_start + i * 4..refs_start + i * 4 + 4]
                            .try_into()
                            .ok()?,
                    )
                };
                if to_id == primary_id {
                    return Some(from_id);
                }
            }
        }
        pos += entry_size;
    }
    None
}

fn find_iloc_item_location(meta_body: &[u8], item_id: u32) -> Option<(u64, u64)> {
    let (start, end) = find_sub_box(meta_body, b"iloc")?;
    let version = meta_body[start + 8];
    let size_info = u16::from_be_bytes(meta_body[start + 12..start + 14].try_into().ok()?);
    let offset_size = ((size_info >> 12) & 0xF) as usize;
    let length_size = ((size_info >> 8) & 0xF) as usize;
    let base_offset_size = ((size_info >> 4) & 0xF) as usize;
    let index_size = (size_info & 0xF) as usize;

    let mut pos = start + 14;
    let item_count = if version < 2 {
        let c = u16::from_be_bytes(meta_body[pos..pos + 2].try_into().ok()?) as u32;
        pos += 2;
        c
    } else {
        let c = u32::from_be_bytes(meta_body[pos..pos + 4].try_into().ok()?);
        pos += 4;
        c
    };

    for _ in 0..item_count {
        if pos >= end {
            break;
        }
        let id = if version < 2 {
            let id = u16::from_be_bytes(meta_body[pos..pos + 2].try_into().ok()?) as u32;
            pos += 2;
            id
        } else {
            let id = u32::from_be_bytes(meta_body[pos..pos + 4].try_into().ok()?);
            pos += 4;
            id
        };

        if version >= 1 {
            pos += 2; // construction_method
        }
        pos += 2; // data_reference_index

        let base_offset = read_uint_probe(&meta_body[pos..], base_offset_size);
        pos += base_offset_size;

        let extent_count = u16::from_be_bytes(meta_body[pos..pos + 2].try_into().ok()?) as usize;
        pos += 2;

        for _ in 0..extent_count {
            if (version == 1 || version == 2) && index_size > 0 {
                pos += index_size;
            }
            let extent_offset = read_uint_probe(&meta_body[pos..], offset_size);
            pos += offset_size;
            let extent_length = read_uint_probe(&meta_body[pos..], length_size);
            pos += length_size;

            if id == item_id {
                let offset = base_offset + extent_offset;
                return Some((offset, extent_length));
            }
        }
    }
    None
}

fn read_uint_probe(data: &[u8], size: usize) -> u64 {
    match size {
        0 => 0,
        2 => u16::from_be_bytes(data[..2].try_into().unwrap_or([0; 2])) as u64,
        4 => u32::from_be_bytes(data[..4].try_into().unwrap_or([0; 4])) as u64,
        8 => u64::from_be_bytes(data[..8].try_into().unwrap_or([0; 8])),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Unified probe: detect format by magic bytes, dispatch to format-specific probe
// ---------------------------------------------------------------------------

/// Probe a file header (typically 8KB) to determine where an embedded
/// thumbnail lives. Detects format by magic bytes and dispatches to the
/// appropriate format-specific probe.
///
/// This is the single entry point for the I/O layer. The caller reads a
/// small probe, passes it here, and gets back instructions for what to read
/// next (if anything).
pub fn probe_embedded_thumbnail(header: &[u8], file_len: u64) -> ProbeResult {
    if header.len() < 8 {
        return ProbeResult::Inconclusive;
    }

    // JPEG: FF D8 FF
    if header[0] == 0xFF && header[1] == 0xD8 {
        return probe_jpeg(header);
    }

    // HEIF/HEIC: ftyp box (check for 'ftyp' at offset 4)
    if &header[4..8] == b"ftyp" {
        return heif_probe_thumbnail_location(header);
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        return probe_png(header);
    }

    // WebP: RIFF....WEBP
    if header.len() >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        return probe_webp(header, file_len);
    }

    // GIF: GIF8
    if header.starts_with(b"GIF8") {
        return ProbeResult::NoThumbnail;
    }

    // BMP: BM
    if header.starts_with(b"BM") {
        return ProbeResult::NoThumbnail;
    }

    // TIFF: II (little-endian) or MM (big-endian) — rare thumbnails, use prefix
    if (header[0] == b'I' && header[1] == b'I') || (header[0] == b'M' && header[1] == b'M') {
        return ProbeResult::Inconclusive; // fall back to prefix read
    }

    ProbeResult::Inconclusive
}

/// JPEG probe: find APP1 marker → return read instructions.
fn probe_jpeg(header: &[u8]) -> ProbeResult {
    match jpeg_probe_app1(header) {
        Some((app1_offset, app1_total_len)) => {
            let end = app1_offset as usize + app1_total_len;
            if end <= header.len() {
                // APP1 is fully within our probe
                ProbeResult::ContainedInProbe {
                    data: header[..end].to_vec(),
                }
            } else {
                // Need to read through end of APP1
                ProbeResult::NeedsRead {
                    offset: 0,
                    length: end,
                }
            }
        }
        None => ProbeResult::NoThumbnail,
    }
}

/// PNG probe: scan chunk headers for eXIf before IDAT.
/// If found, returns the file range containing the eXIf chunk's TIFF data.
fn probe_png(header: &[u8]) -> ProbeResult {
    // PNG: 8-byte signature + chunks [length:4][type:4][data:length][crc:4]
    let mut pos = 8; // skip signature
    while pos + 8 <= header.len() {
        let chunk_len =
            u32::from_be_bytes(header[pos..pos + 4].try_into().unwrap_or([0; 4])) as usize;
        let chunk_type = &header[pos + 4..pos + 8];
        let chunk_total = 4 + 4 + chunk_len + 4; // length + type + data + CRC

        if chunk_type == b"eXIf" {
            let data_start = pos + 8;
            let data_end = data_start + chunk_len;
            return if data_end <= header.len() {
                ProbeResult::ContainedInProbe {
                    data: header[..data_end.min(header.len())].to_vec(),
                }
            } else {
                ProbeResult::NeedsRead {
                    offset: 0,
                    length: data_end,
                }
            };
        } else if chunk_type == b"IDAT" {
            // Hit image data without finding eXIf — no embedded thumbnail
            return ProbeResult::NoThumbnail;
        } else {
            // Skip this chunk
            pos += chunk_total;
        }
    }

    // Ran out of probe data before finding eXIf or IDAT
    if pos + 8 > header.len() {
        ProbeResult::Inconclusive
    } else {
        ProbeResult::NoThumbnail
    }
}

/// WebP probe: check VP8X flags for EXIF presence, then locate EXIF chunk.
/// iv-thumb places the EXIF chunk at the end of the file.
fn probe_webp(header: &[u8], file_len: u64) -> ProbeResult {
    // Simple WebP (VP8/VP8L, no VP8X): no EXIF possible
    if header.len() < 16 || &header[12..16] != b"VP8X" {
        return ProbeResult::NoThumbnail;
    }

    // VP8X chunk: 4 fourcc + 4 size + payload
    if header.len() < 21 {
        return ProbeResult::Inconclusive;
    }
    let flags = header[20];
    let has_exif = (flags & 0x08) != 0;
    if !has_exif {
        return ProbeResult::NoThumbnail;
    }

    // EXIF flag is set — the EXIF chunk is somewhere in the file.
    // iv-thumb places it at the end. We need to hop through chunks to find it.
    // If the file is small enough to be in our probe, scan directly.
    // Otherwise, return a hint to read the tail of the file.

    // Try to find EXIF chunk by scanning chunk headers in our probe
    let vp8x_size = u32::from_le_bytes(header[16..20].try_into().unwrap_or([0; 4])) as usize;
    let mut pos = 20 + vp8x_size;

    while pos + 8 <= header.len() {
        let fourcc = &header[pos..pos + 4];
        let chunk_size =
            u32::from_le_bytes(header[pos + 4..pos + 8].try_into().unwrap_or([0; 4])) as usize;
        let padded = chunk_size + (chunk_size & 1); // RIFF chunks pad to even

        if fourcc == b"EXIF" {
            let data_start = pos + 8;
            let data_end = data_start + chunk_size;
            return if data_end <= header.len() {
                ProbeResult::ContainedInProbe {
                    data: header[..data_end].to_vec(),
                }
            } else {
                ProbeResult::NeedsRead {
                    offset: 0,
                    length: data_end,
                }
            };
        } else {
            pos += 8 + padded;
            continue;
        }
    }

    // Didn't find EXIF chunk in probe — it's past our probe window.
    // Provide a read hint: read the last 64KB of the file (EXIF is near EOF).
    // The I/O layer will handle this as a tail read.
    let tail_size = 64 * 1024_u64;
    if file_len > tail_size {
        ProbeResult::NeedsRead {
            offset: file_len - tail_size,
            length: tail_size as usize,
        }
    } else {
        // Small file — just read the whole thing
        ProbeResult::NeedsRead {
            offset: 0,
            length: file_len as usize,
        }
    }
}

/// Read the EXIF Orientation tag from file bytes.
/// Returns 1 (normal) if no orientation is found.
/// Values 1-8 per EXIF spec:
///   1=normal, 2=flip-h, 3=rotate180, 4=flip-v,
///   5=transpose, 6=rotate90, 7=transverse, 8=rotate270
pub fn read_exif_orientation(data: &[u8]) -> u32 {
    let cursor = Cursor::new(data);
    let exif_reader = exif::Reader::new();
    if let Ok(exif) = exif_reader.read_from_container(&mut std::io::BufReader::new(cursor))
        && let Some(field) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        && let Some(v) = field.value.get_uint(0)
        && (1..=8).contains(&v)
    {
        return v;
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
/// For HEIC/HEIF files, tries the container-level thumbnail via libheif instead.
/// Returns Some(image) if an embedded thumbnail was found.
pub fn try_exif_only(path: &Path) -> (Option<DecodedImage>, DecodeTimings) {
    let mut timings = DecodeTimings::default();
    let start = std::time::Instant::now();

    // HEIC/HEIF: thumbnails are stored in the container, not in EXIF tags
    if is_heif_extension(path) {
        let result = try_heif_thumbnail(path);
        timings.exif_ms = start.elapsed().as_secs_f64() * 1000.0;
        return (result, timings);
    }

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

/// Try EXIF thumbnail extraction from already-loaded bytes (no I/O).
/// For non-HEIC data, parses EXIF and applies orientation.
/// For HEIC data, use `try_heif_thumbnail_from_bytes` directly.
pub fn try_embedded_from_bytes(data: &[u8]) -> Option<DecodedImage> {
    let orientation = read_exif_orientation(data);
    let mut decoded = extract_exif_thumbnail(data)?;

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
    let img = image::load_from_memory(&data).map_err(|e| format!("Failed to decode: {e}"))?;
    let img = if !is_heif_extension(path) {
        let orientation = read_exif_orientation(&data);
        apply_orientation(img, orientation)
    } else {
        img
    };
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

/// Decode a thumbnail from already-loaded file bytes (I/O already done).
/// Applies EXIF orientation for non-HEIF formats. For HEIF, libheif
/// applies orientation internally during decode.
/// `skip_orientation`: set true for HEIC/HEIF files.
pub fn decode_from_bytes(
    data: &[u8],
    max_size: u32,
    skip_orientation: bool,
) -> Result<(DecodedImage, DecodeTimings), String> {
    let mut timings = DecodeTimings::default();
    let start = std::time::Instant::now();

    let img = image::load_from_memory(data).map_err(|e| format!("Failed to decode: {e}"))?;
    let img = if !skip_orientation {
        let orientation = read_exif_orientation(data);
        apply_orientation(img, orientation)
    } else {
        img
    };
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

/// Check whether a file path has an HEIF/HEIC extension.
pub fn is_heif_extension(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(ext.to_ascii_lowercase().as_str(), "heic" | "heif" | "hif"),
        None => false,
    }
}

/// Try to extract the HEIF/HEIC container-level thumbnail via libheif.
/// HEIC stores thumbnails as separate image items (not in EXIF), so the
/// standard EXIF `JPEGInterchangeFormat` approach doesn't work.
pub fn try_heif_thumbnail(path: &Path) -> Option<DecodedImage> {
    use libheif_rs::{ColorSpace, LibHeif, RgbChroma};

    let path_str = path.to_str()?;
    let ctx = libheif_rs::HeifContext::read_from_file(path_str).ok()?;
    let handle = ctx.primary_image_handle().ok()?;

    if handle.number_of_thumbnails() == 0 {
        return None;
    }

    let mut thumb_ids = vec![0u32; 1];
    handle.thumbnail_ids(&mut thumb_ids);
    let thumb_handle = handle.thumbnail(thumb_ids[0]).ok()?;

    let lib_heif = LibHeif::new();
    let image = lib_heif
        .decode(&thumb_handle, ColorSpace::Rgb(RgbChroma::Rgba), None)
        .ok()?;

    let planes = image.planes();
    let plane = planes.interleaved?;
    let width = image.width();
    let height = image.height();
    let stride = plane.stride;
    let data = plane.data;

    // Handle stride != width*4 (row padding)
    let row_bytes = width as usize * 4;
    let pixels = if stride == row_bytes {
        data[..row_bytes * height as usize].to_vec()
    } else {
        let mut pixels = Vec::with_capacity(row_bytes * height as usize);
        for y in 0..height as usize {
            let row_start = y * stride;
            pixels.extend_from_slice(&data[row_start..row_start + row_bytes]);
        }
        pixels
    };

    Some(DecodedImage {
        width,
        height,
        pixels,
    })
}

/// Try to extract the HEIF/HEIC container-level thumbnail from in-memory bytes.
/// Same as `try_heif_thumbnail` but avoids a separate file read.
pub fn try_heif_thumbnail_from_bytes(data: &[u8]) -> Option<DecodedImage> {
    use libheif_rs::{ColorSpace, LibHeif, RgbChroma};

    let ctx = libheif_rs::HeifContext::read_from_bytes(data).ok()?;
    let handle = ctx.primary_image_handle().ok()?;

    if handle.number_of_thumbnails() == 0 {
        return None;
    }

    let mut thumb_ids = vec![0u32; 1];
    handle.thumbnail_ids(&mut thumb_ids);
    let thumb_handle = handle.thumbnail(thumb_ids[0]).ok()?;

    let lib_heif = LibHeif::new();
    let image = lib_heif
        .decode(&thumb_handle, ColorSpace::Rgb(RgbChroma::Rgba), None)
        .ok()?;

    let planes = image.planes();
    let plane = planes.interleaved?;
    let width = image.width();
    let height = image.height();
    let stride = plane.stride;
    let data = plane.data;

    let row_bytes = width as usize * 4;
    let pixels = if stride == row_bytes {
        data[..row_bytes * height as usize].to_vec()
    } else {
        let mut pixels = Vec::with_capacity(row_bytes * height as usize);
        for y in 0..height as usize {
            let row_start = y * stride;
            pixels.extend_from_slice(&data[row_start..row_start + row_bytes]);
        }
        pixels
    };

    Some(DecodedImage {
        width,
        height,
        pixels,
    })
}

/// Try to extract the EXIF embedded thumbnail from file bytes.
pub fn extract_exif_thumbnail(data: &[u8]) -> Option<DecodedImage> {
    let cursor = Cursor::new(data);
    let exif_reader = exif::Reader::new();
    let exif = exif_reader
        .read_from_container(&mut std::io::BufReader::new(cursor))
        .ok()?;

    for field in exif.fields() {
        if field.tag == exif::Tag::JPEGInterchangeFormat
            && let (Some(offset_field), Some(length_field)) = (
                exif.get_field(exif::Tag::JPEGInterchangeFormat, field.ifd_num),
                exif.get_field(exif::Tag::JPEGInterchangeFormatLength, field.ifd_num),
            )
            && let (Some(offset), Some(length)) = (
                offset_field.value.get_uint(0),
                length_field.value.get_uint(0),
            )
        {
            return find_and_decode_exif_jpeg(data, offset, length);
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
