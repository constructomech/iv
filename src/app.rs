use eframe::egui;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Pure (testable) image-loading logic — no GPU context needed
// ---------------------------------------------------------------------------

/// Decoded image data ready for GPU upload.
#[derive(Debug)]
pub struct DecodedImage {
    /// RGBA pixels, row-major.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Load an image file from disk and decode it to RGBA.
pub fn load_image(path: &Path) -> Result<DecodedImage, String> {
    let img = image::open(path).map_err(|e| format!("Failed to load {}: {e}", path.display()))?;
    let rgba = img.to_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

/// Compute the display size that fits `image_size` into `available_size`,
/// preserving aspect ratio, without upscaling beyond 1:1.
/// Returns `(width, height)`.
pub fn fit_size(image_size: (f32, f32), available_size: (f32, f32)) -> (f32, f32) {
    let (iw, ih) = image_size;
    let (aw, ah) = available_size;
    if iw <= 0.0 || ih <= 0.0 || aw <= 0.0 || ah <= 0.0 {
        return (0.0, 0.0);
    }
    let scale = (aw / iw).min(ah / ih).min(1.0);
    (iw * scale, ih * scale)
}

/// Compute the offset to center `inner` within `outer`.
/// Returns `(x_offset, y_offset)`.
pub fn center_offset(inner: (f32, f32), outer: (f32, f32)) -> (f32, f32) {
    ((outer.0 - inner.0) / 2.0, (outer.1 - inner.1) / 2.0)
}

/// Recognized image file extensions (lowercase, without dot).
#[allow(dead_code)] // Used in tests now, used by enumerator in Phase 1
pub const IMAGE_EXTENSIONS: &[&str] = &[
    // Common raster
    "jpg", "jpeg", "png", "webp", "tiff", "tif", "bmp", "gif",
    // RAW (first-class — we extract embedded JPEG previews)
    "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf",
];

/// Check whether a path has a recognized image extension.
#[allow(dead_code)] // Used in tests now, used by enumerator in Phase 1
pub fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// eframe App — thin wrapper that wires testable logic to the GPU
// ---------------------------------------------------------------------------

pub struct App {
    /// Path to the image file.
    path: PathBuf,
    /// Loaded texture handle (None until first frame).
    texture: Option<egui::TextureHandle>,
    /// Error message if loading failed.
    error: Option<String>,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, path: PathBuf) -> Self {
        Self {
            path,
            texture: None,
            error: None,
        }
    }

    fn load_texture(&mut self, ctx: &egui::Context) {
        if self.texture.is_some() || self.error.is_some() {
            return; // Already loaded or failed
        }

        log::info!("Loading image: {}", self.path.display());

        match load_image(&self.path) {
            Ok(decoded) => {
                let size = [decoded.width as usize, decoded.height as usize];
                let color_image =
                    egui::ColorImage::from_rgba_unmultiplied(size, &decoded.pixels);
                self.texture = Some(ctx.load_texture(
                    "image",
                    color_image,
                    egui::TextureOptions::LINEAR,
                ));
                log::info!("Loaded {}x{} image", size[0], size[1]);
            }
            Err(msg) => {
                log::error!("{msg}");
                self.error = Some(msg);
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Load on first frame (keeps startup fast)
        self.load_texture(ctx);

        // Dark background
        let frame_style = egui::Frame::new().fill(egui::Color32::from_rgb(24, 24, 24));

        egui::CentralPanel::default()
            .frame(frame_style)
            .show(ctx, |ui| {
                if let Some(ref error) = self.error {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(egui::Color32::from_rgb(255, 80, 80), error);
                    });
                    return;
                }

                if let Some(ref texture) = self.texture {
                    let available = ui.available_size();
                    let tex_size = texture.size_vec2();

                    let display_size =
                        fit_size((tex_size.x, tex_size.y), (available.x, available.y));
                    let offset = center_offset(display_size, (available.x, available.y));

                    let rect = egui::Rect::from_min_size(
                        ui.min_rect().min + egui::vec2(offset.0, offset.1),
                        egui::vec2(display_size.0, display_size.1),
                    );

                    ui.painter().image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_max(
                            egui::pos2(0.0, 0.0),
                            egui::pos2(1.0, 1.0),
                        ),
                        egui::Color32::WHITE,
                    );
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            });
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // -- fit_size tests --

    #[test]
    fn fit_size_smaller_image_no_upscale() {
        // 100x100 image into 800x600 viewport → stays 100x100 (no upscale)
        let (w, h) = fit_size((100.0, 100.0), (800.0, 600.0));
        assert!((w - 100.0).abs() < 0.01);
        assert!((h - 100.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_landscape_image() {
        // 1600x1200 into 800x600 → scaled to 800x600 (exact fit)
        let (w, h) = fit_size((1600.0, 1200.0), (800.0, 600.0));
        assert!((w - 800.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_portrait_image() {
        // 1200x1600 into 800x600 → height-limited: 450x600
        let (w, h) = fit_size((1200.0, 1600.0), (800.0, 600.0));
        assert!((w - 450.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_exact_match() {
        let (w, h) = fit_size((800.0, 600.0), (800.0, 600.0));
        assert!((w - 800.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
    }

    #[test]
    fn fit_size_zero_image() {
        let (w, h) = fit_size((0.0, 100.0), (800.0, 600.0));
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn fit_size_zero_viewport() {
        let (w, h) = fit_size((100.0, 100.0), (0.0, 600.0));
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    // -- center_offset tests --

    #[test]
    fn center_offset_centered() {
        let (x, y) = center_offset((400.0, 300.0), (800.0, 600.0));
        assert!((x - 200.0).abs() < 0.01);
        assert!((y - 150.0).abs() < 0.01);
    }

    #[test]
    fn center_offset_exact_fit() {
        let (x, y) = center_offset((800.0, 600.0), (800.0, 600.0));
        assert!((x).abs() < 0.01);
        assert!((y).abs() < 0.01);
    }

    // -- is_image_file tests --

    #[test]
    fn is_image_file_jpeg() {
        assert!(is_image_file(Path::new("photo.jpg")));
        assert!(is_image_file(Path::new("photo.jpeg")));
        assert!(is_image_file(Path::new("photo.JPEG")));
        assert!(is_image_file(Path::new("PHOTO.JPG")));
    }

    #[test]
    fn is_image_file_raw() {
        assert!(is_image_file(Path::new("IMG_1234.CR2")));
        assert!(is_image_file(Path::new("image.dng")));
        assert!(is_image_file(Path::new("image.DNG")));
        assert!(is_image_file(Path::new("DSC_0001.NEF")));
        assert!(is_image_file(Path::new("photo.arw")));
    }

    #[test]
    fn is_image_file_other_formats() {
        assert!(is_image_file(Path::new("icon.png")));
        assert!(is_image_file(Path::new("image.webp")));
        assert!(is_image_file(Path::new("image.tiff")));
        assert!(is_image_file(Path::new("image.tif")));
        assert!(is_image_file(Path::new("image.bmp")));
        assert!(is_image_file(Path::new("anim.gif")));
    }

    #[test]
    fn is_image_file_negative() {
        assert!(!is_image_file(Path::new("readme.txt")));
        assert!(!is_image_file(Path::new("video.mp4")));
        assert!(!is_image_file(Path::new("document.pdf")));
        assert!(!is_image_file(Path::new("noextension")));
        assert!(!is_image_file(Path::new(".hidden")));
    }

    // -- load_image tests --

    #[test]
    fn load_image_nonexistent_file() {
        let result = load_image(Path::new("does_not_exist.jpg"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to load"));
    }

    #[test]
    fn load_image_not_an_image() {
        // Cargo.toml exists and is definitely not an image
        let result = load_image(Path::new("Cargo.toml"));
        assert!(result.is_err());
    }
}
