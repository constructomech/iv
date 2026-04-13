use std::path::PathBuf;

use crate::decode::DecodeTimings;

/// Number of rows to buffer above/below the viewport for prefetching.
const BUFFER_ROWS: usize = 3;

/// Thumbnail loading state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbState {
    /// Not yet queued for loading.
    Pending,
    /// Queued or in-progress on a worker thread.
    Loading,
    /// Successfully decoded and uploaded (or ready to upload).
    Loaded,
    /// Failed to decode.
    Failed,
}

/// Per-image scheduling state. No GPU types — purely logical.
#[derive(Debug)]
pub struct EntryState {
    pub path: PathBuf,
    pub state: ThumbState,
    /// True if the current thumbnail came from EXIF (lower quality).
    pub is_exif_quality: bool,
    /// Decode timing data (populated after load completes).
    pub timings: Option<DecodeTimings>,
}

/// A work item to be sent to a decode worker.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub idx: usize,
    pub path: PathBuf,
    pub generation: u64,
}

/// Pure scheduling logic for thumbnail loading, separated from GPU/UI concerns.
///
/// Tracks which tiles are visible, which need loading, and in what priority order.
/// Has no dependency on egui or any GPU context.
pub struct Scheduler {
    entries: Vec<EntryState>,
    /// Current visible tile range [start, end).
    visible_range: (usize, usize),
    /// Number of columns in the grid.
    cols: usize,
    /// Generation counter — incremented on significant scroll.
    generation: u64,
    /// Last scroll offset used for change detection.
    last_scroll_y: f32,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            visible_range: (0, 0),
            cols: 1,
            generation: 0,
            last_scroll_y: 0.0,
        }
    }

    /// Add a newly discovered image entry. Returns its index.
    pub fn add_entry(&mut self, path: PathBuf) -> usize {
        let idx = self.entries.len();
        self.entries.push(EntryState {
            path,
            state: ThumbState::Pending,
            is_exif_quality: false,
            timings: None,
        });
        idx
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Access an entry by index.
    pub fn entry(&self, idx: usize) -> &EntryState {
        &self.entries[idx]
    }

    /// Get the current visible range [start, end).
    #[allow(dead_code)] // Used in tests
    pub fn visible_range(&self) -> (usize, usize) {
        self.visible_range
    }

    /// Get the current generation counter.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Update visibility based on scroll position and viewport size.
    /// `cell_height` is the height of one row of tiles (tile + padding + label).
    /// Returns true if a generation bump occurred (callers should notify workers).
    pub fn update_visibility(
        &mut self,
        scroll_y: f32,
        viewport_height: f32,
        cols: usize,
        cell_height: f32,
    ) -> bool {
        self.cols = cols.max(1);

        let row_height = cell_height;
        let first_visible_row = (scroll_y / row_height).floor().max(0.0) as usize;
        let visible_rows = (viewport_height / row_height).ceil() as usize + 1;

        let start = first_visible_row * self.cols;
        let end = ((first_visible_row + visible_rows) * self.cols).min(self.entries.len());

        let old_range = self.visible_range;
        self.visible_range = (start, end);

        let mut bumped = false;

        // If scroll changed significantly, bump generation and reset Loading→Pending
        if old_range != self.visible_range && (scroll_y - self.last_scroll_y).abs() > row_height {
            self.generation += 1;
            self.last_scroll_y = scroll_y;

            // Reset in-flight tiles to Pending so they can be re-scheduled
            for entry in &mut self.entries {
                if entry.state == ThumbState::Loading {
                    entry.state = ThumbState::Pending;
                }
            }
            bumped = true;
        }

        bumped
    }

    /// Get the next batch of work items to send to decode workers.
    /// Returns Pending tiles in the visible + buffer range, sorted by
    /// distance from viewport center (closest first).
    /// Transitions returned tiles from Pending → Loading.
    pub fn get_work_batch(&mut self) -> Vec<WorkItem> {
        let (vis_start, vis_end) = self.visible_range;
        if vis_end == 0 && self.entries.is_empty() {
            return Vec::new();
        }

        // Expand range by buffer rows
        let buffer_tiles = BUFFER_ROWS * self.cols;
        let load_start = vis_start.saturating_sub(buffer_tiles);
        let load_end = (vis_end + buffer_tiles).min(self.entries.len());

        // Compute viewport center tile index for priority sorting
        let center = if vis_start < vis_end {
            (vis_start + vis_end) / 2
        } else {
            0
        };

        // Collect pending tiles with their priority (distance from center)
        let mut pending: Vec<(usize, usize)> = Vec::new();
        for idx in load_start..load_end {
            if self.entries[idx].state == ThumbState::Pending {
                let distance = if idx > center {
                    idx - center
                } else {
                    center - idx
                };
                pending.push((idx, distance));
            }
        }

        // Sort by distance from center (closest first)
        pending.sort_by_key(|&(_, distance)| distance);

        // Build work items and transition to Loading
        let current_gen = self.generation;
        pending
            .into_iter()
            .map(|(idx, _)| {
                self.entries[idx].state = ThumbState::Loading;
                WorkItem {
                    idx,
                    path: self.entries[idx].path.clone(),
                    generation: current_gen,
                }
            })
            .collect()
    }

    /// Mark a tile as successfully loaded.
    /// Always accepts results — even from an older generation,
    /// because the decoded pixels are valid regardless.
    pub fn complete(&mut self, idx: usize, is_exif: bool, timings: DecodeTimings) {
        if idx < self.entries.len() {
            self.entries[idx].state = ThumbState::Loaded;
            self.entries[idx].is_exif_quality = is_exif;
            self.entries[idx].timings = Some(timings);
        }
    }

    /// Mark a tile as failed to decode.
    pub fn fail(&mut self, idx: usize) {
        if idx < self.entries.len() {
            self.entries[idx].state = ThumbState::Failed;
        }
    }

    /// Check whether a worker should skip a work item (stale generation).
    #[allow(dead_code)] // Used in tests
    pub fn should_skip(&self, work_generation: u64) -> bool {
        work_generation < self.generation
    }

    /// Count of tiles in a given state.
    pub fn count_in_state(&self, state: ThumbState) -> usize {
        self.entries.iter().filter(|e| e.state == state).count()
    }

    /// Returns true if any tiles are still Pending or Loading.
    pub fn has_pending_work(&self) -> bool {
        self.entries
            .iter()
            .any(|e| matches!(e.state, ThumbState::Pending | ThumbState::Loading))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> DecodeTimings {
        DecodeTimings::default()
    }

    fn make_scheduler(n: usize) -> Scheduler {
        let mut s = Scheduler::new();
        for i in 0..n {
            s.add_entry(PathBuf::from(format!("img_{i:04}.jpg")));
        }
        s
    }

    const CELL_H: f32 = 188.0; // TILE_SIZE(160) + TILE_PADDING(8) + LABEL_HEIGHT(20)

    // -----------------------------------------------------------------------
    // Basic entry management
    // -----------------------------------------------------------------------

    #[test]
    fn add_entries() {
        let mut s = Scheduler::new();
        assert_eq!(s.len(), 0);

        let idx = s.add_entry(PathBuf::from("a.jpg"));
        assert_eq!(idx, 0);
        assert_eq!(s.len(), 1);
        assert_eq!(s.entry(0).state, ThumbState::Pending);

        let idx2 = s.add_entry(PathBuf::from("b.jpg"));
        assert_eq!(idx2, 1);
        assert_eq!(s.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Visibility + scheduling
    // -----------------------------------------------------------------------

    #[test]
    fn initial_visibility_queues_visible_tiles() {
        let mut s = make_scheduler(100);
        // Viewport shows ~3 rows of 5 cols = 15 tiles
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();

        // Should include visible tiles (0..20) + buffer (3 rows * 5 cols = 15 more)
        assert!(!batch.is_empty());

        // All visible tiles should be in the batch
        let visible_end = s.visible_range().1;
        for idx in 0..visible_end {
            assert!(
                batch.iter().any(|w| w.idx == idx),
                "visible tile {idx} should be in work batch"
            );
        }
    }

    #[test]
    fn priority_order_center_outward() {
        let mut s = make_scheduler(100);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();

        // Compute center from the visible + buffer range
        let (vis_start, vis_end) = s.visible_range();
        let center = (vis_start + vis_end) / 2;

        // Each item's distance from center should be non-decreasing
        let mut prev_dist = 0;
        for item in &batch {
            let dist = if item.idx > center {
                item.idx - center
            } else {
                center - item.idx
            };
            assert!(
                dist >= prev_dist,
                "work items should be ordered by distance from center: idx={} dist={} prev_dist={}",
                item.idx,
                dist,
                prev_dist
            );
            prev_dist = dist;
        }
    }

    #[test]
    fn only_pending_tiles_queued() {
        let mut s = make_scheduler(20);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);

        // First batch queues some tiles
        let batch1 = s.get_work_batch();
        assert!(!batch1.is_empty());

        // All returned tiles should now be Loading
        for item in &batch1 {
            assert_eq!(s.entry(item.idx).state, ThumbState::Loading);
        }

        // Second batch should return nothing (no more Pending in range)
        let batch2 = s.get_work_batch();
        assert!(batch2.is_empty());
    }

    #[test]
    fn large_folder_only_loads_visible_buffer() {
        let mut s = make_scheduler(10_000);
        // 5 cols, viewport shows 3 rows = 15 tiles
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();

        // Should load visible (15) + buffer above (0) + buffer below (15) = ~30 max
        let max_expected = (3 + BUFFER_ROWS * 2) * 5;
        assert!(
            batch.len() <= max_expected,
            "should only load visible+buffer, got {} (max {max_expected})",
            batch.len()
        );
        assert!(batch.len() > 0);
    }

    // -----------------------------------------------------------------------
    // Scroll + generation
    // -----------------------------------------------------------------------

    #[test]
    fn scroll_resets_loading_to_pending() {
        let mut s = make_scheduler(100);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();
        let loading_count = batch.len();
        assert!(loading_count > 0);

        // Scroll far down — should bump generation and reset Loading→Pending
        let bumped = s.update_visibility(CELL_H * 10.0, CELL_H * 3.0, 5, CELL_H);
        assert!(bumped, "big scroll should bump generation");
        assert_eq!(s.generation(), 1);

        // Previously Loading tiles should be back to Pending
        assert_eq!(s.count_in_state(ThumbState::Loading), 0);
        assert!(s.count_in_state(ThumbState::Pending) >= loading_count);
    }

    #[test]
    fn scroll_reschedules_new_visible() {
        let mut s = make_scheduler(100);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let _batch1 = s.get_work_batch();

        // Scroll to show tiles ~50-65
        s.update_visibility(CELL_H * 10.0, CELL_H * 3.0, 5, CELL_H);
        let batch2 = s.get_work_batch();

        // New batch should contain tiles near the new viewport
        assert!(!batch2.is_empty());
        let new_center = (50 + 65) / 2;
        let avg_idx: usize = batch2.iter().map(|w| w.idx).sum::<usize>() / batch2.len();
        let distance = if avg_idx > new_center {
            avg_idx - new_center
        } else {
            new_center - avg_idx
        };
        assert!(
            distance < 20,
            "new work should be near new viewport, avg_idx={avg_idx} expected near {new_center}"
        );
    }

    #[test]
    fn small_scroll_does_not_bump_generation() {
        let mut s = make_scheduler(100);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let _batch = s.get_work_batch();

        // Small scroll within one row height — should NOT bump
        let bumped = s.update_visibility(CELL_H * 0.5, CELL_H * 3.0, 5, CELL_H);
        assert!(!bumped, "small scroll should not bump generation");
        assert_eq!(s.generation(), 0);
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    #[test]
    fn completed_results_always_accepted() {
        let mut s = make_scheduler(100);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();
        let first_idx = batch[0].idx;
        let first_gen = batch[0].generation;

        // Scroll away — bumps generation
        s.update_visibility(CELL_H * 10.0, CELL_H * 3.0, 5, CELL_H);
        assert!(s.generation() > first_gen);

        // Complete with OLD generation — should still be accepted
        s.complete(first_idx, false, t());
        assert_eq!(
            s.entry(first_idx).state,
            ThumbState::Loaded,
            "completed results should always be accepted, even from old generation"
        );
    }

    #[test]
    fn loaded_not_requeued() {
        let mut s = make_scheduler(20);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();

        // Complete a few
        for item in batch.iter().take(5) {
            s.complete(item.idx, false, t());
        }

        // Scroll away and back
        s.update_visibility(CELL_H * 10.0, CELL_H * 3.0, 5, CELL_H);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);

        let batch2 = s.get_work_batch();

        // The 5 completed tiles should NOT appear in the new batch
        let completed_in_batch: Vec<_> = batch2
            .iter()
            .filter(|w| batch.iter().take(5).any(|b| b.idx == w.idx))
            .collect();
        assert!(
            completed_in_batch.is_empty(),
            "loaded tiles should not be re-queued"
        );
    }

    #[test]
    fn failed_not_requeued() {
        let mut s = make_scheduler(20);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();

        // Fail tile 0
        s.fail(batch[0].idx);
        assert_eq!(s.entry(batch[0].idx).state, ThumbState::Failed);

        // Scroll away and back
        s.update_visibility(CELL_H * 10.0, CELL_H * 3.0, 5, CELL_H);
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);

        let batch2 = s.get_work_batch();
        let has_failed = batch2.iter().any(|w| w.idx == batch[0].idx);
        assert!(!has_failed, "failed tiles should not be re-queued");
    }

    #[test]
    fn scroll_back_loads_previously_skipped() {
        let mut s = make_scheduler(100);
        // View page 1
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let _batch1 = s.get_work_batch();

        // Scroll to page 3 — resets page 1 loading
        s.update_visibility(CELL_H * 6.0, CELL_H * 3.0, 5, CELL_H);
        let _batch2 = s.get_work_batch();

        // Scroll back to page 1 — resets page 3 loading
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch3 = s.get_work_batch();

        // Page 1 tiles should be in the new batch (they were reset to Pending)
        assert!(!batch3.is_empty(), "should re-schedule page 1 tiles");
        assert!(
            batch3.iter().any(|w| w.idx < 15),
            "should include page 1 tiles"
        );
    }

    // -----------------------------------------------------------------------
    // Worker skip logic
    // -----------------------------------------------------------------------

    #[test]
    fn should_skip_stale_generation() {
        let mut s = make_scheduler(10);
        assert!(!s.should_skip(0));

        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let _batch = s.get_work_batch();

        // Bump generation
        s.update_visibility(CELL_H * 10.0, CELL_H * 3.0, 5, CELL_H);
        assert!(s.should_skip(0), "generation 0 should be stale after bump");
        assert!(
            !s.should_skip(s.generation()),
            "current gen should not be stale"
        );
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_scheduler() {
        let mut s = Scheduler::new();
        s.update_visibility(0.0, 600.0, 5, CELL_H);
        let batch = s.get_work_batch();
        assert!(batch.is_empty());
        assert!(!s.has_pending_work());
    }

    #[test]
    fn entries_arriving_after_visibility_set() {
        let mut s = Scheduler::new();
        // Set visibility before any entries
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch1 = s.get_work_batch();
        assert!(batch1.is_empty());

        // Now entries arrive
        for i in 0..20 {
            s.add_entry(PathBuf::from(format!("img_{i}.jpg")));
        }

        // Re-run visibility (same scroll position) — should pick up new entries
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch2 = s.get_work_batch();
        assert!(
            !batch2.is_empty(),
            "new entries in visible range should be scheduled"
        );
    }

    #[test]
    fn has_pending_work_tracks_correctly() {
        let mut s = make_scheduler(5);
        assert!(s.has_pending_work());

        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let batch = s.get_work_batch();
        assert!(s.has_pending_work()); // Now Loading

        for item in &batch {
            s.complete(item.idx, false, t());
        }
        assert!(!s.has_pending_work()); // All done
    }

    #[test]
    fn complete_and_fail_with_out_of_bounds_idx() {
        let mut s = make_scheduler(5);
        // Should not panic
        s.complete(999, false, t());
        s.fail(999);
    }

    #[test]
    fn resize_columns_adjusts_visibility() {
        let mut s = make_scheduler(100);

        // 5 columns: visible = 15 tiles (3 rows)
        s.update_visibility(0.0, CELL_H * 3.0, 5, CELL_H);
        let (_, end5) = s.visible_range();

        // 3 columns: visible = 9 tiles (3 rows)
        s.update_visibility(0.0, CELL_H * 3.0, 3, CELL_H);
        let (_, end3) = s.visible_range();

        assert!(end5 > end3, "fewer columns should mean fewer visible tiles");
    }
}
