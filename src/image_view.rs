use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use crate::app::{DecodedImage, load_image, load_raw_full, load_raw_preview_image};
use crate::decode::is_raw_extension;

/// Duration of the crossfade from preview to full-res (seconds).
const CROSSFADE_DURATION: f32 = 0.3;

/// Full-resolution image viewer with zoom, pan, and navigation.
pub struct ImageView {
    /// All image paths in the folder (for left/right navigation).
    paths: Vec<PathBuf>,
    /// Current image index into `paths`.
    current: usize,
    /// Currently displayed texture (or full-res after upgrade).
    texture: Option<egui::TextureHandle>,
    /// Preview texture kept alive during crossfade.
    preview_texture: Option<egui::TextureHandle>,
    /// When the crossfade started (None if not crossfading).
    crossfade_start: Option<Instant>,
    /// Error message for the current image.
    error: Option<String>,
    /// Zoom level (1.0 = fit to window).
    zoom: f32,
    /// Pan offset in image pixels (only used when zoomed).
    pan: egui::Vec2,
    /// Whether we're in "fit to window" mode.
    fit_mode: bool,
    /// Background loader for the current image.
    loader: Option<mpsc::Receiver<Result<DecodedImage, String>>>,
    /// Whether a full-res upgrade is pending (progressive raw loading).
    upgrade_pending: bool,
    /// Background loader for full-res raw upgrade.
    upgrade_loader: Option<mpsc::Receiver<Option<DecodedImage>>>,
}

impl ImageView {
    pub fn new(paths: Vec<PathBuf>, start_index: usize) -> Self {
        let mut view = Self {
            paths,
            current: start_index,
            texture: None,
            preview_texture: None,
            crossfade_start: None,
            error: None,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            fit_mode: true,
            loader: None,
            upgrade_pending: false,
            upgrade_loader: None,
        };
        view.start_loading(start_index);
        view
    }

    /// Start loading an image on a background thread.
    /// For raw files, loads the fast preview first, then kicks off LibRaw upgrade.
    fn start_loading(&mut self, index: usize) {
        if index >= self.paths.len() {
            return;
        }

        let path = self.paths[index].clone();
        let is_raw = is_raw_extension(&path);
        let (tx, rx) = mpsc::channel();
        self.loader = Some(rx);
        self.upgrade_pending = is_raw;
        self.upgrade_loader = None;
        self.preview_texture = None;
        self.crossfade_start = None;

        if is_raw {
            // Phase 1: fast embedded JPEG preview (~8ms)
            thread::spawn(move || {
                let result = load_raw_preview_image(&path);
                let _ = tx.send(result);
            });
        } else {
            thread::spawn(move || {
                let result = load_image(&path);
                let _ = tx.send(result);
            });
        }
    }

    /// Start the full-res LibRaw decode after the preview is displayed.
    fn start_raw_upgrade(&mut self) {
        if !self.upgrade_pending {
            return;
        }
        let Some(path) = self.paths.get(self.current).cloned() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.upgrade_loader = Some(rx);

        thread::spawn(move || {
            let result = load_raw_full(&path);
            let _ = tx.send(result);
        });
    }

    /// Start preloading adjacent images.
    /// Pre-read adjacent files to warm OS file cache.
    fn preload_adjacent(&self) {
        for delta in [-1i64, 1] {
            let idx = self.current as i64 + delta;
            if idx >= 0 && (idx as usize) < self.paths.len() {
                let path = self.paths[idx as usize].clone();
                thread::spawn(move || {
                    let _ = std::fs::read(&path);
                });
            }
        }
    }

