//! iv-thumb — Losslessly embed thumbnails into image files that lack them.
//!
//! Supports HEIC/HEIF, JPEG, WebP, and PNG.
//! The primary image data is never re-encoded.
//!
//! Usage:
//!   iv-thumb <path>              # embed thumbnails in all supported files
//!   iv-thumb --dry-run <path>    # report which files would be modified
//!   iv-thumb --recursive <path>  # walk subdirectories

use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let dry_run = args.iter().any(|a| a == "--dry-run");
    let recursive = args.iter().any(|a| a == "--recursive");

    let path = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);

    let path = match path {
        Some(p) => p,
        None => {
            eprintln!("Usage: iv-thumb [--dry-run] [--recursive] <path>");
            eprintln!("  Embeds thumbnails into image files that lack them.");
            eprintln!("  Primary image data is never re-encoded (lossless).");
            process::exit(1);
        }
    };

    if !path.exists() {
        eprintln!("Error: path does not exist: {}", path.display());
        process::exit(1);
    }

    // Register HEIF decoder hooks
    libheif_rs::integration::image::register_all_decoding_hooks();

    let files = collect_files(&path, recursive);
    if files.is_empty() {
        eprintln!("No supported image files found.");
        process::exit(0);
    }

    println!("Scanning {} files...\n", files.len());

    let mut stats = Stats::default();

    for file in &files {
        match process_file(file, dry_run) {
            FileResult::AlreadyHasThumb => {
                stats.already_has += 1;
            }
            FileResult::Injected { ms } => {
                stats.injected += 1;
                println!(
                    "  \x1b[32m✓\x1b[0m {} ({:.0}ms)",
                    file.display(),
                    ms
                );
            }
            FileResult::WouldInject => {
                stats.would_inject += 1;
                println!("  → {}", file.display());
            }
            FileResult::Unsupported => {
                stats.unsupported += 1;
            }
            FileResult::Error(e) => {
                stats.errors += 1;
                eprintln!("  \x1b[31m✗\x1b[0m {} — {}", file.display(), e);
            }
        }
    }

    println!("\nDone.");
    println!(
        "  Already has thumbnail: {}",
        stats.already_has
    );
    if dry_run {
        println!("  Would inject: {}", stats.would_inject);
    } else {
        println!("  Injected: {}", stats.injected);
    }
    if stats.unsupported > 0 {
        println!("  Unsupported format: {}", stats.unsupported);
    }
    if stats.errors > 0 {
        println!("  Errors: {}", stats.errors);
    }
}

#[derive(Default)]
struct Stats {
    already_has: usize,
    injected: usize,
    would_inject: usize,
    unsupported: usize,
    errors: usize,
}

enum FileResult {
    AlreadyHasThumb,
    Injected { ms: f64 },
    WouldInject,
    Unsupported,
    Error(String),
}

fn collect_files(path: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if path.is_file() {
        files.push(path.to_path_buf());
    } else if path.is_dir() {
        collect_dir(path, recursive, &mut files);
    }
    files.sort();
    files
}

fn collect_dir(dir: &Path, recursive: bool, files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_supported(&path) {
            files.push(path);
        } else if path.is_dir() && recursive {
            collect_dir(&path, recursive, files);
        }
    }
}

fn is_supported(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_ascii_lowercase().as_str(),
            "heic" | "heif" | "jpg" | "jpeg" | "webp" | "png"
        ),
        None => false,
    }
}

fn is_heif(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(ext.to_ascii_lowercase().as_str(), "heic" | "heif"),
        None => false,
    }
}

fn is_jpeg(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg"),
        None => false,
    }
}

fn is_webp(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => ext.eq_ignore_ascii_case("webp"),
        None => false,
    }
}

fn is_png(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => ext.eq_ignore_ascii_case("png"),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn process_file(path: &Path, dry_run: bool) -> FileResult {
    if is_heif(path) {
        process_heif(path, dry_run)
    } else if is_jpeg(path) {
        process_jpeg(path, dry_run)
    } else if is_webp(path) || is_png(path) {
        process_exif_container(path, dry_run)
    } else {
        FileResult::Unsupported
    }
}

// ---------------------------------------------------------------------------
// HEIC/HEIF — libheif HEVC thumbnail injection
// ---------------------------------------------------------------------------

fn process_heif(path: &Path, dry_run: bool) -> FileResult {
    let path_str = match path.to_str() {
        Some(s) => s,
        None => return FileResult::Error("non-UTF8 path".into()),
    };

    // Check if it already has a thumbnail via libheif
    {
        let ctx = match libheif_rs::HeifContext::read_from_file(path_str) {
            Ok(c) => c,
            Err(e) => return FileResult::Error(format!("read: {e}")),
        };
        let handle = match ctx.primary_image_handle() {
            Ok(h) => h,
            Err(e) => return FileResult::Error(format!("handle: {e}")),
        };
        if handle.number_of_thumbnails() > 0 {
            return FileResult::AlreadyHasThumb;
        }
    }

    if dry_run {
        return FileResult::WouldInject;
    }

    let start = std::time::Instant::now();

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return FileResult::Error(format!("read: {e}")),
    };

    // Encode thumbnail as HEVC via libheif, then extract raw bitstream + hvcC config
    let (thumb_hevc, hvcc_box, thumb_w, thumb_h) = match encode_hevc_thumbnail(&data) {
        Ok(v) => v,
        Err(e) => return FileResult::Error(e),
    };

    match heif_inject_thumbnail(&data, &thumb_hevc, &hvcc_box, thumb_w, thumb_h) {
        Ok(output) => {
            if let Err(e) = std::fs::write(path, &output) {
                return FileResult::Error(format!("write: {e}"));
            }
        }
        Err(e) => return FileResult::Error(e),
    }

    let ms = start.elapsed().as_secs_f64() * 1000.0;
    FileResult::Injected { ms }
}

