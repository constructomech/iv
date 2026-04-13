use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::app::DecodedImage;
use crate::decode;
use crate::decode::DecodeTimings;
use crate::enumerator::{self, EnumHandle, EnumMessage};
use crate::scheduler::{Scheduler, ThumbState, WorkItem, WorkTier};

/// Returns true if IV_DEBUG env var is set to a truthy value.
fn debug_mode() -> bool {
    std::env::var("IV_DEBUG").map_or(false, |v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Thumbnail tile size in the grid.
const TILE_SIZE: f32 = 160.0;
/// Padding between tiles.
const TILE_PADDING: f32 = 8.0;
/// Height reserved for the filename label below the tile.
const LABEL_HEIGHT: f32 = 20.0;
/// Full cell size including padding and label.
const CELL_SIZE: f32 = TILE_SIZE + TILE_PADDING + LABEL_HEIGHT;
/// Thumbnail decode resolution (pixels).
const THUMB_DECODE_SIZE: u32 = 160;
/// Number of thumbnail worker threads.
/// Reserves 2 cores for UI + enumerator, uses the rest for decoding.
fn num_thumb_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(2))
        .unwrap_or(4)
}

/// Result sent back from a thumbnail worker.
enum ThumbResult {
    /// EXIF extraction succeeded — here's the thumbnail.
    ExifOk {
        idx: usize,
        image: DecodedImage,
        timings: decode::DecodeTimings,
    },
    /// EXIF extraction failed — tile needs full decode.
    ExifMiss {
        idx: usize,
        timings: decode::DecodeTimings,
    },
    /// Full decode succeeded.
    FullOk {
        idx: usize,
        image: DecodedImage,
        timings: decode::DecodeTimings,
    },
    /// Full decode failed.
    FullErr { idx: usize, error: String },
}

/// Manages a pool of worker threads that decode thumbnails.
struct ThumbLoader {
    work_tx: crossbeam_channel::Sender<WorkItem>,
    result_rx: crossbeam_channel::Receiver<ThumbResult>,
    /// Shared generation counter — workers check this to skip stale items.
    generation: Arc<AtomicU64>,
}

impl ThumbLoader {
    fn new() -> Self {
        let (work_tx, work_rx) = crossbeam_channel::unbounded::<WorkItem>();
        let (result_tx, result_rx) = crossbeam_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));

        let num_workers = num_thumb_workers();
        log::info!("Starting {num_workers} thumbnail worker threads");
        for _ in 0..num_workers {
            let work_rx = work_rx.clone(); // crossbeam Receiver is Clone — no mutex needed
            let result_tx = result_tx.clone();
            let generation = generation.clone();
            thread::spawn(move || {
                while let Ok(work) = work_rx.recv() {
                    // Skip stale work
                    if work.generation < generation.load(Ordering::Relaxed) {
                        continue;
                    }

                    let result = match work.tier {
                        WorkTier::ExifOnly => {
                            let (exif_result, timings) = decode::try_exif_only(&work.path);
                            match exif_result {
                                Some(image) => ThumbResult::ExifOk {
                                    idx: work.idx,
                                    image,
                                    timings,
                                },
                                None => ThumbResult::ExifMiss {
                                    idx: work.idx,
                                    timings,
                                },
                            }
                        }
                        WorkTier::FullDecode => {
                            match decode::decode_full_thumbnail(&work.path, THUMB_DECODE_SIZE) {
                                Ok((image, timings)) => ThumbResult::FullOk {
                                    idx: work.idx,
                                    image,
                                    timings,
                                },
                                Err(e) => ThumbResult::FullErr {
                                    idx: work.idx,
                                    error: e,
                                },
                            }
                        }
                    };

                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            });
        }

        Self {
            work_tx,
            result_rx,
            generation,
        }
    }

    /// Send work items to the thread pool.
    fn send_batch(&self, batch: Vec<WorkItem>) {
        for item in batch {
            let _ = self.work_tx.send(item);
        }
    }

    /// Sync the shared generation counter with the scheduler's.
    fn sync_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
    }
}

