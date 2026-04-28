//! Grid — a testable data structure for a scrollable tile grid.
//!
//! Owns layout math, viewport tracking, tile state, and visible-range
//! computation. No GPU, no I/O, no egui dependency.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// How tiles are sorted for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// Alphabetical by filename (default). Instant — no metadata needed.
    Name,
    /// By EXIF date-taken. Requires background metadata scan.
    DateTaken,
}

/// State of a single tile in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileState {
    /// Not yet loaded — no work has been requested.
    NotLoaded,
    /// Extracting embedded thumbnail (EXIF/BMFF).
    LoadingEmbedded,
    /// Embedded extraction done, no thumbnail found — awaiting full decode.
    EmbeddedMissed,
    /// Full decode + downscale in progress.
    CreatingThumbnail,
    /// Thumbnail is ready for display.
    Loaded,
    /// Thumbnail creation failed. Tile remains visible as an error placeholder.
    Failed,
}

impl std::fmt::Display for TileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TileState::NotLoaded => write!(f, "not loaded"),
            TileState::LoadingEmbedded => write!(f, "loading…"),
            TileState::EmbeddedMissed => write!(f, "no embed"),
            TileState::CreatingThumbnail => write!(f, "creating…"),
            TileState::Loaded => write!(f, "loaded"),
            TileState::Failed => write!(f, "failed"),
        }
    }
}

/// Configuration for the grid layout (pixel dimensions).
#[derive(Debug, Clone)]
pub struct GridConfig {
    /// Width of each tile in pixels.
    pub tile_width: f32,
    /// Height of each tile in pixels.
    pub tile_height: f32,
    /// Padding between tiles (horizontal and vertical).
    pub padding: f32,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            tile_width: 275.0,
            tile_height: 275.0,
            padding: 8.0,
        }
    }
}

impl GridConfig {
    /// Total width of one cell (tile + padding).
    pub fn cell_width(&self) -> f32 {
        self.tile_width + self.padding
    }

    /// Total height of one cell (tile + padding).
    pub fn cell_height(&self) -> f32 {
        self.tile_height + self.padding
    }
}

/// Viewport state — represents the visible window into the grid.
#[derive(Debug, Clone)]
pub struct Viewport {
    /// Width of the viewport in pixels.
    pub width: f32,
    /// Height of the viewport in pixels.
    pub height: f32,
    /// Vertical scroll offset in pixels (0 = top of grid).
    pub scroll_y: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1200.0,
            height: 800.0,
            scroll_y: 0.0,
        }
    }
}

/// The visible range of rows, expressed as a half-open range [first, last).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRows {
    /// First visible row (inclusive).
    pub first: usize,
    /// Last visible row (exclusive).
    pub last: usize,
}

// ---------------------------------------------------------------------------
// Activity log
// ---------------------------------------------------------------------------

/// A single recorded event in the grid's activity log.
#[derive(Debug, Clone)]
pub struct GridEvent {
    /// Microseconds since the grid was created.
    pub time_us: u64,
    /// What happened.
    pub kind: GridEventKind,
}

/// What kind of event occurred.
#[derive(Debug, Clone)]
pub enum GridEventKind {
    /// Tiles were added (count, first index).
    TilesAdded { count: usize, first_idx: usize },
    /// Viewport scrolled to a new position.
    Scrolled {
        scroll_y: f32,
        visible_first: usize,
        visible_last: usize,
    },
    /// Tile state changed.
    StateChange {
        idx: usize,
        from: TileState,
        to: TileState,
    },
    /// Work was scheduled (indices + tier description).
    WorkScheduled { indices: Vec<usize>, tier: String },
    /// A result was received from a worker.
    ResultReceived { idx: usize, kind: String, ms: f64 },
    /// Generation bumped (stale work invalidated).
    GenerationBump { generation: u64 },
    /// Frame timing: how long the UI frame took.
    FrameTiming {
        frame_ms: f64,
        poll_ms: f64,
        schedule_ms: f64,
        render_ms: f64,
        results_processed: usize,
        results_pending: usize,
    },
}

/// The scrollable tile grid.
///
/// Pure data structure — no I/O, no GPU, fully testable.
pub struct Grid {
    config: GridConfig,
    viewport: Viewport,
    states: Vec<TileState>,
    names: Vec<String>,
    paths: Vec<PathBuf>,
    /// Per-tile EXIF date-taken string (e.g. "2023:04:15 14:30:00").
    dates: Vec<Option<String>>,
    /// Display order: maps display position → tile index.
    /// In Name mode this is the identity mapping (0, 1, 2, …).
    /// In DateTaken mode the first `sorted_count` entries are date-sorted,
    /// and the remainder are tiles whose dates are not yet known.
    display_order: Vec<usize>,
    /// How many entries at the front of `display_order` are in their final
    /// sorted position (only meaningful in DateTaken mode).
    sorted_count: usize,
    /// Current sort mode.
    sort_mode: SortMode,
    /// Activity log for diagnostics. Only populated when logging is enabled.
    log: Vec<GridEvent>,
    /// Whether activity logging is enabled.
    logging: bool,
    /// Creation time for relative timestamps.
    created: Instant,
}

impl Grid {
    /// Create a new empty grid with the given configuration.
    pub fn new(config: GridConfig) -> Self {
        Self {
            config,
            viewport: Viewport::default(),
            states: Vec::new(),
            names: Vec::new(),
            paths: Vec::new(),
            dates: Vec::new(),
            display_order: Vec::new(),
            sorted_count: 0,
            sort_mode: SortMode::Name,
            log: Vec::new(),
            logging: false,
            created: Instant::now(),
        }
    }

    /// Enable activity logging for diagnostics.
    pub fn enable_logging(&mut self) {
        self.logging = true;
    }

    /// Get the activity log.
    pub fn log(&self) -> &[GridEvent] {
        &self.log
    }

    /// Take and clear the activity log.
    pub fn take_log(&mut self) -> Vec<GridEvent> {
        std::mem::take(&mut self.log)
    }