/// Encode a thumbnail as HEVC using libheif's x265 encoder.
///
/// Returns `(hevc_bitstream, hvcc_box, width, height)`.
/// We create a standalone single-image HEIC via libheif, then extract the raw
/// coded data from its mdat and the hvcC config box from its ipco.
fn encode_hevc_thumbnail(original_data: &[u8]) -> Result<(Vec<u8>, Vec<u8>, u32, u32), String> {
    use libheif_rs::{ColorSpace, Channel, CompressionFormat, EncoderQuality, LibHeif, RgbChroma};

    // Decode original image to get pixels for downscaling
    let img = image::load_from_memory(original_data)
        .map_err(|e| format!("decode: {e}"))?;
    let thumb = img.thumbnail(416, 416);
    let thumb = thumb.to_rgba8();
    let thumb_w = thumb.width();
    let thumb_h = thumb.height();

    // Create libheif Image with interleaved RGBA
    let mut heif_img = libheif_rs::Image::new(thumb_w, thumb_h, ColorSpace::Rgb(RgbChroma::Rgba))
        .map_err(|e| format!("heif image create: {e}"))?;
    heif_img
        .create_plane(Channel::Interleaved, thumb_w, thumb_h, 8)
        .map_err(|e| format!("heif add plane: {e}"))?;
    {
        let planes = heif_img.planes_mut();
        let plane = planes.interleaved.ok_or("no interleaved plane")?;
        let stride = plane.stride;
        let data = plane.data;
        let row_bytes = thumb_w as usize * 4;
        for y in 0..thumb_h as usize {
            let src_start = y * row_bytes;
            let dst_start = y * stride;
            data[dst_start..dst_start + row_bytes]
                .copy_from_slice(&thumb.as_raw()[src_start..src_start + row_bytes]);
        }
    }

    // Encode as HEVC into a standalone HEIC
    let lib_heif = LibHeif::new();
    let mut encoder = lib_heif
        .encoder_for_format(CompressionFormat::Hevc)
        .map_err(|e| format!("no HEVC encoder: {e}"))?;
    encoder
        .set_quality(EncoderQuality::Lossy(50))
        .map_err(|e| format!("set quality: {e}"))?;

    let mut ctx = libheif_rs::HeifContext::new()
        .map_err(|e| format!("context: {e}"))?;
    ctx.encode_image(&heif_img, &mut encoder, None)
        .map_err(|e| format!("encode: {e}"))?;

    let heic_bytes = ctx
        .write_to_bytes()
        .map_err(|e| format!("write: {e}"))?;

    // Extract raw HEVC bitstream from the standalone HEIC's mdat
    let (mdat_start, mdat_end) = find_isobmff_box(&heic_bytes, b"mdat")
        .ok_or("no mdat in encoded HEIC")?;
    // mdat body starts after the 8-byte header (or 16-byte if extended)
    let mdat_header_size = {
        let size = u32::from_be_bytes(heic_bytes[mdat_start..mdat_start + 4].try_into().unwrap());
        if size == 1 { 16 } else { 8 }
    };
    let hevc_data = heic_bytes[mdat_start + mdat_header_size..mdat_end].to_vec();

    // Extract hvcC box from the standalone HEIC's meta > iprp > ipco
    let (meta_start, meta_end) = find_isobmff_box(&heic_bytes, b"meta")
        .ok_or("no meta in encoded HEIC")?;
    let meta_body = &heic_bytes[meta_start + 12..meta_end]; // skip fullbox header
    let (iprp_start, iprp_end) = find_subbox_in(meta_body, b"iprp")
        .ok_or("no iprp in encoded HEIC")?;
    let iprp_body = &meta_body[iprp_start + 8..iprp_end];
    let (ipco_start, ipco_end) = find_subbox_in(iprp_body, b"ipco")
        .ok_or("no ipco in encoded HEIC")?;
    let ipco_body = &iprp_body[ipco_start + 8..ipco_end];
    let (hvcc_start, hvcc_end) = find_subbox_in(ipco_body, b"hvcC")
        .ok_or("no hvcC in encoded HEIC")?;
    let hvcc_box = ipco_body[hvcc_start..hvcc_end].to_vec();

    Ok((hevc_data, hvcc_box, thumb_w, thumb_h))
}

// ---------------------------------------------------------------------------
// Raw ISOBMFF binary surgery for HEIF thumbnail injection
// ---------------------------------------------------------------------------

