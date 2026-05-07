/// Apples-to-apples decode benchmark for iv.
///
/// Decodes a source raw file via LibRaw, then encodes the same pixels into
/// every supported format at the same resolution.  Benchmarks grid-thumbnail
/// and full-decode performance for each format.
///
/// Usage:
///   cargo run --release --bin iv-bench -- --raw <source.raw> [--raw extra.raw ...]
///
/// Generated files are cached next to the source. Delete to regenerate.
///
/// HEIC fixture: real-world HEIC content from iPhones is HEVC-coded, but the
/// libheif build shipped with iv only includes an AV1 encoder (HEVC encoders
/// are GPL via x265, which we deliberately exclude from the main build). So
/// `iv-bench` does not synthesize HEIC fixtures. Instead, it pulls a real
/// HEIC from disk in this order:
///   1. A sibling `<source-stem>.heic` (or `.HEIC`) next to the RAW.
///   2. Any `*.heic` / `*.HEIC` in the RAW's directory.
///   3. `IV_BENCH_HEIC_FIXTURE` environment variable.
/// If none of these is found, the HEIC row is skipped with a printed note.
use image::{ImageFormat, RgbImage, RgbaImage};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

const ITERATIONS: usize = 5;

fn parse_raw_args(args: &[String]) -> Vec<PathBuf> {
    if args.is_empty() {
        print_usage_and_exit();
    }

    let mut raw_files = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--raw" => {
                index += 1;
                if index >= args.len() {
                    eprintln!("Missing path after --raw");
                    print_usage_and_exit();
                }
                raw_files.push(validate_existing_path(&args[index]));
            }
            "--dng" => {
                eprintln!("--dng has been renamed to --raw");
                index += 1;
                if index >= args.len() {
                    eprintln!("Missing path after --dng");
                    print_usage_and_exit();
                }
                raw_files.push(validate_existing_path(&args[index]));
            }
            "-h" | "--help" => print_usage_and_exit(),
            arg if arg.starts_with('-') => {
                eprintln!("Unknown option: {arg}");
                print_usage_and_exit();
            }
            arg => {
                raw_files.push(validate_existing_path(arg));
            }
        }
        index += 1;
    }

    if raw_files.is_empty() {
        print_usage_and_exit();
    }
    raw_files
}

fn validate_existing_path(value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if !path.exists() {
        eprintln!("File not found: {}", path.display());
        std::process::exit(1);
    }
    path
}

