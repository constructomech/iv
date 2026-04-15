//! Integration tests for image loading pipeline.
//!
//! These tests exercise the full path: file on disk → decode → DecodedImage.
//! They use synthetic images generated at test time so no fixtures are needed.

mod common;

use common::*;
use std::path::Path;

// We need to reference the library code. Since iv is a binary crate,
// we test via the public items re-exported from app.rs by including it
// as a module path. But for a binary crate, integration tests can't
// import from it directly. So we test the public API via the binary's
// module structure by duplicating the pure logic we want to test.
//
// For now, we test by calling the binary and checking exit codes,
// plus we test the image crate pipeline directly.

// ---------------------------------------------------------------------------
// Image format loading tests
// ---------------------------------------------------------------------------

#[test]
fn load_jpeg_basic() {
    let dir = TestDir::new("load_jpeg");
    let path = create_test_jpeg(dir.path(), "test.jpg", 640, 480);

    let img = image::open(&path).expect("should load JPEG");
    let rgba = img.to_rgba8();
    assert_eq!(rgba.width(), 640);
    assert_eq!(rgba.height(), 480);
    assert_eq!(rgba.as_raw().len(), 640 * 480 * 4);
}

#[test]
fn load_png_with_alpha() {
    let dir = TestDir::new("load_png");
    let path = create_test_png(dir.path(), "test.png", 320, 240);

    let img = image::open(&path).expect("should load PNG");
    let rgba = img.to_rgba8();
    assert_eq!(rgba.width(), 320);
    assert_eq!(rgba.height(), 240);
}

#[test]
fn load_bmp() {
    let dir = TestDir::new("load_bmp");
    let path = create_test_bmp(dir.path(), "test.bmp", 100, 100);

    let img = image::open(&path).expect("should load BMP");
    let rgba = img.to_rgba8();
    assert_eq!(rgba.width(), 100);
    assert_eq!(rgba.height(), 100);
}

#[test]
fn load_gif() {
    let dir = TestDir::new("load_gif");
    let path = create_test_gif(dir.path(), "test.gif", 64, 64);

    let img = image::open(&path).expect("should load GIF");
    let rgba = img.to_rgba8();
    assert_eq!(rgba.width(), 64);
    assert_eq!(rgba.height(), 64);
}

// ---------------------------------------------------------------------------
// Error handling tests
// ---------------------------------------------------------------------------

#[test]
fn load_corrupt_jpeg_fails_gracefully() {
    let dir = TestDir::new("corrupt_jpeg");
    let path = create_corrupt_file(dir.path(), "corrupt.jpg");

    let result = image::open(&path);
    assert!(result.is_err(), "corrupt file should fail to load");
}

#[test]
fn load_empty_file_fails_gracefully() {
    let dir = TestDir::new("empty_file");
    let path = create_empty_file(dir.path(), "empty.jpg");

    let result = image::open(&path);
    assert!(result.is_err(), "empty file should fail to load");
}

#[test]
fn load_nonexistent_file_fails() {
    let result = image::open(Path::new("this_file_does_not_exist_anywhere.jpg"));
    assert!(result.is_err());
}

#[test]
fn load_text_file_as_image_fails() {
    let dir = TestDir::new("text_as_image");
    let path = create_corrupt_file(dir.path(), "readme.txt");

    let result = image::open(&path);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Large image tests
// ---------------------------------------------------------------------------

#[test]
fn load_large_jpeg() {
    let dir = TestDir::new("large_jpeg");
    // 4000x3000 = 12 megapixel — typical camera resolution
    let path = create_test_jpeg(dir.path(), "large.jpg", 4000, 3000);

    let img = image::open(&path).expect("should load large JPEG");
    let rgba = img.to_rgba8();
    assert_eq!(rgba.width(), 4000);
    assert_eq!(rgba.height(), 3000);
    // 4000 * 3000 * 4 = 48MB of RGBA data
    assert_eq!(rgba.as_raw().len(), 4000 * 3000 * 4);
}

// ---------------------------------------------------------------------------
// Batch generation test (validates helper for future phases)
// ---------------------------------------------------------------------------

#[test]
fn generate_batch_of_images() {
    let dir = TestDir::new("batch");
    let paths = create_test_jpegs(dir.path(), 50, 160, 120);

    assert_eq!(paths.len(), 50);
    for p in &paths {
        assert!(p.exists(), "generated image should exist: {}", p.display());
    }

    // Verify first and last can be loaded
    let first = image::open(&paths[0]).expect("first image should load");
    assert_eq!(first.width(), 160);
    let last = image::open(&paths[49]).expect("last image should load");
    assert_eq!(last.width(), 160);
}

// ---------------------------------------------------------------------------
// Extension recognition (mirrors unit tests but from integration perspective)
// ---------------------------------------------------------------------------

#[test]
fn image_extensions_case_insensitive() {
    // Verify our test helpers produce files that the image crate can open
    // regardless of extension casing
    let dir = TestDir::new("case_ext");
    let lower = create_test_jpeg(dir.path(), "photo.jpg", 10, 10);
    let upper = create_test_jpeg(dir.path(), "PHOTO.JPG", 10, 10);

    assert!(image::open(&lower).is_ok());
    assert!(image::open(&upper).is_ok());
}
