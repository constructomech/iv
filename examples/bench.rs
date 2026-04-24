/// Apples-to-apples decode benchmark for iv.
///
/// Decodes a source raw file via LibRaw, then encodes the same pixels into
/// every supported format at the same resolution.  Benchmarks grid-thumbnail
/// and full-decode performance for each format.
///
/// Usage:
///   cargo run --release --example bench -- <source.dng> [extra_raw1.cr2 ...]
///
/// Generated files are cached next to the source. Delete to regenerate.
use image::{ImageFormat, RgbImage, RgbaImage};
use libheif_rs::{
    Channel, ColorSpace, CompressionFormat, EncoderQuality, HeifContext, Image, LibHeif, RgbChroma,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

const ITERATIONS: usize = 5;

fn main() {
    libheif_rs::integration::image::register_all_decoding_hooks();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0].starts_with('-') {
        eprintln!("Usage: bench <source.dng> [extra.cr2 ...]");
        std::process::exit(1);
    }

    let source = PathBuf::from(&args[0]);
    if !source.exists() {
        eprintln!("File not found: {}", source.display());
        std::process::exit(1);
    }
    let extra_raws: Vec<PathBuf> = args[1..].iter().map(PathBuf::from).collect();

    // Decode source to full-res RGB
    println!("Decoding source via LibRaw: {}", source.display());
    let t0 = Instant::now();
    let src_data = std::fs::read(&source).expect("read failed");
    let decoded = iv::decode_raw_libraw(&src_data).expect("LibRaw failed");
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
    if !bench_dir.exists() || dir_is_empty(&bench_dir) {
        std::fs::create_dir_all(&bench_dir).unwrap();
        println!("Generating test images in {}", bench_dir.display());
        generate_formats(&bench_dir, &rgb, &rgba);
        println!();
    } else {
        println!("Using cached images in {}\n", bench_dir.display());
    }

    // Collect files to benchmark
    let mut files: Vec<(PathBuf, &str)> = Vec::new();
    files.push((source.clone(), "RAW"));
    for r in &extra_raws {
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
        let (f_ms, f_px) = bench_full(path);
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
    println!("\nThumb = grid path | Full = viewer path");
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

fn generate_formats(dir: &Path, rgb: &RgbImage, rgba: &RgbaImage) {
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
    encode_step("  HEIC (AV1 q50+thumb)", || {
        encode_heic(dir, rgb, "heic.heic");
    });
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

fn encode_heic(dir: &Path, src: &RgbImage, name: &str) {
    let lh = LibHeif::new();
    let img = rgb_to_heif(src);
    let mut ctx = HeifContext::new().unwrap();
    let mut enc = lh.encoder_for_format(CompressionFormat::Av1).unwrap();
    enc.set_quality(EncoderQuality::Lossy(50)).unwrap();
    let h = ctx.encode_image(&img, &mut enc, None).unwrap();
    let mut te = lh.encoder_for_format(CompressionFormat::Av1).unwrap();
    te.set_quality(EncoderQuality::Lossy(40)).unwrap();
    let _ = ctx.encode_thumbnail(&img, &h, 320, &mut te, None).unwrap();
    ctx.write_to_file(dir.join(name).to_str().unwrap()).unwrap();
}

fn rgb_to_heif(src: &RgbImage) -> Image {
    let (w, h) = (src.width(), src.height());
    let mut img = Image::new(w, h, ColorSpace::Rgb(RgbChroma::C444)).unwrap();
    img.create_plane(Channel::R, w, h, 8).unwrap();
    img.create_plane(Channel::G, w, h, 8).unwrap();
    img.create_plane(Channel::B, w, h, 8).unwrap();
    let p = img.planes_mut();
    let s = p.r.as_ref().unwrap().stride;
    let (dr, dg, db) = (p.r.unwrap().data, p.g.unwrap().data, p.b.unwrap().data);
    for y in 0..h {
        for x in 0..w {
            let px = src.get_pixel(x, y);
            let i = s * y as usize + x as usize;
            dr[i] = px[0];
            dg[i] = px[1];
            db[i] = px[2];
        }
    }
    img
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

fn bench_full(path: &Path) -> (f64, String) {
    let _ = iv::load_image(path);
    let mut times = Vec::with_capacity(ITERATIONS);
    let mut px = String::new();
    for _ in 0..ITERATIONS {
        let s = Instant::now();
        match iv::load_image(path) {
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
