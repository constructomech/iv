use eframe::egui;
use std::path::{Path, PathBuf};

use crate::folder_view::FolderView;
use crate::image_view::ImageView;

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

/// Load an image file from disk and decode it to RGBA, applying EXIF orientation.
/// For HEIC/HEIF, libheif applies orientation internally, so we skip it.
pub fn load_image(path: &Path) -> Result<DecodedImage, String> {
    let data =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let img = image::load_from_memory(&data)
        .map_err(|e| format!("Failed to decode {}: {e}", path.display()))?;
    // libheif applies orientation during decode for HEIC/HEIF,
    // so only apply manual orientation for other formats.
    let img = if !crate::decode::is_heif_extension(path) {
        let orientation = crate::decode::read_exif_orientation(&data);
        crate::decode::apply_orientation(img, orientation)
    } else {
        img
    };
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
pub const IMAGE_EXTENSIONS: &[&str] = &[
    // Common raster
    "jpg", "jpeg", "png", "webp", "tiff", "tif", "bmp", "gif", // HEIF/HEIC
    "heic", "heif", // RAW (first-class — we extract embedded JPEG previews)
    "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf",
];

/// Check whether a path has a recognized image extension.
pub fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// eframe App — thin wrapper that wires testable logic to the GPU
// ---------------------------------------------------------------------------

/// What the app is currently showing.
enum Mode {
    /// Single image view (opened directly from CLI).
    Image {
        path: PathBuf,
        texture: Option<egui::TextureHandle>,
        error: Option<String>,
    },
    /// Folder grid view.
    Folder(FolderView),
    /// Full-resolution image view (opened from folder grid).
    ImageView(ImageView),
}

pub struct App {
    mode: Mode,
    /// Folder path stored for returning from image view.
    folder_path: Option<PathBuf>,
}

impl App {
    /// Open the app pointing at a file (single image view).
    pub fn new_image(_cc: &eframe::CreationContext<'_>, path: PathBuf) -> Self {
        Self {
            mode: Mode::Image {
                path,
                texture: None,
                error: None,
            },
            folder_path: None,
        }
    }

    /// Open the app pointing at a folder (grid view).
    pub fn new_folder(_cc: &eframe::CreationContext<'_>, folder: PathBuf) -> Self {
        let fv = FolderView::new(folder.clone());
        Self {
            mode: Mode::Folder(fv),
            folder_path: Some(folder),
        }
    }

    fn show_image(
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        path: &Path,
        texture: &mut Option<egui::TextureHandle>,
        error: &mut Option<String>,
    ) {
        // Load on first frame
        if texture.is_none() && error.is_none() {
            log::info!("Loading image: {}", path.display());
            match load_image(path) {
                Ok(decoded) => {
                    let size = [decoded.width as usize, decoded.height as usize];
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(size, &decoded.pixels);
                    *texture =
                        Some(ctx.load_texture("image", color_image, egui::TextureOptions::LINEAR));
                    log::info!("Loaded {}x{} image", size[0], size[1]);
                }
                Err(msg) => {
                    log::error!("{msg}");
                    *error = Some(msg);
                }
            }
        }

        if let Some(err) = error {
            ui.centered_and_justified(|ui| {
                ui.colored_label(egui::Color32::from_rgb(255, 80, 80), err.as_str());
            });
            return;
        }

        if let Some(tex) = texture {
            let available = ui.available_size();
            let tex_size = tex.size_vec2();

            let display_size = fit_size((tex_size.x, tex_size.y), (available.x, available.y));
            let offset = center_offset(display_size, (available.x, available.y));

            let rect = egui::Rect::from_min_size(
                ui.min_rect().min + egui::vec2(offset.0, offset.1),
                egui::vec2(display_size.0, display_size.1),
            );

            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let frame_style = egui::Frame::new().fill(egui::Color32::from_rgb(24, 24, 24));

        // Check for mode transition
        let mut transition = None;

        egui::CentralPanel::default()
            .frame(frame_style)
            .show(ctx, |ui| match &mut self.mode {
                Mode::Image {
                    path,
                    texture,
                    error,
                } => {
                    Self::show_image(ctx, ui, path, texture, error);
                }
                Mode::Folder(folder_view) => {
                    if let Some(clicked_idx) = folder_view.show(ctx, ui) {
                        let paths = folder_view.entry_paths();
                        if !paths.is_empty() {
                            transition = Some(Mode::ImageView(ImageView::new(paths, clicked_idx)));
                        }
                    }
                }
                Mode::ImageView(image_view) => {
                    if image_view.show(ctx, ui) {
                        // Go back to folder view
                        if let Some(folder) = &self.folder_path {
                            transition = Some(Mode::Folder(FolderView::new(folder.clone())));
                        }
                    }
                }
            });

        if let Some(new_mode) = transition {
            self.mode = new_mode;
        }
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
        assert!(result.unwrap_err().contains("Failed to read"));
    }

    #[test]
    fn load_image_not_an_image() {
        // Cargo.toml exists and is definitely not an image
        let result = load_image(Path::new("Cargo.toml"));
        assert!(result.is_err());
    }
}