/// Inject an HEVC thumbnail into a HEIF file's ISOBMFF container.
///
/// Output layout: ftyp | meta (modified) | thumb_mdat | original_mdat | rest
/// The thumbnail data is placed right after meta so it's readable in the
/// first few KB of the file — important for fast network thumbnail extraction.
fn heif_inject_thumbnail(
    data: &[u8],
    thumb_hevc: &[u8],
    hvcc_box: &[u8],
    thumb_w: u32,
    thumb_h: u32,
) -> Result<Vec<u8>, String> {
    // Locate top-level boxes
    let (ftyp_start, ftyp_end) =
        find_isobmff_box(data, b"ftyp").ok_or("no ftyp box")?;
    let (meta_start, meta_end) =
        find_isobmff_box(data, b"meta").ok_or("no meta box")?;
    let (mdat_start, _mdat_end) =
        find_isobmff_box(data, b"mdat").ok_or("no mdat box")?;

    // Parse pitm to get primary item ID
    // meta box: [size:4][fourcc:4][version:1][flags:3][children...]
    let meta_body_start = meta_start + 12; // children start after fullbox header
    let meta_version_flags = &data[meta_start + 8..meta_start + 12]; // version+flags bytes
    let primary_id = find_pitm_item_id(&data[meta_body_start..meta_end])
        .ok_or("no pitm in meta")?;

    // Find max item_id from iinf
    let max_id = find_max_item_id(&data[meta_body_start..meta_end])
        .ok_or("no iinf in meta")?;
    let thumb_id = max_id + 1;

    // The new thumb mdat box: [size:4][mdat:4][hevc_data]
    let thumb_mdat_size = 8 + thumb_hevc.len();

    // ---- Patch meta bytes directly ----
    // Build the new meta body by finding each sub-box and appending entries

    let orig_meta_body = &data[meta_body_start..meta_end];
    let mut patched = orig_meta_body.to_vec();

    // Helper: find a sub-box, append data to it, update its size
    fn append_to_subbox(buf: &mut Vec<u8>, fourcc: &[u8; 4], extra: &[u8]) -> Result<(), String> {
        let (start, end) = find_subbox_in(buf, fourcc)
            .ok_or_else(|| format!("sub-box '{}' not found", String::from_utf8_lossy(fourcc)))?;
        buf.splice(end..end, extra.iter().copied());
        // Update box size
        let old_size = u32::from_be_bytes(buf[start..start + 4].try_into().unwrap());
        let new_size = old_size + extra.len() as u32;
        buf[start..start + 4].copy_from_slice(&new_size.to_be_bytes());
        Ok(())
    }

    // 1. Append infe to iinf and update entry count
    append_to_subbox(&mut patched, b"iinf", &build_infe_entry(thumb_id))?;
    // Update iinf entry count (version-dependent: v0=u16 at offset 10, v2+=u32 at offset 10)
    {
        let (iinf_start, _) = find_subbox_in(&patched, b"iinf").unwrap();
        let version = patched[iinf_start + 8];
        if version == 0 {
            let count_off = iinf_start + 12; // after size(4)+type(4)+ver(1)+flags(3)
            let old_count = u16::from_be_bytes(patched[count_off..count_off + 2].try_into().unwrap());
            patched[count_off..count_off + 2].copy_from_slice(&(old_count + 1).to_be_bytes());
        } else {
            let count_off = iinf_start + 12;
            let old_count = u32::from_be_bytes(patched[count_off..count_off + 4].try_into().unwrap());
            patched[count_off..count_off + 4].copy_from_slice(&(old_count + 1).to_be_bytes());
        }
    }

    // 2. Append iloc entry (with placeholder offset — will fix after assembly)
    let iloc_entry = build_iloc_entry_v1(thumb_id, 0, thumb_hevc.len() as u64, &patched)?;
    append_to_subbox(&mut patched, b"iloc", &iloc_entry)?;
    // Update iloc item_count
    {
        let (iloc_start, _) = find_subbox_in(&patched, b"iloc").unwrap();
        let version = patched[iloc_start + 8];
        let count_off = iloc_start + 14; // after size(4)+type(4)+ver(1)+flags(3)+size_info(2)
        if version < 2 {
            let old_count = u16::from_be_bytes(patched[count_off..count_off + 2].try_into().unwrap());
            patched[count_off..count_off + 2].copy_from_slice(&(old_count + 1).to_be_bytes());
        } else {
            let old_count = u32::from_be_bytes(patched[count_off..count_off + 4].try_into().unwrap());
            patched[count_off..count_off + 4].copy_from_slice(&(old_count + 1).to_be_bytes());
        }
    }

    // 3. Append hvcC and ispe to ipco, then associate both via ipma
    {
        // Find iprp, then ipco within it
        let (iprp_start, iprp_end) = find_subbox_in(&patched, b"iprp")
            .ok_or("no iprp")?;
        let iprp_body = iprp_start + 8;
        let (ipco_rel_start, ipco_rel_end) = find_subbox_in(&patched[iprp_body..iprp_end], b"ipco")
            .ok_or("no ipco")?;
        let ipco_abs_start = iprp_body + ipco_rel_start;
        let ipco_abs_end = iprp_body + ipco_rel_end;

        // Count existing properties
        let n_props = count_subboxes(&patched[ipco_abs_start + 8..ipco_abs_end]);
        let hvcc_prop_index = (n_props + 1) as u16; // hvcC will be first new property
        let ispe_prop_index = (n_props + 2) as u16; // ispe will be second

        // Append hvcC then ispe to ipco
        let ispe = build_ispe_box(thumb_w, thumb_h);
        let extra_len = hvcc_box.len() + ispe.len();
        patched.splice(ipco_abs_end..ipco_abs_end, hvcc_box.iter().chain(ispe.iter()).copied());
        // Update ipco size
        let old_ipco_size = u32::from_be_bytes(patched[ipco_abs_start..ipco_abs_start + 4].try_into().unwrap());
        patched[ipco_abs_start..ipco_abs_start + 4].copy_from_slice(&(old_ipco_size + extra_len as u32).to_be_bytes());
        // Update iprp size
        let old_iprp_size = u32::from_be_bytes(patched[iprp_start..iprp_start + 4].try_into().unwrap());
        patched[iprp_start..iprp_start + 4].copy_from_slice(&(old_iprp_size + extra_len as u32).to_be_bytes());

        // 4. Find ipma (re-find since positions shifted) and append association
        //    Associate BOTH hvcC (essential=true) and ispe (essential=false)
        let (iprp_start2, iprp_end2) = find_subbox_in(&patched, b"iprp").unwrap();
        let iprp_body2 = iprp_start2 + 8;
        let (ipma_rel_start, ipma_rel_end) = find_subbox_in(&patched[iprp_body2..iprp_end2], b"ipma")
            .ok_or("no ipma")?;
        let ipma_abs_start = iprp_body2 + ipma_rel_start;
        let ipma_abs_end = iprp_body2 + ipma_rel_end;

        let ipma_version = patched[ipma_abs_start + 8];
        let ipma_flags_byte = patched[ipma_abs_start + 11];
        let ipma_entry = build_ipma_association_hevc(
            thumb_id, hvcc_prop_index, ispe_prop_index, ipma_version, ipma_flags_byte,
        );
        let ipma_entry_len = ipma_entry.len();
        patched.splice(ipma_abs_end..ipma_abs_end, ipma_entry.iter().copied());
        // Update ipma size
        let old_ipma_size = u32::from_be_bytes(patched[ipma_abs_start..ipma_abs_start + 4].try_into().unwrap());
        patched[ipma_abs_start..ipma_abs_start + 4].copy_from_slice(&(old_ipma_size + ipma_entry_len as u32).to_be_bytes());
        // Update ipma entry count
        let count_off = ipma_abs_start + 12;
        let old_count = u32::from_be_bytes(patched[count_off..count_off + 4].try_into().unwrap());
        patched[count_off..count_off + 4].copy_from_slice(&(old_count + 1).to_be_bytes());
        // Update iprp size again
        let (iprp_start3, _) = find_subbox_in(&patched, b"iprp").unwrap();
        let old_iprp_size2 = u32::from_be_bytes(patched[iprp_start3..iprp_start3 + 4].try_into().unwrap());
        patched[iprp_start3..iprp_start3 + 4].copy_from_slice(&(old_iprp_size2 + ipma_entry_len as u32).to_be_bytes());
    }

    // 5. iref: append thmb reference
    let thmb_ref = build_iref_thmb_entry(thumb_id, primary_id);
    match find_subbox_in(&patched, b"iref") {
        Some(_) => {
            append_to_subbox(&mut patched, b"iref", &thmb_ref)?;
        }
        None => {
            // Create new iref box and append to meta body
            let iref_box = build_iref_box(thumb_id, primary_id);
            patched.extend_from_slice(&iref_box);
        }
    }

    // Build new meta box
    // meta box = [size:4]["meta":4][version:1][flags:3] + children
    // The version+flags were preserved from the original, patched = children only
    let new_meta_box_size = (8 + 4 + patched.len()) as u32; // header(8) + ver+flags(4) + children

    // Calculate the thumb_mdat offset in the final file:
    // ftyp_size + new_meta_size + 8 (thumb mdat header)
    let ftyp_size = ftyp_end - ftyp_start;
    let thumb_data_offset = ftyp_size + new_meta_box_size as usize + 8; // +8 for thumb mdat header

    // Size delta: how much everything after meta shifted
    let old_meta_box_size = meta_end - meta_start;
    let meta_delta = new_meta_box_size as i64 - old_meta_box_size as i64;
    let total_shift = meta_delta + thumb_mdat_size as i64; // meta grew + thumb mdat inserted

    // Fix iloc offsets in patched meta body
    fix_iloc_offsets(
        &mut patched,
        thumb_id,
        thumb_data_offset as u64,
        total_shift,
        meta_start, // original meta started here, mdat was after it
    )?;

    // Assemble output: ftyp + meta + thumb_mdat + original_mdat + rest
    let mut output = Vec::with_capacity(data.len() + thumb_mdat_size + (new_meta_box_size as usize - old_meta_box_size));
    // ftyp
    output.extend_from_slice(&data[ftyp_start..ftyp_end]);
    // meta
    output.extend_from_slice(&new_meta_box_size.to_be_bytes());
    output.extend_from_slice(b"meta");
    output.extend_from_slice(meta_version_flags);
    output.extend_from_slice(&patched);
    // thumb mdat
    output.extend_from_slice(&(thumb_mdat_size as u32).to_be_bytes());
    output.extend_from_slice(b"mdat");
    output.extend_from_slice(thumb_hevc);
    // Everything from original mdat onwards (shifted)
    output.extend_from_slice(&data[mdat_start..]);

    Ok(output)
}