fn print_usage_and_exit() -> ! {
    eprintln!("Usage: iv-bench --raw <source.raw> [--raw extra.raw ...]");
    eprintln!(
        "The first --raw file seeds generated comparison images; later raw files are benchmarked as extra RAW rows."
    );
    eprintln!(
        "HEIC fixture: pulled from a sibling <stem>.heic, then any *.heic in the source's directory,"
    );
    eprintln!("              then $IV_BENCH_HEIC_FIXTURE. Skipped if none of those is found.");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let raw_files = parse_raw_args(&args);
    let source = raw_files[0].clone();
    let extra_raws = &raw_files[1..];

    // Decode source to full-res RGB
    println!("Decoding source via LibRaw: {}", source.display());
    let t0 = Instant::now();
    let src_data = std::fs::read(&source).expect("read failed");
    let decoded = iv::decode_raw_libraw(&src_data).unwrap_or_else(|| {
        eprintln!(
            "LibRaw failed to decode source raw file: {}",
            source.display()
        );
        std::process::exit(1);
    });
    println!(
        "  {}x{} in {:.0}ms\n",
        decoded.width,
        decoded.height,
        t0.elapsed().as_secs_f64() * 1000.0
    );

    let rgb =
        RgbImage::from_raw(decoded.width, decoded.height, rgba_to_rgb(&decoded.pixels)).unwrap();
    let rgba = RgbaImage::from_raw(decoded.width, decoded.height, decoded.pixels).unwrap();

    // Cache directory
    let bench_dir = source.parent().unwrap_or(Path::new(".")).join(format!(
        "{}_bench",
        source.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let heic_fixture = resolve_heic_fixture(&source);
    if !bench_dir.exists() || dir_is_empty(&bench_dir) {
        std::fs::create_dir_all(&bench_dir).unwrap();
        println!("Generating test images in {}", bench_dir.display());
        generate_formats(&bench_dir, &rgb, &rgba, heic_fixture.as_deref());
        println!();
    } else {
        println!("Using cached images in {}\n", bench_dir.display());
    }

    // Report what kind of HEIC the bench will use, if any.
    let heic_path = bench_dir.join("heic.heic");
    if heic_path.exists() {
        report_heic_codec(&heic_path);
    } else {
        println!(
            "HEIC fixture: <none found> (set $IV_BENCH_HEIC_FIXTURE or place a *.heic next to {} to include this row)",
            source.display()
        );
    }

    // Collect files to benchmark
    let mut files: Vec<(PathBuf, &str)> = Vec::new();
    files.push((source.clone(), "RAW"));
    for r in extra_raws {
        if r.exists() {
            files.push((r.clone(), "RAW"));
        }
    }
    for name in [
        "jpeg.jpg",
        "tiff.tiff",
        "png.png",
        "webp.webp",
        "heic.heic",
        "gif.gif",
    ] {
        let p = bench_dir.join(name);
        if p.exists() {
            files.push((p, "GEN"));
        }
    }

    // Main table
    let res = format!("{}x{}", rgb.width(), rgb.height());
    println!("Benchmark: {res}, {ITERATIONS} iterations, median\n");
    println!(
        "{:<30} {:>5} {:>9} {:>10} {:>10} {:>10}",
        "File", "Kind", "Size(KB)", "Thumb(ms)", "Full(ms)", "Pixels"
    );
    println!("{}", "-".repeat(80));

    for (path, kind) in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let kb = std::fs::metadata(path).map(|m| m.len() / 1024).unwrap_or(0);
        let (t_ms, t_px) = bench_thumb(path);
        let (f_ms, f_px) = bench_full(path, *kind == "RAW");
        let px = if f_px.is_empty() { &t_px } else { &f_px };
        println!(
            "{:<30} {:>5} {:>9} {:>10.1} {:>10.1} {:>10}",
            trunc(&name, 30),
            kind,
            kb,
            t_ms,
            f_ms,
            px,
        );
    }
    println!("{}", "-".repeat(80));
    println!("\nThumb = grid path | Full = full decode path (RAW via LibRaw)");
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

fn generate_formats(dir: &Path, rgb: &RgbImage, rgba: &RgbaImage, heic_fixture: Option<&Path>) {
    encode_step("  JPEG (q92+EXIF thumb)", || {
        let mut buf = Vec::new();
        let mut e = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 92);
        e.encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
        std::fs::write(dir.join("jpeg.jpg"), add_exif_thumb(&buf, rgb)).unwrap();
    });
    encode_step("  TIFF (uncompressed)", || {
        rgb.save_with_format(dir.join("tiff.tiff"), ImageFormat::Tiff)
            .unwrap();
    });
    encode_step("  PNG", || {
        rgba.save_with_format(dir.join("png.png"), ImageFormat::Png)
            .unwrap();
    });
    encode_step("  WebP", || {
        rgba.save_with_format(dir.join("webp.webp"), ImageFormat::WebP)
            .unwrap();
    });
    if let Some(src) = heic_fixture {
        encode_step(&format!("  HEIC (copy from {})", src.display()), || {
            std::fs::copy(src, dir.join("heic.heic"))
                .expect("failed to copy HEIC fixture into bench dir");
        });
    } else {
        eprintln!(
            "  HEIC: skipped — no real HEIC fixture found. Real iPhone content is HEVC, but the"
        );
        eprintln!(
            "        bundled libheif build only has an AV1 encoder, so synthesizing one would"
        );
        eprintln!("        give a misleading number. Place a *.heic next to the source RAW or set");
        eprintln!("        $IV_BENCH_HEIC_FIXTURE to include the HEIC row in this bench.");
    }
    encode_step("  GIF (256-color)", || {
        rgba.save_with_format(dir.join("gif.gif"), ImageFormat::Gif)
            .unwrap();
    });
}

fn encode_step(label: &str, f: impl FnOnce()) {
    print!("{label}...");
    std::io::stdout().flush().unwrap();
    let t = Instant::now();
    f();
    println!(" {:.1}s", t.elapsed().as_secs_f64());
}

fn add_exif_thumb(jpeg: &[u8], src: &RgbImage) -> Vec<u8> {
    let th = image::imageops::thumbnail(src, 160, 120);
    let mut tj = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut tj, 75)
        .encode(
            th.as_raw(),
            th.width(),
            th.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    let app1 = build_app1(&tj);
    let mut out = Vec::with_capacity(jpeg.len() + app1.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn build_app1(thumb: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(b"II");
    t.extend_from_slice(&42u16.to_le_bytes());
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&1u16.to_le_bytes());
    t.extend_from_slice(&0x0112u16.to_le_bytes());
    t.extend_from_slice(&3u16.to_le_bytes());
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&1u16.to_le_bytes());
    t.extend_from_slice(&0u16.to_le_bytes());
    let i1 = t.len() as u32 + 4;
    t.extend_from_slice(&i1.to_le_bytes());
    t.extend_from_slice(&2u16.to_le_bytes());
    let toff = t.len() as u32 + 12 * 2 + 4;
    t.extend_from_slice(&0x0201u16.to_le_bytes());
    t.extend_from_slice(&4u16.to_le_bytes());
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&toff.to_le_bytes());
    t.extend_from_slice(&0x0202u16.to_le_bytes());
    t.extend_from_slice(&4u16.to_le_bytes());
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&(thumb.len() as u32).to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    t.extend_from_slice(thumb);
    let mut a = Vec::new();
    a.extend_from_slice(&[0xFF, 0xE1]);
    a.extend_from_slice(&((t.len() + 8) as u16).to_be_bytes());
    a.extend_from_slice(b"Exif\0\0");
    a.extend_from_slice(&t);
    a
}

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(rgba.len() / 4 * 3);
    for c in rgba.chunks_exact(4) {
        v.extend_from_slice(&c[..3]);
    }
    v
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_thumb(path: &Path) -> (f64, String) {
    let heif = iv::is_heif_extension(path);
    if heif {
        let _ = iv::try_heif_thumbnail(path);
    } else {
        let _ = iv::try_embedded_from_bytes(&std::fs::read(path).unwrap_or_default());
    }
    let mut times = Vec::with_capacity(ITERATIONS);
    let mut px = String::new();
    for _ in 0..ITERATIONS {
        let s = Instant::now();
        let r = if heif {
            iv::try_heif_thumbnail(path)
        } else {
            iv::try_embedded_from_bytes(&std::fs::read(path).unwrap_or_default())
        };
        times.push(s.elapsed().as_secs_f64() * 1000.0);
        px = r
            .map(|i| format!("{}x{}", i.width, i.height))
            .unwrap_or("none".into());
    }
    (median(&mut times), px)
}

