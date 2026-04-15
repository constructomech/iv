//! Grid — a testable data structure for a scrollable tile grid.
//!
//! Owns layout math, viewport tracking, tile state, and visible-range
//! computation. No GPU, no I/O, no egui dependency.

/// State of a single tile in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileState {
    /// Not yet loaded — no work has been requested.
    NotLoaded,
    /// Extracting embedded thumbnail (EXIF/BMFF).
    LoadingEmbedded,
    /// Full decode + downscale in progress.
    CreatingThumbnail,
    /// Thumbnail is ready for display.
    Loaded,
}

impl std::fmt::Display for TileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TileState::NotLoaded => write!(f, "not loaded"),
            TileState::LoadingEmbedded => write!(f, "loading…"),
            TileState::CreatingThumbnail => write!(f, "creating…"),
            TileState::Loaded => write!(f, "loaded"),
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
            tile_width: 160.0,
            tile_height: 160.0,
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

/// The scrollable tile grid.
///
/// Pure data structure — no I/O, no GPU, fully testable.
pub struct Grid {
    config: GridConfig,
    viewport: Viewport,
    states: Vec<TileState>,
    names: Vec<String>,
}

impl Grid {
    /// Create a new empty grid with the given configuration.
    pub fn new(config: GridConfig) -> Self {
        Self {
            config,
            viewport: Viewport::default(),
            states: Vec::new(),
            names: Vec::new(),
        }
    }

    // -- Tile management ---------------------------------------------------

    /// Add a named tile to the grid. Returns its index.
    pub fn add_tile(&mut self, name: impl Into<String>) -> usize {
        let idx = self.states.len();
        self.states.push(TileState::NotLoaded);
        self.names.push(name.into());
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
        self.states[idx] = state;
    }

    /// Get the name of a tile.
    pub fn tile_name(&self, idx: usize) -> &str {
        &self.names[idx]
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
    pub fn total_rows(&self) -> usize {
        let cols = self.cols();
        if cols == 0 {
            return 0;
        }
        self.states.len().div_ceil(cols)
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
        self.viewport.scroll_y = scroll_y.clamp(0.0, max_scroll);
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

    /// Compute the range of tile indices that are visible [start, end).
    pub fn visible_tile_range(&self) -> (usize, usize) {
        let vr = self.visible_rows();
        let cols = self.cols();
        let start = vr.first * cols;
        let end = (vr.last * cols).min(self.states.len());
        (start, end)
    }

    /// Iterate over visible tile indices.
    pub fn visible_tiles(&self) -> impl Iterator<Item = usize> {
        let (start, end) = self.visible_tile_range();
        start..end
    }

    /// Iterate over visible tiles, yielding (index, name, state) for each.
    pub fn visible_tile_info(&self) -> impl Iterator<Item = (usize, &str, TileState)> {
        let (start, end) = self.visible_tile_range();
        (start..end).map(move |idx| (idx, self.names[idx].as_str(), self.states[idx]))
    }

    // -- Config access -----------------------------------------------------

    pub fn config(&self) -> &GridConfig {
        &self.config
    }

    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_grid(tile_count: usize) -> Grid {
        let mut g = Grid::new(GridConfig::default());
        g.set_viewport_size(1200.0, 800.0);
        for i in 0..tile_count {
            g.add_tile(format!("img_{i:05}.jpg"));
        }
        g
    }

    // -- Layout tests ------------------------------------------------------

    #[test]
    fn cols_from_viewport_width() {
        let mut g = Grid::new(GridConfig::default());
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
        assert_eq!(TileState::CreatingThumbnail.to_string(), "creating…");
        assert_eq!(TileState::Loaded.to_string(), "loaded");
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
        let tiles: Vec<usize> = g.visible_tiles().collect();
        assert_eq!(tiles, (0..10).collect::<Vec<_>>());
    }

    // -- Dynamic growth tests ----------------------------------------------

    #[test]
    fn grid_grows_dynamically() {
        let mut g = Grid::new(GridConfig::default());
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

            let (start, end) = g.visible_tile_range();
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

        let info: Vec<_> = g.visible_tile_info().collect();
        assert_eq!(info.len(), 3);
        assert_eq!(info[0], (0, "a.jpg", TileState::NotLoaded));
        assert_eq!(info[1], (1, "b.png", TileState::Loaded));
        assert_eq!(info[2], (2, "c.heic", TileState::NotLoaded));
    }

    #[test]
    fn visible_tile_info_only_visible() {
        let mut g = make_grid(100); // 15 rows, viewport shows 5
        g.set_scroll(336.0); // rows 2..7 visible

        let info: Vec<_> = g.visible_tile_info().collect();
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
            let tiles: Vec<_> = g.visible_tile_info().collect();
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
            let visible: Vec<_> = g.visible_tile_info().collect();
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
}
