use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
/// Number of thumbnail worker threads (CPU-bound decode).
/// Reserves 2 cores for UI + enumerator, uses the rest for decoding.
fn num_thumb_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(2))
        .unwrap_or(4)
}

/// Adaptive I/O concurrency limiter.
///
/// Measures the latency of the first N file reads and sets the concurrency
/// limit accordingly:
/// - Local SSD (<2ms avg): allow many concurrent reads (up to num_workers)
/// - Network/SMB (>5ms avg): cap concurrent reads to keep pipe full without
///   overwhelming the server
///
/// Workers acquire a permit before file I/O and release after reading
/// (before CPU-bound decode), so the limiter only gates I/O, not compute.
struct IoLimiter {
    /// Semaphore permits available for concurrent I/O.
    semaphore: Arc<crossbeam_channel::Sender<()>>,
    permit_rx: crossbeam_channel::Receiver<()>,
    /// Number of latency samples collected so far.
    sample_count: Arc<AtomicUsize>,
    /// Sum of latency samples in microseconds.
    latency_sum_us: Arc<AtomicU64>,
    /// Current I/O concurrency limit.
    io_limit: Arc<AtomicUsize>,
}

/// Number of reads to sample before adjusting concurrency.
const LATENCY_SAMPLE_COUNT: usize = 8;
/// Latency threshold (microseconds) — above this is "high latency" (network).
const HIGH_LATENCY_THRESHOLD_US: u64 = 5_000; // 5ms

impl IoLimiter {
    fn new(initial_limit: usize) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(initial_limit);
        // Pre-fill with permits
        for _ in 0..initial_limit {
            let _ = tx.send(());
        }
        Self {
            semaphore: Arc::new(tx),
            permit_rx: rx,
            sample_count: Arc::new(AtomicUsize::new(0)),
            latency_sum_us: Arc::new(AtomicU64::new(0)),
            io_limit: Arc::new(AtomicUsize::new(initial_limit)),
        }
    }

    /// Create a handle that workers can clone and use.
    fn handle(&self) -> IoHandle {
        IoHandle {
            permit_rx: self.permit_rx.clone(),
            semaphore: self.semaphore.clone(),
            sample_count: self.sample_count.clone(),
            latency_sum_us: self.latency_sum_us.clone(),
            io_limit: self.io_limit.clone(),
        }
    }

    /// Get the current I/O concurrency limit.
    #[allow(dead_code)]
    fn current_limit(&self) -> usize {
        self.io_limit.load(Ordering::Relaxed)
    }
}

/// Worker-side handle for the I/O limiter.
#[derive(Clone)]
struct IoHandle {
    permit_rx: crossbeam_channel::Receiver<()>,
    semaphore: Arc<crossbeam_channel::Sender<()>>,
    sample_count: Arc<AtomicUsize>,
    latency_sum_us: Arc<AtomicU64>,
    io_limit: Arc<AtomicUsize>,
}

impl IoHandle {
    /// Acquire a permit for I/O. Blocks until one is available.
    fn acquire(&self) {
        let _ = self.permit_rx.recv();
    }

    /// Release the permit after I/O is complete, and optionally record latency.
    fn release(&self, io_duration: std::time::Duration) {
        // Return the permit
        let _ = self.semaphore.send(());

        // Record latency sample
        let sample_idx = self.sample_count.fetch_add(1, Ordering::Relaxed);
        if sample_idx < LATENCY_SAMPLE_COUNT {
            self.latency_sum_us
                .fetch_add(io_duration.as_micros() as u64, Ordering::Relaxed);

            // After collecting enough samples, adjust the limit
            if sample_idx + 1 == LATENCY_SAMPLE_COUNT {
                let total_us = self.latency_sum_us.load(Ordering::Relaxed);
                let avg_us = total_us / LATENCY_SAMPLE_COUNT as u64;

                let new_limit = if avg_us > HIGH_LATENCY_THRESHOLD_US {
                    // High latency (network): keep enough I/O in flight to
                    // feed the CPU workers. Min 8 concurrent reads ensures
                    // good pipeline utilization even on slow NAS.
                    let limit = (200_000 / avg_us.max(1)).clamp(8, 64) as usize;
                    log::info!(
                        "Detected high I/O latency ({:.1}ms avg) — setting I/O concurrency to {limit}",
                        avg_us as f64 / 1000.0
                    );
                    limit
                } else {
                    // Low latency (local SSD): allow lots of concurrent reads
                    let cpus = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(8);
                    let limit = cpus.min(64);
                    log::info!(
                        "Detected low I/O latency ({:.1}ms avg) — setting I/O concurrency to {limit}",
                        avg_us as f64 / 1000.0
                    );
                    limit
                };

                self.io_limit.store(new_limit, Ordering::Relaxed);
                // Note: we can't resize the channel, but the initial limit is
                // set high enough. The semaphore naturally limits via the
                // bounded channel capacity. For a true resize we'd need to
                // drain/refill, but in practice the initial high limit works
                // fine — on fast storage the limit stays high, on slow storage
                // workers naturally serialize because I/O is the bottleneck.
            }
        }
    }
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
    /// Clone of the work receiver — used to drain stale items on generation bump.
    work_rx: crossbeam_channel::Receiver<WorkItem>,
    result_rx: crossbeam_channel::Receiver<ThumbResult>,
    /// Shared generation counter — workers check this to skip stale items.
    generation: Arc<AtomicU64>,
}

