/// Benchmark suite for iv decode pipeline.
///
/// Generates high-resolution test images in all supported formats,
/// with and without EXIF thumbnails, then measures decode performance
/// at configurable thumbnail resolutions.
///
/// Usage:
///   cargo run --release --example bench -- [--thumb-size 160] [--dir path/to/bench_images]
///
/// The benchmark generates test images on first run and reuses them on subsequent runs.
/// Delete the benchmark directory to regenerate.
use image::{ImageFormat, RgbImage, RgbaImage};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Default resolution matching modern cameras/phones (12MP).
const DEFAULT_WIDTH: u32 = 4000;
const DEFAULT_HEIGHT: u32 = 3000;

/// Number of iterations per benchmark.
const ITERATIONS: usize = 5;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let thumb_size: u32 = args
        .iter()
        .position(|a| a == "--thumb-size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(160);

    let bench_dir: PathBuf = args
        .iter()
        .position(|a| a == "--dir")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("iv_bench_images"));

    println!("iv decode benchmark");
    println!("  Thumbnail size: {thumb_size}×{thumb_size}");
    println!("  Source resolution: {DEFAULT_WIDTH}×{DEFAULT_HEIGHT} (12MP)");
    println!("  Iterations: {ITERATIONS}");
    println!("  Image directory: {}", bench_dir.display());
    println!();

    // Generate test images if needed
    if !bench_dir.exists() || dir_is_empty(&bench_dir) {
        println!("Generating test images...");
        std::fs::create_dir_all(&bench_dir).expect("failed to create bench dir");
        generate_test_images(&bench_dir);
        println!("Done generating.\n");
    } else {
        println!("Using existing test images.\n");
    }

    // Collect test files
    let test_files = collect_test_files(&bench_dir);
    if test_files.is_empty() {
        eprintln!("No test files found in {}", bench_dir.display());
        std::process::exit(1);
    }

    // Run benchmarks
    println!(
        "{:<35} {:>10} {:>10} {:>10} {:>10}",
        "File", "EXIF (ms)", "Full (ms)", "Size (KB)", "Pixels"
    );
    println!("{}", "-".repeat(80));

    for file in &test_files {
        let file_size_kb = std::fs::metadata(file).map(|m| m.len() / 1024).unwrap_or(0);

        let file_name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Benchmark EXIF extraction
        let exif_ms = bench_exif(file);

        // Benchmark full thumbnail decode
        let (full_ms, pixels) = bench_full_decode(file, thumb_size);

        println!(
            "{:<35} {:>10.2} {:>10.2} {:>10} {:>10}",
            truncate(&file_name, 35),
            exif_ms,
            full_ms,
            file_size_kb,
            pixels
        );
    }

    println!("\n{}", "-".repeat(80));
    println!("Note: EXIF = file read (256KB max) + EXIF parse + thumbnail decode + orientation.");
    println!("      Full = file read (entire) + full decode + downscale + orientation.");
    println!("      EXIF time for non-JPEG formats is just the failed parse attempt.");
}

fn dir_is_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

/// Generate a realistic-looking test image (gradient + noise pattern).
fn generate_source_image(w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        // Create a pattern with gradients and some variation
        let r = ((x as f32 / w as f32) * 200.0 + (y % 37) as f32) as u8;
        let g = ((y as f32 / h as f32) * 180.0 + (x % 41) as f32) as u8;
        let b = (((x + y) as f32 / (w + h) as f32) * 220.0 + ((x * y) % 29) as f32) as u8;
        image::Rgb([r, g, b])
    })
}

fn generate_source_image_rgba(w: u32, h: u32) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        let r = ((x as f32 / w as f32) * 200.0 + (y % 37) as f32) as u8;
        let g = ((y as f32 / h as f32) * 180.0 + (x % 41) as f32) as u8;
        let b = (((x + y) as f32 / (w + h) as f32) * 220.0 + ((x * y) % 29) as f32) as u8;
        image::Rgba([r, g, b, 255])
    })
}

