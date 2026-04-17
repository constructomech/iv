//! GridView — renders a Grid using egui with row-based virtualization,
//! and drives the thumbnail loading state machine.
//!
//! Two-phase loading: all visible embedded thumbnails are extracted first
//! (fast, ~1-10ms each). Only after all visible tiles have something to
//! show does full decode begin for tiles that had no embedded thumbnail.
//!
//! Pipeline architecture:
//!   I/O pool (tokio, auto-scaled blocking threads) → Decode pool (cores-2 threads)
//!   The I/O pool reads bytes from disk; the decode pool does CPU-bound work.
//!   This keeps decode threads saturated while I/O is in flight.

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
    std::env::var("IV_DEBUG").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Per-frame time budget for scheduling + result polling (ms).
/// Keeps the UI thread responsive at 60fps (~16ms frame budget).
const FRAME_WORK_BUDGET_MS: f64 = 4.0;
/// Thumbnail decode resolution (pixels).
const THUMB_SIZE: u32 = 160;

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

/// A decode request sent from the I/O pool to the decode pool.
/// The file bytes have already been read; this is pure CPU work.
struct DecodeRequest {
    idx: usize,
    path: PathBuf,
    data: Vec<u8>,
    generation: u64,
    tier: WorkTier,
    is_heif: bool,
}

/// A completed result from a decode worker.
enum WorkResult {
    /// Embedded thumbnail extracted successfully.
    EmbeddedOk {
        idx: usize,
        image: DecodedImage,
        ms: f64,
    },
    /// No embedded thumbnail found. Includes file bytes for reuse.
    EmbeddedMiss {
        idx: usize,
        ms: f64,
        data: Option<Vec<u8>>,
    },
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
    /// Current tile size for the slider (pixels, square).
    tile_size: f32,
    /// GPU textures, indexed same as grid tiles.
    textures: Vec<Option<egui::TextureHandle>>,
    /// Per-tile timing data for debug overlay.
    timings: Vec<TileTiming>,
    /// Cached file bytes from HEIC embedded extraction (avoids re-read for full decode).
    cached_data: Vec<Option<Vec<u8>>>,
    /// Tokio runtime for async I/O (file reads).
    io_runtime: tokio::runtime::Runtime,
    /// Decode request channel: I/O pool → decode workers.
    decode_tx: crossbeam_channel::Sender<DecodeRequest>,
    decode_rx: crossbeam_channel::Receiver<DecodeRequest>,
    /// Result channel: decode workers → UI thread.
    result_rx: crossbeam_channel::Receiver<WorkResult>,
    /// Generation counter for stale work invalidation.
    generation: Arc<AtomicU64>,
    /// Last scroll position for change detection.
    last_scroll_y: f32,
    /// Decode worker thread handles for clean shutdown.
    decode_workers: Vec<thread::JoinHandle<()>>,
}

/// Size of the prefix read for non-HEIC EXIF extraction.
const EXIF_PREFIX_SIZE: usize = 256 * 1024;