fn bench_full(path: &Path, raw: bool) -> (f64, String) {
    let _ = load_full_for_bench(path, raw);
    let mut times = Vec::with_capacity(ITERATIONS);
    let mut px = String::new();
    for _ in 0..ITERATIONS {
        let s = Instant::now();
        match load_full_for_bench(path, raw) {
            Ok(i) => {
                times.push(s.elapsed().as_secs_f64() * 1000.0);
                px = format!("{}x{}", i.width, i.height);
            }
            Err(_) => {
                times.push(s.elapsed().as_secs_f64() * 1000.0);
                px = "ERROR".into();
            }
        }
    }
    (median(&mut times), px)
}

fn load_full_for_bench(path: &Path, raw: bool) -> Result<iv::DecodedImage, String> {
    if raw {
        let data = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
        iv::decode_raw_libraw(&data).ok_or_else(|| "LibRaw failed".to_string())
    } else {
        iv::load_image(path)
    }
}

fn median(data: &mut [f64]) -> f64 {
    data.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if data.is_empty() {
        return 0.0;
    }
    let m = data.len() / 2;
    if data.len().is_multiple_of(2) {
        (data[m - 1] + data[m]) / 2.0
    } else {
        data[m]
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

fn dir_is_empty(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

/// Pick a HEIC file to use as the bench fixture.
///
/// Preference order:
///   1. Sibling `<source-stem>.heic` / `.HEIC` next to the source RAW.
///   2. Any `*.heic` / `*.HEIC` in the source RAW's parent directory.
///   3. `IV_BENCH_HEIC_FIXTURE` environment variable.
fn resolve_heic_fixture(source: &Path) -> Option<PathBuf> {
    if let Some(stem) = source.file_stem() {
        for ext in ["heic", "HEIC", "heif", "HEIF"] {
            let mut candidate = source.with_file_name(stem);
            candidate.set_extension(ext);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    if let Some(parent) = source.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    let lower = e.to_ascii_lowercase();
                    lower == "heic" || lower == "heif"
                }) {
                    return Some(path);
                }
            }
        }
    }
    if let Ok(env_path) = std::env::var("IV_BENCH_HEIC_FIXTURE") {
        let path = PathBuf::from(env_path);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Print whether the fixture HEIC contains HEVC (`hvc1`/`hev1`) or AV1 (`av01`)
/// items, so the bench output makes the codec choice explicit. Reads only the
/// first 16 KiB of the file and walks just enough ISOBMFF boxes to find the
/// item types.
fn report_heic_codec(path: &Path) {
    use std::io::Read;
    let mut buf = vec![0u8; 16384];
    let read = match std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => {
            println!("HEIC fixture: {} (codec: <unreadable>)", path.display());
            return;
        }
    };
    buf.truncate(read);
    let codec = inspect_heif_codec(&buf).unwrap_or_else(|| "<unknown>".into());
    println!(
        "HEIC fixture: {} (codec: {}{})",
        path.display(),
        codec,
        if codec == "AV1" {
            " — synthetic, NOT representative of typical iPhone HEVC content"
        } else {
            ""
        }
    );
}

fn inspect_heif_codec(buf: &[u8]) -> Option<String> {
    // Skip past `ftyp` so we don't match bytes there (very unlikely to contain
    // codec 4CCs, but safest to start in the meta box). For real-world iPhone
    // HEICs the meta box is much larger than our 16 KiB buffer, so once we
    // enter meta we just scan the rest of the head buffer for codec 4CCs.
    let mut p = 0usize;
    while p + 8 <= buf.len() {
        let size32 = u32::from_be_bytes(buf[p..p + 4].try_into().ok()?) as usize;
        let box_type = std::str::from_utf8(&buf[p + 4..p + 8]).ok()?;
        let (header_size, total_size) = if size32 == 1 {
            if p + 16 > buf.len() {
                break;
            }
            let big = u64::from_be_bytes(buf[p + 8..p + 16].try_into().ok()?);
            (16usize, big as usize)
        } else if size32 == 0 {
            (8usize, buf.len() - p)
        } else {
            (8usize, size32)
        };
        if total_size < header_size {
            break;
        }
        if box_type == "meta" {
            let body_start = p + header_size + 4; // FullBox: skip version+flags
            let body_end = (p + total_size).min(buf.len());
            if body_start >= body_end {
                break;
            }
            for window in buf[body_start..body_end].windows(4) {
                match window {
                    b"hvc1" | b"hev1" => return Some("HEVC".into()),
                    b"av01" => return Some("AV1".into()),
                    _ => {}
                }
            }
            // We saw meta but no codec 4CC in the head buffer.
            return None;
        }
        // Step over the box. If it would extend past the buffer, stop walking
        // (any subsequent boxes are out of reach).
        let next = p + total_size;
        if next > buf.len() {
            break;
        }
        p = next;
    }
    None
}
