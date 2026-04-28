# Architecture

This document describes how iv's folder view works: the Grid data structure,
the tile state machine, the worker threading model, and the thumbnail
extraction pipeline.

## Overview

```
┌──────────────┐    ┌───────────────┐     ┌────────────┐     ┌──────────┐
│  Enumerator  │───>│     Grid      │<───>│  GridView  │────>│  egui    │
│  (bg thread) │    │ (pure data)   │     │ (workers + │     │ renderer │
│              │    │               │     │  textures) │     │          │
└──────────────┘    └───────────────┘     └────────────┘     └──────────┘
```

- **Enumerator**: Background thread that walks a directory and sends file
  paths via `mpsc::channel`. The UI thread polls `try_recv()` each frame.
- **Grid**: Pure data structure (no GPU, no threads, no I/O). Owns tile
  states, names, paths, layout math, and viewport tracking. Fully testable
  with 30 unit tests.
- **GridView**: Owns the Grid, GPU textures, and a pool of decode workers.
  Drives the tile state machine each frame. Renders only visible rows.
- **FolderTree**: In-memory, lazy folder browser for the left pane. It does
  not persist a catalog; expanding a folder reads only that folder's immediate
  child directories on a background thread.
- **egui**: Immediate-mode GUI. GridView uses `ScrollArea` with manual row
  layout — only visible rows ± 2 buffer rows are rendered.

## Grid Data Structure

The Grid uses a **struct-of-vectors** layout for cache efficiency:

```rust
struct Grid {
    states: Vec<TileState>,   // Hot: scanned every frame for visible tiles
    names:  Vec<String>,      // Warm: read during rendering
    paths:  Vec<PathBuf>,     // Cold: read only when scheduling work
    dates:  Vec<Option<String>>, // EXIF date-taken, populated by date scan
    display_order: Vec<usize>,   // Maps display position → tile index
    sorted_count: usize,         // How many display_order entries are sorted
    sort_mode: SortMode,         // Name or DateTaken
    // ...layout config, viewport, activity log
}
```

Tile state is a one-byte enum. Scanning visible states (typically ~35 tiles)
touches a contiguous 35-byte region — one cache line.

### Display Order and Sort Modes

The `display_order` vector maps display position → tile index, decoupling
storage order from display order. Two sort modes are supported:

- **Name** (default): Identity mapping `[0, 1, 2, …]`. Tiles arrive from
  NTFS's B+ tree in alphabetical order, so no sorting is needed.
- **DateTaken**: Tiles with known EXIF dates are sorted by date in the
  first `sorted_count` positions. Tiles whose dates are not yet scanned
  remain in the suffix. A visual separator row divides the two sections.

When a tile's date arrives via `set_tile_date()`, it is binary-search
inserted into the sorted prefix. Tiles without EXIF dates (PNG, BMP, etc.)
are moved to the end of the sorted prefix via `set_tile_no_date()`.
Switching back to Name mode rebuilds the identity mapping.

All methods that return visible tile information (`visible_in_state()`,
`visible_tiles()`, `visible_tile_info()`, `all_paths()`) go through
`display_order`, so scheduling and rendering automatically respect the
current sort.

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
                    │  NotLoaded  │
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
               │ Loaded  │  │EmbeddedMissed│
               └────┬────┘  └──────┬───────┘
                    │              │ schedule_visible_work (Phase 2)
                    │       ┌──────▼───────────┐
                    │       │CreatingThumbnail │  ← worker: decode_from_bytes()
                    │       └──────┬───────────┘
                    │              │
                    │       ┌──────▼───┐
                    └──────>│ Loaded   │
                            └────┬─────┘
                                 │ schedule_upscales (Phase 3)
                                 │ (only if decoded size < tile display size)
                            ┌────▼────────┐
                            │  Upscaling  │  ← worker: decode_for_upscale()
                            └────┬────────┘
                                 │
                            ┌────▼─────┐
                            │ Loaded   │  (higher-res texture replaces old)
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

## Five-Phase Scheduling

`schedule_visible_work()` runs once per frame inside the `show()` method.
It strictly prioritizes phases in order, using a frame-time deadline to
avoid jank:

**Phase 1**: Find visible tiles in `NotLoaded` state. Send as
`EmbeddedOnly` work requests for images and `VideoThumbnail` work requests for
videos. Transition each to `LoadingEmbedded`. **Return early** — don't start
Phase 2 until no visible `NotLoaded` tiles remain.

**Phase 2**: Find visible tiles in `EmbeddedMissed` state. Send as
`FullDecode` work requests. Transition each to `CreatingThumbnail`.

**Phase 3**: Find visible `Loaded` tiles whose decoded dimensions are
smaller than the current tile display size (`needs_upscale()`). Send as
`Upscale` work requests. For raw files, `decode_for_upscale()` extracts
the larger embedded JPEG preview (typically 1024–1600px) without a full
LibRaw demosaic. For standard formats, it does a full decode + downscale
to tile size. The upgraded texture replaces the old one in-place.
An `upgrading` set tracks in-flight upscale requests to prevent duplicates.