    /// Write the activity log to a JSON file.
    /// Returns the path written, or None if logging is disabled/empty.
    pub fn dump_log(&self, path: &Path) -> Option<PathBuf> {
        if self.log.is_empty() {
            return None;
        }
        let mut out = String::from("[\n");
        for (i, event) in self.log.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
            }
            let kind_json = match &event.kind {
                GridEventKind::TilesAdded { count, first_idx } => {
                    format!(r#"{{"type":"tiles_added","count":{count},"first_idx":{first_idx}}}"#)
                }
                GridEventKind::Scrolled {
                    scroll_y,
                    visible_first,
                    visible_last,
                } => {
                    format!(
                        r#"{{"type":"scrolled","scroll_y":{scroll_y:.1},"visible_first":{visible_first},"visible_last":{visible_last}}}"#
                    )
                }
                GridEventKind::StateChange { idx, from, to } => {
                    format!(r#"{{"type":"state_change","idx":{idx},"from":"{from}","to":"{to}"}}"#)
                }
                GridEventKind::WorkScheduled { indices, tier } => {
                    let idx_str: Vec<String> = indices.iter().map(|i| i.to_string()).collect();
                    format!(
                        r#"{{"type":"work_scheduled","tier":"{tier}","count":{},"indices":[{}]}}"#,
                        indices.len(),
                        idx_str.join(",")
                    )
                }
                GridEventKind::ResultReceived { idx, kind, ms } => {
                    format!(r#"{{"type":"result","idx":{idx},"kind":"{kind}","ms":{ms:.2}}}"#)
                }
                GridEventKind::GenerationBump { generation } => {
                    format!(r#"{{"type":"generation_bump","generation":{generation}}}"#)
                }
                GridEventKind::FrameTiming {
                    frame_ms,
                    poll_ms,
                    schedule_ms,
                    render_ms,
                    results_processed,
                    results_pending,
                } => {
                    format!(
                        r#"{{"type":"frame","frame_ms":{frame_ms:.2},"poll_ms":{poll_ms:.2},"schedule_ms":{schedule_ms:.2},"render_ms":{render_ms:.2},"results_processed":{results_processed},"results_pending":{results_pending}}}"#
                    )
                }
            };
            out.push_str(&format!(
                r#"  {{"time_us":{},"event":{kind_json}}}"#,
                event.time_us
            ));
        }
        out.push_str("\n]\n");

        let dest = path.to_path_buf();
        if std::fs::write(&dest, &out).is_ok() {
            Some(dest)
        } else {
            None
        }
    }

    fn record(&mut self, kind: GridEventKind) {
        if self.logging {
            self.log.push(GridEvent {
                time_us: self.created.elapsed().as_micros() as u64,
                kind,
            });
        }
    }

    /// Record an external event (e.g., from GridView scheduling).
    pub fn record_event(&mut self, kind: GridEventKind) {
        self.record(kind);
    }

    // -- Tile management ---------------------------------------------------

    /// Add a named tile to the grid. Returns its index.
    pub fn add_tile(&mut self, name: impl Into<String>) -> usize {
        let idx = self.states.len();
        self.states.push(TileState::NotLoaded);
        self.names.push(name.into());
        self.paths.push(PathBuf::new());
        self.dates.push(None);
        self.display_order.push(idx);
        idx
    }

    /// Add a tile with a full file path. Name is extracted from the path.
    pub fn add_tile_with_path(&mut self, path: PathBuf) -> usize {
        let idx = self.states.len();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.states.push(TileState::NotLoaded);
        self.names.push(name);
        self.paths.push(path);
        self.dates.push(None);
        // In DateTaken mode, new tiles go into the unsorted section (after sorted_count).
        // In Name mode, identity mapping — new tile goes at the end.
        self.display_order.push(idx);
        // Batch-log: record when adding first tile or every 100th tile
        if idx == 0 || (idx + 1).is_multiple_of(100) {
            self.record(GridEventKind::TilesAdded {
                count: idx + 1,
                first_idx: 0,
            });
        }
        idx
    }

    /// Number of tiles in the grid.
    pub fn tile_count(&self) -> usize {
        self.states.len()
    }

    /// Get the state of a tile.
    pub fn tile_state(&self, idx: usize) -> TileState {
        self.states[idx]
    }

    /// Set the state of a tile.
    pub fn set_tile_state(&mut self, idx: usize, state: TileState) {
        let from = self.states[idx];
        if from != state {
            self.record(GridEventKind::StateChange {
                idx,
                from,
                to: state,
            });
        }
        self.states[idx] = state;
    }

    /// Get the name of a tile.
    pub fn tile_name(&self, idx: usize) -> &str {
        &self.names[idx]
    }

    /// Get the file path of a tile.
    pub fn tile_path(&self, idx: usize) -> &Path {
        &self.paths[idx]
    }

    /// Get visible tile indices that are in the given state.
    /// O(visible) — only scans visible display positions.
    pub fn visible_in_state(&self, state: TileState) -> Vec<usize> {
        let (start, end) = self.visible_tile_range();
        (start..end)
            .map(|pos| self.display_order[pos])
            .filter(|&idx| self.states[idx] == state)
            .collect()
    }

    // -- Sort / display order ----------------------------------------------

    /// Get the current sort mode.
    pub fn sort_mode(&self) -> SortMode {
        self.sort_mode
    }

    /// Switch sort mode. Rebuilds the display order.
    pub fn set_sort_mode(&mut self, mode: SortMode) {
        if self.sort_mode == mode {
            return;
        }
        self.sort_mode = mode;
        self.rebuild_display_order();
    }

    /// Set a tile's date-taken string and, if in DateTaken mode, re-sort it
    /// into the sorted section of the display order.
    pub fn set_tile_date(&mut self, idx: usize, date: Option<String>) {
        self.dates[idx] = date;
        if self.sort_mode == SortMode::DateTaken {
            self.insert_into_sorted(idx);
        }
    }

    /// Get a tile's date-taken string.
    pub fn tile_date(&self, idx: usize) -> Option<&str> {
        self.dates[idx].as_deref()
    }

    /// How many tiles have been sorted (in DateTaken mode).
    pub fn sorted_count(&self) -> usize {
        if self.sort_mode == SortMode::DateTaken {
            self.sorted_count
        } else {
            self.display_order.len()
        }
    }

    /// Whether the date scan is complete (all tiles have dates or no-date).
    pub fn date_scan_complete(&self) -> bool {
        self.sort_mode != SortMode::DateTaken || self.sorted_count >= self.display_order.len()
    }

    /// Map a display position to a tile index.
    pub fn display_to_tile(&self, pos: usize) -> usize {
        self.display_order[pos]
    }

    /// Get display order as a slice (for rendering).
    pub fn display_order(&self) -> &[usize] {
        &self.display_order
    }

    /// Rebuild display_order from scratch based on current sort mode.
    fn rebuild_display_order(&mut self) {
        let n = self.states.len();
        match self.sort_mode {
            SortMode::Name => {
                // Identity mapping — tiles are already in alphabetical order from NTFS
                self.display_order = (0..n).collect();
                self.sorted_count = n;
            }
            SortMode::DateTaken => {
                // Partition: tiles with dates go to the sorted prefix,
                // tiles without go to the unsorted suffix.
                let mut with_dates: Vec<usize> =
                    (0..n).filter(|&i| self.dates[i].is_some()).collect();
                let without_dates: Vec<usize> =
                    (0..n).filter(|&i| self.dates[i].is_none()).collect();
                // Sort the dated tiles by date string (lexicographic on "YYYY:MM:DD HH:MM:SS" works)
                with_dates.sort_by(|&a, &b| {
                    self.dates[a]
                        .as_deref()
                        .unwrap_or("")
                        .cmp(self.dates[b].as_deref().unwrap_or(""))
                });
                self.sorted_count = with_dates.len();
                self.display_order = with_dates;
                self.display_order.extend(without_dates);
            }
        }
    }

    /// Insert a tile (that just got a date) into the sorted section.
    /// Removes it from the unsorted section and binary-searches the sorted
    /// prefix for the correct insertion point.
    fn insert_into_sorted(&mut self, tile_idx: usize) {
        // Find and remove from current position (should be in unsorted section)
        let Some(pos) = self.display_order.iter().position(|&i| i == tile_idx) else {
            return;
        };
        self.display_order.remove(pos);
        // If it was already in the sorted section, adjust sorted_count
        if pos < self.sorted_count {
            self.sorted_count -= 1;
        }

        let date = self.dates[tile_idx].as_deref().unwrap_or("");
        // Binary search within the sorted prefix for insertion point
        let insert_pos = self.display_order[..self.sorted_count]
            .partition_point(|&i| self.dates[i].as_deref().unwrap_or("") <= date);
        self.display_order.insert(insert_pos, tile_idx);
        self.sorted_count += 1;
    }

    /// Mark a tile as having no date (scan completed, no EXIF date found).
    /// In DateTaken mode, moves it to the end of the sorted section
    /// (treated as "unknown date", sorts after all dated tiles).
    pub fn set_tile_no_date(&mut self, idx: usize) {
        if self.sort_mode == SortMode::DateTaken {
            self.insert_into_sorted_no_date(idx);
        }
    }

    /// Move a dateless tile from the unsorted suffix into the end of the sorted prefix.
    fn insert_into_sorted_no_date(&mut self, tile_idx: usize) {
        let Some(pos) = self.display_order.iter().position(|&i| i == tile_idx) else {
            return;
        };
        // Only move if it's currently in the unsorted section
        if pos < self.sorted_count {
            return;
        }
        self.display_order.remove(pos);
        // Insert at the end of the sorted section (after all dated tiles)
        self.display_order.insert(self.sorted_count, tile_idx);
        self.sorted_count += 1;
    }

    /// Get tile indices that still need date scanning.
    /// Returns indices from the unsorted section of display_order.
    pub fn tiles_needing_date_scan(&self) -> Vec<usize> {
        if self.sort_mode != SortMode::DateTaken {
            return Vec::new();
        }
        self.display_order[self.sorted_count..].to_vec()
    }

    // -- Layout ------------------------------------------------------------

    /// Number of columns that fit in the current viewport width.
    pub fn cols(&self) -> usize {
        let cw = self.config.cell_width();
        if cw <= 0.0 {
            return 1;
        }
        ((self.viewport.width + self.config.padding) / cw)
            .floor()
            .max(1.0) as usize
    }

    /// Total number of rows (based on tile count and columns).
    /// In DateTaken mode with pending scans, includes a separator row.
    pub fn total_rows(&self) -> usize {
        let cols = self.cols();
        if cols == 0 {
            return 0;
        }
        let n = self.states.len();
        if n == 0 {
            return 0;
        }
        if self.sort_mode == SortMode::DateTaken && !self.date_scan_complete() {
            let sorted_rows = self.sorted_count.div_ceil(cols);
            let unsorted_count = n - self.sorted_count;
            let unsorted_rows = unsorted_count.div_ceil(cols);
            let separator = if unsorted_count > 0 { 1 } else { 0 };
            sorted_rows + separator + unsorted_rows
        } else {
            n.div_ceil(cols)
        }
    }

    /// The display-position row where the separator sits (only in DateTaken mode).
    /// Returns None if not in DateTaken mode or scan is complete.
    pub fn separator_row(&self) -> Option<usize> {
        if self.sort_mode != SortMode::DateTaken || self.date_scan_complete() {
            return None;
        }
        let cols = self.cols();
        if cols == 0 {
            return None;
        }
        let unsorted_count = self.states.len() - self.sorted_count;
        if unsorted_count == 0 {
            return None;
        }
        Some(self.sorted_count.div_ceil(cols))
    }

    /// Total content height in pixels.
    pub fn content_height(&self) -> f32 {
        self.total_rows() as f32 * self.config.cell_height()
    }

    /// Convert a tile index to (row, col).
    pub fn tile_to_row_col(&self, idx: usize) -> (usize, usize) {
        let cols = self.cols();
        (idx / cols, idx % cols)
    }

    /// Convert (row, col) to tile index, if valid.
    pub fn row_col_to_tile(&self, row: usize, col: usize) -> Option<usize> {
        let cols = self.cols();
        if col >= cols {
            return None;
        }
        let idx = row * cols + col;
        if idx < self.states.len() {
            Some(idx)
        } else {
            None
        }
    }

    // -- Viewport ----------------------------------------------------------

    /// Set the viewport dimensions.
    pub fn set_viewport_size(&mut self, width: f32, height: f32) {
        self.viewport.width = width;
        self.viewport.height = height;
    }

    /// Set the scroll position (clamped to valid range).
    pub fn set_scroll(&mut self, scroll_y: f32) {
        let max_scroll = (self.content_height() - self.viewport.height).max(0.0);
        let new_y = scroll_y.clamp(0.0, max_scroll);
        let old_y = self.viewport.scroll_y;
        self.viewport.scroll_y = new_y;
        // Log significant scroll changes (>1 cell height)
        if self.logging && (new_y - old_y).abs() > self.config.cell_height() {
            let vr = self.visible_rows();
            self.log.push(GridEvent {
                time_us: self.created.elapsed().as_micros() as u64,
                kind: GridEventKind::Scrolled {
                    scroll_y: new_y,
                    visible_first: vr.first,
                    visible_last: vr.last,
                },
            });
        }
    }

    /// Get the current scroll position.
    pub fn scroll_y(&self) -> f32 {
        self.viewport.scroll_y
    }

    /// Compute which rows are currently visible (including partially visible).
    pub fn visible_rows(&self) -> VisibleRows {
        let ch = self.config.cell_height();
        if ch <= 0.0 || self.states.is_empty() {
            return VisibleRows { first: 0, last: 0 };
        }
        let total = self.total_rows();
        let first = (self.viewport.scroll_y / ch).floor().max(0.0) as usize;
        let last = ((self.viewport.scroll_y + self.viewport.height) / ch)
            .ceil()
            .min(total as f32) as usize;
        VisibleRows { first, last }
    }

    /// Compute the range of display-order positions that are visible [start, end).
    /// Accounts for the separator row in DateTaken mode.
    pub fn visible_tile_range(&self) -> (usize, usize) {
        let vr = self.visible_rows();
        let cols = self.cols();
        let n = self.display_order.len();
        if n == 0 || cols == 0 {
            return (0, 0);
        }

        let sep = self.separator_row();

        // Convert a visual row to a display-order start position
        let row_to_start = |row: usize| -> usize {
            match sep {
                Some(sep_row) if row > sep_row => {
                    // After separator: skip sorted tiles + separator row
                    let unsorted_pos = (row - sep_row - 1) * cols;
                    self.sorted_count + unsorted_pos
                }
                _ => row * cols,
            }
        };

        let start = row_to_start(vr.first).min(n);
        // For end, compute the start of the row *after* the last visible row
        let end = row_to_start(vr.last).min(n);
        (start, end)
    }

    /// Iterate over visible tile indices (actual tile indices, not display positions).
    pub fn visible_tiles(&self) -> Vec<usize> {
        let (start, end) = self.visible_tile_range();
        (start..end).map(|pos| self.display_order[pos]).collect()
    }

    /// Iterate over visible tiles, yielding (tile_index, name, state) for each.
    pub fn visible_tile_info(&self) -> Vec<(usize, &str, TileState)> {
        let (start, end) = self.visible_tile_range();
        (start..end)
            .map(|pos| {
                let idx = self.display_order[pos];
                (idx, self.names[idx].as_str(), self.states[idx])
            })
            .collect()
    }

    // -- Config access -----------------------------------------------------

    pub fn config(&self) -> &GridConfig {
        &self.config
    }

    /// Set the tile display size. Re-clamps scroll position since the
    /// content height changes when tile size changes.
    pub fn set_tile_size(&mut self, width: f32, height: f32) {
        self.config.tile_width = width;
        self.config.tile_height = height;
        // Re-clamp scroll — content height changed
        let max_scroll = (self.content_height() - self.viewport.height).max(0.0);
        self.viewport.scroll_y = self.viewport.scroll_y.clamp(0.0, max_scroll);
    }

    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    /// Get all file paths in display order (for image view navigation).
    pub fn all_paths(&self) -> Vec<PathBuf> {
        self.display_order
            .iter()
            .map(|&idx| self.paths[idx].clone())
            .collect()
    }

    /// Get all file paths in display order with their display positions.
    pub fn all_paths_with_positions(&self) -> Vec<(usize, PathBuf)> {
        self.display_order
            .iter()
            .enumerate()
            .map(|(pos, &idx)| (pos, self.paths[idx].clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_grid(tile_count: usize) -> Grid {
        // Use fixed 160px tiles for test stability (independent of app default)
        let config = GridConfig {
            tile_width: 160.0,
            tile_height: 160.0,
            padding: 8.0,
        };
        let mut g = Grid::new(config);
        g.set_viewport_size(1200.0, 800.0);
        for i in 0..tile_count {
            g.add_tile(format!("img_{i:05}.jpg"));
        }
        g
    }

    // -- Layout tests ------------------------------------------------------

    #[test]
    fn cols_from_viewport_width() {
        let config = GridConfig {
            tile_width: 160.0,
            tile_height: 160.0,
            padding: 8.0,
        };
        let mut g = Grid::new(config);
        // cell_width = 160 + 8 = 168
        // (1200 + 8) / 168 = 7.19 → 7 cols
        g.set_viewport_size(1200.0, 800.0);
        assert_eq!(g.cols(), 7);

        // Narrow window: (340 + 8) / 168 = 2.07 → 2 cols
        g.set_viewport_size(340.0, 800.0);
        assert_eq!(g.cols(), 2);

        // Very narrow: always at least 1
        g.set_viewport_size(100.0, 800.0);
        assert_eq!(g.cols(), 1);
    }

    #[test]
    fn total_rows_calculation() {
        let g = make_grid(0);
        assert_eq!(g.total_rows(), 0);

        let g = make_grid(7); // exactly 1 row
        assert_eq!(g.total_rows(), 1);

        let g = make_grid(8); // 7+1 = 2 rows
        assert_eq!(g.total_rows(), 2);

        let g = make_grid(21); // 3 full rows
        assert_eq!(g.total_rows(), 3);

        let g = make_grid(22); // 3 full + 1 partial
        assert_eq!(g.total_rows(), 4);
    }

    #[test]
    fn content_height() {
        let g = make_grid(21); // 3 rows, cell_height = 168
        assert_eq!(g.content_height(), 3.0 * 168.0);
    }

    #[test]
    fn tile_to_row_col_conversion() {
        let g = make_grid(21); // 7 cols
        assert_eq!(g.tile_to_row_col(0), (0, 0));
        assert_eq!(g.tile_to_row_col(6), (0, 6));
        assert_eq!(g.tile_to_row_col(7), (1, 0));
        assert_eq!(g.tile_to_row_col(20), (2, 6));
    }

    #[test]
    fn row_col_to_tile_valid() {
        let g = make_grid(21);
        assert_eq!(g.row_col_to_tile(0, 0), Some(0));
        assert_eq!(g.row_col_to_tile(2, 6), Some(20));
        assert_eq!(g.row_col_to_tile(3, 0), None); // out of bounds
        assert_eq!(g.row_col_to_tile(0, 7), None); // col out of range
    }

    // -- Tile state tests --------------------------------------------------

    #[test]
    fn initial_tile_state() {
        let g = make_grid(5);
        for i in 0..5 {
            assert_eq!(g.tile_state(i), TileState::NotLoaded);
        }
    }

    #[test]
    fn tile_state_transitions() {
        let mut g = make_grid(3);
        assert_eq!(g.tile_state(0), TileState::NotLoaded);

        g.set_tile_state(0, TileState::LoadingEmbedded);
        assert_eq!(g.tile_state(0), TileState::LoadingEmbedded);

        g.set_tile_state(0, TileState::CreatingThumbnail);
        assert_eq!(g.tile_state(0), TileState::CreatingThumbnail);

        g.set_tile_state(0, TileState::Loaded);
        assert_eq!(g.tile_state(0), TileState::Loaded);
    }

    #[test]
    fn tile_state_display() {
        assert_eq!(TileState::NotLoaded.to_string(), "not loaded");
        assert_eq!(TileState::LoadingEmbedded.to_string(), "loading…");
        assert_eq!(TileState::EmbeddedMissed.to_string(), "no embed");
        assert_eq!(TileState::CreatingThumbnail.to_string(), "creating…");
        assert_eq!(TileState::Loaded.to_string(), "loaded");
        assert_eq!(TileState::Failed.to_string(), "failed");
    }

    // -- Viewport + scroll tests -------------------------------------------

    #[test]
    fn scroll_clamped_to_bounds() {
        let mut g = make_grid(21); // 3 rows, 504px content, 800px viewport
        // Content fits in viewport → max_scroll = 0
        g.set_scroll(100.0);
        assert_eq!(g.scroll_y(), 0.0);

        // Larger grid: 100 tiles → 15 rows → 2520px content
        let mut g = make_grid(100);
        g.set_scroll(5000.0);
        assert_eq!(g.scroll_y(), 2520.0 - 800.0); // 1720

        // Negative scroll clamped to 0
        g.set_scroll(-50.0);
        assert_eq!(g.scroll_y(), 0.0);
    }

    #[test]
    fn visible_rows_at_top() {
        let g = make_grid(100); // 15 rows
        // At scroll=0, viewport=800, cell_height=168
        // first = 0, last = ceil(800/168) = 5
        let vr = g.visible_rows();
        assert_eq!(vr.first, 0);
        assert_eq!(vr.last, 5);
    }

    #[test]
    fn visible_rows_scrolled() {
        let mut g = make_grid(100); // 15 rows
        // Scroll to 2 rows down: scroll_y = 336
        g.set_scroll(336.0);
        let vr = g.visible_rows();
        assert_eq!(vr.first, 2);
        // last = ceil((336 + 800) / 168) = ceil(6.76) = 7
        assert_eq!(vr.last, 7);
    }

    #[test]
    fn visible_rows_at_bottom() {
        let mut g = make_grid(100); // 15 rows, content=2520
        g.set_scroll(2520.0); // clamped to 1720
        let vr = g.visible_rows();
        // first = floor(1720/168) = 10
        assert_eq!(vr.first, 10);
        // last = ceil((1720+800)/168) = ceil(15) = 15
        assert_eq!(vr.last, 15);
    }

    #[test]
    fn visible_rows_empty_grid() {
        let g = make_grid(0);
        let vr = g.visible_rows();
        assert_eq!(vr.first, 0);
        assert_eq!(vr.last, 0);
    }

    #[test]
    fn visible_tile_range_at_top() {
        let g = make_grid(100); // 7 cols
        let (start, end) = g.visible_tile_range();
        assert_eq!(start, 0);
        // 5 visible rows × 7 cols = 35
        assert_eq!(end, 35);
    }

    #[test]
    fn visible_tile_range_partial_last_row() {
        let g = make_grid(10); // 7+3 = 2 rows, but only 10 tiles
        let (start, end) = g.visible_tile_range();
        assert_eq!(start, 0);
        assert_eq!(end, 10); // clamped to tile count
    }

    #[test]
    fn visible_tiles_iterator() {
        let g = make_grid(10);
        let tiles: Vec<usize> = g.visible_tiles();
        assert_eq!(tiles, (0..10).collect::<Vec<_>>());
    }

    // -- Dynamic growth tests ----------------------------------------------

    #[test]
    fn grid_grows_dynamically() {
        let config = GridConfig {
            tile_width: 160.0,
            tile_height: 160.0,
            padding: 8.0,
        };
        let mut g = Grid::new(config);
        g.set_viewport_size(1200.0, 800.0);

        assert_eq!(g.tile_count(), 0);
        assert_eq!(g.total_rows(), 0);

        // Simulate enumeration adding tiles one at a time
        for i in 0..50 {
            let idx = g.add_tile(format!("img_{i:05}.jpg"));
            assert_eq!(idx, i);
        }

        assert_eq!(g.tile_count(), 50);
        assert_eq!(g.total_rows(), 8); // ceil(50/7) = 8
    }

    #[test]
    fn tile_names_stored() {
        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);
        g.add_tile("photo_001.heic");
        g.add_tile("sunset.jpg");
        g.add_tile("screenshot.png");

        assert_eq!(g.tile_name(0), "photo_001.heic");
        assert_eq!(g.tile_name(1), "sunset.jpg");
        assert_eq!(g.tile_name(2), "screenshot.png");
    }

    #[test]
    fn scroll_adjusts_as_grid_grows() {
        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);

        // Start with small grid — content fits in viewport
        for i in 0..7 {
            g.add_tile(format!("img_{i}.jpg"));
        }
        g.set_scroll(500.0);
        assert_eq!(g.scroll_y(), 0.0); // clamped — content fits

        // Grid grows beyond viewport
        for i in 7..207 {
            g.add_tile(format!("img_{i}.jpg"));
        }
        g.set_scroll(500.0);
        assert!(g.scroll_y() > 0.0); // now there's room to scroll
    }

    // -- Resize tests ------------------------------------------------------

    #[test]
    fn viewport_resize_changes_cols() {
        let mut g = make_grid(50);
        assert_eq!(g.cols(), 7);

        g.set_viewport_size(600.0, 800.0);
        // (600 + 8) / 168 = 3.6 → 3 cols
        assert_eq!(g.cols(), 3);
        // 50 tiles / 3 cols = 17 rows
        assert_eq!(g.total_rows(), 17);
    }

    // -- Scroll simulation (mimics rapid user scrolling) -------------------

    #[test]
    fn rapid_scroll_simulation() {
        let mut g = make_grid(1000);

        // Scroll through the grid in jumps
        let content = g.content_height();
        let steps = 20;
        for i in 0..=steps {
            let scroll = content * (i as f32 / steps as f32);
            g.set_scroll(scroll);
            let vr = g.visible_rows();

            // Visible rows should always be a valid range
            assert!(vr.first <= vr.last, "invalid range at scroll {scroll}");
            assert!(
                vr.last <= g.total_rows(),
                "visible past total at scroll {scroll}"
            );

            // Visible tile range should be within bounds
            let (start, end) = g.visible_tile_range();
            assert!(
                end <= g.tile_count(),
                "tile range past count at scroll {scroll}"
            );
            assert!(start <= end);
        }
    }

    #[test]
    fn scroll_while_growing() {
        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);

        // Simulate enumeration + scrolling interleaved
        for batch in 0..20 {
            // Add a batch of tiles
            for j in 0..50 {
                g.add_tile(format!("batch{batch}_img{j:03}.jpg"));
            }

            // Scroll to ~middle of current content
            let mid = g.content_height() / 2.0;
            g.set_scroll(mid);

            let vr = g.visible_rows();
            assert!(vr.first <= vr.last);
            assert!(vr.last <= g.total_rows());

            let (_start, end) = g.visible_tile_range();
            assert!(end <= g.tile_count());

            // Verify visible tiles are valid indices
            for idx in g.visible_tiles() {
                assert!(
                    idx < g.tile_count(),
                    "batch {batch}: idx {idx} >= count {}",
                    g.tile_count()
                );
            }
        }
    }

    // -- Visible tile info tests -------------------------------------------

    #[test]
    fn visible_tile_info_yields_names_and_states() {
        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);
        g.add_tile("a.jpg");
        g.add_tile("b.png");
        g.add_tile("c.heic");

        g.set_tile_state(1, TileState::Loaded);

        let info: Vec<_> = g.visible_tile_info();
        assert_eq!(info.len(), 3);
        assert_eq!(info[0], (0, "a.jpg", TileState::NotLoaded));
        assert_eq!(info[1], (1, "b.png", TileState::Loaded));
        assert_eq!(info[2], (2, "c.heic", TileState::NotLoaded));
    }

    #[test]
    fn visible_tile_info_only_visible() {
        let mut g = make_grid(100); // 15 rows, viewport shows 5
        g.set_scroll(336.0); // rows 2..7 visible

        let info: Vec<_> = g.visible_tile_info();
        // rows 2..7 = 5 rows × 7 cols = 35 tiles
        assert_eq!(info.len(), 35);
        // First visible tile should be row 2, col 0 = idx 14
        assert_eq!(info[0].0, 14);
        assert_eq!(info[0].1, "img_00014.jpg");
    }

    // -- Enumeration timing under growth + scroll --------------------------

    #[test]
    fn enumeration_while_scrolling_stays_fast() {
        use std::time::Instant;

        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);

        let mut add_times_us = Vec::new();
        let mut enum_times_us = Vec::new();
        let mut total_tiles = 0usize;

        // Simulate: add 100 tiles per batch, 100 batches = 10,000 tiles.
        // Between each batch, scroll to a random-ish position and
        // enumerate all visible tiles.
        for batch in 0..100 {
            // Add tiles
            let t0 = Instant::now();
            for j in 0..100 {
                g.add_tile(format!("img_{:06}.heic", total_tiles + j));
            }
            total_tiles += 100;
            add_times_us.push(t0.elapsed().as_micros());

            // Scroll to various positions
            let scroll = match batch % 4 {
                0 => 0.0,                       // top
                1 => g.content_height() / 2.0,  // middle
                2 => g.content_height(),        // bottom (clamped)
                _ => g.content_height() * 0.75, // 3/4
            };
            g.set_scroll(scroll);

            // Enumerate visible tiles (the hot path)
            let t1 = Instant::now();
            let mut count = 0;
            for (idx, name, state) in g.visible_tile_info() {
                assert!(idx < total_tiles);
                assert!(!name.is_empty());
                assert_eq!(state, TileState::NotLoaded);
                count += 1;
            }
            enum_times_us.push(t1.elapsed().as_micros());

            assert!(count > 0, "should have visible tiles at batch {batch}");
            assert!(count <= 50, "shouldn't enumerate more than ~50 tiles");
        }

        // Stats
        let add_avg = add_times_us.iter().sum::<u128>() as f64 / add_times_us.len() as f64;
        let enum_avg = enum_times_us.iter().sum::<u128>() as f64 / enum_times_us.len() as f64;
        let enum_max = *enum_times_us.iter().max().unwrap();

        println!("\n=== Growth + Scroll + Enumerate Timing ===");
        println!("  Total tiles:     {total_tiles}");
        println!("  Add 100 tiles:   {add_avg:.0}µs avg");
        println!("  Enumerate vis:   {enum_avg:.0}µs avg, {enum_max}µs max");
        println!("==========================================\n");

        // Assertions: enumeration should be <100µs even at 10k tiles
        assert!(
            enum_max < 100,
            "visible enumeration took {enum_max}µs, expected <100µs"
        );
    }