fn generate_test_images(dir: &Path) {
    let src = generate_source_image(DEFAULT_WIDTH, DEFAULT_HEIGHT);
    let src_rgba = generate_source_image_rgba(DEFAULT_WIDTH, DEFAULT_HEIGHT);

    // 1. JPEG without EXIF thumbnail
    print!("  JPEG (no EXIF)...");
    std::io::stdout().flush().unwrap();
    src.save_with_format(dir.join("jpeg_no_exif.jpg"), ImageFormat::Jpeg)
        .unwrap();
    println!(" done");

    // 2. JPEG with EXIF thumbnail (manually embed)
    print!("  JPEG (with EXIF)...");
    std::io::stdout().flush().unwrap();
    create_jpeg_with_exif_thumbnail(dir, &src);
    println!(" done");

    // 3. PNG
    print!("  PNG...");
    std::io::stdout().flush().unwrap();
    src_rgba
        .save_with_format(dir.join("png_test.png"), ImageFormat::Png)
        .unwrap();
    println!(" done");

    // 4. WebP
    print!("  WebP...");
    std::io::stdout().flush().unwrap();
    src_rgba
        .save_with_format(dir.join("webp_test.webp"), ImageFormat::WebP)
        .unwrap();
    println!(" done");

    // 5. TIFF
    print!("  TIFF...");
    std::io::stdout().flush().unwrap();
    src.save_with_format(dir.join("tiff_test.tiff"), ImageFormat::Tiff)
        .unwrap();
    println!(" done");

    // 6. BMP
    print!("  BMP...");
    std::io::stdout().flush().unwrap();
    src.save_with_format(dir.join("bmp_test.bmp"), ImageFormat::Bmp)
        .unwrap();
    println!(" done");

    // 7. GIF (downscaled — GIF at 12MP would be huge and slow)
    print!("  GIF (1024x768)...");
    std::io::stdout().flush().unwrap();
    let gif_src = generate_source_image(1024, 768);
    gif_src
        .save_with_format(dir.join("gif_test.gif"), ImageFormat::Gif)
        .unwrap();
    println!(" done");
}

/// Create a JPEG with an embedded EXIF thumbnail.
/// We do this by:
/// 1. Encode the full image as JPEG
/// 2. Encode a small thumbnail as JPEG
/// 3. Build a minimal EXIF APP1 segment with the thumbnail
/// 4. Splice the APP1 into the full JPEG
fn create_jpeg_with_exif_thumbnail(dir: &Path, src: &RgbImage) {
    // Encode full image
    let mut full_jpeg = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut full_jpeg, 85);
    encoder
        .encode(
            src.as_raw(),
            src.width(),
            src.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();

    // Create thumbnail (160x120)
    let thumb = image::imageops::thumbnail(src, 160, 120);
    let mut thumb_jpeg = Vec::new();
    let mut thumb_encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumb_jpeg, 75);
    thumb_encoder
        .encode(
            thumb.as_raw(),
            thumb.width(),
            thumb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();

    // Build minimal EXIF APP1 with thumbnail
    let exif_app1 = build_exif_app1_with_thumbnail(&thumb_jpeg);

    // Splice: SOI + APP1 + rest of original JPEG (skip SOI)
    let mut output = Vec::with_capacity(full_jpeg.len() + exif_app1.len());
    output.extend_from_slice(&[0xFF, 0xD8]); // SOI
    output.extend_from_slice(&exif_app1); // APP1 with EXIF
    output.extend_from_slice(&full_jpeg[2..]); // rest of original (skip its SOI)

    std::fs::write(dir.join("jpeg_with_exif.jpg"), &output).unwrap();
}