    /// Poll the background loader for results.
    fn poll_loader(&mut self, ctx: &egui::Context) {
        // Poll primary loader (fast preview or standard decode)
        if let Some(ref rx) = self.loader {
            match rx.try_recv() {
                Ok(Ok(decoded)) => {
                    let size = [decoded.width as usize, decoded.height as usize];
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(size, &decoded.pixels);
                    self.texture = Some(ctx.load_texture(
                        format!("full_{}", self.current),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.error = None;
                    self.loader = None;
                    self.reset_view();
                    self.preload_adjacent();
                    // For raw files, kick off the full-res upgrade
                    self.start_raw_upgrade();
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.texture = None;
                    self.loader = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.texture.is_none() && self.error.is_none() {
                        self.error = Some(rust_i18n::t!("error.load_failed").to_string());
                    }
                    self.loader = None;
                }
            }
        }

        // Poll raw upgrade loader (full-res LibRaw result)
        if let Some(ref rx) = self.upgrade_loader {
            match rx.try_recv() {
                Ok(Some(decoded)) => {
                    let size = [decoded.width as usize, decoded.height as usize];
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(size, &decoded.pixels);
                    // Move old preview to crossfade layer
                    self.preview_texture = self.texture.take();
                    self.texture = Some(ctx.load_texture(
                        format!("full_hires_{}", self.current),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.crossfade_start = Some(Instant::now());
                    self.upgrade_loader = None;
                    self.upgrade_pending = false;
                }
                Ok(None) => {
                    // LibRaw failed — keep the preview
                    self.upgrade_loader = None;
                    self.upgrade_pending = false;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.upgrade_loader = None;
                    self.upgrade_pending = false;
                }
            }
        }
    }

    /// Navigate to a different image.
    fn navigate_to(&mut self, index: usize) {
        if index >= self.paths.len() || index == self.current {
            return;
        }
        self.current = index;
        self.texture = None;
        self.preview_texture = None;
        self.crossfade_start = None;
        self.error = None;
        self.loader = None;
        self.upgrade_loader = None;
        self.upgrade_pending = false;
        self.reset_view();

        self.start_loading(index);
    }

    fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan = egui::Vec2::ZERO;
        self.fit_mode = true;
    }

    /// Render the image view. Returns true if user wants to go back to folder view.
    pub fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> bool {
        self.poll_loader(ctx);

        // Handle keyboard input — collect actions first, apply after
        let mut go_back = false;
        let mut nav_to: Option<usize> = None;
        let mut do_reset = false;
        let path_count = self.paths.len();

        ctx.input(|input| {
            if input.key_pressed(egui::Key::Escape) || input.key_pressed(egui::Key::Backspace) {
                go_back = true;
            }
            if input.key_pressed(egui::Key::ArrowLeft) && self.current > 0 {
                nav_to = Some(self.current - 1);
            }
            if input.key_pressed(egui::Key::ArrowRight) && self.current + 1 < path_count {
                nav_to = Some(self.current + 1);
            }
            if input.key_pressed(egui::Key::F) {
                do_reset = true;
            }
            if input.key_pressed(egui::Key::Home) {
                nav_to = Some(0);
            }
            if input.key_pressed(egui::Key::End) {
                nav_to = Some(path_count.saturating_sub(1));
            }
        });

        if go_back {
            return true;
        }
        if do_reset {
            self.reset_view();
        }
        if let Some(idx) = nav_to {
            self.navigate_to(idx);
        }

        // Status bar
        let filename = self
            .paths
            .get(self.current)
            .and_then(|p| p.file_name())
            .unwrap_or_default()
            .to_string_lossy();
        let status = rust_i18n::t!(
            "image.status",
            filename = filename,
            current = self.current + 1,
            total = self.paths.len()
        );
        ui.label(
            egui::RichText::new(status)
                .color(egui::Color32::from_rgb(180, 180, 180))
                .size(13.0),
        );
        ui.add_space(4.0);

        // Image display area
        let available = ui.available_size();

        if let Some(ref error) = self.error {
            ui.centered_and_justified(|ui| {
                ui.colored_label(egui::Color32::from_rgb(255, 80, 80), error.as_str());
            });
            return false;
        }

        // Extract texture info before mutable borrows
        let tex_info = self.texture.as_ref().map(|t| (t.id(), t.size_vec2()));

        // Compute crossfade progress (0.0 = all preview, 1.0 = all full-res)
        let crossfade_t = self.crossfade_start.map(|start| {
            let elapsed = start.elapsed().as_secs_f32();
            (elapsed / CROSSFADE_DURATION).min(1.0)
        });

        // Clean up crossfade when complete
        if crossfade_t == Some(1.0) {
            self.preview_texture = None;
            self.crossfade_start = None;
        }

        // Request repaints during crossfade animation
        if crossfade_t.is_some_and(|t| t < 1.0) {
            ctx.request_repaint();
        }

        let preview_info = self
            .preview_texture
            .as_ref()
            .map(|t| (t.id(), t.size_vec2()));

        if let Some((tex_id, tex_size)) = tex_info {
            // Compute display size — scale to fill viewport, allowing upscale
            let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
            let (fit_w, fit_h) = (tex_size.x * scale, tex_size.y * scale);

            let (display_w, display_h) = if self.fit_mode {
                (fit_w, fit_h)
            } else {
                (tex_size.x * self.zoom, tex_size.y * self.zoom)
            };

            // Handle mouse interactions
            let response = ui.allocate_rect(
                egui::Rect::from_min_size(ui.min_rect().min, available),
                egui::Sense::click_and_drag(),
            );

            let scroll_delta = ctx.input(|input| input.smooth_scroll_delta.y);
            if scroll_delta != 0.0 {
                self.fit_mode = false;
                let zoom_factor = if scroll_delta > 0.0 { 1.1 } else { 1.0 / 1.1 };
                self.zoom = (self.zoom * zoom_factor).clamp(0.1, 20.0);
            }

            if response.dragged() {
                self.fit_mode = false;
                self.pan += response.drag_delta();
            }

            if response.double_clicked() {
                if self.fit_mode {
                    self.fit_mode = false;
                    self.zoom = 1.0;
                    self.pan = egui::Vec2::ZERO;
                } else {
                    self.reset_view();
                }
            }

            // Compute image rect
            let center = egui::pos2(
                ui.min_rect().min.x + available.x / 2.0 + self.pan.x,
                ui.min_rect().min.y + available.y / 2.0 + self.pan.y,
            );
            let img_rect = egui::Rect::from_center_size(center, egui::vec2(display_w, display_h));
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

            if let (Some(t), Some((preview_id, _))) = (crossfade_t, preview_info) {
                // Crossfading: draw preview at (1-t) alpha, then full-res at t alpha
                // Use smooth ease-in-out for perceptually pleasant transition
                let t_smooth = t * t * (3.0 - 2.0 * t); // smoothstep
                let preview_alpha = ((1.0 - t_smooth) * 255.0) as u8;
                let full_alpha = (t_smooth * 255.0) as u8;

                // Preview layer (fading out)
                ui.painter().image(
                    preview_id,
                    img_rect,
                    uv,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, preview_alpha),
                );
                // Full-res layer (fading in)
                ui.painter().image(
                    tex_id,
                    img_rect,
                    uv,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, full_alpha),
                );
            } else {
                // Normal display — single texture at full alpha
                ui.painter()
                    .image(tex_id, img_rect, uv, egui::Color32::WHITE);
            }
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
        }

        false
    }
}
