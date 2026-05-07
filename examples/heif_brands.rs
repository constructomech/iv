// One-shot scanner for HEIF/HEIC files in a directory tree.
//
// Reports the distribution of codec brands (HEVC vs AV1) so we can estimate
// how many files would benefit from the FFmpeg `image2` HEIC fast path
// (HEVC) versus how many fall through to libheif (AV1 + everything else).
//
// Usage: cargo run --release --example heif_brands -- <root-dir>

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const HEAD_BYTES: usize = 16384;

#[derive(Default, Debug, Clone)]
struct Counts {
    files: usize,
    hevc_items: usize,
    av1_items: usize,
    has_thumbnail_item: usize,
    parse_failed: usize,
    bytes: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: heif_brands <root-dir> [--list-av1]");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[1]);
    let list_av1 = args.iter().any(|a| a == "--list-av1");

    let mut by_major: BTreeMap<String, Counts> = BTreeMap::new();
    let mut total = Counts::default();
    let mut compat_brands: BTreeMap<String, usize> = BTreeMap::new();
    let mut av1_files: Vec<PathBuf> = Vec::new();

    walk(&root, &mut |path| {
        if !is_heif(path) {
            return;
        }
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mut head = vec![0u8; HEAD_BYTES];
        let read = match File::open(path).and_then(|mut f| f.read(&mut head)) {
            Ok(n) => n,
            Err(_) => {
                let entry = by_major.entry("<read-error>".into()).or_default();
                entry.files += 1;
                entry.parse_failed += 1;
                entry.bytes += len;
                total.files += 1;
                total.parse_failed += 1;
                total.bytes += len;
                return;
            }
        };
        head.truncate(read);

        let info = match parse_isobmff(&head) {
            Some(info) => info,
            None => {
                let entry = by_major.entry("<parse-failed>".into()).or_default();
                entry.files += 1;
                entry.parse_failed += 1;
                entry.bytes += len;
                total.files += 1;
                total.parse_failed += 1;
                total.bytes += len;
                return;
            }
        };

        for c in &info.compatible_brands {
            *compat_brands.entry(c.clone()).or_default() += 1;
        }

        let entry = by_major.entry(info.major_brand.clone()).or_default();
        entry.files += 1;
        entry.hevc_items += info.hevc_items;
        entry.av1_items += info.av1_items;
        if info.has_thumbnail_item {
            entry.has_thumbnail_item += 1;
        }
        entry.bytes += len;

        total.files += 1;
        total.hevc_items += info.hevc_items;
        total.av1_items += info.av1_items;
        if info.has_thumbnail_item {
            total.has_thumbnail_item += 1;
        }
        total.bytes += len;

        if info.av1_items > 0 && info.hevc_items == 0 {
            av1_files.push(path.to_path_buf());
        }
    });

    println!("Scanned root: {}", root.display());
    println!("Total HEIF/HEIC files: {}", total.files);
    println!(
        "Total bytes: {} ({:.1} GiB)",
        total.bytes,
        total.bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    println!();
    println!("Codec classification:");
    let av1_only = total.files.saturating_sub(count_files_with_hevc(&by_major));
    let hevc_only = count_files_with_hevc_only(&by_major);
    let mixed = count_files_with_mixed(&by_major);
    println!(
        "  files with HEVC items only: {} ({:.1}%)",
        hevc_only,
        pct(hevc_only, total.files)
    );
    println!(
        "  files with AV1 items only:  {} ({:.1}%)",
        av1_only,
        pct(av1_only, total.files)
    );
    println!(
        "  files with HEVC+AV1 items:  {} ({:.1}%)",
        mixed,
        pct(mixed, total.files)
    );
    println!(
        "  files with neither parsed:  {} ({:.1}%)",
        total.parse_failed,
        pct(total.parse_failed, total.files)
    );
    println!();
    println!("By major brand:");
    for (brand, c) in &by_major {
        println!(
            "  {:6} files={:5} hevc_items={:5} av1_items={:5} thumb_item={:5} bytes_GiB={:.2}",
            brand,
            c.files,
            c.hevc_items,
            c.av1_items,
            c.has_thumbnail_item,
            c.bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        );
    }
    println!();
    println!("Compatible brands (file count where present):");
    for (brand, count) in &compat_brands {
        println!("  {brand:6} {count}");
    }

    if list_av1 && !av1_files.is_empty() {
        println!();
        println!("AV1-only files ({}):", av1_files.len());
        for p in av1_files.iter().take(50) {
            println!("  {}", p.display());
        }
        if av1_files.len() > 50 {
            println!("  ... and {} more", av1_files.len() - 50);
        }
    }
}

fn pct(n: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * n as f64 / total as f64
    }
}

fn count_files_with_hevc(by_major: &BTreeMap<String, Counts>) -> usize {
    by_major
        .values()
        .filter(|c| c.hevc_items > 0)
        .map(|c| c.files)
        .sum()
}