    #[test]
    fn rapid_scroll_during_growth_enumeration() {
        use std::time::Instant;

        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);

        // Add 1000 tiles upfront
        for i in 0..1000 {
            g.add_tile(format!("img_{i:05}.jpg"));
        }

        // Simulate rapid scrolling: 200 scroll jumps, enumerate each time
        let mut enum_times = Vec::new();
        let content = g.content_height();
        for step in 0..200 {
            // Zigzag scroll pattern
            let scroll = if step % 2 == 0 {
                content * (step as f32 / 200.0)
            } else {
                content * (1.0 - step as f32 / 200.0)
            };
            g.set_scroll(scroll);

            // Add a few more tiles mid-scroll (simulates ongoing enumeration)
            for j in 0..5 {
                g.add_tile(format!("late_{step}_{j}.jpg"));
            }

            let t = Instant::now();
            let tiles: Vec<_> = g.visible_tile_info();
            enum_times.push(t.elapsed().as_micros());

            assert!(!tiles.is_empty());
            for &(idx, _, _) in &tiles {
                assert!(idx < g.tile_count());
            }
        }

        let max_us = *enum_times.iter().max().unwrap();
        let avg_us = enum_times.iter().sum::<u128>() as f64 / enum_times.len() as f64;

        println!("\n=== Rapid Scroll + Growth Enumeration ===");
        println!("  Final tiles:     {}", g.tile_count());
        println!("  Enumerate vis:   {avg_us:.0}µs avg, {max_us}µs max");
        println!("=========================================\n");