/// State for the folder grid view.
pub struct FolderView {
    /// Directory being viewed.
    folder: PathBuf,
    /// Scheduling logic (no GPU dependency).
    scheduler: Scheduler,
    /// GPU textures, indexed same as scheduler entries.
    textures: Vec<Option<egui::TextureHandle>>,
    /// Whether enumeration has completed.
    enum_done: bool,
    /// Handle to the background enumerator.
    enum_handle: Option<EnumHandle>,
    /// Thumbnail loader thread pool.
    thumb_loader: ThumbLoader,
    /// Error from enumeration, if any.
    error: Option<String>,
}

impl FolderView {
    pub fn new(folder: PathBuf) -> Self {
        let enum_handle = Some(enumerator::enumerate_folder(folder.clone()));
        Self {
            folder,
            scheduler: Scheduler::new(),
            textures: Vec::new(),
            enum_done: false,
            enum_handle,
            thumb_loader: ThumbLoader::new(),
            error: None,
        }
    }

    /// Drain pending messages from the enumerator (non-blocking).
    fn poll_enumerator(&mut self) {
        if let Some(ref handle) = self.enum_handle {
            loop {
                match handle.receiver.try_recv() {
                    Ok(EnumMessage::Found(path)) => {
                        self.scheduler.add_entry(path);
                        self.textures.push(None);
                    }
                    Ok(EnumMessage::Done(count)) => {
                        self.enum_done = true;
                        log::info!(
                            "Enumeration complete: {} images in {}",
                            count,
                            self.folder.display()
                        );
                        break;
                    }
                    Ok(EnumMessage::Error(e)) => {
                        log::error!("Enumeration error: {e}");
                        self.error = Some(e);
                        self.enum_done = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if !self.enum_done {
                            self.enum_done = true;
                            log::warn!("Enumerator disconnected unexpectedly");
                        }
                        break;
                    }
                }
            }
        }

        if self.enum_done {
            self.enum_handle = None;
        }
    }