impl GridView {
    /// Create a new GridView with the given grid, spawning I/O runtime + decode workers.
    pub fn new(grid: Grid) -> Self {
        let (decode_tx, decode_rx) = crossbeam_channel::unbounded::<DecodeRequest>();
        let (result_tx, result_rx) = crossbeam_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));

        // Tokio runtime for I/O: 1 async thread dispatching to auto-scaled blocking pool.
        let io_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(128)
            .thread_name("iv-io")
            .build()
            .expect("failed to create tokio runtime");

        // Decode workers: CPU-bound, matched to available cores.
        let num_decoders = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(2))
            .unwrap_or(4);

        let mut decode_workers = Vec::with_capacity(num_decoders);
        for _ in 0..num_decoders {
            let decode_rx = decode_rx.clone();
            let result_tx = result_tx.clone();
            let generation = generation.clone();
            let handle = thread::Builder::new()
                .name("iv-decode".into())
                .spawn(move || {
                    while let Ok(req) = decode_rx.recv() {
                        if req.generation < generation.load(Ordering::Relaxed) {
                            continue;
                        }

                        let start = std::time::Instant::now();
                        match req.tier {
                            WorkTier::EmbeddedOnly => {
                                let thumb = if req.is_heif {
                                    decode::try_heif_thumbnail_from_bytes(&req.data)
                                } else {
                                    decode::try_embedded_from_bytes(&req.data)
                                };
                                let ms = start.elapsed().as_secs_f64() * 1000.0;
                                match thumb {
                                    Some(image) => {
                                        let _ = result_tx.send(WorkResult::EmbeddedOk {
                                            idx: req.idx,
                                            image,
                                            ms,
                                        });
                                    }
                                    None => {
                                        // For HEIC, pass full file bytes for FullDecode reuse.
                                        let data = if req.is_heif { Some(req.data) } else { None };
                                        let _ = result_tx.send(WorkResult::EmbeddedMiss {
                                            idx: req.idx,
                                            ms,
                                            data,
                                        });
                                    }
                                }
                            }
                            WorkTier::FullDecode => {
                                let result =
                                    decode::decode_from_bytes(&req.data, THUMB_SIZE, req.is_heif);
                                let ms = start.elapsed().as_secs_f64() * 1000.0;
                                match result {
                                    Ok((image, _)) => {
                                        let _ = result_tx.send(WorkResult::FullOk {
                                            idx: req.idx,
                                            image,
                                            ms,
                                        });
                                    }
                                    Err(_) => {
                                        let _ = result_tx.send(WorkResult::Failed { idx: req.idx });
                                    }
                                }
                            }
                        }
                    }
                })
                .expect("failed to spawn decode worker");
            decode_workers.push(handle);
        }

        Self {
            tile_size: grid.config().tile_width,
            grid,
            debug: debug_mode(),
            textures: Vec::new(),
            timings: Vec::new(),
            cached_data: Vec::new(),
            io_runtime,
            decode_tx,
            decode_rx,
            result_rx,
            generation,
            last_scroll_y: 0.0,
            decode_workers,
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
        while self.cached_data.len() <= idx {
            self.cached_data.push(None);
        }
    }

    fn poll_results(&mut self, ctx: &egui::Context, deadline: &std::time::Instant) -> usize {
        let mut processed = 0;
        loop {
            if std::time::Instant::now() >= *deadline {
                break;
            }
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
                WorkResult::EmbeddedMiss { idx, ms, data } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        self.timings[idx].embedded_ms = ms;
                        if data.is_some() {
                            self.cached_data[idx] = data;
                        }
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
        processed
    }

    // -- Scheduling ---------------------------------------------------------

    /// Two-phase scheduling:
    /// 1. All visible NotLoaded tiles → EmbeddedOnly (fast, get something on screen)
    /// 2. Only after no visible NotLoaded remain: visible EmbeddedMissed → FullDecode
    ///
    /// I/O is dispatched to tokio's blocking pool; decoded bytes flow to decode workers.
    /// Stops scheduling if the frame time budget is exceeded.
    fn schedule_visible_work(&mut self, deadline: &std::time::Instant) {
        let current_gen = self.generation.load(Ordering::Relaxed);

        // Phase 1: embedded thumbnails for NotLoaded tiles (highest priority)
        let not_loaded = self.grid.visible_in_state(TileState::NotLoaded);
        if !not_loaded.is_empty() {
            let mut scheduled_indices = Vec::new();
            for idx in not_loaded {
                if std::time::Instant::now() >= *deadline {
                    break;
                }
                let path = self.grid.tile_path(idx).to_path_buf();
                if path.as_os_str().is_empty() {
                    continue;
                }
                self.grid.set_tile_state(idx, TileState::LoadingEmbedded);
                let is_heif = decode::is_heif_extension(&path);
                self.spawn_io_read(idx, path, current_gen, WorkTier::EmbeddedOnly, is_heif);
                scheduled_indices.push(idx);
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
        for idx in needs_full {
            if std::time::Instant::now() >= *deadline {
                break;
            }
            let path = self.grid.tile_path(idx).to_path_buf();
            if path.as_os_str().is_empty() {
                continue;
            }
            self.grid.set_tile_state(idx, TileState::CreatingThumbnail);
            let is_heif = decode::is_heif_extension(&path);
            // If we have cached bytes from EmbeddedMiss, skip I/O entirely
            let cached = if idx < self.cached_data.len() {
                self.cached_data[idx].take()
            } else {
                None
            };
            if let Some(data) = cached {
                // Bypass I/O pool — send directly to decode workers
                let _ = self.decode_tx.send(DecodeRequest {
                    idx,
                    path,
                    data,
                    generation: current_gen,
                    tier: WorkTier::FullDecode,
                    is_heif,
                });
            } else {
                self.spawn_io_read(idx, path, current_gen, WorkTier::FullDecode, is_heif);
            }
            scheduled_indices.push(idx);
        }
        if !scheduled_indices.is_empty() {
            self.grid.record_event(GridEventKind::WorkScheduled {
                indices: scheduled_indices,
                tier: "full_decode".into(),
            });
        }
    }

    /// Spawn a tokio blocking task to read a file and push bytes to the decode pool.
    fn spawn_io_read(
        &self,
        idx: usize,
        path: PathBuf,
        generation: u64,
        tier: WorkTier,
        is_heif: bool,
    ) {
        let decode_tx = self.decode_tx.clone();
        let gen_counter = self.generation.clone();
        self.io_runtime.spawn_blocking(move || {
            // Check generation before I/O
            if generation < gen_counter.load(Ordering::Relaxed) {
                return;
            }
            let data = match tier {
                WorkTier::EmbeddedOnly if !is_heif => {
                    // Non-HEIC: only read 256KB prefix for EXIF extraction
                    let Ok(mut file) = std::fs::File::open(&path) else {
                        return;
                    };
                    let file_len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
                    let read_len = file_len.min(EXIF_PREFIX_SIZE);
                    let mut buf = vec![0u8; read_len];
                    if std::io::Read::read_exact(&mut file, &mut buf).is_err() {
                        return;
                    }
                    buf
                }
                _ => {
                    // HEIC EmbeddedOnly or any FullDecode: read full file
                    match std::fs::read(&path) {
                        Ok(d) => d,
                        Err(_) => return,
                    }
                }
            };
            // Check generation again after I/O (may have scrolled during read)
            if generation < gen_counter.load(Ordering::Relaxed) {
                return;
            }
            let _ = decode_tx.send(DecodeRequest {
                idx,
                path,
                data,
                generation,
                tier,
                is_heif,
            });
        });
    }

    // -- Scroll generation --------------------------------------------------

    fn check_scroll_generation(&mut self) {
        let scroll = self.grid.scroll_y();
        let cell_h = self.grid.config().cell_height();
        if (scroll - self.last_scroll_y).abs() > cell_h * 2.0 {
            self.last_scroll_y = scroll;
            let new_gen = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
            // Drain pending decode requests (I/O tasks check generation before sending)
            while self.decode_rx.try_recv().is_ok() {}
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
        // Suppress egui's default solid white selection/focus rectangles
        let mut style = (*ctx.style()).clone();
        style.visuals.widgets.active.bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.active.weak_bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
        style.visuals.widgets.hovered.bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        style.visuals.selection.bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.selection.stroke = egui::Stroke::NONE;
        ctx.set_style(style);

        let frame_start = std::time::Instant::now();
        let deadline =
            frame_start + std::time::Duration::from_secs_f64(FRAME_WORK_BUDGET_MS / 1000.0);

        let poll_start = std::time::Instant::now();
        let results_processed = self.poll_results(ctx, &deadline);
        let results_pending = self.result_rx.len();
        let poll_ms = poll_start.elapsed().as_secs_f64() * 1000.0;

        let config = self.grid.config().clone();
        let tile_w = config.tile_width;
        let tile_h = config.tile_height;
        let padding = config.padding;
        let cell_h = config.cell_height();
        let available_width = ui.available_width();

        // Reserve space at the bottom for the status bar
        let bar_height = 24.0;
        let available_for_grid = ui.available_height() - bar_height - 4.0;

        let mut clicked = None;

        let sched_render_start = std::time::Instant::now();
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .max_height(available_for_grid)
            .show(ui, |ui| {
                self.grid
                    .set_viewport_size(available_width, ui.clip_rect().height());
                let scroll_offset = ui.clip_rect().min.y - ui.min_rect().min.y;
                self.grid.set_scroll(scroll_offset);

                self.check_scroll_generation();
                self.schedule_visible_work(&deadline);

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

        // Status bar at bottom: tile size slider (left) + item count (right)
        ui.add_space(4.0);
        let total = self.grid.tile_count();
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 120.0;
            if ui
                .add(egui::Slider::new(&mut self.tile_size, 60.0..=400.0).show_value(false))
                .changed()
            {
                self.grid.set_tile_size(self.tile_size, self.tile_size);
            }
            ui.label(
                egui::RichText::new(format!("{}px", self.tile_size as u32))
                    .color(egui::Color32::from_rgb(120, 120, 120))
                    .size(11.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(rust_i18n::t!("status.item_count", count = total))
                        .color(egui::Color32::from_rgb(160, 160, 160))
                        .size(12.0),
                );
            });
        });

        let render_ms = sched_render_start.elapsed().as_secs_f64() * 1000.0;
        let frame_ms = frame_start.elapsed().as_secs_f64() * 1000.0;

        // Record frame timing
        self.grid.record_event(GridEventKind::FrameTiming {
            frame_ms,
            poll_ms,
            schedule_ms: 0.0, // included in render_ms
            render_ms,
            results_processed,
            results_pending,
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
    #[allow(clippy::too_many_arguments)]
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
        let desired_size = egui::vec2(tile_w, tile_h);
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());

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

            // Hover/click highlight — subtle alpha brightening
            if response.hovered() || response.is_pointer_button_down_on() {
                let alpha = if response.is_pointer_button_down_on() {
                    50
                } else {
                    30
                };
                painter.rect_filled(
                    rect,
                    2.0,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha),
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
        // Drop decode channels to signal decode workers to exit.
        let decode_tx = std::mem::replace(
            &mut self.decode_tx,
            crossbeam_channel::unbounded::<DecodeRequest>().0,
        );
        drop(decode_tx);
        let decode_rx = std::mem::replace(
            &mut self.decode_rx,
            crossbeam_channel::unbounded::<DecodeRequest>().1,
        );
        drop(decode_rx);
        // Wait for decode workers to finish any in-progress work
        for handle in self.decode_workers.drain(..) {
            let _ = handle.join();
        }
        // Tokio runtime shuts down when dropped (waits for blocking tasks).
    }
}
