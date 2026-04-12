//! Test helpers — synthetic image generation for integration tests.
//!
//! These helpers create real image files on disk in a temp directory so we can
//! test the full load pipeline without shipping test fixtures.

use image::{ImageBuffer, Rgba, RgbImage, ImageFormat};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A temporary directory that cleans up on drop. Wraps `tempfile::TempDir`
/// but we avoid the extra dep — just use std.
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// Create a new temporary test directory.
    pub fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("iv_test_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("failed to create test dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return a path within this directory.
    #[allow(dead_code)]
    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create a solid-color JPEG file. Returns the path.
pub fn create_test_jpeg(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let img = RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let path = dir.join(name);
    img.save_with_format(&path, ImageFormat::Jpeg)
        .expect("failed to save test JPEG");
    path
}

/// Create a solid-color PNG file with alpha channel. Returns the path.
pub fn create_test_png(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let img = ImageBuffer::<Rgba<u8>, _>::from_fn(width, height, |x, y| {
        Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
    });
    let path = dir.join(name);
    img.save_with_format(&path, ImageFormat::Png)
        .expect("failed to save test PNG");
    path
}

/// Create a BMP file. Returns the path.
pub fn create_test_bmp(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let img = RgbImage::from_fn(width, height, |_, _| image::Rgb([200, 100, 50]));
    let path = dir.join(name);
    img.save_with_format(&path, ImageFormat::Bmp)
        .expect("failed to save test BMP");
    path
}

/// Create a GIF file. Returns the path.
pub fn create_test_gif(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let img = RgbImage::from_fn(width, height, |_, _| image::Rgb([50, 200, 100]));
    let path = dir.join(name);
    img.save_with_format(&path, ImageFormat::Gif)
        .expect("failed to save test GIF");
    path
}

/// Create a file with the given extension but garbage content (not a valid image).
pub fn create_corrupt_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).expect("failed to create corrupt file");
    f.write_all(b"this is not an image file at all")
        .expect("failed to write");
    path
}

/// Create a zero-byte file.
pub fn create_empty_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::File::create(&path).expect("failed to create empty file");
    path
}

/// Create N test JPEG files named `img_0000.jpg` .. `img_NNNN.jpg`.
pub fn create_test_jpegs(dir: &Path, count: usize, width: u32, height: u32) -> Vec<PathBuf> {
    (0..count)
        .map(|i| create_test_jpeg(dir, &format!("img_{i:04}.jpg"), width, height))
        .collect()
}
