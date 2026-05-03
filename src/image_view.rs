use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::app::{
    DecodedImage, load_image_with_develop, load_raw_full_with_develop,
    load_raw_preview_image_with_develop,
};
use crate::decode::{ExifMetadata, is_raw_extension, read_exif_metadata_from_path};
use crate::develop::{XmpDevelopSetting, XmpDevelopSettings, read_xmp_develop_settings_from_path};

/// Duration of the crossfade from preview to full-res (seconds).
const CROSSFADE_DURATION: f32 = 0.3;
const INFO_PANE_WIDTH: f32 = 260.0;

#[derive(Debug, Clone, Default)]
struct ImageInfo {
    modified: Option<String>,
    exif: ExifMetadata,
    develop: XmpDevelopSettings,
}

fn read_image_info(path: &PathBuf) -> ImageInfo {
    ImageInfo {
        modified: std::fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(format_system_time),
        exif: read_exif_metadata_from_path(path),
        develop: read_xmp_develop_settings_from_path(path),
    }
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("iv").join("config.txt"))
}

fn load_info_pane_open() -> bool {
    let Some(path) = config_path() else {
        return true;
    };
    match std::fs::read_to_string(path) {
        Ok(text) => !text.lines().any(|line| line.trim() == "info_pane=false"),
        Err(_) => true,
    }
}

fn save_info_pane_open(open: bool) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        format!("info_pane={}\n", if open { "true" } else { "false" }),
    );
}