    /// Drain completed results from the loader and update scheduler/textures.
    fn poll_thumbnails(&mut self, ctx: &egui::Context) {
        let mut exif_ok = 0u32;
        let mut exif_miss = 0u32;
        let mut full_ok = 0u32;
        let mut full_err = 0u32;
        while let Ok(result) = self.thumb_loader.result_rx.try_recv() {
            match result {
                ThumbResult::ExifOk {
                    idx,
                    image,
                    timings,
                } => {
                    exif_ok += 1;
                    if idx < self.scheduler.len() {
                        let size = [image.width as usize, image.height as usize];
                        let color_image =
                            egui::ColorImage::from_rgba_unmultiplied(size, &image.pixels);
                        let texture = ctx.load_texture(
                            format!("thumb_{idx}"),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.textures[idx] = Some(texture);
                        self.scheduler.complete(idx, true, timings);
                    }
                }
                ThumbResult::ExifMiss { idx, timings } => {
                    exif_miss += 1;
                    if idx < self.scheduler.len() {
                        self.scheduler.exif_failed(idx, timings);
                    }
                }
                ThumbResult::FullOk {
                    idx,
                    image,
                    timings,
                } => {
                    full_ok += 1;
                    if idx < self.scheduler.len() {
                        let size = [image.width as usize, image.height as usize];
                        let color_image =
                            egui::ColorImage::from_rgba_unmultiplied(size, &image.pixels);
                        let texture = ctx.load_texture(
                            format!("thumb_{idx}"),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.textures[idx] = Some(texture);
                        self.scheduler.complete(idx, false, timings);
                    }
                }
                ThumbResult::FullErr { idx, error } => {
                    full_err += 1;
                    if idx < self.scheduler.len() {
                        log::warn!(
                            "Thumbnail failed for {}: {error}",
                            self.scheduler.entry(idx).path.display()
                        );
                        self.scheduler.fail(idx);
                    }
                }
            }
        }
        let total = exif_ok + exif_miss + full_ok + full_err;
        if total > 0 {
            log::debug!(
                "poll_thumbnails: {} results (exif_ok={}, exif_miss={}, full_ok={}, full_err={})",
                total,
                exif_ok,
                exif_miss,
                full_ok,
                full_err
            );
        }
    }

    /// Render the folder view. Returns Some(index) if a tile was clicked.
    pub fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> Option<usize> {
        self.poll_enumerator();
        self.poll_thumbnails(ctx);

        // Request repaint while work is pending
        if !self.enum_done || self.scheduler.has_pending_work() {
            ctx.request_repaint();
        }

        // Show error if enumeration failed
        if let Some(ref error) = self.error {
            ui.centered_and_justified(|ui| {
                ui.colored_label(egui::Color32::from_rgb(255, 80, 80), error);
            });
            return None;
        }

        // Status bar at top
        let total = self.scheduler.len();
        let loaded = self.scheduler.count_in_state(ThumbState::Loaded);
        let status = if self.enum_done && loaded == total {
            format!("{} — {} images", self.folder.display(), total)
        } else if self.enum_done {
            format!(
                "{} — loading thumbnails ({}/{})…",
                self.folder.display(),
                loaded,
                total
            )
        } else {
            format!("{} — scanning… ({} found)", self.folder.display(), total)
        };
        ui.label(
            egui::RichText::new(status)
                .color(egui::Color32::from_rgb(180, 180, 180))
                .size(13.0),
        );
        ui.add_space(4.0);

        let mut clicked_index = None;

        // Scrollable grid
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                let available_width = ui.available_width();
                let cols = ((available_width + TILE_PADDING) / (TILE_SIZE + TILE_PADDING))
                    .floor()
                    .max(1.0) as usize;

                // Compute scroll position and update scheduler
                let scroll_offset = ui.clip_rect().min.y - ui.min_rect().min.y;
                let viewport_height = ui.clip_rect().height();
                let bumped = self.scheduler.update_visibility(
                    scroll_offset,
                    viewport_height,
                    cols,
                    CELL_SIZE,
                );
                if bumped {
                    self.thumb_loader
                        .sync_generation(self.scheduler.generation());
                }

                // Get work batch and send to thread pool
                let batch = self.scheduler.get_work_batch();
                if !batch.is_empty() {
                    log::debug!(
                        "Sending batch: {} items, tier={:?}",
                        batch.len(),
                        batch.first().map(|w| w.tier)
                    );
                    self.thumb_loader.send_batch(batch);
                }

                // Render tiles
                let debug = debug_mode();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(TILE_PADDING, TILE_PADDING);

                    for idx in 0..self.scheduler.len() {
                        let entry = self.scheduler.entry(idx);
                        let texture = &self.textures[idx];
                        let debug_info = if debug { entry.timings.as_ref() } else { None };
                        let response = Self::render_tile(ui, &entry.path, texture, debug_info);
                        if response.clicked() {
                            clicked_index = Some(idx);
                        }
                    }
                });
            });

        clicked_index
    }

    /// Render a single tile (thumbnail or placeholder + filename).
    fn render_tile(
        ui: &mut egui::Ui,
        path: &PathBuf,
        texture: &Option<egui::TextureHandle>,
        debug_timings: Option<&DecodeTimings>,
    ) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(TILE_SIZE, CELL_SIZE), egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);

            let thumb_rect = egui::Rect::from_min_size(rect.min, egui::vec2(TILE_SIZE, TILE_SIZE));

            if let Some(tex) = texture {
                // Draw the decoded thumbnail, centered within the tile
                let tex_size = tex.size_vec2();
                let scale = (TILE_SIZE / tex_size.x).min(TILE_SIZE / tex_size.y);
                let display_w = tex_size.x * scale;
                let display_h = tex_size.y * scale;
                let offset_x = (TILE_SIZE - display_w) / 2.0;
                let offset_y = (TILE_SIZE - display_h) / 2.0;

                // Dark background for letterboxing
                painter.rect_filled(thumb_rect, 2.0, egui::Color32::from_rgb(24, 24, 24));

                let img_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.min.x + offset_x, rect.min.y + offset_y),
                    egui::vec2(display_w, display_h),
                );
                painter.image(
                    tex.id(),
                    img_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                // Hover overlay
                if response.hovered() {
                    painter.rect_filled(
                        thumb_rect,
                        2.0,
                        egui::Color32::from_rgba_premultiplied(255, 255, 255, 20),
                    );
                }

                // Debug: timing overlay
                if let Some(timings) = debug_timings {
                    let mut lines: Vec<(String, egui::Color32)> = Vec::new();

                    // EXIF line — green if it was used, gray if attempted but failed
                    if timings.full_ms == 0.0 {
                        // EXIF was used (no full decode needed)
                        lines.push((
                            format!("EXIF {:.1}ms", timings.exif_ms),
                            egui::Color32::from_rgb(80, 220, 80),
                        ));
                    } else {
                        // EXIF attempted but not found, show in gray
                        lines.push((
                            format!("EXIF {:.1}ms", timings.exif_ms),
                            egui::Color32::from_rgb(140, 140, 140),
                        ));
                        lines.push((
                            format!("Full {:.1}ms", timings.full_ms),
                            egui::Color32::from_rgb(220, 180, 80),
                        ));
                    }

                    let line_h = 13.0;
                    let badge_h = lines.len() as f32 * line_h + 4.0;
                    let badge_w = 80.0;
                    let badge_rect = egui::Rect::from_min_size(
                        egui::pos2(thumb_rect.max.x - badge_w - 2.0, thumb_rect.min.y + 2.0),
                        egui::vec2(badge_w, badge_h),
                    );
                    painter.rect_filled(
                        badge_rect,
                        2.0,
                        egui::Color32::from_rgba_premultiplied(0, 0, 0, 180),
                    );

                    for (i, (text, color)) in lines.iter().enumerate() {
                        let y = badge_rect.min.y + 2.0 + i as f32 * line_h + line_h / 2.0;
                        painter.text(
                            egui::pos2(badge_rect.center().x, y),
                            egui::Align2::CENTER_CENTER,
                            text,
                            egui::FontId::monospace(9.0),
                            *color,
                        );
                    }
                }
            } else {
                // Gray placeholder
                let bg_color = if response.hovered() {
                    egui::Color32::from_rgb(70, 70, 70)
                } else {
                    egui::Color32::from_rgb(48, 48, 48)
                };
                painter.rect_filled(thumb_rect, 2.0, bg_color);
            }

            // Filename label
            let filename = path.file_name().unwrap_or_default().to_string_lossy();
            let label_rect = egui::Rect::from_min_size(
                egui::pos2(rect.min.x, rect.min.y + TILE_SIZE + 2.0),
                egui::vec2(TILE_SIZE, LABEL_HEIGHT),
            );
            painter.text(
                label_rect.center(),
                egui::Align2::CENTER_CENTER,
                truncate_filename(&filename, 20),
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgb(200, 200, 200),
            );
        }

        response
    }

    /// Access scheduler (for testing).
    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.scheduler.len()
    }

    /// Check if enumeration is done (for testing).
    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        self.enum_done
    }

    /// Get all entry paths (for image view navigation).
    pub fn entry_paths(&self) -> Vec<PathBuf> {
        (0..self.scheduler.len())
            .map(|i| self.scheduler.entry(i).path.clone())
            .collect()
    }
}