// --- ISOBMFF binary helpers ---

/// Find a box within a buffer by scanning box headers.
fn find_subbox_in(data: &[u8], fourcc: &[u8; 4]) -> Option<(usize, usize)> {
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

/// Count number of direct child boxes in a container body.
fn count_subboxes(data: &[u8]) -> usize {
    let mut count = 0;
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap_or([0; 4])) as usize;
        if size < 8 || pos + size > data.len() {
            break;
        }
        count += 1;
        pos += size;
    }
    count
}

/// Read pitm primary item ID from meta children.
fn find_pitm_item_id(meta_body: &[u8]) -> Option<u32> {
    let (start, _end) = find_subbox_in(meta_body, b"pitm")?;
    let version = meta_body[start + 8];
    if version == 0 {
        let id = u16::from_be_bytes(meta_body[start + 12..start + 14].try_into().ok()?) as u32;
        Some(id)
    } else {
        let id = u32::from_be_bytes(meta_body[start + 12..start + 16].try_into().ok()?);
        Some(id)
    }
}

/// Find max item_id from iinf entries.
fn find_max_item_id(meta_body: &[u8]) -> Option<u32> {
    let (start, end) = find_subbox_in(meta_body, b"iinf")?;
    let version = meta_body[start + 8];
    let entries_start = if version == 0 { start + 14 } else { start + 16 };
    let mut max_id = 0u32;
    let mut pos = entries_start;
    while pos + 8 < end {
        let entry_size = u32::from_be_bytes(meta_body[pos..pos + 4].try_into().ok()?) as usize;
        if entry_size < 8 || pos + entry_size > end {
            break;
        }
        let entry_type = &meta_body[pos + 4..pos + 8];
        if entry_type == b"infe" {
            let infe_version = meta_body[pos + 8];
            let item_id = if infe_version < 3 {
                u16::from_be_bytes(meta_body[pos + 12..pos + 14].try_into().unwrap_or([0; 2])) as u32
            } else {
                u32::from_be_bytes(meta_body[pos + 12..pos + 16].try_into().unwrap_or([0; 4]))
            };
            if item_id > max_id {
                max_id = item_id;
            }
        }
        pos += entry_size;
    }
    Some(max_id)
}

/// Build an infe (ItemInfoEntry) box for an HEVC thumbnail.
/// Uses version 2 with u16 item_id for broad compatibility.
fn build_infe_entry(item_id: u32) -> Vec<u8> {
    // infe v2: [size:4]["infe":4][version:1=2][flags:3=0]
    //          [item_id:2][item_protection_index:2][item_type:4]["item_name\0"]
    let item_name = b"\0"; // empty name
    let box_size = 8 + 4 + 2 + 2 + 4 + item_name.len();
    let mut buf = Vec::with_capacity(box_size);
    buf.extend_from_slice(&(box_size as u32).to_be_bytes());
    buf.extend_from_slice(b"infe");
    buf.push(2); // version
    buf.extend_from_slice(&[0, 0, 0]); // flags
    buf.extend_from_slice(&(item_id as u16).to_be_bytes()); // item_id
    buf.extend_from_slice(&0u16.to_be_bytes()); // protection_index
    buf.extend_from_slice(b"hvc1"); // item_type — HEVC coded image
    buf.extend_from_slice(item_name); // name (null-terminated empty)
    buf
}