fn format_system_time(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

/// Full-resolution image viewer with zoom, pan, and navigation.
pub struct ImageView {
    /// All image paths in the folder (for left/right navigation).
    paths: Vec<PathBuf>,
    /// Optional paired Live Photo movie for each image path.
    live_videos: Vec<Option<PathBuf>>,
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
    /// Whether the next primary image load should reset zoom/pan.
    reset_view_after_load: bool,
    /// Whether a full-res upgrade is pending (progressive raw loading).
    upgrade_pending: bool,
    /// Background loader for full-res raw upgrade.
    upgrade_loader: Option<mpsc::Receiver<Option<DecodedImage>>>,
    /// Viewport size at last layout (for resolution-aware upgrade decision).
    last_viewport: egui::Vec2,
    /// Whether the left info pane is visible.
    info_pane_open: bool,
    /// Whether XMP lossless/develop edits are applied to decoded pixels.
    apply_lossless_edits: bool,
    /// Cached metadata for the current image.
    image_info: Option<ImageInfo>,
    /// Background loader for current image metadata.
    info_loader: Option<mpsc::Receiver<ImageInfo>>,
}

impl ImageView {
    pub fn new(paths: Vec<PathBuf>, live_videos: Vec<Option<PathBuf>>, start_index: usize) -> Self {
        let mut view = Self {
            paths,
            live_videos,
            current: start_index,
            texture: None,
            preview_texture: None,
            crossfade_start: None,
            error: None,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            fit_mode: true,
            loader: None,
            reset_view_after_load: true,
            upgrade_pending: false,
            upgrade_loader: None,
            last_viewport: egui::Vec2::ZERO,
            info_pane_open: load_info_pane_open(),
            apply_lossless_edits: true,
            image_info: None,
            info_loader: None,
        };
        view.start_loading(start_index, true, true);
        view
    }

    /// Start loading an image on a background thread.
    /// For raw files, loads the fast preview first, then kicks off LibRaw upgrade.
    fn start_loading(&mut self, index: usize, reload_info: bool, reset_view_after_load: bool) {
        if index >= self.paths.len() {
            return;
        }

        let path = self.paths[index].clone();
        let is_raw = is_raw_extension(&path);
        let apply_lossless_edits = self.apply_lossless_edits;
        let (tx, rx) = mpsc::channel();
        self.loader = Some(rx);
        self.reset_view_after_load = reset_view_after_load;
        self.upgrade_pending = is_raw;
        self.upgrade_loader = None;
        self.preview_texture = None;
        self.crossfade_start = None;
        if reload_info {
            self.image_info = None;
            self.start_info_loading(index);
        }

        if is_raw {
            // Phase 1: fast embedded JPEG preview (~8ms)
            thread::spawn(move || {
                let result = load_raw_preview_image_with_develop(&path, apply_lossless_edits);
                let _ = tx.send(result);
            });
        } else {
            thread::spawn(move || {
                let result = load_image_with_develop(&path, apply_lossless_edits);
                let _ = tx.send(result);
            });
        }
    }

    fn start_info_loading(&mut self, index: usize) {
        let Some(path) = self.paths.get(index).cloned() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.info_loader = Some(rx);
        thread::spawn(move || {
            let info = read_image_info(&path);
            let _ = tx.send(info);
        });
    }

    /// Start the full-res LibRaw decode after the preview is displayed.
    /// Skips the upgrade if the preview already covers the viewport.
    fn start_raw_upgrade(&mut self) {
        if !self.upgrade_pending {
            return;
        }

        // Check if the preview already has enough resolution for the viewport
        if let Some(ref tex) = self.texture {
            let tex_size = tex.size_vec2();
            if !crate::decode::needs_upscale(
                tex_size.x as u32,
                tex_size.y as u32,
                self.last_viewport.x,
                self.last_viewport.y,
            ) {
                self.upgrade_pending = false;
                return;
            }
        }

        let Some(path) = self.paths.get(self.current).cloned() else {
            return;
        };
        let apply_lossless_edits = self.apply_lossless_edits;
        let (tx, rx) = mpsc::channel();
        self.upgrade_loader = Some(rx);

        thread::spawn(move || {
            let result = load_raw_full_with_develop(&path, apply_lossless_edits);
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
                    if self.reset_view_after_load {
                        self.reset_view();
                    }
                    self.reset_view_after_load = false;
                    self.preload_adjacent();
                    // For raw files, kick off the full-res upgrade
                    self.start_raw_upgrade();
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.texture = None;
                    self.loader = None;
                    self.reset_view_after_load = false;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.texture.is_none() && self.error.is_none() {
                        self.error = Some(rust_i18n::t!("error.load_failed").to_string());
                    }
                    self.loader = None;
                    self.reset_view_after_load = false;
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

        if let Some(ref rx) = self.info_loader {
            match rx.try_recv() {
                Ok(info) => {
                    self.image_info = Some(info);
                    self.info_loader = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.info_loader = None;
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
        self.info_loader = None;
        self.upgrade_pending = false;
        self.image_info = None;
        self.reset_view();

        self.start_loading(index, true, true);
    }

    fn reload_current_image(&mut self, reset_view_after_load: bool) {
        self.texture = None;
        self.preview_texture = None;
        self.crossfade_start = None;
        self.error = None;
        self.loader = None;
        self.upgrade_loader = None;
        self.upgrade_pending = false;
        self.start_loading(self.current, false, reset_view_after_load);
    }

    fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan = egui::Vec2::ZERO;
        self.fit_mode = true;
    }

    fn toggle_info_pane(&mut self) {
        self.info_pane_open = !self.info_pane_open;
        save_info_pane_open(self.info_pane_open);
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
            if input.key_pressed(egui::Key::I) {
                self.toggle_info_pane();
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

        if self.info_pane_open {
            egui::SidePanel::left("iv_image_info_pane")
                .resizable(true)
                .default_width(INFO_PANE_WIDTH)
                .width_range(200.0..=360.0)
                .show_inside(ui, |ui| {
                    self.render_info_pane(ui);
                });
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
        ui.horizontal(|ui| {
            if !self.info_pane_open
                && ui
                    .add_sized(
                        [22.0, 20.0],
                        egui::Button::new(rust_i18n::t!("image.show_info_pane_button")),
                    )
                    .on_hover_text(rust_i18n::t!("image.show_info_pane"))
                    .clicked()
            {
                self.toggle_info_pane();
            }
            ui.label(
                egui::RichText::new(status)
                    .color(egui::Color32::from_rgb(180, 180, 180))
                    .size(13.0),
            );
            if self.current_live_video().is_some()
                && ui
                    .add_sized([96.0, 22.0], egui::Button::new(rust_i18n::t!("image.live")))
                    .on_hover_text(rust_i18n::t!("image.play_live_photo_movie"))
                    .clicked()
            {
                self.open_current_live_video();
            }
        });
        ui.add_space(4.0);

        // Image display area
        let image_rect = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let response = ui.allocate_rect(image_rect, egui::Sense::click_and_drag());
        let available = response.rect.size();
        self.last_viewport = available;

        if let Some(ref error) = self.error {
            ui.painter().text(
                response.rect.center(),
                egui::Align2::CENTER_CENTER,
                error.as_str(),
                egui::FontId::proportional(14.0),
                egui::Color32::from_rgb(255, 80, 80),
            );
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

            let scroll_delta = ctx.input(|input| input.smooth_scroll_delta.y);
            if response.hovered() && scroll_delta != 0.0 {
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
            let center = response.rect.center() + self.pan;
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
            let spinner_rect =
                egui::Rect::from_center_size(response.rect.center(), egui::vec2(24.0, 24.0));
            ui.put(spinner_rect, egui::Spinner::new());
        }

        false
    }

    fn current_live_video(&self) -> Option<&PathBuf> {
        self.live_videos.get(self.current)?.as_ref()
    }

    fn open_current_live_video(&self) {
        let Some(path) = self.current_live_video() else {
            return;
        };
        crate::launcher::open_with_default_app(path);
    }

    fn render_info_pane(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(rust_i18n::t!("image.info.title"))
                    .color(egui::Color32::from_rgb(210, 210, 210))
                    .size(14.0)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_sized(
                        [22.0, 20.0],
                        egui::Button::new(rust_i18n::t!("image.hide_info_pane_button")),
                    )
                    .on_hover_text(rust_i18n::t!("image.hide_info_pane"))
                    .clicked()
                {
                    self.toggle_info_pane();
                }
            });
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    let Some((name, path_text)) = self.paths.get(self.current).map(|path| {
                        (
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string(),
                            path.display().to_string(),
                        )
                    }) else {
                        Self::info_row(
                            ui,
                            &rust_i18n::t!("image.info.file"),
                            &rust_i18n::t!("image.info.not_available"),
                        );
                        return;
                    };

                    Self::info_row(ui, &rust_i18n::t!("image.info.file"), &name);
                    if let Some(modified) = self
                        .image_info
                        .as_ref()
                        .and_then(|info| info.modified.as_deref())
                    {
                        Self::info_row(ui, &rust_i18n::t!("image.info.file_date"), modified);
                    } else {
                        Self::info_row(
                            ui,
                            &rust_i18n::t!("image.info.file_date"),
                            &rust_i18n::t!("status.loading"),
                        );
                    }

                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(rust_i18n::t!("image.info.exif"))
                            .color(egui::Color32::from_rgb(170, 170, 170))
                            .size(11.0)
                            .strong(),
                    );

                    if let Some(info) = &self.image_info {
                        Self::info_row_opt(
                            ui,
                            &rust_i18n::t!("image.info.date_taken"),
                            info.exif.date_taken.as_deref(),
                        );
                        Self::info_row_opt(
                            ui,
                            &rust_i18n::t!("image.info.camera"),
                            info.exif.camera.as_deref(),
                        );
                        Self::info_row_opt(
                            ui,
                            &rust_i18n::t!("image.info.lens"),
                            info.exif.lens.as_deref(),
                        );
                        Self::info_row_opt(
                            ui,
                            &rust_i18n::t!("image.info.focal_length"),
                            info.exif.focal_length.as_deref(),
                        );
                        Self::info_row_opt(
                            ui,
                            &rust_i18n::t!("image.info.aperture"),
                            info.exif.aperture.as_deref(),
                        );
                        Self::info_row_opt(
                            ui,
                            &rust_i18n::t!("image.info.shutter"),
                            info.exif.shutter_speed.as_deref(),
                        );
                        Self::info_row_opt(
                            ui,
                            &rust_i18n::t!("image.info.iso"),
                            info.exif.iso.as_deref(),
                        );
                    } else {
                        Self::info_row(
                            ui,
                            &rust_i18n::t!("image.info.date_taken"),
                            &rust_i18n::t!("status.loading"),
                        );
                        Self::info_row(
                            ui,
                            &rust_i18n::t!("image.info.camera"),
                            &rust_i18n::t!("status.loading"),
                        );
                        Self::info_row(
                            ui,
                            &rust_i18n::t!("image.info.focal_length"),
                            &rust_i18n::t!("status.loading"),
                        );
                    }

                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(rust_i18n::t!("image.info.develop"))
                            .color(egui::Color32::from_rgb(170, 170, 170))
                            .size(11.0)
                            .strong(),
                    );
                    if Self::lossless_edits_toggle(ui, &mut self.apply_lossless_edits).changed() {
                        self.reload_current_image(false);
                    }

                    if let Some(info) = &self.image_info {
                        if !info.develop.has_visible_settings() {
                            Self::info_row(
                                ui,
                                &rust_i18n::t!("image.info.settings"),
                                &rust_i18n::t!("image.info.not_available"),
                            );
                        } else {
                            let source = info
                                .develop
                                .source
                                .map(|source| source.label())
                                .unwrap_or("XMP");
                            Self::info_row(ui, &rust_i18n::t!("image.info.source"), source);
                            for setting in info.develop.visible_settings() {
                                Self::develop_row(ui, setting, self.apply_lossless_edits);
                            }
                        }
                    } else {
                        Self::info_row(
                            ui,
                            &rust_i18n::t!("image.info.settings"),
                            &rust_i18n::t!("status.loading"),
                        );
                    }

                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(path_text)
                            .color(egui::Color32::from_rgb(100, 100, 100))
                            .size(10.0),
                    );
                });
            });
    }

    fn develop_row(ui: &mut egui::Ui, setting: &XmpDevelopSetting, edits_active: bool) {
        let color = if setting.unsupported {
            egui::Color32::from_rgb(235, 76, 76)
        } else if edits_active && setting.applied {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(105, 105, 105)
        };
        ui.horizontal_wrapped(|ui| {
            ui.set_min_height(18.0);
            ui.label(egui::RichText::new(&setting.name).color(color).size(11.0));
            ui.label(egui::RichText::new(&setting.value).color(color).size(12.0));
        });
    }

    fn lossless_edits_toggle(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
        let row_height = 24.0;
        let switch_size = egui::vec2(38.0, 20.0);
        let (rect, mut response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::click(),
        );
        if response.clicked() {
            *value = !*value;
            response.mark_changed();
        }

        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let switch_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - switch_size.y * 0.5),
            switch_size,
        );
        let track_color = match (*value, response.hovered()) {
            (true, true) => egui::Color32::from_rgb(74, 155, 96),
            (true, false) => egui::Color32::from_rgb(58, 135, 82),
            (false, true) => egui::Color32::from_rgb(92, 92, 92),
            (false, false) => egui::Color32::from_rgb(70, 70, 70),
        };
        let knob_radius = 7.5;
        let knob_x = if *value {
            switch_rect.right() - switch_size.y * 0.5
        } else {
            switch_rect.left() + switch_size.y * 0.5
        };
        ui.painter()
            .rect_filled(switch_rect, switch_size.y * 0.5, track_color);
        ui.painter().circle_filled(
            egui::pos2(knob_x, switch_rect.center().y),
            knob_radius,
            egui::Color32::from_rgb(235, 235, 235),
        );
        ui.painter().text(
            egui::pos2(switch_rect.right() + 8.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            rust_i18n::t!("image.info.apply_lossless_edits"),
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(205, 205, 205),
        );

        response.on_hover_text(rust_i18n::t!("image.info.apply_lossless_edits_hover"))
    }

    fn info_row_opt(ui: &mut egui::Ui, label: &str, value: Option<&str>) {
        Self::info_row(
            ui,
            label,
            value.unwrap_or(&rust_i18n::t!("image.info.not_available")),
        );
    }

    fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
        ui.horizontal_wrapped(|ui| {
            ui.set_min_height(18.0);
            ui.label(
                egui::RichText::new(label)
                    .color(egui::Color32::from_rgb(120, 120, 120))
                    .size(11.0),
            );
            ui.label(
                egui::RichText::new(value)
                    .color(egui::Color32::from_rgb(205, 205, 205))
                    .size(12.0),
            );
        });
    }
}