/// Truncate a filename for display, keeping extension visible.
fn truncate_filename(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        return name.to_string();
    }
    // Keep extension visible: "longfilena….jpg"
    if let Some(dot_pos) = name.rfind('.') {
        let ext = &name[dot_pos..];
        let keep = max_len.saturating_sub(ext.len() + 1);
        if keep > 0 {
            return format!("{}…{}", &name[..keep], ext);
        }
    }
    format!("{}…", &name[..max_len - 1])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_name() {
        assert_eq!(truncate_filename("photo.jpg", 20), "photo.jpg");
    }

    #[test]
    fn truncate_long_name() {
        let name = "a_very_long_filename_that_exceeds_limit.jpg";
        let result = truncate_filename(name, 20);
        assert!(result.len() <= 24); // allow for multi-byte ellipsis
        assert!(result.ends_with(".jpg"));
        assert!(result.contains('…'));
    }

    #[test]
    fn truncate_no_extension() {
        let result = truncate_filename("a_very_long_filename_without_ext", 10);
        assert!(result.contains('…'));
    }

    #[test]
    fn folder_view_enumerates_with_pending_state() {
        let dir = std::env::temp_dir().join(format!("iv_fv_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let img = image::RgbImage::from_fn(100, 100, |_, _| image::Rgb([128, 64, 32]));
        img.save(dir.join("a.jpg")).unwrap();
        img.save(dir.join("b.png")).unwrap();
        std::fs::write(dir.join("c.txt"), b"not image").unwrap();

        let mut fv = FolderView::new(dir.clone());

        for _ in 0..100 {
            fv.poll_enumerator();
            if fv.is_done() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(fv.is_done());
        assert_eq!(fv.entry_count(), 2);

        // Entries should be in Pending state (via scheduler)
        for i in 0..fv.entry_count() {
            assert_eq!(fv.scheduler.entry(i).state, ThumbState::Pending);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