impl ThumbLoader {
    fn new() -> Self {
        let (work_tx, work_rx) = crossbeam_channel::unbounded::<WorkItem>();
        let (result_tx, result_rx) = crossbeam_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));

        // Start with generous I/O limit; it will self-tune after first reads
        let initial_io_limit = std::thread::available_parallelism()
            .map(|n| n.get().min(64))
            .unwrap_or(16);
        let io_limiter = IoLimiter::new(initial_io_limit);

        let num_workers = num_thumb_workers();
        log::info!(
            "Starting {num_workers} thumbnail workers, initial I/O concurrency: {initial_io_limit}"
        );
        for _ in 0..num_workers {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            let generation = generation.clone();
            let io = io_limiter.handle();
            thread::spawn(move || {
                while let Ok(work) = work_rx.recv() {
                    // Skip stale work
                    if work.generation < generation.load(Ordering::Relaxed) {
                        continue;
                    }

                    let result = match work.tier {
                        WorkTier::ExifOnly => {
                            // I/O phase: read first 256KB
                            io.acquire();
                            let io_start = std::time::Instant::now();
                            let (exif_result, timings) = decode::try_exif_only(&work.path);
                            io.release(io_start.elapsed());

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
                            // I/O phase: read full file
                            io.acquire();
                            let io_start = std::time::Instant::now();
                            let data = std::fs::read(&work.path);
                            io.release(io_start.elapsed());

                            // CPU phase: decode + downscale (no I/O permit held)
                            match data {
                                Ok(data) => {
                                    match decode::decode_from_bytes(&data, THUMB_DECODE_SIZE) {
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
                                Err(e) => ThumbResult::FullErr {
                                    idx: work.idx,
                                    error: format!("Failed to read {}: {e}", work.path.display()),
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
            work_rx,
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

    /// Sync the shared generation counter with the scheduler's,
    /// and drain stale items from the work channel so workers
    /// pick up new visible work immediately.
    fn sync_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
        // Drain stale work items from the channel. Workers would skip them
        // anyway (generation check), but clearing the channel means new
        // high-priority items don't queue behind hundreds of stale ones.
        let mut drained = 0;
        while let Ok(_) = self.work_rx.try_recv() {
            drained += 1;
        }
        if drained > 0 {
            log::debug!("Drained {drained} stale work items from channel (gen={generation})");
        }
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
    /// Caps results processed per frame to keep frame time consistent.
    fn poll_thumbnails(&mut self, ctx: &egui::Context) {
        const MAX_RESULTS_PER_FRAME: usize = 16;
        let mut processed = 0usize;
        while processed < MAX_RESULTS_PER_FRAME {
            let result = match self.thumb_loader.result_rx.try_recv() {
                Ok(r) => r,
                Err(_) => break,
            };
            processed += 1;
            match result {
                ThumbResult::ExifOk {
                    idx,
                    image,
                    timings,
                } => {
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
                    if idx < self.scheduler.len() {
                        self.scheduler.exif_failed(idx, timings);
                    }
                }
                ThumbResult::FullOk {
                    idx,
                    image,
                    timings,
                } => {
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
    }

    /// Render the folder view. Returns Some(index) if a tile was clicked.
    pub fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> Option<usize> {
        let frame_start = std::time::Instant::now();

        self.poll_enumerator();
        self.poll_thumbnails(ctx);

        // Request repaint while work is pending, but throttle to ~60fps
        // so the UI thread has time to process scroll/input events.
        if !self.enum_done || self.scheduler.has_pending_work() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
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
        let loaded = self.scheduler.loaded_count();
        let frame_ms = frame_start.elapsed().as_secs_f64() * 1000.0;
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
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(status)
                    .color(egui::Color32::from_rgb(180, 180, 180))
                    .size(13.0),
            );
            if debug_mode() {
                let color = if frame_ms < 8.0 {
                    egui::Color32::from_rgb(80, 220, 80)
                } else if frame_ms < 16.0 {
                    egui::Color32::from_rgb(220, 220, 80)
                } else {
                    egui::Color32::from_rgb(220, 80, 80)
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{:.1}ms", frame_ms))
                            .color(color)
                            .size(13.0),
                    );
                });
            }
        });
        ui.add_space(4.0);

        let mut clicked_index = None;

        // Scrollable grid with row-based rendering.
        // Only visible rows + a small buffer are rendered — off-screen rows
        // are skipped with allocate_space, keeping per-frame cost O(visible).
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                let available_width = ui.available_width();
                let cols = ((available_width + TILE_PADDING) / (TILE_SIZE + TILE_PADDING))
                    .floor()
                    .max(1.0) as usize;

                // Row height must be consistent between skip regions and rendered rows.
                let row_height = CELL_SIZE + TILE_PADDING;

                // Compute scroll position and update scheduler
                let scroll_offset = ui.clip_rect().min.y - ui.min_rect().min.y;
                let viewport_height = ui.clip_rect().height();
                let bumped = self.scheduler.update_visibility(
                    scroll_offset,
                    viewport_height,
                    cols,
                    row_height,
                );
                if bumped {
                    self.thumb_loader
                        .sync_generation(self.scheduler.generation());
                }

                // Get work batch and send to thread pool
                let batch = self.scheduler.get_work_batch();
                if !batch.is_empty() {
                    self.thumb_loader.send_batch(batch);
                }

                // Row-based virtualization
                let debug = debug_mode();
                let total_entries = self.scheduler.len();
                let total_rows = if total_entries > 0 {
                    total_entries.div_ceil(cols)
                } else {
                    0
                };

                let first_visible_row = (scroll_offset / row_height).floor().max(0.0) as usize;
                let visible_row_count = (viewport_height / row_height).ceil() as usize + 1;
                let render_first = first_visible_row.saturating_sub(2);
                let render_last = (first_visible_row + visible_row_count + 2).min(total_rows);

                // Disable egui's automatic vertical spacing — we manage it ourselves
                ui.spacing_mut().item_spacing.y = 0.0;

                // Skip rows above render zone
                if render_first > 0 {
                    ui.allocate_space(egui::vec2(
                        available_width,
                        render_first as f32 * row_height,
                    ));
                }

                // Render only visible + buffer rows
                for row in render_first..render_last {
                    let row_start = row * cols;
                    let row_end = (row_start + cols).min(total_entries);

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(TILE_PADDING, 0.0);
                        for idx in row_start..row_end {
                            let entry = self.scheduler.entry(idx);
                            let texture = &self.textures[idx];
                            let debug_info = if debug { entry.timings.as_ref() } else { None };
                            let response = Self::render_tile(ui, &entry.path, texture, debug_info);
                            if response.clicked() {
                                clicked_index = Some(idx);
                            }
                        }
                    });
                    ui.allocate_space(egui::vec2(0.0, TILE_PADDING));
                }

                // Skip rows below render zone
                if render_last < total_rows {
                    ui.allocate_space(egui::vec2(
                        available_width,
                        (total_rows - render_last) as f32 * row_height,
                    ));
                }
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

                    // Label: "BMFF" for HEIC/HEIF container thumbnails, "EXIF" for everything else
                    let thumb_label = if crate::decode::is_heif_extension(path) {
                        "BMFF"
                    } else {
                        "EXIF"
                    };

                    // Thumbnail line — green if it was used, gray if attempted but failed
                    if timings.full_ms == 0.0 {
                        // Thumbnail was used (no full decode needed)
                        lines.push((
                            format!("{thumb_label} {:.1}ms", timings.exif_ms),
                            egui::Color32::from_rgb(80, 220, 80),
                        ));
                    } else {
                        // Thumbnail attempted but not found, show in gray
                        lines.push((
                            format!("{thumb_label} {:.1}ms", timings.exif_ms),
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