/// Build a minimal EXIF APP1 segment containing a JPEG thumbnail.
fn build_exif_app1_with_thumbnail(thumb_jpeg: &[u8]) -> Vec<u8> {
    // TIFF header starts after "Exif\0\0"
    // IFD0 with minimal tags + IFD1 with thumbnail offset/length

    let mut tiff = Vec::new();

    // TIFF header (little-endian)
    tiff.extend_from_slice(b"II"); // Little-endian
    tiff.extend_from_slice(&42u16.to_le_bytes()); // Magic
    tiff.extend_from_slice(&8u32.to_le_bytes()); // Offset to IFD0

    // IFD0: 1 tag (Orientation = 1)
    let ifd0_count: u16 = 1;
    tiff.extend_from_slice(&ifd0_count.to_le_bytes());

    // Tag: Orientation (0x0112), SHORT, count=1, value=1 (Normal)
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Tag
    tiff.extend_from_slice(&3u16.to_le_bytes()); // Type: SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // Count
    tiff.extend_from_slice(&1u16.to_le_bytes()); // Value
    tiff.extend_from_slice(&0u16.to_le_bytes()); // Padding

    // Offset to IFD1 (follows immediately: 8 + 2 + 12*1 + 4 = 26)
    let ifd1_offset = tiff.len() as u32 + 4;
    tiff.extend_from_slice(&ifd1_offset.to_le_bytes());

    // IFD1: 2 tags (JPEGInterchangeFormat, JPEGInterchangeFormatLength)
    let ifd1_count: u16 = 2;
    tiff.extend_from_slice(&ifd1_count.to_le_bytes());

    // Thumbnail data will follow after IFD1
    // IFD1 size: 2 + 12*2 + 4 = 30 bytes
    let thumb_offset = tiff.len() as u32 + 12 * 2 + 4;

    // Tag: JPEGInterchangeFormat (0x0201), LONG, count=1
    tiff.extend_from_slice(&0x0201u16.to_le_bytes());
    tiff.extend_from_slice(&4u16.to_le_bytes()); // Type: LONG
    tiff.extend_from_slice(&1u32.to_le_bytes()); // Count
    tiff.extend_from_slice(&thumb_offset.to_le_bytes()); // Value

    // Tag: JPEGInterchangeFormatLength (0x0202), LONG, count=1
    tiff.extend_from_slice(&0x0202u16.to_le_bytes());
    tiff.extend_from_slice(&4u16.to_le_bytes()); // Type: LONG
    tiff.extend_from_slice(&1u32.to_le_bytes()); // Count
    tiff.extend_from_slice(&(thumb_jpeg.len() as u32).to_le_bytes());

    // No next IFD
    tiff.extend_from_slice(&0u32.to_le_bytes());

    // Thumbnail JPEG data
    tiff.extend_from_slice(thumb_jpeg);

    // Build APP1 marker
    let mut app1 = Vec::new();
    app1.extend_from_slice(&[0xFF, 0xE1]); // APP1 marker
    let app1_length = (tiff.len() + 2 + 6) as u16; // +2 for length field, +6 for "Exif\0\0"
    app1.extend_from_slice(&app1_length.to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);

    app1
}

fn collect_test_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

fn bench_exif(path: &Path) -> f64 {
    // Warm up
    let _ = iv::extract_exif_thumbnail(&std::fs::read(path).unwrap_or_default());

    let mut times = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        // Read file fresh each time to include I/O
        let start = Instant::now();
        let data = std::fs::read(path).unwrap_or_default();
        let _ = iv::extract_exif_thumbnail(&data);
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    median(&mut times)
}

fn bench_full_decode(path: &Path, thumb_size: u32) -> (f64, String) {
    // Warm up
    let _ = iv::decode_thumbnail(path, thumb_size);

    let mut times = Vec::with_capacity(ITERATIONS);
    let mut pixel_info = String::new();
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        match iv::decode_thumbnail(path, thumb_size) {
            Ok(img) => {
                times.push(start.elapsed().as_secs_f64() * 1000.0);
                pixel_info = format!("{}×{}", img.width, img.height);
            }
            Err(_) => {
                times.push(f64::NAN);
                pixel_info = "ERROR".to_string();
            }
        }
    }
    (median(&mut times), pixel_info)
}

fn median(data: &mut Vec<f64>) -> f64 {
    data.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if data.is_empty() {
        return 0.0;
    }
    let mid = data.len() / 2;
    if data.len() % 2 == 0 {
        (data[mid - 1] + data[mid]) / 2.0
    } else {
        data[mid]
    }
}
