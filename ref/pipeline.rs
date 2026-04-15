/// Headless pipeline tests — exercise the scheduler + decode pipeline
/// without a GPU or window. Validates correctness and measures throughput.
///
/// Run with: cargo test --test pipeline -- --nocapture
use std::path::{Path, PathBuf};
use std::time::Instant;

use iv::{DecodeTimings, EnumMessage, Scheduler, WorkTier, enumerate_folder};

/// Generate `count` small JPEG test images in a temp directory.
/// Returns the directory path.
fn generate_test_folder(name: &str, count: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("iv_pipeline_test_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for i in 0..count {
        let img = image::RgbImage::from_fn(200, 150, |x, y| {
            image::Rgb([
                ((x + i as u32 * 7) % 256) as u8,
                ((y + i as u32 * 13) % 256) as u8,
                128,
            ])
        });
        img.save(dir.join(format!("img_{i:05}.jpg"))).unwrap();
    }

    dir
}

/// Simulate the full pipeline: enumerate → schedule → decode → complete.
/// Returns (total_ms, exif_hit_count, full_decode_count, fail_count).
fn run_pipeline(dir: &Path, thumb_size: u32, viewport_rows: usize, cols: usize) -> PipelineResult {
    let cell_height: f32 = 188.0; // matches TILE_SIZE + TILE_PADDING + LABEL_HEIGHT
    let start = Instant::now();

    // 1. Enumerate
    let handle = enumerate_folder(dir.to_path_buf());
    let mut scheduler = Scheduler::new();
    loop {
        match handle.receiver.recv() {
            Ok(EnumMessage::Found(path)) => {
                scheduler.add_entry(path);
            }
            Ok(EnumMessage::Done(_)) => break,
            Ok(EnumMessage::Error(e)) => panic!("Enumeration error: {e}"),
            Err(_) => break,
        }
    }
    let enum_ms = start.elapsed().as_secs_f64() * 1000.0;
    let total_entries = scheduler.len();

    // 2. Set up visibility (simulated viewport at top)
    scheduler.update_visibility(0.0, cell_height * viewport_rows as f32, cols, cell_height);

    // 3. Decode loop: keep pulling batches and decoding until no pending work
    let mut exif_hits = 0usize;
    let mut full_decodes = 0usize;
    let mut failures = 0usize;
    let mut batches = 0usize;
    let mut total_exif_ms = 0.0f64;
    let mut total_full_ms = 0.0f64;

    loop {
        let batch = scheduler.get_work_batch();
        if batch.is_empty() {
            break;
        }
        batches += 1;

        for item in &batch {
            match item.tier {
                WorkTier::ExifOnly => {
                    let (result, timings) = iv::try_exif_only(&item.path);
                    total_exif_ms += timings.exif_ms;
                    match result {
                        Some(_img) => {
                            exif_hits += 1;
                            scheduler.complete(item.idx, true, timings);
                        }
                        None => {
                            scheduler.exif_failed(item.idx, timings);
                        }
                    }
                }
                WorkTier::FullDecode => {
                    match iv::decode_thumbnail(&item.path, thumb_size) {
                        Ok(_img) => {
                            full_decodes += 1;
                            let timings = DecodeTimings {
                                exif_ms: 0.0,
                                full_ms: 0.0, // we track wall time separately
                            };
                            scheduler.complete(item.idx, false, timings);
                        }
                        Err(_e) => {
                            failures += 1;
                            scheduler.fail(item.idx);
                        }
                    }
                }
            }
        }
    }

    let total_ms = start.elapsed().as_secs_f64() * 1000.0;

    PipelineResult {
        total_entries,
        total_ms,
        enum_ms,
        exif_hits,
        full_decodes,
        failures,
        batches,
        total_exif_ms,
        total_full_ms,
        loaded_count: scheduler.loaded_count(),
        has_pending: scheduler.has_pending_work(),
    }
}

