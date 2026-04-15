# Architecture

This document describes how iv's folder view works: the Grid data structure,
the tile state machine, the worker threading model, and the thumbnail
extraction pipeline.

## Overview

```
┌──────────────┐     ┌───────────────┐     ┌────────────┐     ┌──────────┐
│  Enumerator  │────▶│     Grid      │◀───▶│  GridView   │────▶│  egui    │
│  (bg thread) │     │ (pure data)   │     │ (workers +  │     │ renderer │
│              │     │               │     │  textures)  │     │          │
└──────────────┘     └───────────────┘     └────────────┘     └──────────┘
```

- **Enumerator**: Background thread that walks a directory and sends file
  paths via `mpsc::channel`. The UI thread polls `try_recv()` each frame.
- **Grid**: Pure data structure (no GPU, no threads, no I/O). Owns tile
  states, names, paths, layout math, and viewport tracking. Fully testable
  with 30 unit tests.
- **GridView**: Owns the Grid, GPU textures, and a pool of decode workers.
  Drives the tile state machine each frame. Renders only visible rows.
- **egui**: Immediate-mode GUI. GridView uses `ScrollArea` with manual row
  layout — only visible rows ± 2 buffer rows are rendered.

## Grid Data Structure

The Grid uses a **struct-of-vectors** layout for cache efficiency:

```rust
struct Grid {
    states: Vec<TileState>,   // Hot: scanned every frame for visible tiles
    names:  Vec<String>,      // Warm: read during rendering
    paths:  Vec<PathBuf>,     // Cold: read only when scheduling work
    // ...layout config, viewport, activity log
}
```

Tile state is a one-byte enum. Scanning visible states (typically ~35 tiles)
touches a contiguous 35-byte region — one cache line.

### Layout Math

All layout is computed from three values:
- `viewport.width` → `cols = floor((width + padding) / (tile_width + padding))`
- `tile_count` → `total_rows = ceil(tile_count / cols)`
- `viewport.scroll_y` → `visible_rows = floor(scroll_y / cell_height)..ceil((scroll_y + height) / cell_height)`

`visible_tile_range()` converts visible rows to a contiguous index range
`[first_row * cols, last_row * cols)`, clamped to `tile_count`. This is O(1).

### Dynamic Growth

Tiles are appended via `add_tile_with_path()` as the enumerator discovers
files. The grid grows dynamically — layout recomputes automatically since
`cols`, `total_rows`, and `content_height` are derived from `tile_count`.

Scroll position is clamped to `[0, content_height - viewport_height]` on
every `set_scroll()` call, so growing the grid never leaves the viewport
in an invalid state.

## Tile State Machine

Each tile progresses through these states:

```
                    ┌─────────────┐
                    │  NotLoaded   │
                    └──────┬──────┘
                           │ schedule_visible_work (Phase 1)
                    ┌──────▼──────┐
                    │LoadingEmbed │  ← worker: try_exif_only()
                    └──────┬──────┘
                      ┌────┴────┐
                      │         │
               embedded OK   embedded miss
                      │         │
               ┌──────▼──┐  ┌──▼───────────┐
               │ Loaded   │  │EmbeddedMissed│
               └──────────┘  └──────┬───────┘
                                    │ schedule_visible_work (Phase 2)
                             ┌──────▼──────────┐
                             │CreatingThumbnail │  ← worker: decode_from_bytes()
                             └──────┬───────────┘
                                    │
                             ┌──────▼──┐
                             │ Loaded   │
                             └──────────┘
```

### State Descriptions

| State | Meaning | Scheduled? |
|-------|---------|------------|
| `NotLoaded` | No work requested. Tile is a gray placeholder. | Yes → Phase 1 |
| `LoadingEmbedded` | Worker is extracting embedded thumbnail. | No (in-flight) |
| `EmbeddedMissed` | Embedded extraction completed but found nothing. | Yes → Phase 2 |
| `CreatingThumbnail` | Worker is doing full decode + downscale. | No (in-flight) |
| `Loaded` | Thumbnail is decoded and uploaded to GPU. | No (done) |

### Why `EmbeddedMissed` Exists

Without this state, tiles that failed embedded extraction went directly to
`CreatingThumbnail`. But `schedule_visible_work` queried
`visible_in_state(CreatingThumbnail)` and re-sent the same tile every frame
— producing duplicate work requests. The `EmbeddedMissed` state separates
"waiting to be scheduled" from "worker is processing", preventing duplicates.

## Two-Phase Scheduling

`schedule_visible_work()` runs once per frame inside the `show()` method.
It strictly prioritizes embedded extraction over full decode:

**Phase 1**: Find visible tiles in `NotLoaded` state. Send up to 12 as
`EmbeddedOnly` work requests. Transition each to `LoadingEmbedded`.
**Return early** — don't start Phase 2 until no visible `NotLoaded` tiles
remain.