        assert!(max_us < 100, "enumeration took {max_us}µs, expected <100µs");
    }

    // -- Async enumeration simulation --------------------------------------

    #[test]
    fn async_enumeration_feeds_grid() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let (tx, rx) = mpsc::channel::<String>();

        // Simulate an enumerator thread sending file names
        thread::spawn(move || {
            for i in 0..500 {
                tx.send(format!("IMG_{i:05}.heic")).unwrap();
                // Simulate filesystem latency — ~20µs per entry
                thread::sleep(Duration::from_micros(20));
            }
        });

        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);

        let mut frames = 0;
        let mut visible_counts = Vec::new();
        let start = Instant::now();

        // Simulate frame loop: poll channel, scroll around, enumerate visible
        loop {
            // Poll enumeration results (like the real app does in update())
            let mut added = 0;
            while let Ok(name) = rx.try_recv() {
                g.add_tile(name);
                added += 1;
                // Batch limit per frame — avoid starving rendering
                if added >= 50 {
                    break;
                }
            }

            // Simulate scrolling to different positions
            let scroll = match frames % 5 {
                0 => 0.0,
                1 => g.content_height() * 0.25,
                2 => g.content_height() * 0.5,
                3 => g.content_height() * 0.75,
                _ => g.content_height(),
            };
            g.set_scroll(scroll);

            // Enumerate visible tiles
            let visible: Vec<_> = g.visible_tile_info();
            visible_counts.push(visible.len());

            // Validate all visible tiles
            for &(idx, name, state) in &visible {
                assert!(idx < g.tile_count());
                assert!(!name.is_empty());
                assert_eq!(state, TileState::NotLoaded);
            }

            frames += 1;

            // Stop when enumeration is done and grid has all tiles
            if g.tile_count() >= 500 {
                break;
            }

            // Safety timeout
            if start.elapsed() > Duration::from_secs(5) {
                panic!("async enumeration didn't complete in 5 seconds");
            }

            thread::sleep(Duration::from_millis(1)); // ~1000fps frame loop
        }

        assert_eq!(g.tile_count(), 500);
        assert_eq!(g.tile_name(0), "IMG_00000.heic");
        assert_eq!(g.tile_name(499), "IMG_00499.heic");

        println!("\n=== Async Enumeration Simulation ===");
        println!("  Tiles:      {}", g.tile_count());
        println!("  Frames:     {frames}");
        println!(
            "  Avg visible: {:.0}",
            visible_counts.iter().sum::<usize>() as f64 / visible_counts.len() as f64
        );
        println!("====================================\n");
    }

    #[test]
    fn visible_not_loaded_filter() {
        // Simulates the future pattern: "give me visible tiles that need work"
        let mut g = make_grid(100);
        g.set_scroll(336.0); // rows 2..7

        // Mark some tiles as loaded
        for idx in 14..21 {
            g.set_tile_state(idx, TileState::Loaded);
        }

        // Filter visible tiles to find ones still needing work
        let need_work: Vec<usize> = g
            .visible_tile_info()
            .into_iter()
            .filter(|&(_, _, state)| state == TileState::NotLoaded)
            .map(|(idx, _, _)| idx)
            .collect();

        // 35 visible - 7 loaded = 28 need work
        assert_eq!(need_work.len(), 28);
        // None of the loaded tiles should be in the list
        for idx in 14..21 {
            assert!(!need_work.contains(&idx));
        }
    }

    // -- State machine correctness -----------------------------------------

    #[test]
    fn no_duplicate_scheduling_for_full_decode() {
        // Reproduces the bug from the real log where CreatingThumbnail tiles
        // were re-scheduled for full_decode every frame because the state
        // didn't transition to prevent re-scheduling.
        let mut g = make_grid(35); // ~5 rows visible
        g.enable_logging();

        // Phase 1: all tiles go through embedded extraction
        for idx in 0..35 {
            g.set_tile_state(idx, TileState::LoadingEmbedded);
        }

        // Some succeed, some miss
        for idx in 0..35 {
            if idx % 3 == 0 {
                g.set_tile_state(idx, TileState::EmbeddedMissed); // needs full decode
            } else {
                g.set_tile_state(idx, TileState::Loaded); // embedded ok
            }
        }

        // Phase 2: schedule full decode for EmbeddedMissed tiles
        let needs_full = g.visible_in_state(TileState::EmbeddedMissed);
        assert!(!needs_full.is_empty());

        // Transition to CreatingThumbnail (as schedule_visible_work should)
        for &idx in &needs_full {
            g.set_tile_state(idx, TileState::CreatingThumbnail);
        }

        // Now check: there should be NO EmbeddedMissed or CreatingThumbnail
        // tiles returned by visible_in_state for scheduling
        let still_needs_full = g.visible_in_state(TileState::EmbeddedMissed);
        assert!(
            still_needs_full.is_empty(),
            "EmbeddedMissed tiles should not be re-schedulable: {:?}",
            still_needs_full
        );

        // CreatingThumbnail means "in-flight" — should NOT be scheduled again
        let creating = g.visible_in_state(TileState::CreatingThumbnail);
        assert!(
            !creating.is_empty(),
            "should have in-flight CreatingThumbnail tiles"
        );

        // Simulate completion
        for &idx in &needs_full {
            g.set_tile_state(idx, TileState::Loaded);
        }

        // Everything should be loaded now
        let remaining = g.visible_in_state(TileState::EmbeddedMissed);
        assert!(remaining.is_empty());
        let creating = g.visible_in_state(TileState::CreatingThumbnail);
        assert!(creating.is_empty());
    }

    #[test]
    fn state_machine_full_lifecycle() {
        // Test the complete state machine: NotLoaded -> LoadingEmbedded ->
        // EmbeddedMissed -> CreatingThumbnail -> Loaded
        let mut g = make_grid(1);

        assert_eq!(g.tile_state(0), TileState::NotLoaded);

        // Scheduled for embedded extraction
        g.set_tile_state(0, TileState::LoadingEmbedded);
        assert_eq!(g.tile_state(0), TileState::LoadingEmbedded);

        // Embedded extraction failed
        g.set_tile_state(0, TileState::EmbeddedMissed);
        assert_eq!(g.tile_state(0), TileState::EmbeddedMissed);

        // Verify it shows up as needing full decode
        let needs_full = g.visible_in_state(TileState::EmbeddedMissed);
        assert_eq!(needs_full, vec![0]);

        // Scheduled for full decode
        g.set_tile_state(0, TileState::CreatingThumbnail);
        assert_eq!(g.tile_state(0), TileState::CreatingThumbnail);

        // Should NOT show up as needing scheduling anymore
        let needs_full = g.visible_in_state(TileState::EmbeddedMissed);
        assert!(needs_full.is_empty());

        // Full decode completed
        g.set_tile_state(0, TileState::Loaded);
        assert_eq!(g.tile_state(0), TileState::Loaded);
    }

    // -- Tile resize tests -------------------------------------------------

    #[test]
    fn set_tile_size_changes_layout() {
        let mut g = make_grid(100); // 7 cols at 160px, 15 rows
        assert_eq!(g.cols(), 7);
        assert_eq!(g.total_rows(), 15);

        // Make tiles bigger → fewer columns → more rows
        g.set_tile_size(300.0, 300.0);
        // cell_width = 300 + 8 = 308, (1200 + 8) / 308 = 3.9 → 3 cols
        assert_eq!(g.cols(), 3);
        // 100 / 3 = 34 rows
        assert_eq!(g.total_rows(), 34);

        // Make tiles smaller → more columns → fewer rows
        g.set_tile_size(80.0, 80.0);
        // cell_width = 80 + 8 = 88, (1200 + 8) / 88 = 13.7 → 13 cols
        assert_eq!(g.cols(), 13);
        // 100 / 13 = 8 rows
        assert_eq!(g.total_rows(), 8);
    }

    #[test]
    fn scroll_clamped_after_tile_resize() {
        let mut g = make_grid(100);
        // Scroll to the bottom
        g.set_scroll(99999.0);
        let scroll_before = g.scroll_y();
        assert!(scroll_before > 0.0);

        // Make tiles much bigger → content height grows, but fewer fit
        // so max_scroll changes
        g.set_tile_size(300.0, 300.0);
        let scroll_after = g.scroll_y();

        // Scroll should be re-clamped to valid range
        let max_scroll = (g.content_height() - g.viewport().height).max(0.0);
        assert!(
            scroll_after <= max_scroll,
            "scroll {scroll_after} exceeds max {max_scroll}"
        );
    }

    #[test]
    fn tile_resize_preserves_visible_invariants() {
        let mut g = make_grid(200);

        let sizes: &[(f32, f32)] = &[
            (80.0, 80.0),
            (120.0, 120.0),
            (160.0, 160.0),
            (200.0, 200.0),
            (320.0, 320.0),
        ];

        for &(w, h) in sizes {
            g.set_tile_size(w, h);
            g.set_scroll(g.content_height() / 2.0);

            let vr = g.visible_rows();
            assert!(vr.first <= vr.last, "invalid range at size {w}x{h}");
            assert!(vr.last <= g.total_rows(), "visible past total at {w}x{h}");

            let (start, end) = g.visible_tile_range();
            assert!(end <= g.tile_count(), "tile range past count at {w}x{h}");
            assert!(start <= end);
        }
    }

    // -- Sort / display order tests ----------------------------------------

    #[test]
    fn default_sort_is_name() {
        let g = make_grid(5);
        assert_eq!(g.sort_mode(), SortMode::Name);
        // Display order should be identity
        assert_eq!(g.display_order(), &[0, 1, 2, 3, 4]);
    }

    #[test]
    fn date_taken_sort_partitions_by_date() {
        let mut g = make_grid(5);
        g.set_sort_mode(SortMode::DateTaken);
        // Initially all tiles are unsorted (no dates)
        assert_eq!(g.sorted_count(), 0);
        assert!(!g.date_scan_complete());

        // Set dates for some tiles
        g.set_tile_date(2, Some("2023:01:15 10:00:00".into()));
        g.set_tile_date(0, Some("2023:06:20 14:00:00".into()));
        assert_eq!(g.sorted_count(), 2);

        // First two display positions should be sorted by date
        let order = g.display_order();
        assert_eq!(order[0], 2); // earlier date
        assert_eq!(order[1], 0); // later date

        // Remaining 3 tiles should be unsorted
        assert_eq!(g.tiles_needing_date_scan().len(), 3);
    }

    #[test]
    fn set_tile_no_date_moves_to_sorted() {
        let mut g = make_grid(3);
        g.set_sort_mode(SortMode::DateTaken);

        g.set_tile_date(1, Some("2023:01:01 00:00:00".into()));
        g.set_tile_no_date(2); // no EXIF date
        assert_eq!(g.sorted_count(), 2);

        // Tile 1 (with date) should come first, tile 2 (no date) second
        let order = g.display_order();
        assert_eq!(order[0], 1);
        assert_eq!(order[1], 2);
    }

    #[test]
    fn switch_back_to_name_restores_identity() {
        let mut g = make_grid(5);
        g.set_sort_mode(SortMode::DateTaken);
        g.set_tile_date(2, Some("2023:01:15 10:00:00".into()));

        // Switch back to name sort
        g.set_sort_mode(SortMode::Name);
        assert_eq!(g.display_order(), &[0, 1, 2, 3, 4]);
        assert_eq!(g.sorted_count(), 5); // all "sorted" in name mode
    }

    #[test]
    fn separator_row_exists_during_date_scan() {
        let mut g = make_grid(14); // 2 full rows of 7
        g.set_sort_mode(SortMode::DateTaken);

        // Set dates for first 7 tiles (one full row)
        for i in 0..7 {
            g.set_tile_date(i, Some(format!("2023:01:{:02} 00:00:00", i + 1)));
        }

        // Separator should be after row 0 (which contains the 7 sorted tiles)
        assert_eq!(g.separator_row(), Some(1));
        // Total rows: 1 (sorted) + 1 (separator) + 1 (unsorted) = 3
        assert_eq!(g.total_rows(), 3);
    }

    #[test]
    fn separator_disappears_when_scan_complete() {
        let mut g = make_grid(3);
        g.set_sort_mode(SortMode::DateTaken);

        for i in 0..3 {
            g.set_tile_date(i, Some(format!("2023:01:{:02} 00:00:00", i + 1)));
        }

        assert!(g.date_scan_complete());
        assert_eq!(g.separator_row(), None);
    }

    #[test]
    fn all_paths_respects_display_order() {
        let mut g = Grid::new(GridConfig {
            tile_width: 160.0,
            tile_height: 160.0,
            padding: 8.0,
        });
        g.add_tile_with_path("c.jpg".into());
        g.add_tile_with_path("a.jpg".into());
        g.add_tile_with_path("b.jpg".into());

        g.set_sort_mode(SortMode::DateTaken);
        g.set_tile_date(0, Some("2023:03:01 00:00:00".into())); // c.jpg
        g.set_tile_date(1, Some("2023:01:01 00:00:00".into())); // a.jpg
        g.set_tile_date(2, Some("2023:02:01 00:00:00".into())); // b.jpg

        let paths = g.all_paths();
        assert_eq!(paths[0].to_str().unwrap(), "a.jpg");
        assert_eq!(paths[1].to_str().unwrap(), "b.jpg");
        assert_eq!(paths[2].to_str().unwrap(), "c.jpg");
    }
}