/// Build an iloc entry. We need to match the existing iloc's offset/length/base_offset sizes.
fn build_iloc_entry_v1(
    item_id: u32,
    offset: u64,
    length: u64,
    meta_body: &[u8],
) -> Result<Vec<u8>, String> {
    let (iloc_start, _iloc_end) = find_subbox_in(meta_body, b"iloc")
        .ok_or("no iloc")?;
    let version = meta_body[iloc_start + 8];
    let size_info = u16::from_be_bytes(
        meta_body[iloc_start + 12..iloc_start + 14].try_into().unwrap(),
    );
    let offset_size = ((size_info >> 12) & 0xF) as usize;
    let length_size = ((size_info >> 8) & 0xF) as usize;
    let base_offset_size = ((size_info >> 4) & 0xF) as usize;
    let _index_size = (size_info & 0xF) as usize;

    let mut buf = Vec::new();

    // item_id: u16 for v0/v1, u32 for v2
    if version < 2 {
        buf.extend_from_slice(&(item_id as u16).to_be_bytes());
    } else {
        buf.extend_from_slice(&item_id.to_be_bytes());
    }

    // construction_method: v1/v2 only, u16 with 4-bit field
    if version >= 1 {
        buf.extend_from_slice(&0u16.to_be_bytes()); // file offset method
    }

    // data_reference_index: u16
    buf.extend_from_slice(&0u16.to_be_bytes());

    // base_offset
    write_uint(&mut buf, 0, base_offset_size);

    // extent_count: u16
    buf.extend_from_slice(&1u16.to_be_bytes());

    // extent: offset + length (no index for v0/v1)
    write_uint(&mut buf, offset, offset_size);
    write_uint(&mut buf, length, length_size);

    Ok(buf)
}

fn write_uint(buf: &mut Vec<u8>, val: u64, size: usize) {
    match size {
        0 => {}
        2 => buf.extend_from_slice(&(val as u16).to_be_bytes()),
        4 => buf.extend_from_slice(&(val as u32).to_be_bytes()),
        8 => buf.extend_from_slice(&val.to_be_bytes()),
        _ => {}
    }
}

fn read_uint(data: &[u8], size: usize) -> u64 {
    match size {
        0 => 0,
        2 => u16::from_be_bytes(data[..2].try_into().unwrap()) as u64,
        4 => u32::from_be_bytes(data[..4].try_into().unwrap()) as u64,
        8 => u64::from_be_bytes(data[..8].try_into().unwrap()),
        _ => 0,
    }
}

/// Build an ispe box.
fn build_ispe_box(width: u32, height: u32) -> Vec<u8> {
    // ispe: [size:4]["ispe":4][version:1=0][flags:3=0][width:4][height:4]
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&20u32.to_be_bytes());
    buf.extend_from_slice(b"ispe");
    buf.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    buf.extend_from_slice(&width.to_be_bytes());
    buf.extend_from_slice(&height.to_be_bytes());
    buf
}

/// Build an ipma entry (PropertyAssociations) for one item.
#[allow(dead_code)]
fn build_ipma_association(item_id: u32, prop_index: u16, ipma_version: u8, flags_byte: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    // item_id: u16 for v0, u32 for v1
    if ipma_version < 1 {
        buf.extend_from_slice(&(item_id as u16).to_be_bytes());
    } else {
        buf.extend_from_slice(&item_id.to_be_bytes());
    }
    // association_count: u8
    buf.push(1);
    // Each association: if flags & 1 (large prop index): [essential:1 + index:15], else [essential:1 + index:7]
    let large = (flags_byte & 1) != 0;
    if large {
        let val = 0u16 | (prop_index & 0x7FFF); // essential=false (bit 15 = 0)
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        let val = (prop_index & 0x7F) as u8; // essential=false (bit 7 = 0)
        buf.push(val);
    }
    buf
}

/// Build an ipma entry with TWO property associations for an HEVC thumbnail:
///   1. hvcC — essential (required for decoding)
///   2. ispe — non-essential (descriptive)
fn build_ipma_association_hevc(
    item_id: u32,
    hvcc_prop_index: u16,
    ispe_prop_index: u16,
    ipma_version: u8,
    flags_byte: u8,
) -> Vec<u8> {
    let mut buf = Vec::new();
    if ipma_version < 1 {
        buf.extend_from_slice(&(item_id as u16).to_be_bytes());
    } else {
        buf.extend_from_slice(&item_id.to_be_bytes());
    }
    buf.push(2); // association_count = 2
    let large = (flags_byte & 1) != 0;
    if large {
        // hvcC: essential=true (bit 15 set)
        let val = 0x8000u16 | (hvcc_prop_index & 0x7FFF);
        buf.extend_from_slice(&val.to_be_bytes());
        // ispe: essential=false
        let val = ispe_prop_index & 0x7FFF;
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        // hvcC: essential=true (bit 7 set)
        let val = 0x80u8 | (hvcc_prop_index & 0x7F) as u8;
        buf.push(val);
        // ispe: essential=false
        let val = (ispe_prop_index & 0x7F) as u8;
        buf.push(val);
    }
    buf
}

