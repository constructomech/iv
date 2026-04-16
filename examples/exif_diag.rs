/// Quick diagnostic: check EXIF thumbnail extraction on real files.
/// Run with: cargo run --example exif_diag -- <file1> <file2> ...
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("Usage: exif_diag <image-path> ...");
        std::process::exit(1);
    }

    // Register HEIF/HEIC decoder hooks so full decode works on HEIC files
    iv::register_heif_hooks();

    for path_str in &args {
        let path = std::path::Path::new(path_str);
        println!("\n=== {} ===", path.display());

        // Step 1: Can we read EXIF at all?
        match std::fs::File::open(path) {
            Ok(file) => {
                let mut reader = std::io::BufReader::new(&file);
                let exif_reader = exif::Reader::new();
                match exif_reader.read_from_container(&mut reader) {
                    Ok(exif) => {
                        println!("  EXIF: found ({} fields)", exif.fields().count());

                        // Check for thumbnail tags in all IFDs
                        for field in exif.fields() {
                            if field.tag == exif::Tag::JPEGInterchangeFormat
                                || field.tag == exif::Tag::JPEGInterchangeFormatLength
                            {
                                println!(
                                    "  IFD {:?}: {} = {:?}",
                                    field.ifd_num,
                                    field.tag,
                                    field.value.get_uint(0)
                                );
                            }
                        }
                    }
                    Err(e) => println!("  EXIF: parse error: {e}"),
                }
            }
            Err(e) => println!("  Can't open: {e}"),
        }

        // Step 2: Try our EXIF extraction
        let file_data = std::fs::read(path).expect("failed to read file");
        let start = std::time::Instant::now();
        match iv::extract_exif_thumbnail(&file_data) {
            Some(img) => println!(
                "  EXIF thumb: {}x{} ({:.1}ms)",
                img.width,
                img.height,
                start.elapsed().as_secs_f64() * 1000.0
            ),
            None => println!(
                "  EXIF thumb: None ({:.1}ms)",
                start.elapsed().as_secs_f64() * 1000.0
            ),
        }

        // Step 2b: Try HEIF container thumbnail (for HEIC/HEIF files)
        let start = std::time::Instant::now();
        match iv::try_heif_thumbnail(path) {
            Some(img) => println!(
                "  HEIF thumb: {}x{} ({:.1}ms)",
                img.width,
                img.height,
                start.elapsed().as_secs_f64() * 1000.0
            ),
            None => println!(
                "  HEIF thumb: None ({:.1}ms)",
                start.elapsed().as_secs_f64() * 1000.0
            ),
        }

        // Step 3: Progressive result
        let start = std::time::Instant::now();
        match iv::decode_thumbnail_progressive(path, 160) {
            Ok((img, is_exif, timings)) => println!(
                "  Progressive: {}x{}, is_exif={} ({:.1}ms total, exif={:.1}ms, full={:.1}ms)",
                img.width,
                img.height,
                is_exif,
                start.elapsed().as_secs_f64() * 1000.0,
                timings.exif_ms,
                timings.full_ms,
            ),
            Err(e) => println!("  Progressive: error: {e}"),
        }

        // Step 4: Direct decode_thumbnail (measures the zune-jpeg fast path)
        let start = std::time::Instant::now();
        match iv::decode_thumbnail(path, 160) {
            Ok(img) => println!(
                "  decode_thumbnail: {}x{} ({:.1}ms)",
                img.width,
                img.height,
                start.elapsed().as_secs_f64() * 1000.0
            ),
            Err(e) => println!("  decode_thumbnail: error: {e}"),
        }
    }
}