**Phase 2**: Find visible tiles in `EmbeddedMissed` state. Send up to 12 as
`FullDecode` work requests. Transition each to `CreatingThumbnail`.

This ensures every visible tile shows *something* (even a low-res embedded
thumbnail) before any expensive full decodes begin. On a typical viewport
of 35 tiles where 80% have embedded thumbnails, all 28 will show within
one round-trip (~200ms on network), and only the remaining 7 need full
decode.

## Worker Threading Model

GridView spawns `N` decode workers where `N = available_parallelism - 2`
(minimum 2). Workers share an unbounded `crossbeam_channel` for work
requests and results.

```
                          ┌─────────────┐
  schedule_visible_work ──│  work_tx    │──▶ Worker 0
                          │ (unbounded) │──▶ Worker 1
                          │             │──▶ Worker 2
                          └─────────────┘──▶ Worker N
                                              │
                          ┌─────────────┐     │
  poll_results ◀──────────│  result_rx  │◀────┘
                          │ (unbounded) │
                          └─────────────┘
```

### Generation Counter

A shared `AtomicU64` generation counter invalidates stale work:

- When the user scrolls significantly (>2 cell heights), the generation
  increments and the work channel is drained.
- Workers check `req.generation < generation.load()` before starting I/O.
  Stale requests are skipped without doing any work.
- In-flight tiles (`LoadingEmbedded`, `CreatingThumbnail`) are reset to
  `NotLoaded` so they can be re-scheduled for the new viewport position.

### Result Processing

`poll_results()` runs at the start of each frame, processing up to 16
results. Each result triggers a texture upload (`ctx.load_texture`) and a
state transition. Results for tiles that have already been loaded (e.g.,
from a stale generation) are harmlessly applied — the texture is replaced
and the state stays `Loaded`.

## Thumbnail Extraction Pipeline

### Embedded Thumbnails (Phase 1)

`try_exif_only()` attempts fast thumbnail extraction by reading minimal
data from disk:

**JPEG / TIFF / RAW files**: Reads at most 256KB (`EXIF_READ_SIZE`). Parses
the EXIF IFD structure to find `JPEGInterchangeFormat` and
`JPEGInterchangeFormatLength` tags. Searches for JPEG SOI/EOI markers in
the indicated byte range and decodes the embedded JPEG with `zune-jpeg`.
This works for JPEG, TIFF, DNG, CR2, NEF, and ARW files — they all use
the same EXIF/TIFF IFD structure.

**HEIC / HEIF files**: Uses `libheif-rs` to open the file as an ISOBMFF
container, get the primary image handle, check `number_of_thumbnails()`,
and decode the first thumbnail via `LibHeif::decode()` in RGBA colorspace.
HEIC files store thumbnails as separate image items in the container (not
in EXIF tags), so the EXIF approach doesn't work for them.

**Typical timing**: 40–80ms on local SSD, 100–300ms on network (SMB).
The bottleneck is I/O latency, not decode time.

### Full Decode (Phase 2)

`decode_from_bytes()` reads the entire file, decodes it via the `image`
crate (which dispatches to format-specific decoders including `libheif-rs`
for HEIC), applies EXIF orientation, and downscales to 160×160 using
`image::imageops::thumbnail()`.

**Typical timing**: 100–600ms depending on format and I/O. HEIC full
decode is the slowest due to HEVC/AV1 decompression.

### EXIF Orientation

Both paths apply EXIF orientation after decoding. `read_exif_orientation()`
parses the orientation tag (values 1–8) and `apply_orientation()` performs
the corresponding rotation/mirror transform.

## Row-Based Virtualization

The render loop only creates egui widgets for visible rows:

```
┌─────────────────────────┐ ← allocate_space (skip height)
│   rows 0..render_first  │
├─────────────────────────┤
│   render_first          │ ← ui.horizontal() + render_tile() per tile
│   ...                   │
│   render_last - 1       │
├─────────────────────────┤
│   render_last..total    │ ← allocate_space (skip height)
└─────────────────────────┘
```

`render_first = visible_first - 2` (buffer above)
`render_last = visible_last + 2` (buffer below)

Off-screen rows are represented by a single `allocate_space()` call that
reserves the correct height. This makes per-frame rendering cost O(visible)
regardless of total tile count.

Vertical item spacing is set to 0 — row gaps are managed with explicit
`allocate_space(padding)` after each row to ensure pixel-perfect alignment
between skip regions and rendered rows.

## Activity Logging

When `--log` is passed on the command line, the Grid records every
significant event with microsecond timestamps:

- `TilesAdded` — batch additions from enumerator
- `StateChange` — every tile state transition
- `Scrolled` — significant scroll position changes
- `WorkScheduled` — batch of tiles sent to workers (with tier)
- `ResultReceived` — worker result (with timing in ms)
- `GenerationBump` — stale work invalidation

On exit, the log is written to `%TEMP%/iv_grid_log.json`. This can be
analyzed to diagnose scheduling issues, duplicate work, and timing
bottlenecks without visual feedback.