/// Build a thmb reference entry (for appending into an existing iref box).
fn build_iref_thmb_entry(thumb_id: u32, primary_id: u32) -> Vec<u8> {
    // SingleItemTypeReferenceBox: [size:4]["thmb":4][from_item_id:2][ref_count:2][to_item_id:2]
    // Using v0 (u16 item IDs) for compatibility
    let mut buf = Vec::with_capacity(14);
    buf.extend_from_slice(&14u32.to_be_bytes());
    buf.extend_from_slice(b"thmb");
    buf.extend_from_slice(&(thumb_id as u16).to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    buf.extend_from_slice(&(primary_id as u16).to_be_bytes());
    buf
}

/// Build a complete iref box with one thmb reference.
fn build_iref_box(thumb_id: u32, primary_id: u32) -> Vec<u8> {
    let entry = build_iref_thmb_entry(thumb_id, primary_id);
    let box_size = 12 + entry.len(); // 8 header + 4 fullbox fields
    let mut buf = Vec::with_capacity(box_size);
    buf.extend_from_slice(&(box_size as u32).to_be_bytes());
    buf.extend_from_slice(b"iref");
    buf.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags
    buf.extend_from_slice(&entry);
    buf
}

/// Fix iloc offsets after meta has been patched and thumb mdat will be inserted.
fn fix_iloc_offsets(
    meta_body: &mut [u8],
    thumb_id: u32,
    thumb_data_offset: u64,
    total_shift: i64,
    _original_meta_start: usize,
) -> Result<(), String> {
    let (iloc_start, iloc_end) = find_subbox_in(meta_body, b"iloc")
        .ok_or("no iloc for offset fixup")?;
    let version = meta_body[iloc_start + 8];
    let size_info = u16::from_be_bytes(
        meta_body[iloc_start + 12..iloc_start + 14].try_into().unwrap(),
    );
    let offset_size = ((size_info >> 12) & 0xF) as usize;
    let length_size = ((size_info >> 8) & 0xF) as usize;
    let base_offset_size = ((size_info >> 4) & 0xF) as usize;
    let index_size = (size_info & 0xF) as usize;

    let mut pos = iloc_start + 14; // after header + size_info
    if version >= 1 {
        pos += 0; // no extra fields in header for v1/v2 beyond size_info
    }

    let item_count = if version < 2 {
        let c = u16::from_be_bytes(meta_body[pos..pos + 2].try_into().unwrap()) as u32;
        pos += 2;
        c
    } else {
        let c = u32::from_be_bytes(meta_body[pos..pos + 4].try_into().unwrap());
        pos += 4;
        c
    };

    for _ in 0..item_count {
        if pos >= iloc_end {
            break;
        }

        let item_id = if version < 2 {
            let id = u16::from_be_bytes(meta_body[pos..pos + 2].try_into().unwrap()) as u32;
            pos += 2;
            id
        } else {
            let id = u32::from_be_bytes(meta_body[pos..pos + 4].try_into().unwrap());
            pos += 4;
            id
        };

        let construction_method = if version >= 1 {
            let cm = u16::from_be_bytes(meta_body[pos..pos + 2].try_into().unwrap());
            pos += 2;
            cm & 0xF
        } else {
            0
        };

        // data_reference_index
        pos += 2;

        // base_offset
        let base_offset_pos = pos;
        let base_offset = read_uint(&meta_body[pos..], base_offset_size);
        pos += base_offset_size;

        // extent_count
        let extent_count = u16::from_be_bytes(meta_body[pos..pos + 2].try_into().unwrap());
        pos += 2;

        for _ in 0..extent_count {
            // index (v1/v2 only)
            if (version == 1 || version == 2) && index_size > 0 {
                pos += index_size;
            }

            let extent_offset_pos = pos;
            let extent_offset = read_uint(&meta_body[pos..], offset_size);
            pos += offset_size;

            let _extent_length = read_uint(&meta_body[pos..], length_size);
            pos += length_size;

            if construction_method != 0 {
                continue; // only fix file-relative offsets
            }

            if item_id == thumb_id {
                // Set thumb offset to point at the inserted thumb mdat data
                write_uint_at(meta_body, extent_offset_pos, thumb_data_offset, offset_size);
            } else {
                // Shift existing offset: data after meta moved by total_shift
                if base_offset > 0 {
                    // base_offset handles the shift — do it once per item, not per extent
                } else if extent_offset > 0 {
                    let new_offset = (extent_offset as i64 + total_shift) as u64;
                    write_uint_at(meta_body, extent_offset_pos, new_offset, offset_size);
                }
            }
        }

        // Fix base_offset for non-thumb items
        if item_id != thumb_id && construction_method == 0 && base_offset > 0 {
            let new_base = (base_offset as i64 + total_shift) as u64;
            write_uint_at(meta_body, base_offset_pos, new_base, base_offset_size);
        }
    }

    Ok(())
}

fn write_uint_at(buf: &mut [u8], pos: usize, val: u64, size: usize) {
    match size {
        2 => buf[pos..pos + 2].copy_from_slice(&(val as u16).to_be_bytes()),
        4 => buf[pos..pos + 4].copy_from_slice(&(val as u32).to_be_bytes()),
        8 => buf[pos..pos + 8].copy_from_slice(&val.to_be_bytes()),
        _ => {}
    }
}

/// Scan ISOBMFF top-level boxes to find one by its fourcc type.
/// Returns `(start, end)` byte offsets of the box in the file data.
fn find_isobmff_box(data: &[u8], fourcc: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
        let box_type = &data[pos + 4..pos + 8];
        let box_size = if size == 1 {
            if pos + 16 > data.len() {
                break;
            }
            u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap())
        } else if size == 0 {
            (data.len() - pos) as u64
        } else {
            size
        };
        if box_size < 8 || pos as u64 + box_size > data.len() as u64 {
            break;
        }
        if box_type == fourcc {
            return Some((pos, pos + box_size as usize));
        }
        pos += box_size as usize;
    }
    None
}

// ---------------------------------------------------------------------------
// JPEG — EXIF APP1 thumbnail injection
// ---------------------------------------------------------------------------