fn count_files_with_hevc_only(by_major: &BTreeMap<String, Counts>) -> usize {
    // Approximate: a brand bucket either has hevc>0,av1=0 or vice versa for its files.
    // We don't track per-file mixedness here; the by_major stats approximate
    // that. For a cleaner answer we'd track per-file kinds, but the totals
    // below are good enough for our purposes.
    by_major
        .values()
        .filter(|c| c.hevc_items > 0 && c.av1_items == 0)
        .map(|c| c.files)
        .sum()
}

fn count_files_with_mixed(by_major: &BTreeMap<String, Counts>) -> usize {
    by_major
        .values()
        .filter(|c| c.hevc_items > 0 && c.av1_items > 0)
        .map(|c| c.files)
        .sum()
}

fn is_heif(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase()),
        Some(ext) if ext == "heic" || ext == "heif" || ext == "hif" || ext == "avif"
    )
}

fn walk<F: FnMut(&Path)>(root: &Path, f: &mut F) {
    let entries = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk(&path, f);
        } else if ft.is_file() {
            f(&path);
        }
    }
}

#[derive(Debug, Default)]
struct IsobmffInfo {
    major_brand: String,
    compatible_brands: Vec<String>,
    hevc_items: usize,
    av1_items: usize,
    has_thumbnail_item: bool,
}

fn parse_isobmff(buf: &[u8]) -> Option<IsobmffInfo> {
    let mut info = IsobmffInfo::default();

    let mut p = 0usize;
    while p + 8 <= buf.len() {
        let size32 = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]) as usize;
        let box_type = ascii(&buf[p + 4..p + 8]);
        // size==1 means the real 64-bit size lives in the 8 bytes after the type.
        let (header_size, total_size) = if size32 == 1 {
            if p + 16 > buf.len() {
                break;
            }
            let big = u64::from_be_bytes([
                buf[p + 8],
                buf[p + 9],
                buf[p + 10],
                buf[p + 11],
                buf[p + 12],
                buf[p + 13],
                buf[p + 14],
                buf[p + 15],
            ]);
            (16usize, big as usize)
        } else if size32 == 0 {
            (8usize, buf.len() - p) // box extends to end-of-file
        } else {
            (8usize, size32)
        };
        if total_size < header_size {
            break;
        }
        let body_end = p + total_size;
        let body_start = p + header_size;
        if body_end > buf.len() {
            // Truncated box (we only loaded the head). Keep what we already have.
            break;
        }

        match box_type.as_str() {
            "ftyp" => {
                if body_start + 8 <= body_end {
                    info.major_brand = ascii(&buf[body_start..body_start + 4]);
                    let mut q = body_start + 8;
                    while q + 4 <= body_end {
                        info.compatible_brands.push(ascii(&buf[q..q + 4]));
                        q += 4;
                    }
                }
            }
            "meta" => {
                let meta_body = body_start + 4;
                if meta_body <= body_end {
                    parse_meta(&buf[meta_body..body_end], &mut info);
                }
            }
            _ => {}
        }
        p = body_end;
    }
    if info.major_brand.is_empty() {
        return None;
    }
    Some(info)
}

fn parse_meta(buf: &[u8], info: &mut IsobmffInfo) {
    let mut p = 0usize;
    while p + 8 <= buf.len() {
        let size = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]) as usize;
        let box_type = ascii(&buf[p + 4..p + 8]);
        let header_size = if size == 1 { 16 } else { 8 };
        let body_end = if size == 0 {
            buf.len()
        } else if size < 8 {
            return;
        } else {
            p + size
        };
        let body_start = p + header_size;
        if body_end > buf.len() {
            break;
        }
        if box_type == "iinf" {
            parse_iinf(&buf[body_start..body_end], info);
        }
        p = body_end;
    }
}

fn parse_iinf(buf: &[u8], info: &mut IsobmffInfo) {
    if buf.len() < 4 {
        return;
    }
    let version = buf[0];
    let count_size = if version == 0 { 2 } else { 4 };
    if buf.len() < 4 + count_size {
        return;
    }
    let mut p = 4 + count_size;
    while p + 8 <= buf.len() {
        let size = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]) as usize;
        let box_type = ascii(&buf[p + 4..p + 8]);
        let body_end = if size == 0 {
            buf.len()
        } else if size < 8 {
            return;
        } else {
            p + size
        };
        let body_start = p + 8;
        if body_end > buf.len() {
            break;
        }
        if box_type == "infe" {
            parse_infe(&buf[body_start..body_end], info);
        }
        p = body_end;
    }
}

fn parse_infe(buf: &[u8], info: &mut IsobmffInfo) {
    if buf.len() < 4 {
        return;
    }
    let version = buf[0];
    if version < 2 {
        return;
    }
    let item_type_off = if version == 2 { 4 + 2 + 2 } else { 4 + 4 + 2 };
    if buf.len() < item_type_off + 4 {
        return;
    }
    let item_type = ascii(&buf[item_type_off..item_type_off + 4]);
    match item_type.as_str() {
        "hvc1" | "hev1" => info.hevc_items += 1,
        "av01" => info.av1_items += 1,
        "thmb" => info.has_thumbnail_item = true,
        _ => {}
    }
}

fn ascii(b: &[u8]) -> String {
    b.iter()
        .map(|c| {
            if c.is_ascii_graphic() {
                *c as char
            } else {
                '.'
            }
        })
        .collect()
}
