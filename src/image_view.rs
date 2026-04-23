use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::app::{DecodedImage, load_image};

/// Full-resolution image viewer with zoom, pan, and navigation.
pub struct ImageView {
    /// All image paths in the folder (for left/right navigation).
    paths: Vec<PathBuf>,
    /// Current image index into `paths`.
    current: usize,
    /// Currently displayed texture.
    texture: Option<egui::TextureHandle>,
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
}

impl ImageView {
    pub fn new(paths: Vec<PathBuf>, start_index: usize) -> Self {
        let mut view = Self {
            paths,
            current: start_index,
            texture: None,
            error: None,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            fit_mode: true,
            loader: None,
        };
        view.start_loading(start_index);
        view
    }

    /// Start loading an image on a background thread.
    fn start_loading(&mut self, index: usize) {
        if index >= self.paths.len() {
            return;
        }

        let path = self.paths[index].clone();
        let (tx, rx) = mpsc::channel();
        self.loader = Some(rx);

        thread::spawn(move || {
            let result = load_image(&path);
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
    }

    /// Navigate to a different image.
    fn navigate_to(&mut self, index: usize) {
        if index >= self.paths.len() || index == self.current {
            return;
        }
        self.current = index;
        self.texture = None;
        self.error = None;
        self.loader = None;
        self.reset_view();

        self.loader = None;
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

            ui.painter().image(
                tex_id,
                img_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
        }

        false
    }
}
