//! GridView — renders a Grid using egui with row-based virtualization,
//! and drives the thumbnail loading state machine.
//!
//! Only visible rows are rendered. Workers decode thumbnails for
//! visible NotLoaded tiles, transitioning them through the state machine.

use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::app::DecodedImage;
use crate::decode;
use crate::grid::{Grid, GridConfig, TileState};

/// Returns true if IV_DEBUG env var is set to a truthy value.
fn debug_mode() -> bool {
    std::env::var("IV_DEBUG").map_or(false, |v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Maximum number of tiles to schedule per frame.
const MAX_SCHEDULE_PER_FRAME: usize = 8;
/// Thumbnail decode resolution (pixels).
const THUMB_SIZE: u32 = 160;
/// Maximum results to process per frame (texture uploads).
const MAX_RESULTS_PER_FRAME: usize = 16;

/// A work request sent to a decode worker.
struct WorkRequest {
    idx: usize,
    path: PathBuf,
    generation: u64,
}

/// A completed result from a decode worker.
enum WorkResult {
    /// Embedded thumbnail extracted successfully.
    EmbeddedOk { idx: usize, image: DecodedImage },
    /// No embedded thumbnail — needs full decode.
    EmbeddedMiss { idx: usize },
    /// Full decode completed.
    FullOk { idx: usize, image: DecodedImage },
    /// Decode failed.
    Failed { idx: usize },
}

/// Visual rendering of a Grid + thumbnail loading pipeline.
pub struct GridView {
    grid: Grid,
    debug: bool,
    /// GPU textures, indexed same as grid tiles.
    textures: Vec<Option<egui::TextureHandle>>,
    /// Worker pool channels.
    work_tx: crossbeam_channel::Sender<WorkRequest>,
    work_rx: crossbeam_channel::Receiver<WorkRequest>,
    result_rx: crossbeam_channel::Receiver<WorkResult>,
    /// Generation counter — incremented on significant scroll to invalidate stale work.
    generation: Arc<AtomicU64>,
    /// Last scroll position for change detection.
    last_scroll_y: f32,
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

        for _ in 0..num_workers {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            let generation = generation.clone();
            thread::spawn(move || {
                while let Ok(req) = work_rx.recv() {
                    if req.generation < generation.load(Ordering::Relaxed) {
                        continue;
                    }

                    // Try embedded thumbnail first
                    let (exif_result, _timings) = decode::try_exif_only(&req.path);
                    match exif_result {
                        Some(image) => {
                            let _ = result_tx.send(WorkResult::EmbeddedOk {
                                idx: req.idx,
                                image,
                            });
                        }
                        None => {
                            let _ = result_tx.send(WorkResult::EmbeddedMiss { idx: req.idx });

                            if req.generation < generation.load(Ordering::Relaxed) {
                                continue;
                            }

                            // Full decode
                            match std::fs::read(&req.path) {
                                Ok(data) => match decode::decode_from_bytes(&data, THUMB_SIZE) {
                                    Ok((image, _)) => {
                                        let _ = result_tx.send(WorkResult::FullOk {
                                            idx: req.idx,
                                            image,
                                        });
                                    }
                                    Err(_) => {
                                        let _ = result_tx.send(WorkResult::Failed { idx: req.idx });
                                    }
                                },
                                Err(_) => {
                                    let _ = result_tx.send(WorkResult::Failed { idx: req.idx });
                                }
                            }
                        }
                    }
                }
            });
        }

        Self {
            grid,
            debug: debug_mode(),
            textures: Vec::new(),
            work_tx,
            work_rx,
            result_rx,
            generation,
            last_scroll_y: 0.0,
        }
    }

    /// Create a demo grid with `n` tiles, all in NotLoaded state (no paths).
    pub fn new_demo(n: usize) -> Self {
        let mut grid = Grid::new(GridConfig::default());
        for i in 0..n {
            grid.add_tile(format!("img_{i:05}.jpg"));
        }
        Self::new(grid)
    }

    /// Access the underlying grid.
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// Poll completed results from workers and update grid state + textures.
    fn poll_results(&mut self, ctx: &egui::Context) {
        let mut processed = 0;
        while processed < MAX_RESULTS_PER_FRAME {
            let result = match self.result_rx.try_recv() {
                Ok(r) => r,
                Err(_) => break,
            };
            processed += 1;

            match result {
                WorkResult::EmbeddedOk { idx, image } | WorkResult::FullOk { idx, image } => {
                    if idx < self.grid.tile_count() {
                        while self.textures.len() <= idx {
                            self.textures.push(None);
                        }
                        let size = [image.width as usize, image.height as usize];
                        let color_image =
                            egui::ColorImage::from_rgba_unmultiplied(size, &image.pixels);
                        let texture = ctx.load_texture(
                            format!("thumb_{idx}"),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.textures[idx] = Some(texture);
                        self.grid.set_tile_state(idx, TileState::Loaded);
                    }
                }
                WorkResult::EmbeddedMiss { idx } => {
                    if idx < self.grid.tile_count() {
                        self.grid.set_tile_state(idx, TileState::CreatingThumbnail);
                    }
                }
                WorkResult::Failed { idx } => {
                    if idx < self.grid.tile_count() {
                        // Leave as NotLoaded — could retry or add Failed state
                    }
                    let _ = idx;
                }
            }
        }
    }

    /// Schedule visible NotLoaded tiles for decoding.
    fn schedule_visible_work(&mut self) {
        let not_loaded = self.grid.visible_in_state(TileState::NotLoaded);
        let current_gen = self.generation.load(Ordering::Relaxed);

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
            });
            scheduled += 1;
        }
    }

    /// Check for significant scroll and bump generation to invalidate stale work.
    fn check_scroll_generation(&mut self) {
        let scroll = self.grid.scroll_y();
        let cell_h = self.grid.config().cell_height();
        if (scroll - self.last_scroll_y).abs() > cell_h * 2.0 {
            self.last_scroll_y = scroll;
            self.generation.fetch_add(1, Ordering::Relaxed);
            // Drain stale items from work channel
            while self.work_rx.try_recv().is_ok() {}
            // Reset in-flight tiles back to NotLoaded so they can be re-scheduled
            for idx in 0..self.grid.tile_count() {
                match self.grid.tile_state(idx) {
                    TileState::LoadingEmbedded | TileState::CreatingThumbnail => {
                        self.grid.set_tile_state(idx, TileState::NotLoaded);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Render the grid. Returns clicked tile index if any.
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
            egui::RichText::new(format!("{total} tiles"))
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
                            let response = Self::render_tile(
                                ui, idx, name, state, texture, tile_w, tile_h, debug,
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

        // Request repaint while there's pending work
        if self.grid.tile_count() > 0 {
            let has_pending = self.grid.visible_in_state(TileState::NotLoaded).len() > 0
                || self.grid.visible_in_state(TileState::LoadingEmbedded).len() > 0
                || self
                    .grid
                    .visible_in_state(TileState::CreatingThumbnail)
                    .len()
                    > 0;
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
        tile_w: f32,
        tile_h: f32,
        debug: bool,
    ) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(tile_w, tile_h), egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);

            if let Some(tex) = texture {
                let tex_size = tex.size_vec2();
                let scale = (tile_w / tex_size.x).min(tile_h / tex_size.y);
                let display_w = tex_size.x * scale;
                let display_h = tex_size.y * scale;
                let offset_x = (tile_w - display_w) / 2.0;
                let offset_y = (tile_h - display_h) / 2.0;

                painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(24, 24, 24));

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
            } else {
                let bg = match state {
                    TileState::NotLoaded => egui::Color32::from_rgb(48, 48, 48),
                    TileState::LoadingEmbedded => egui::Color32::from_rgb(60, 55, 40),
                    TileState::CreatingThumbnail => egui::Color32::from_rgb(40, 55, 60),
                    TileState::Loaded => egui::Color32::from_rgb(35, 60, 35),
                };
                painter.rect_filled(rect, 2.0, bg);
            }

            if response.hovered() {
                painter.rect_filled(
                    rect,
                    2.0,
                    egui::Color32::from_rgba_premultiplied(255, 255, 255, 20),
                );
            }

            painter.text(
                egui::pos2(rect.center().x, rect.max.y - 4.0),
                egui::Align2::CENTER_BOTTOM,
                name,
                egui::FontId::proportional(10.0),
                egui::Color32::from_rgb(170, 170, 170),
            );

            if debug {
                painter.text(
                    egui::pos2(rect.max.x - 4.0, rect.min.y + 4.0),
                    egui::Align2::RIGHT_TOP,
                    &state.to_string(),
                    egui::FontId::monospace(10.0),
                    egui::Color32::from_rgb(180, 180, 180),
                );
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