fn process_jpeg(path: &Path, dry_run: bool) -> FileResult {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return FileResult::Error(format!("read: {e}")),
    };

    // Check if it already has an EXIF thumbnail
    if has_exif_thumbnail(&data) {
        return FileResult::AlreadyHasThumb;
    }

    if dry_run {
        return FileResult::WouldInject;
    }

    let start = std::time::Instant::now();

    // Decode to get pixels for thumbnail
    let img = match image::load_from_memory(&data) {
        Ok(i) => i,
        Err(e) => return FileResult::Error(format!("decode: {e}")),
    };
    let thumb = img.thumbnail(416, 416);
    let thumb_rgb = thumb.to_rgb8();

    // Encode thumbnail as JPEG
    let mut thumb_jpeg = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumb_jpeg, 75);
    if let Err(e) = encoder.encode(
        thumb_rgb.as_raw(),
        thumb_rgb.width(),
        thumb_rgb.height(),
        image::ExtendedColorType::Rgb8,
    ) {
        return FileResult::Error(format!("thumb encode: {e}"));
    }

    // Build EXIF APP1 with thumbnail
    let exif_app1 = build_exif_app1_with_thumbnail(&thumb_jpeg, &data);

    // Splice into JPEG: SOI + new APP1 + rest (skip old APP1 if present)
    let mut output = Vec::with_capacity(data.len() + exif_app1.len());
    output.extend_from_slice(&[0xFF, 0xD8]); // SOI
    output.extend_from_slice(&exif_app1);

    // Skip SOI and any existing APP1 in original
    let rest_start = find_jpeg_rest_start(&data);
    output.extend_from_slice(&data[rest_start..]);

    if let Err(e) = std::fs::write(path, &output) {
        return FileResult::Error(format!("write: {e}"));
    }

    let ms = start.elapsed().as_secs_f64() * 1000.0;
    FileResult::Injected { ms }
}

// ---------------------------------------------------------------------------
// WebP / PNG — EXIF container injection
// ---------------------------------------------------------------------------

fn process_exif_container(path: &Path, dry_run: bool) -> FileResult {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return FileResult::Error(format!("read: {e}")),
    };

    // Check for existing EXIF thumbnail
    if has_exif_thumbnail(&data) {
        return FileResult::AlreadyHasThumb;
    }

    if dry_run {
        return FileResult::WouldInject;
    }

    let start = std::time::Instant::now();

    // Decode to get pixels for thumbnail
    let img = match image::load_from_memory(&data) {
        Ok(i) => i,
        Err(e) => return FileResult::Error(format!("decode: {e}")),
    };
    let thumb = img.thumbnail(416, 416);
    let thumb_rgb = thumb.to_rgb8();

    // Encode thumbnail as JPEG
    let mut thumb_jpeg = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumb_jpeg, 75);
    if let Err(e) = encoder.encode(
        thumb_rgb.as_raw(),
        thumb_rgb.width(),
        thumb_rgb.height(),
        image::ExtendedColorType::Rgb8,
    ) {
        return FileResult::Error(format!("thumb encode: {e}"));
    }

    // Build EXIF/TIFF block with thumbnail
    let exif_tiff = build_exif_tiff_with_thumbnail(&thumb_jpeg);

    if is_webp(path) {
        match inject_exif_webp(&data, &exif_tiff) {
            Ok(output) => {
                if let Err(e) = std::fs::write(path, &output) {
                    return FileResult::Error(format!("write: {e}"));
                }
            }
            Err(e) => return FileResult::Error(e),
        }
    } else if is_png(path) {
        match inject_exif_png(&data, &exif_tiff) {
            Ok(output) => {
                if let Err(e) = std::fs::write(path, &output) {
                    return FileResult::Error(format!("write: {e}"));
                }
            }
            Err(e) => return FileResult::Error(e),
        }
    }

    let ms = start.elapsed().as_secs_f64() * 1000.0;
    FileResult::Injected { ms }
}

// ---------------------------------------------------------------------------
// EXIF helpers
// ---------------------------------------------------------------------------

/// Check if raw file data contains an EXIF thumbnail (JPEG IFD1).
fn has_exif_thumbnail(data: &[u8]) -> bool {
    let cursor = std::io::Cursor::new(data);
    let reader = exif::Reader::new();
    if let Ok(exif) = reader.read_from_container(&mut std::io::BufReader::new(cursor)) {
        for field in exif.fields() {
            if field.tag == exif::Tag::JPEGInterchangeFormat {
                return true;
            }
        }
    }
    false
}

/// Read EXIF orientation from raw bytes. Returns 1 if not found.
fn read_orientation(data: &[u8]) -> u32 {
    let cursor = std::io::Cursor::new(data);
    let reader = exif::Reader::new();
    if let Ok(exif) = reader.read_from_container(&mut std::io::BufReader::new(cursor))
        && let Some(field) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        && let Some(v) = field.value.get_uint(0)
        && (1..=8).contains(&v)
    {
        return v;
    }
    1
}

/// Build a minimal EXIF APP1 segment with a JPEG thumbnail.
/// If the original data has existing EXIF, preserves the orientation tag.
fn build_exif_app1_with_thumbnail(thumb_jpeg: &[u8], original: &[u8]) -> Vec<u8> {
    let orientation = read_orientation(original);
    let tiff = build_exif_tiff_with_thumbnail_and_orientation(thumb_jpeg, orientation);

    let mut app1 = Vec::new();
    app1.extend_from_slice(&[0xFF, 0xE1]); // APP1 marker
    let length = (tiff.len() + 2 + 6) as u16; // +2 length field, +6 "Exif\0\0"
    app1.extend_from_slice(&length.to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);
    app1
}

/// Build EXIF TIFF structure with just orientation + thumbnail.
fn build_exif_tiff_with_thumbnail(thumb_jpeg: &[u8]) -> Vec<u8> {
    build_exif_tiff_with_thumbnail_and_orientation(thumb_jpeg, 1)
}

