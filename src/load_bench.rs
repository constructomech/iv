use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const FIXTURE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBenchFixture {
    pub schema_version: u32,
    pub grid: LoadBenchGrid,
    pub tiles: Vec<LoadBenchTile>,
    pub viewport_timeline: Vec<LoadBenchViewport>,
    #[serde(default)]
    pub first_paints: Vec<LoadBenchFirstPaint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBenchGrid {
    pub tile_width: f32,
    pub tile_height: f32,
    pub padding: f32,
    pub sort_mode: LoadBenchSortMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBenchSortMode {
    Name,
    DateTaken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBenchTile {
    pub path: PathBuf,
    pub display_pos: usize,
    pub media_kind: LoadBenchMediaKind,
    pub date_taken: Option<String>,
    pub live_video: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBenchMediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBenchViewport {
    pub time_us: u64,
    pub width: f32,
    pub height: f32,
    pub scroll_y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBenchFirstPaint {
    pub time_us: u64,
    pub display_pos: usize,
    pub visible_fraction: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GridActivityLogEntry {
    pub time_us: u64,
    pub event: GridActivityLogEvent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum GridActivityLogEvent {
    #[serde(rename = "viewport")]
    Viewport {
        width: f32,
        height: f32,
        scroll_y: f32,
        visible_first: usize,
        visible_last: usize,
        tile_width: f32,
        tile_height: f32,
        padding: f32,
    },
    #[serde(rename = "first_textured_paint")]
    FirstTexturedPaint {
        idx: usize,
        display_pos: usize,
        visible_fraction: f32,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadBenchScore {
    pub weighted_blank_ms: f64,
    pub first_visible_texture_ms: Option<f64>,
    pub fully_nonblank_ms: Option<f64>,
}

impl LoadBenchFixture {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != FIXTURE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported fixture schema version {}; expected {}",
                self.schema_version, FIXTURE_SCHEMA_VERSION
            ));
        }
        if self.tiles.is_empty() {
            return Err("fixture has no tiles".into());
        }
        if self.viewport_timeline.is_empty() {
            return Err("fixture has no viewport timeline".into());
        }
        Ok(())
    }
}

pub fn score_fixture(fixture: &LoadBenchFixture) -> LoadBenchScore {
    let mut score = LoadBenchScore::default();
    let mut previous = &fixture.viewport_timeline[0];
    let first_paint_times = first_paint_times(fixture);

    for viewport in fixture.viewport_timeline.iter().skip(1) {
        let visible = visible_display_positions(fixture, previous);
        for (display_pos, visible_fraction) in visible {
            let first_paint_us = first_paint_times.get(display_pos).and_then(|time| *time);
            if first_paint_us.is_some_and(|time| time <= previous.time_us) {
                continue;
            }
            let blank_until_us = first_paint_us
                .map(|time| time.min(viewport.time_us))
                .unwrap_or(viewport.time_us);
            let duration_ms = blank_until_us.saturating_sub(previous.time_us) as f64 / 1000.0;
            score.weighted_blank_ms += duration_ms * visible_fraction as f64;
        }
        previous = viewport;
    }

    score.first_visible_texture_ms = fixture
        .first_paints
        .iter()
        .map(|paint| paint.time_us as f64 / 1000.0)
        .min_by(f64::total_cmp);
    score.fully_nonblank_ms = fully_nonblank_ms(fixture, &first_paint_times);

    score
}

pub fn score_activity_log(entries: &[GridActivityLogEntry]) -> LoadBenchScore {
    let mut score = LoadBenchScore::default();
    let mut first_paint_times = Vec::new();

    for entry in entries {
        if let GridActivityLogEvent::FirstTexturedPaint { display_pos, .. } = entry.event {
            if first_paint_times.len() <= display_pos {
                first_paint_times.resize(display_pos + 1, None);
            }
            let slot = &mut first_paint_times[display_pos];
            if slot.is_none_or(|existing| entry.time_us < existing) {
                *slot = Some(entry.time_us);
            }
        }
    }

    let mut previous = None;
    let mut saw_visible_viewport = false;
    for entry in entries {
        let GridActivityLogEvent::Viewport {
            width,
            height,
            scroll_y,
            visible_first,
            visible_last,
            tile_width,
            tile_height,
            padding,
        } = entry.event
        else {
            continue;
        };
        if visible_first < visible_last {
            saw_visible_viewport = true;
        }
        let viewport = ActivityViewport {
            time_us: entry.time_us,
            width,
            height,
            scroll_y,
            visible_first,
            visible_last,
            tile_width,
            tile_height,
            padding,
        };
        if let Some(prev) = &previous {
            add_activity_interval_score(&mut score, prev, &viewport, &first_paint_times);
        }
        previous = Some(viewport);
    }

    score.first_visible_texture_ms = entries
        .iter()
        .filter_map(|entry| match entry.event {
            GridActivityLogEvent::FirstTexturedPaint { .. } => Some(entry.time_us as f64 / 1000.0),
            _ => None,
        })
        .min_by(f64::total_cmp);
    if saw_visible_viewport {
        score.fully_nonblank_ms = activity_fully_nonblank_ms(entries, &first_paint_times);
    }

    score
}

#[derive(Debug, Clone)]
struct ActivityViewport {
    time_us: u64,
    width: f32,
    height: f32,
    scroll_y: f32,
    visible_first: usize,
    visible_last: usize,
    tile_width: f32,
    tile_height: f32,
    padding: f32,
}

fn add_activity_interval_score(
    score: &mut LoadBenchScore,
    previous: &ActivityViewport,
    next: &ActivityViewport,
    first_paint_times: &[Option<u64>],
) {
    for display_pos in previous.visible_first..previous.visible_last {
        let visible_fraction = activity_visible_fraction(previous, display_pos);
        if visible_fraction <= 0.0 {
            continue;
        }
        let first_paint_us = first_paint_times.get(display_pos).and_then(|time| *time);
        if first_paint_us.is_some_and(|time| time <= previous.time_us) {
            continue;
        }
        let blank_until_us = first_paint_us
            .map(|time| time.min(next.time_us))
            .unwrap_or(next.time_us);
        let duration_ms = blank_until_us.saturating_sub(previous.time_us) as f64 / 1000.0;
        score.weighted_blank_ms += duration_ms * visible_fraction as f64;
    }
}

fn activity_fully_nonblank_ms(
    entries: &[GridActivityLogEntry],
    first_paint_times: &[Option<u64>],
) -> Option<f64> {
    let mut latest = None;
    for entry in entries {
        let GridActivityLogEvent::Viewport {
            width,
            height,
            scroll_y,
            visible_first,
            visible_last,
            tile_width,
            tile_height,
            padding,
        } = entry.event
        else {
            continue;
        };
        let viewport = ActivityViewport {
            time_us: entry.time_us,
            width,
            height,
            scroll_y,
            visible_first,
            visible_last,
            tile_width,
            tile_height,
            padding,
        };
        for display_pos in visible_first..visible_last {
            if activity_visible_fraction(&viewport, display_pos) <= 0.0 {
                continue;
            }
            let time_us = first_paint_times.get(display_pos).copied().flatten()?;
            latest = Some(latest.map_or(time_us, |current: u64| current.max(time_us)));
        }
    }
    latest.map(|time_us| time_us as f64 / 1000.0)
}

fn activity_visible_fraction(viewport: &ActivityViewport, display_pos: usize) -> f32 {
    let cols = ((viewport.width + viewport.padding) / (viewport.tile_width + viewport.padding))
        .floor()
        .max(1.0) as usize;
    let row = display_pos / cols;
    let cell_top = row as f32 * (viewport.tile_height + viewport.padding);
    let cell_bottom = cell_top + viewport.tile_height;
    let shown =
        cell_bottom.min(viewport.scroll_y + viewport.height) - cell_top.max(viewport.scroll_y);
    (shown / viewport.tile_height).clamp(0.0, 1.0)
}

fn first_paint_times(fixture: &LoadBenchFixture) -> Vec<Option<u64>> {
    let mut times = vec![None; fixture.tiles.len()];
    for paint in &fixture.first_paints {
        if let Some(slot) = times.get_mut(paint.display_pos) {
            if slot.is_none_or(|existing| paint.time_us < existing) {
                *slot = Some(paint.time_us);
            }
        }
    }
    times
}

fn fully_nonblank_ms(fixture: &LoadBenchFixture, first_paint_times: &[Option<u64>]) -> Option<f64> {
    let mut latest = None;
    for viewport in &fixture.viewport_timeline {
        for (display_pos, _) in visible_display_positions(fixture, viewport) {
            let Some(time_us) = first_paint_times.get(display_pos).copied().flatten() else {
                return None;
            };
            latest = Some(latest.map_or(time_us, |current: u64| current.max(time_us)));
        }
    }
    latest.map(|time_us| time_us as f64 / 1000.0)
}

fn visible_display_positions(
    fixture: &LoadBenchFixture,
    viewport: &LoadBenchViewport,
) -> Vec<(usize, f32)> {
    let cols = ((viewport.width + fixture.grid.padding)
        / (fixture.grid.tile_width + fixture.grid.padding))
        .floor()
        .max(1.0) as usize;
    let cell_height = fixture.grid.tile_height + fixture.grid.padding;
    let first_row = (viewport.scroll_y / cell_height).floor().max(0.0) as usize;
    let last_row = ((viewport.scroll_y + viewport.height) / cell_height).ceil() as usize;
    let mut visible = Vec::new();

    for row in first_row..last_row {
        let cell_top = row as f32 * cell_height;
        let cell_bottom = cell_top + fixture.grid.tile_height;
        let shown =
            cell_bottom.min(viewport.scroll_y + viewport.height) - cell_top.max(viewport.scroll_y);
        let visible_fraction = (shown / fixture.grid.tile_height).clamp(0.0, 1.0);
        if visible_fraction <= 0.0 {
            continue;
        }
        for col in 0..cols {
            let display_pos = row * cols + col;
            if display_pos >= fixture.tiles.len() {
                break;
            }
            visible.push((display_pos, visible_fraction));
        }
    }

    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_counts_visible_blank_tile_time() {
        let fixture = LoadBenchFixture {
            schema_version: FIXTURE_SCHEMA_VERSION,
            grid: LoadBenchGrid {
                tile_width: 275.0,
                tile_height: 275.0,
                padding: 8.0,
                sort_mode: LoadBenchSortMode::Name,
            },
            tiles: vec![LoadBenchTile {
                path: PathBuf::from("a.jpg"),
                display_pos: 0,
                media_kind: LoadBenchMediaKind::Image,
                date_taken: None,
                live_video: None,
            }],
            viewport_timeline: vec![
                LoadBenchViewport {
                    time_us: 0,
                    width: 600.0,
                    height: 300.0,
                    scroll_y: 0.0,
                },
                LoadBenchViewport {
                    time_us: 10_000,
                    width: 600.0,
                    height: 300.0,
                    scroll_y: 0.0,
                },
            ],
            first_paints: Vec::new(),
        };

        assert_eq!(score_fixture(&fixture).weighted_blank_ms, 10.0);
    }

    #[test]
    fn score_stops_counting_after_first_paint() {
        let mut fixture = one_tile_fixture();
        fixture.viewport_timeline.push(LoadBenchViewport {
            time_us: 20_000,
            width: 600.0,
            height: 300.0,
            scroll_y: 0.0,
        });
        fixture.first_paints.push(LoadBenchFirstPaint {
            time_us: 12_000,
            display_pos: 0,
            visible_fraction: 1.0,
        });

        let score = score_fixture(&fixture);
        assert_eq!(score.weighted_blank_ms, 12.0);
        assert_eq!(score.first_visible_texture_ms, Some(12.0));
        assert_eq!(score.fully_nonblank_ms, Some(12.0));
    }

    #[test]
    fn incomplete_paints_are_not_fully_nonblank() {
        let mut fixture = one_tile_fixture();
        fixture.viewport_timeline.push(LoadBenchViewport {
            time_us: 20_000,
            width: 600.0,
            height: 300.0,
            scroll_y: 0.0,
        });

        assert_eq!(score_fixture(&fixture).fully_nonblank_ms, None);
    }

    #[test]
    fn activity_log_score_uses_first_paint_events() {
        let entries = vec![
            GridActivityLogEntry {
                time_us: 0,
                event: GridActivityLogEvent::Viewport {
                    width: 600.0,
                    height: 300.0,
                    scroll_y: 0.0,
                    visible_first: 0,
                    visible_last: 1,
                    tile_width: 275.0,
                    tile_height: 275.0,
                    padding: 8.0,
                },
            },
            GridActivityLogEntry {
                time_us: 12_000,
                event: GridActivityLogEvent::FirstTexturedPaint {
                    idx: 42,
                    display_pos: 0,
                    visible_fraction: 1.0,
                },
            },
            GridActivityLogEntry {
                time_us: 20_000,
                event: GridActivityLogEvent::Viewport {
                    width: 600.0,
                    height: 300.0,
                    scroll_y: 0.0,
                    visible_first: 0,
                    visible_last: 1,
                    tile_width: 275.0,
                    tile_height: 275.0,
                    padding: 8.0,
                },
            },
        ];

        let score = score_activity_log(&entries);
        assert_eq!(score.weighted_blank_ms, 12.0);
        assert_eq!(score.first_visible_texture_ms, Some(12.0));
        assert_eq!(score.fully_nonblank_ms, Some(12.0));
    }

    #[test]
    fn score_weights_partially_visible_tiles() {
        let mut fixture = one_tile_fixture();
        fixture.viewport_timeline[0].scroll_y = 100.0;
        fixture.viewport_timeline[0].height = 237.5;
        fixture.viewport_timeline.push(LoadBenchViewport {
            time_us: 10_000,
            width: 600.0,
            height: 237.5,
            scroll_y: 100.0,
        });

        assert!((score_fixture(&fixture).weighted_blank_ms - 6.363_636).abs() < 0.001);
    }

    fn one_tile_fixture() -> LoadBenchFixture {
        LoadBenchFixture {
            schema_version: FIXTURE_SCHEMA_VERSION,
            grid: LoadBenchGrid {
                tile_width: 275.0,
                tile_height: 275.0,
                padding: 8.0,
                sort_mode: LoadBenchSortMode::Name,
            },
            tiles: vec![LoadBenchTile {
                path: PathBuf::from("a.jpg"),
                display_pos: 0,
                media_kind: LoadBenchMediaKind::Image,
                date_taken: None,
                live_video: None,
            }],
            viewport_timeline: vec![LoadBenchViewport {
                time_us: 0,
                width: 600.0,
                height: 300.0,
                scroll_y: 0.0,
            }],
            first_paints: Vec::new(),
        }
    }
}