#[derive(Debug)]
struct PipelineResult {
    total_entries: usize,
    total_ms: f64,
    enum_ms: f64,
    exif_hits: usize,
    full_decodes: usize,
    failures: usize,
    batches: usize,
    total_exif_ms: f64,
    total_full_ms: f64,
    loaded_count: usize,
    has_pending: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn pipeline_completes_all_images() {
    let dir = generate_test_folder("complete", 50);
    let result = run_pipeline(&dir, 160, 3, 5);

    println!("Pipeline result: {result:#?}");

    // All images should be processed
    assert_eq!(result.total_entries, 50);
    assert_eq!(result.loaded_count, 50);
    assert!(!result.has_pending);
    assert_eq!(result.failures, 0);

    // Every image should have been decoded at least once
    assert!(
        result.exif_hits + result.full_decodes >= 50,
        "expected at least 50 decodes, got {}",
        result.exif_hits + result.full_decodes
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pipeline_handles_empty_folder() {
    let dir = generate_test_folder("empty", 0);
    let result = run_pipeline(&dir, 160, 3, 5);

    assert_eq!(result.total_entries, 0);
    assert_eq!(result.loaded_count, 0);
    assert!(!result.has_pending);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scheduler_counters_consistent() {
    let dir = generate_test_folder("counters", 20);
    let result = run_pipeline(&dir, 160, 3, 5);

    // loaded_count should match exif_hits + full_decodes
    assert_eq!(
        result.loaded_count,
        result.exif_hits + result.full_decodes,
        "loaded_count should equal successful decodes"
    );

    // No pending work should remain
    assert!(!result.has_pending, "no pending work should remain");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pipeline_throughput_report() {
    // Larger test for throughput measurement — only prints, doesn't assert on timing
    let dir = generate_test_folder("throughput", 200);

    let start = Instant::now();
    let result = run_pipeline(&dir, 160, 5, 7);
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;

    let images_per_sec = result.total_entries as f64 / (wall_ms / 1000.0);

    println!("\n=== Pipeline Throughput Report ===");
    println!("  Images:       {}", result.total_entries);
    println!("  Wall time:    {:.1}ms", wall_ms);
    println!("  Enumerate:    {:.1}ms", result.enum_ms);
    println!(
        "  EXIF hits:    {} ({:.1}ms total)",
        result.exif_hits, result.total_exif_ms
    );
    println!("  Full decodes: {}", result.full_decodes);
    println!("  Failures:     {}", result.failures);
    println!("  Batches:      {}", result.batches);
    println!("  Throughput:   {:.0} images/sec", images_per_sec);
    println!(
        "  Loaded:       {}/{}",
        result.loaded_count, result.total_entries
    );
    println!("  Pending:      {}", result.has_pending);
    println!("=================================\n");

    assert_eq!(result.loaded_count, result.total_entries);
    assert!(!result.has_pending);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test that scroll simulation works: decode near viewport, scroll, decode again.
#[test]
fn scroll_simulation() {
    let dir = generate_test_folder("scroll", 100);
    let cell_height: f32 = 188.0;
    let cols = 5;

    let handle = enumerate_folder(dir.to_path_buf());
    let mut scheduler = Scheduler::new();
    loop {
        match handle.receiver.recv() {
            Ok(EnumMessage::Found(path)) => {
                scheduler.add_entry(path);
            }
            Ok(EnumMessage::Done(_)) => break,
            _ => break,
        }
    }

    // View page 1 (rows 0-2)
    scheduler.update_visibility(0.0, cell_height * 3.0, cols, cell_height);

    // Decode visible batch
    let batch1 = scheduler.get_work_batch();
    assert!(!batch1.is_empty(), "should have work for page 1");
    let page1_count = batch1.len();
    for item in &batch1 {
        assert!(
            item.idx < 35,
            "page 1 work should be near top: idx={}",
            item.idx
        );
        // Simulate EXIF miss → will need full decode
        scheduler.exif_failed(item.idx, DecodeTimings::default());
    }

    // Scroll to page 5 (rows 10-12)
    let bumped =
        scheduler.update_visibility(cell_height * 10.0, cell_height * 3.0, cols, cell_height);
    assert!(bumped, "big scroll should bump generation");

    // New batch should target the new viewport area
    let batch2 = scheduler.get_work_batch();
    assert!(!batch2.is_empty(), "should have work for page 5");

    let avg_idx: usize = batch2.iter().map(|w| w.idx).sum::<usize>() / batch2.len();
    assert!(
        avg_idx > 30,
        "page 5 work should target tiles near row 10+, avg_idx={avg_idx}"
    );

    println!(
        "Scroll simulation: page1={page1_count} items, page5={} items, avg_idx={avg_idx}",
        batch2.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Benchmark against a real folder if --real-dir is provided via env var.
/// Set IV_TEST_DIR=path/to/folder to run.
#[test]
fn real_folder_benchmark() {
    let dir = match std::env::var("IV_TEST_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            println!("Skipped: set IV_TEST_DIR=path/to/folder to run real folder benchmark");
            return;
        }
    };

    iv::register_heif_hooks();

    let start = Instant::now();
    let result = run_pipeline(&dir, 160, 5, 7);
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;

    println!("\n=== Real Folder Benchmark: {} ===", dir.display());
    println!("  Images:       {}", result.total_entries);
    println!("  Wall time:    {:.1}ms", wall_ms);
    println!("  Enumerate:    {:.1}ms", result.enum_ms);
    println!(
        "  EXIF hits:    {} ({:.1}ms total)",
        result.exif_hits, result.total_exif_ms
    );
    println!("  Full decodes: {}", result.full_decodes);
    println!("  Failures:     {}", result.failures);
    println!("  Batches:      {}", result.batches);
    let images_per_sec = if wall_ms > 0.0 {
        result.total_entries as f64 / (wall_ms / 1000.0)
    } else {
        0.0
    };
    println!("  Throughput:   {:.0} images/sec", images_per_sec);
    println!(
        "  Loaded:       {}/{}",
        result.loaded_count, result.total_entries
    );
    println!("=======================================\n");
}

/// Stress test: measure per-frame cost of scheduler operations at scale.
/// This tests the UI thread's work (no decoding, no GPU) to ensure
/// it stays under budget even with thousands of entries.
#[test]
fn scheduler_frame_cost_stress() {
    let sizes = [100, 1_000, 10_000, 100_000];
    let cell_height: f32 = 196.0; // CELL_SIZE + TILE_PADDING
    let cols = 7;
    let viewport_rows = 4;

    println!("\n=== Scheduler Per-Frame Cost ===");
    println!(
        "{:>10} {:>12} {:>12} {:>12}",
        "Entries", "update(µs)", "batch(µs)", "total(µs)"
    );

    for &n in &sizes {
        let mut s = Scheduler::new();
        for i in 0..n {
            s.add_entry(PathBuf::from(format!("img_{i:06}.jpg")));
        }

        // Simulate initial view
        s.update_visibility(0.0, cell_height * viewport_rows as f32, cols, cell_height);
        let _ = s.get_work_batch();

        // Simulate scrolling to middle — this is the expensive case
        let scroll_y = cell_height * (n as f32 / cols as f32 / 2.0);
        let iterations = 100;

        let start = Instant::now();
        for _ in 0..iterations {
            s.update_visibility(
                scroll_y,
                cell_height * viewport_rows as f32,
                cols,
                cell_height,
            );
        }
        let update_us = start.elapsed().as_micros() as f64 / iterations as f64;

        let start = Instant::now();
        for _ in 0..iterations {
            let _ = s.get_work_batch();
        }
        let batch_us = start.elapsed().as_micros() as f64 / iterations as f64;

        let total_us = update_us + batch_us;
        println!(
            "{:>10} {:>12.1} {:>12.1} {:>12.1}",
            n, update_us, batch_us, total_us
        );

        // Frame budget: 16ms = 16,000µs. Scheduler should use <1ms even at 100k.
        assert!(
            total_us < 1000.0,
            "Scheduler per-frame cost at {n} entries ({total_us:.0}µs) exceeds 1ms budget"
        );
    }
    println!("================================\n");
}