fn build_exif_tiff_with_thumbnail_and_orientation(thumb_jpeg: &[u8], orientation: u32) -> Vec<u8> {
    let mut tiff = Vec::new();

    // TIFF header (little-endian)
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // offset to IFD0

    // IFD0: 1 tag (Orientation)
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation tag
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&(orientation as u16).to_le_bytes());
    tiff.extend_from_slice(&0u16.to_le_bytes()); // padding

    // Next IFD offset → IFD1
    let ifd1_offset = tiff.len() as u32 + 4;
    tiff.extend_from_slice(&ifd1_offset.to_le_bytes());

    // IFD1: 2 tags
    tiff.extend_from_slice(&2u16.to_le_bytes());
    let thumb_offset = tiff.len() as u32 + 12 * 2 + 4;

    // JPEGInterchangeFormat
    tiff.extend_from_slice(&0x0201u16.to_le_bytes());
    tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&thumb_offset.to_le_bytes());

    // JPEGInterchangeFormatLength
    tiff.extend_from_slice(&0x0202u16.to_le_bytes());
    tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&(thumb_jpeg.len() as u32).to_le_bytes());

    // No next IFD
    tiff.extend_from_slice(&0u32.to_le_bytes());

    // Thumbnail data
    tiff.extend_from_slice(thumb_jpeg);

    tiff
}

/// Find where the JPEG data starts after SOI + any existing APP1.
fn find_jpeg_rest_start(data: &[u8]) -> usize {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return 2; // just skip SOI
    }
    let mut pos = 2;
    // Skip consecutive APP markers (APP0=FFE0, APP1=FFE1, etc.)
    while pos + 3 < data.len() && data[pos] == 0xFF && (data[pos + 1] & 0xF0) == 0xE0 {
        let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 2 + len;
    }
    pos
}

// ---------------------------------------------------------------------------
// WebP EXIF injection
// ---------------------------------------------------------------------------

fn inject_exif_webp(data: &[u8], exif_tiff: &[u8]) -> Result<Vec<u8>, String> {
    // WebP is RIFF-based: "RIFF" <size> "WEBP" <chunks...>
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err("not a WebP file".into());
    }

    let mut output = Vec::with_capacity(data.len() + exif_tiff.len() + 100);

    // Check if there's a VP8X chunk (extended format)
    let has_vp8x = data.len() > 16 && &data[12..16] == b"VP8X";

    if has_vp8x {
        // Copy header + VP8X chunk, set EXIF flag, then insert EXIF chunk
        output.extend_from_slice(&data[..12]); // RIFF + size + WEBP

        // VP8X chunk: 4 bytes fourcc + 4 bytes size + payload
        let vp8x_size = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as usize;
        let vp8x_end = 20 + vp8x_size;

        // Copy VP8X with EXIF flag set (bit 3 of flags byte at offset 20)
        output.extend_from_slice(&data[12..20]); // VP8X fourcc + size
        if vp8x_end > 20 {
            let mut flags = data[20];
            flags |= 0x08; // Set EXIF flag
            output.push(flags);
            output.extend_from_slice(&data[21..vp8x_end]);
        }

        // Copy remaining chunks (skip any existing EXIF chunk)
        let mut pos = vp8x_end;
        while pos + 8 <= data.len() {
            let fourcc = &data[pos..pos + 4];
            let chunk_size = u32::from_le_bytes([
                data[pos + 4],
                data[pos + 5],
                data[pos + 6],
                data[pos + 7],
            ]) as usize;
            let chunk_total = 8 + chunk_size + (chunk_size & 1); // pad to even

            if fourcc == b"EXIF" {
                pos += chunk_total; // skip existing EXIF
                continue;
            }
            output.extend_from_slice(&data[pos..pos + chunk_total.min(data.len() - pos)]);
            pos += chunk_total;
        }

        // Add new EXIF chunk
        output.extend_from_slice(b"EXIF");
        output.extend_from_slice(&(exif_tiff.len() as u32).to_le_bytes());
        output.extend_from_slice(exif_tiff);
        if exif_tiff.len() & 1 != 0 {
            output.push(0); // pad to even
        }
    } else {
        // Simple WebP (no VP8X) — need to upgrade to extended format
        // This is more complex; for now, report unsupported
        return Err("simple WebP without VP8X — upgrade not implemented yet".into());
    }

    // Update RIFF size
    let riff_size = (output.len() - 8) as u32;
    output[4..8].copy_from_slice(&riff_size.to_le_bytes());

    Ok(output)
}

// ---------------------------------------------------------------------------
// PNG eXIf injection
// ---------------------------------------------------------------------------

fn inject_exif_png(data: &[u8], exif_tiff: &[u8]) -> Result<Vec<u8>, String> {
    // PNG: 8-byte signature + chunks (type[4] + length[4] + data + CRC[4])
    if data.len() < 8 || &data[0..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("not a PNG file".into());
    }

    let mut output = Vec::with_capacity(data.len() + exif_tiff.len() + 20);
    output.extend_from_slice(&data[0..8]); // PNG signature

    let mut pos = 8;
    let mut inserted = false;

    while pos + 8 <= data.len() {
        let length = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
            as usize;
        let chunk_type = &data[pos + 4..pos + 8];
        let chunk_total = 4 + 4 + length + 4; // length + type + data + CRC

        // Skip existing eXIf chunk
        if chunk_type == b"eXIf" {
            pos += chunk_total;
            continue;
        }

        // Insert our eXIf chunk before the first IDAT chunk
        if !inserted && chunk_type == b"IDAT" {
            // eXIf chunk
            let exif_len = exif_tiff.len() as u32;
            output.extend_from_slice(&exif_len.to_be_bytes());
            output.extend_from_slice(b"eXIf");
            output.extend_from_slice(exif_tiff);
            // CRC over type + data
            let crc = crc32_png(b"eXIf", exif_tiff);
            output.extend_from_slice(&crc.to_be_bytes());
            inserted = true;
        }

        // Copy this chunk
        let end = (pos + chunk_total).min(data.len());
        output.extend_from_slice(&data[pos..end]);
        pos += chunk_total;
    }

    Ok(output)
}

/// Compute CRC32 for a PNG chunk (type + data).
fn crc32_png(chunk_type: &[u8], chunk_data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in chunk_type.iter().chain(chunk_data.iter()) {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xFFFFFFFF
}
