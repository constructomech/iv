//! GridView — renders a Grid using egui with row-based virtualization,
//! and drives the thumbnail loading state machine.
//!
//! Two-phase loading: all visible embedded thumbnails are extracted first
//! (fast, ~1-10ms each). Only after all visible tiles have something to
//! show does full decode begin for tiles that had no embedded thumbnail.

use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::app::DecodedImage;
use crate::decode;
use crate::grid::{Grid, GridConfig, GridEventKind, TileState};

/// Returns true if IV_DEBUG env var is set to a truthy value.
fn debug_mode() -> bool {
    std::env::var("IV_DEBUG").map_or(false, |v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Maximum tiles to schedule per frame.
const MAX_SCHEDULE_PER_FRAME: usize = 12;
/// Thumbnail decode resolution (pixels).
const THUMB_SIZE: u32 = 160;
/// Maximum results to process per frame.
const MAX_RESULTS_PER_FRAME: usize = 16;

// ---------------------------------------------------------------------------
// Worker protocol
// ---------------------------------------------------------------------------

/// What kind of work to do.
#[derive(Debug, Clone, Copy)]
enum WorkTier {
    /// Try embedded thumbnail only (EXIF/BMFF). Fast, reads ~256KB.
    EmbeddedOnly,
    /// Full file decode + downscale. Slow, reads entire file.
    FullDecode,
}

/// A work request sent to a decode worker.
struct WorkRequest {
    idx: usize,
    path: PathBuf,
    generation: u64,
    tier: WorkTier,
}

/// A completed result from a decode worker.
enum WorkResult {
    /// Embedded thumbnail extracted successfully.
    EmbeddedOk {
        idx: usize,
        image: DecodedImage,
        ms: f64,
    },
    /// No embedded thumbnail found.
    EmbeddedMiss { idx: usize, ms: f64 },
    /// Full decode completed.
    FullOk {
        idx: usize,
        image: DecodedImage,
        ms: f64,
    },
    /// Decode failed.
    Failed { idx: usize },
}

// ---------------------------------------------------------------------------
// Per-tile timing data
// ---------------------------------------------------------------------------

/// Timing info for debug overlay.
#[derive(Debug, Clone, Default)]
struct TileTiming {
    /// Time for embedded thumbnail extraction (ms). Always populated.
    embedded_ms: f64,
    /// Time for full decode (ms). 0 if embedded succeeded.
    full_ms: f64,
}

// ---------------------------------------------------------------------------
// GridView
// ---------------------------------------------------------------------------

/// Visual rendering of a Grid + thumbnail loading pipeline.
pub struct GridView {
    grid: Grid,
    debug: bool,
    /// GPU textures, indexed same as grid tiles.
    textures: Vec<Option<egui::TextureHandle>>,
    /// Per-tile timing data for debug overlay.
    timings: Vec<TileTiming>,
    /// Worker pool channels.
    work_tx: crossbeam_channel::Sender<WorkRequest>,
    work_rx: crossbeam_channel::Receiver<WorkRequest>,
    result_rx: crossbeam_channel::Receiver<WorkResult>,
    /// Generation counter for stale work invalidation.
    generation: Arc<AtomicU64>,
    /// Last scroll position for change detection.
    last_scroll_y: f32,
    /// Worker thread handles for clean shutdown.
    workers: Vec<thread::JoinHandle<()>>,
}

impl GridView {
    /// Create a new GridView with the given grid, spawning decode workers.
    pub fn new(grid: Grid) -> Self {
        let (work_tx, work_rx) = crossbeam_channel::unbounded::<WorkRequest>();
        let (result_tx, result_rx) = crossbeam_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));

        let num_workers = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(2))
            .unwrap_or(4);

        let mut workers = Vec::with_capacity(num_workers);
        for _ in 0..num_workers {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            let generation = generation.clone();
            let handle = thread::spawn(move || {
                while let Ok(req) = work_rx.recv() {
                    if req.generation < generation.load(Ordering::Relaxed) {
                        continue;
                    }

                    match req.tier {
                        WorkTier::EmbeddedOnly => {
                            if decode::is_heif_extension(&req.path) {
                                // HEIC: read file once, try BMFF thumbnail,
                                // and if it misses, do full decode from same buffer.
                                // This avoids reading a 3MB file over network twice.
                                let start = std::time::Instant::now();
                                let data = match std::fs::read(&req.path) {
                                    Ok(d) => d,
                                    Err(_) => {
                                        let _ = result_tx.send(WorkResult::Failed { idx: req.idx });
                                        continue;
                                    }
                                };
                                let read_ms = start.elapsed().as_secs_f64() * 1000.0;

                                // Try BMFF thumbnail from in-memory buffer
                                let thumb_start = std::time::Instant::now();
                                let thumb = decode::try_heif_thumbnail_from_bytes(&data);
                                let thumb_ms =
                                    read_ms + thumb_start.elapsed().as_secs_f64() * 1000.0;

                                match thumb {
                                    Some(image) => {
                                        let _ = result_tx.send(WorkResult::EmbeddedOk {
                                            idx: req.idx,
                                            image,
                                            ms: thumb_ms,
                                        });
                                    }
                                    None => {
                                        // No BMFF thumbnail — do full decode
                                        // from the already-loaded buffer
                                        let _ = result_tx.send(WorkResult::EmbeddedMiss {
                                            idx: req.idx,
                                            ms: thumb_ms,
                                        });

                                        if req.generation < generation.load(Ordering::Relaxed) {
                                            continue;
                                        }

                                        let full_start = std::time::Instant::now();
                                        match decode::decode_from_bytes(&data, THUMB_SIZE) {
                                            Ok((image, _)) => {
                                                let full_ms = read_ms
                                                    + full_start.elapsed().as_secs_f64() * 1000.0;
                                                let _ = result_tx.send(WorkResult::FullOk {
                                                    idx: req.idx,
                                                    image,
                                                    ms: full_ms,
                                                });
                                            }
                                            Err(_) => {
                                                let _ = result_tx
                                                    .send(WorkResult::Failed { idx: req.idx });
                                            }
                                        }
                                    }
                                }
                            } else {
                                // Non-HEIC: fast 256KB EXIF-only read
                                let start = std::time::Instant::now();
                                let (result, _) = decode::try_exif_only(&req.path);
                                let ms = start.elapsed().as_secs_f64() * 1000.0;
                                match result {
                                    Some(image) => {
                                        let _ = result_tx.send(WorkResult::EmbeddedOk {
                                            idx: req.idx,
                                            image,
                                            ms,
                                        });
                                    }
                                    None => {
                                        let _ = result_tx
                                            .send(WorkResult::EmbeddedMiss { idx: req.idx, ms });
                                    }
                                }
                            }
                        }
                        WorkTier::FullDecode => {
                            let start = std::time::Instant::now();
                            let result = std::fs::read(&req.path)
                                .ok()
                                .and_then(|data| decode::decode_from_bytes(&data, THUMB_SIZE).ok());
                            let ms = start.elapsed().as_secs_f64() * 1000.0;
                            match result {
                                Some((image, _)) => {
                                    let _ = result_tx.send(WorkResult::FullOk {
                                        idx: req.idx,
                                        image,
                                        ms,
                                    });
                                }
                                None => {
                                    let _ = result_tx.send(WorkResult::Failed { idx: req.idx });
                                }
                            }
                        }
                    }
                }
            });
            workers.push(handle);
        }

        Self {
            grid,
            debug: debug_mode(),
            textures: Vec::new(),
            timings: Vec::new(),
            work_tx,
            work_rx,
            result_rx,
            generation,
            last_scroll_y: 0.0,
            workers,
        }
    }

    /// Create a demo grid with `n` synthetic tiles (no paths — won't decode).
    pub fn new_demo(n: usize) -> Self {
        let mut grid = Grid::new(GridConfig::default());
        for i in 0..n {
            grid.add_tile(format!("img_{i:05}.jpg"));
        }
        Self::new(grid)
    }

    /// Access the underlying grid.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Access the underlying grid mutably.
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    // -- Result polling -----------------------------------------------------

    fn ensure_vecs(&mut self, idx: usize) {
        while self.textures.len() <= idx {
            self.textures.push(None);
        }
        while self.timings.len() <= idx {
            self.timings.push(TileTiming::default());
        }
    }

    fn poll_results(&mut self, ctx: &egui::Context) {
        let mut processed = 0;
        while processed < MAX_RESULTS_PER_FRAME {
            let result = match self.result_rx.try_recv() {
                Ok(r) => r,
                Err(_) => break,
            };
            processed += 1;

            match result {
                WorkResult::EmbeddedOk { idx, image, ms } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        let size = [image.width as usize, image.height as usize];
                        let ci = egui::ColorImage::from_rgba_unmultiplied(size, &image.pixels);
                        self.textures[idx] = Some(ctx.load_texture(
                            format!("t{idx}"),
                            ci,
                            egui::TextureOptions::LINEAR,
                        ));
                        self.timings[idx].embedded_ms = ms;
                        self.grid.record_event(GridEventKind::ResultReceived {
                            idx,
                            kind: "embedded_ok".into(),
                            ms,
                        });
                        self.grid.set_tile_state(idx, TileState::Loaded);
                    }
                }
                WorkResult::EmbeddedMiss { idx, ms } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        self.timings[idx].embedded_ms = ms;
                        self.grid.record_event(GridEventKind::ResultReceived {
                            idx,
                            kind: "embedded_miss".into(),
                            ms,
                        });
                        self.grid.set_tile_state(idx, TileState::EmbeddedMissed);
                    }
                }
                WorkResult::FullOk { idx, image, ms } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        let size = [image.width as usize, image.height as usize];
                        let ci = egui::ColorImage::from_rgba_unmultiplied(size, &image.pixels);
                        self.textures[idx] = Some(ctx.load_texture(
                            format!("t{idx}"),
                            ci,
                            egui::TextureOptions::LINEAR,
                        ));
                        self.timings[idx].full_ms = ms;
                        self.grid.record_event(GridEventKind::ResultReceived {
                            idx,
                            kind: "full_ok".into(),
                            ms,
                        });
                        self.grid.set_tile_state(idx, TileState::Loaded);
                    }
                }
                WorkResult::Failed { idx } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        self.grid.record_event(GridEventKind::ResultReceived {
                            idx,
                            kind: "failed".into(),
                            ms: 0.0,
                        });
                    }
                }
            }
        }
    }

    // -- Scheduling ---------------------------------------------------------

    /// Two-phase scheduling:
    /// 1. All visible NotLoaded tiles → EmbeddedOnly (fast, get something on screen)
    /// 2. Only after no visible NotLoaded remain: visible CreatingThumbnail → FullDecode
    fn schedule_visible_work(&mut self) {
        let current_gen = self.generation.load(Ordering::Relaxed);

        // Phase 1: embedded thumbnails for NotLoaded tiles (highest priority)
        let not_loaded = self.grid.visible_in_state(TileState::NotLoaded);
        if !not_loaded.is_empty() {
            let mut scheduled_indices = Vec::new();
            let mut scheduled = 0;
            for idx in not_loaded {
                if scheduled >= MAX_SCHEDULE_PER_FRAME {
                    break;
                }
                let path = self.grid.tile_path(idx).to_path_buf();
                if path.as_os_str().is_empty() {
                    continue;
                }
                self.grid.set_tile_state(idx, TileState::LoadingEmbedded);
                let _ = self.work_tx.send(WorkRequest {
                    idx,
                    path,
                    generation: current_gen,
                    tier: WorkTier::EmbeddedOnly,
                });
                scheduled_indices.push(idx);
                scheduled += 1;
            }
            if !scheduled_indices.is_empty() {
                self.grid.record_event(GridEventKind::WorkScheduled {
                    indices: scheduled_indices,
                    tier: "embedded".into(),
                });
            }
            return;
        }

        // Phase 2: full decode for tiles where embedded failed
        let needs_full = self.grid.visible_in_state(TileState::EmbeddedMissed);
        let mut scheduled_indices = Vec::new();
        let mut scheduled = 0;
        for idx in needs_full {
            if scheduled >= MAX_SCHEDULE_PER_FRAME {
                break;
            }
            let path = self.grid.tile_path(idx).to_path_buf();
            if path.as_os_str().is_empty() {
                continue;
            }
            self.grid.set_tile_state(idx, TileState::CreatingThumbnail);
            let _ = self.work_tx.send(WorkRequest {
                idx,
                path,
                generation: current_gen,
                tier: WorkTier::FullDecode,
            });
            scheduled_indices.push(idx);
            scheduled += 1;
        }
        if !scheduled_indices.is_empty() {
            self.grid.record_event(GridEventKind::WorkScheduled {
                indices: scheduled_indices,
                tier: "full_decode".into(),
            });
        }
    }

    // -- Scroll generation --------------------------------------------------

    fn check_scroll_generation(&mut self) {
        let scroll = self.grid.scroll_y();
        let cell_h = self.grid.config().cell_height();
        if (scroll - self.last_scroll_y).abs() > cell_h * 2.0 {
            self.last_scroll_y = scroll;
            let new_gen = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
            while self.work_rx.try_recv().is_ok() {}
            let mut reset_count = 0;
            for idx in 0..self.grid.tile_count() {
                match self.grid.tile_state(idx) {
                    TileState::LoadingEmbedded
                    | TileState::EmbeddedMissed
                    | TileState::CreatingThumbnail => {
                        self.grid.set_tile_state(idx, TileState::NotLoaded);
                        reset_count += 1;
                    }
                    _ => {}
                }
            }
            self.grid.record_event(GridEventKind::GenerationBump {
                generation: new_gen,
            });
            let _ = reset_count;
        }
    }

    // -- Rendering ----------------------------------------------------------

    pub fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> Option<usize> {
        self.poll_results(ctx);

        let config = self.grid.config().clone();
        let tile_w = config.tile_width;
        let tile_h = config.tile_height;
        let padding = config.padding;
        let cell_h = config.cell_height();
        let available_width = ui.available_width();

        // Status bar
        let total = self.grid.tile_count();
        ui.label(
            egui::RichText::new(rust_i18n::t!("status.item_count", count = total))
                .color(egui::Color32::from_rgb(180, 180, 180))
                .size(13.0),
        );
        ui.add_space(4.0);

        let mut clicked = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                self.grid
                    .set_viewport_size(available_width, ui.clip_rect().height());
                let scroll_offset = ui.clip_rect().min.y - ui.min_rect().min.y;
                self.grid.set_scroll(scroll_offset);

                self.check_scroll_generation();
                self.schedule_visible_work();

                let cols = self.grid.cols();
                let total_rows = self.grid.total_rows();
                let vr = self.grid.visible_rows();

                let render_first = vr.first.saturating_sub(2);
                let render_last = (vr.last + 2).min(total_rows);

                ui.spacing_mut().item_spacing.y = 0.0;

                if render_first > 0 {
                    ui.allocate_space(egui::vec2(available_width, render_first as f32 * cell_h));
                }

                let tile_count = self.grid.tile_count();
                let debug = self.debug;

                for row in render_first..render_last {
                    let row_start = row * cols;
                    let row_end = (row_start + cols).min(tile_count);

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(padding, 0.0);
                        for idx in row_start..row_end {
                            let state = self.grid.tile_state(idx);
                            let name = self.grid.tile_name(idx);
                            let texture = self.textures.get(idx).and_then(|t| t.as_ref());
                            let timing = self.timings.get(idx);
                            let response = Self::render_tile(
                                ui, idx, name, state, texture, timing, tile_w, tile_h, debug,
                            );
                            if response.clicked() {
                                clicked = Some(idx);
                            }
                        }
                    });
                    ui.allocate_space(egui::vec2(0.0, padding));
                }

                if render_last < total_rows {
                    ui.allocate_space(egui::vec2(
                        available_width,
                        (total_rows - render_last) as f32 * cell_h,
                    ));
                }
            });

        // Repaint while visible tiles are pending
        if self.grid.tile_count() > 0 {
            let has_pending = !self.grid.visible_in_state(TileState::NotLoaded).is_empty()
                || !self
                    .grid
                    .visible_in_state(TileState::LoadingEmbedded)
                    .is_empty()
                || !self
                    .grid
                    .visible_in_state(TileState::EmbeddedMissed)
                    .is_empty()
                || !self
                    .grid
                    .visible_in_state(TileState::CreatingThumbnail)
                    .is_empty();
            if has_pending {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            }
        }

        clicked
    }

    /// Render a single tile.
    fn render_tile(
        ui: &mut egui::Ui,
        idx: usize,
        name: &str,
        state: TileState,
        texture: Option<&egui::TextureHandle>,
        timing: Option<&TileTiming>,
        tile_w: f32,
        tile_h: f32,
        debug: bool,
    ) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(tile_w, tile_h), egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);

            if let Some(tex) = texture {
                // Decoded thumbnail
                let tex_size = tex.size_vec2();
                let scale = (tile_w / tex_size.x).min(tile_h / tex_size.y);
                let dw = tex_size.x * scale;
                let dh = tex_size.y * scale;
                let ox = (tile_w - dw) / 2.0;
                let oy = (tile_h - dh) / 2.0;

                painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(24, 24, 24));
                let img_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.min.x + ox, rect.min.y + oy),
                    egui::vec2(dw, dh),
                );
                painter.image(
                    tex.id(),
                    img_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                // Placeholder
                let bg = match state {
                    TileState::NotLoaded => egui::Color32::from_rgb(48, 48, 48),
                    TileState::LoadingEmbedded => egui::Color32::from_rgb(60, 55, 40),
                    TileState::EmbeddedMissed => egui::Color32::from_rgb(55, 45, 45),
                    TileState::CreatingThumbnail => egui::Color32::from_rgb(40, 55, 60),
                    TileState::Loaded => egui::Color32::from_rgb(35, 60, 35),
                };
                painter.rect_filled(rect, 2.0, bg);
            }

            // Hover
            if response.hovered() {
                painter.rect_filled(
                    rect,
                    2.0,
                    egui::Color32::from_rgba_premultiplied(255, 255, 255, 20),
                );
            }

            // Filename
            painter.text(
                egui::pos2(rect.center().x, rect.max.y - 4.0),
                egui::Align2::CENTER_BOTTOM,
                name,
                egui::FontId::proportional(10.0),
                egui::Color32::from_rgb(170, 170, 170),
            );

            // Debug overlay: state + timing
            if debug {
                let mut lines: Vec<(String, egui::Color32)> = Vec::new();

                // State
                lines.push((state.to_string(), egui::Color32::from_rgb(180, 180, 180)));

                // Timing
                if let Some(t) = timing {
                    if t.embedded_ms > 0.0 {
                        let color = if t.full_ms == 0.0 && state == TileState::Loaded {
                            egui::Color32::from_rgb(80, 220, 80) // green = embedded was used
                        } else {
                            egui::Color32::from_rgb(140, 140, 140) // gray = embedded missed
                        };
                        lines.push((format!("E {:.1}ms", t.embedded_ms), color));
                    }
                    if t.full_ms > 0.0 {
                        lines.push((
                            format!("F {:.1}ms", t.full_ms),
                            egui::Color32::from_rgb(220, 180, 80),
                        ));
                    }
                }

                let line_h = 12.0;
                let badge_h = lines.len() as f32 * line_h + 4.0;
                let badge_w = 80.0;
                let badge_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.max.x - badge_w - 2.0, rect.min.y + 2.0),
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

                // Index in top-left
                painter.text(
                    egui::pos2(rect.min.x + 4.0, rect.min.y + 4.0),
                    egui::Align2::LEFT_TOP,
                    format!("{idx}"),
                    egui::FontId::monospace(9.0),
                    egui::Color32::from_rgb(100, 100, 100),
                );
            }
        }

        response
    }
}

impl Drop for GridView {
    fn drop(&mut self) {
        // Drop channels to signal workers to exit after current work.
        // work_tx drop causes workers' recv() to return Err.
        // work_rx drop ensures no lingering channel references.
        let work_tx = std::mem::replace(
            &mut self.work_tx,
            crossbeam_channel::unbounded::<WorkRequest>().0,
        );
        drop(work_tx);
        let work_rx = std::mem::replace(
            &mut self.work_rx,
            crossbeam_channel::unbounded::<WorkRequest>().1,
        );
        drop(work_rx);
        // Wait for workers to finish any in-progress decode
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}
