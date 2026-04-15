//! GridView — renders a Grid using egui with row-based virtualization.
//!
//! Only visible rows are rendered. Each tile shows its state text
//! when IV_DEBUG is set.

use eframe::egui;

use crate::grid::{Grid, GridConfig, TileState};

/// Returns true if IV_DEBUG env var is set to a truthy value.
fn debug_mode() -> bool {
    std::env::var("IV_DEBUG").map_or(false, |v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Visual rendering of a Grid.
pub struct GridView {
    grid: Grid,
    debug: bool,
}

impl GridView {
    /// Create a new GridView with the given grid.
    pub fn new(grid: Grid) -> Self {
        Self {
            grid,
            debug: debug_mode(),
        }
    }

    /// Create a demo grid with `n` tiles, all in NotLoaded state.
    pub fn new_demo(n: usize) -> Self {
        let mut grid = Grid::new(GridConfig::default());
        for i in 0..n {
            grid.add_tile(format!("img_{i:05}.jpg"));
        }
        Self {
            grid,
            debug: debug_mode(),
        }
    }

    /// Access the underlying grid (e.g., for adding tiles).
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// Render the grid into the given UI area. Returns clicked tile index if any.
    pub fn show(&mut self, _ctx: &egui::Context, ui: &mut egui::Ui) -> Option<usize> {
        let config = self.grid.config().clone();
        let tile_w = config.tile_width;
        let tile_h = config.tile_height;
        let padding = config.padding;
        let cell_h = config.cell_height();

        // Update grid's viewport from egui's available area
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
                // Sync grid viewport with egui
                self.grid
                    .set_viewport_size(available_width, ui.clip_rect().height());
                let scroll_offset = ui.clip_rect().min.y - ui.min_rect().min.y;
                self.grid.set_scroll(scroll_offset);

                let cols = self.grid.cols();
                let total_rows = self.grid.total_rows();
                let vr = self.grid.visible_rows();

                // Buffer: render 2 extra rows above and below
                let render_first = vr.first.saturating_sub(2);
                let render_last = (vr.last + 2).min(total_rows);

                // Disable egui's vertical spacing — we manage row gaps
                ui.spacing_mut().item_spacing.y = 0.0;

                // Skip rows above render zone
                if render_first > 0 {
                    ui.allocate_space(egui::vec2(available_width, render_first as f32 * cell_h));
                }

                // Render visible + buffer rows
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
                            let response =
                                Self::render_tile(ui, idx, name, state, tile_w, tile_h, debug);
                            if response.clicked() {
                                clicked = Some(idx);
                            }
                        }
                    });
                    ui.allocate_space(egui::vec2(0.0, padding));
                }

                // Skip rows below render zone
                if render_last < total_rows {
                    ui.allocate_space(egui::vec2(
                        available_width,
                        (total_rows - render_last) as f32 * cell_h,
                    ));
                }
            });

        clicked
    }

    /// Render a single tile.
    fn render_tile(
        ui: &mut egui::Ui,
        idx: usize,
        name: &str,
        state: TileState,
        tile_w: f32,
        tile_h: f32,
        debug: bool,
    ) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(tile_w, tile_h), egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);

            // Background color based on state
            let bg = match state {
                TileState::NotLoaded => egui::Color32::from_rgb(48, 48, 48),
                TileState::LoadingEmbedded => egui::Color32::from_rgb(60, 55, 40),
                TileState::CreatingThumbnail => egui::Color32::from_rgb(40, 55, 60),
                TileState::Loaded => egui::Color32::from_rgb(35, 60, 35),
            };
            painter.rect_filled(rect, 2.0, bg);

            // Hover highlight
            if response.hovered() {
                painter.rect_filled(
                    rect,
                    2.0,
                    egui::Color32::from_rgba_premultiplied(255, 255, 255, 20),
                );
            }

            // Filename at bottom center
            painter.text(
                egui::pos2(rect.center().x, rect.max.y - 4.0),
                egui::Align2::CENTER_BOTTOM,
                name,
                egui::FontId::proportional(10.0),
                egui::Color32::from_rgb(170, 170, 170),
            );

            // Debug: show state text and index
            if debug {
                let state_text = state.to_string();
                painter.text(
                    egui::pos2(rect.max.x - 4.0, rect.min.y + 4.0),
                    egui::Align2::RIGHT_TOP,
                    &state_text,
                    egui::FontId::monospace(10.0),
                    egui::Color32::from_rgb(180, 180, 180),
                );
                // Index in top-left corner
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