**Phase 4**: Preload off-screen `NotLoaded` tiles as `EmbeddedOnly`,
expanding outward from the visible range so nearby tiles load first. Video files
are intentionally skipped here because frame extraction may launch an external
decoder and should not compete with image thumbnailing for off-screen work.

**Phase 5**: EXIF date-taken metadata scan (only in `DateTaken` sort mode).
JPEG-like files use a small prefix read. TIFF/DNG files use a seekable reader
so the EXIF parser can follow IFD offsets to metadata values without reading
the full raw image into memory. The scan extracts `DateTimeOriginal` (tag
0x9003), falling back to `DateTime` (tag 0x0132). Results flow back as
`DateScanned` work results, which call `set_tile_date()` / `set_tile_no_date()`
to incrementally sort tiles into the display order. A `date_scanning` set
tracks in-flight scans to prevent duplicates.

This ensures every visible tile shows *something* (even a low-res embedded
thumbnail) before any expensive full decodes begin, and tiles that appear
blurry at large tile sizes get silently upgraded in the background.

## Worker Threading Model

GridView spawns `N` decode workers where `N = available_parallelism - 2`
(minimum 2). Workers share an unbounded `crossbeam_channel` for work
requests and results.

```
                          ┌─────────────┐
  schedule_visible_work ──│  work_tx    │──> Worker 0
                          │ (unbounded) │──> Worker 1
                          │             │──> Worker 2
                          └─────────────┘──> Worker N
                                              │
                          ┌─────────────┐     │
  poll_results <──────────│  result_rx  │<────┘
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
results. Each image result triggers a texture upload (`ctx.load_texture`) and
a state transition. `DateScanned` results update the Grid's display order when
date sorting is active. Work results carry the generation they were scheduled
under; results from older generations are ignored. This prevents in-flight
thumbnail or metadata work from a previous scroll position or folder selection
from painting into the current grid.

## Folder Browsing

Folder browsing deliberately avoids a persistent cache or catalog. `IvApp` owns
a `FolderTree` rooted at the launch folder and renders it in a collapsible left
pane while in grid mode. The tree stores only expanded UI state for this app
session. When launched on a folder that has no direct image files, the pane
opens immediately so the user can choose a child folder. Expanding a node starts
a tiny background scan of that directory's immediate child folders. No recursive
counts, thumbnails, or metadata indexes are built. The scan also checks whether
each returned child folder has direct child folders of its own, so known leaf
folders render as plain rows without an expand disclosure.

The pane includes an in-memory text filter. Focusing the search box starts a
session-only recursive folder scan under the tree root. Discovered folders are
streamed back to the UI and merged into the same `FolderTree`, so substring
matches can appear gradually while the user types. The scan still does not read
image files, thumbnails, or metadata, and it does not build a persistent index.

Selecting a folder replaces the current `Grid` with a fresh one, preserves the
current tile size and sort mode, and starts the existing non-recursive image
enumerator for that folder. `GridView::replace_grid()` keeps the decode worker
pool alive but bumps the generation counter, drains pending work queues, clears
textures and per-tile timing state, and ignores any stale results that finish
after the switch.

## Thumbnail Extraction Pipeline

### Embedded Thumbnails (Phase 1)

`try_exif_only()` attempts fast thumbnail extraction by reading minimal
data from disk:

**JPEG / TIFF / RAW files**: For JPEG, the probe scans for the APP1 marker
and reads only the EXIF segment. For TIFF-based formats (DNG, CR2, NEF,
ARW, etc.), a custom IFD parser scans the TIFF directory chain
(IFD0 → SubIFDs → IFD1) within the 64KB probe to locate the embedded JPEG
thumbnail's offset and length. The IFD parser only reads the tags it needs
(Orientation, JPEGInterchangeFormat, JPEGInterchangeFormatLength, SubIFDs),
so it handles truncated data robustly — unlike the full EXIF library which
fails when tag values reference offsets beyond the buffer. The JPEG
thumbnail is then decoded directly with `zune-jpeg`.

**HEIC / HEIF files**: Uses `libheif-rs` to open the file as an ISOBMFF
container, get the primary image handle, check `number_of_thumbnails()`,
and decode the first thumbnail via `LibHeif::decode()` in RGBA colorspace.
HEIC files store thumbnails as separate image items in the container (not
in EXIF tags), so the EXIF approach doesn't work for them.

**Video files**: `.mov`, `.mp4`, and `.webm` entries are enumerated into the
same grid but use a separate `VideoThumbnail` work tier. The worker invokes
`ffmpeg` from `PATH`, seeks near the start, decodes a single frame to PNG on
stdout, then converts that frame to the existing RGBA `DecodedImage` texture
path. If `ffmpeg` is missing or the codec is unsupported, the tile transitions
to `Failed` and remains visible with a video/error placeholder. Video thumbnails
are scheduled only for visible tiles and are not used by date sorting or image
view navigation.

**Typical timing**: 40–80ms on local SSD, 100–300ms on network (SMB).
The bottleneck is I/O latency, not decode time.

### Full Decode (Phase 2)

`decode_from_bytes()` reads the entire file, decodes it via the `image`
crate (which dispatches to format-specific decoders including `libheif-rs`
for HEIC), applies EXIF orientation, and downscales to 160×160 using
`image::imageops::thumbnail()`.

**Typical timing**: 100–600ms depending on format and I/O. HEIC full
decode is the slowest due to HEVC/AV1 decompression.

### Resolution-Aware Upscale (Phase 3)

When tiles are displayed larger than their decoded thumbnail (e.g. after
window resize or with few files), `schedule_upscales()` re-decodes them at
higher resolution. `decode_for_upscale()` chooses the fastest path:

- **Raw files**: Extracts the largest embedded JPEG preview via
  `load_raw_preview()` (fast — no demosaic). Downscales if the preview
  exceeds 2× the target.
- **Standard formats**: Full decode via `decode_from_bytes()` at tile size.

The `needs_upscale()` check compares the decoded pixel dimensions against
the tile's display dimensions. The upscale texture replaces the existing
one, so the tile seamlessly sharpens without flicker.

### EXIF Orientation

Both paths apply EXIF orientation after decoding. `read_exif_orientation()`
parses the orientation tag (values 1–8) and `apply_orientation()` performs
the corresponding rotation/mirror transform. For TIFF-based files, the
custom IFD parser reads orientation directly from tag 0x0112.

### Full-Resolution Raw Decode (LibRaw)

When opening a raw file (DNG, CR2, NEF, ARW, etc.) in full-size image view,
`decode_raw_libraw()` decodes the full sensor data via LibRaw:

1. `libraw_open_buffer` → parse raw file structure
2. `libraw_unpack` → decompress sensor data
3. `libraw_dcraw_process` → demosaic, white balance, color space conversion
4. `libraw_dcraw_make_mem_image` → produce 8-bit sRGB bitmap

LibRaw handles EXIF orientation internally, so no manual rotation is needed.
The output is full sensor resolution (e.g. 6000×4000 for 24MP).

After full-image decode, ImageView applies detected Adobe Camera Raw / Lightroom
XMP develop settings as a best-effort display transform when the default-on
"Apply lossless edits" diagnostic toggle is enabled. This work lives in
`src/develop.rs`, keeping `src/decode.rs` focused on decoding, thumbnails, EXIF,
and raw preview extraction. Sidecar `.xmp` files next to the image take
precedence over embedded XMP packets. TIFF/DNG embedded XMP is read from tag 700
(`XMLPacket`) using targeted seekable I/O, while other containers use a bounded
prefix scan for `<x:xmpmeta>` / `<rdf:RDF>` packets.
The transform intentionally covers only settings with reasonable display-space
approximations: exposure, brightness, contrast, shadows/fill/highlight recovery,
white balance/tint, tone curves, parametric tone regions, saturation/vibrance,
HSL hue/saturation/luminance bands, grayscale conversion, shadow tint, clarity,
vignette/post-crop vignette, split toning, and grain. These effects are
Darktable-inspired category matches, not Adobe-compatible numeric matches.
Camera profiles, sharpening, noise reduction, chromatic aberration, defringe,
and lens-correction fields are parsed for display but are not applied because
matching those faithfully would require a much larger raw development engine.

A thin C wrapper (`src/libraw_wrapper.c`) encapsulates the full pipeline
in one function call, avoiding any Rust dependency on LibRaw's complex
`libraw_data_t` struct layout. LibRaw and its transitive dependencies
(lcms2, zlib, jasper, jpeg) are installed via vcpkg and linked statically.

If LibRaw fails, the viewer falls back to `load_raw_preview()` which
extracts the largest embedded JPEG preview from the TIFF IFD structure
(typically 1024–1600px). This is also used for grid thumbnails since it
only requires reading file headers, not full sensor decode.

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

### Info Pane

ImageView renders a collapsible left-side info pane via egui's side panel
layout. The pane is only shown in full image view and follows the currently
opened image. It shows file name, filesystem modified date, and a concise set
of EXIF fields: date taken, camera, lens, focal length, aperture, shutter
speed, and ISO. It also shows detected XMP develop settings and a default-on
"Apply lossless edits" toggle used to reload the current image with or without
the display transform. The pane only lists non-neutral settings that would affect
rendering. Settings consumed by the active display transform are rendered bright
white; supported settings are gray when the transform toggle is disabled;
non-neutral settings that iv detects but does not implement are rendered red.
Neutral and metadata-only CRS fields are hidden. Metadata is loaded on a background
thread from a small file prefix for JPEG-like files or from seekable TIFF/DNG IFD
traversal, so opening the pane does not block image display on full raw reads.
The pane body scrolls so long Develop sections do not expand the image layout. A
chevron in the full image status row toggles the pane. The image itself is
centered within the remaining visible image rectangle after the pane and status
row have consumed their space. The pane's open/closed state is persisted in
`%APPDATA%/iv/config.txt` as a simple `info_pane=true|false` line and is
restored on startup.

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
