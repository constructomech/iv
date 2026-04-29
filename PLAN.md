# iv — An Extremely Fast Image Viewer

## Philosophy

- **Disk is the library.** No import, no database, no catalog. Open a folder and see its images.
- **Perceived speed over throughput.** Show *something* instantly; refine quality in the background.
- **SSD-native I/O.** Exploit high queue-depth random reads — don't treat storage like a spinning disk.
- **Memory is cheap, latency is not.** Prefer caching decoded data in RAM over re-decoding.
- **Cross-platform.** Windows, macOS, Linux. Pure-Rust dependencies wherever possible to avoid build headaches.
- **Lean and mean.** Minimal dependencies. No framework unless it earns its place.

---

## Architecture Overview

```
┌─────────────┐     ┌──────────────┐     ┌────────────────┐     ┌───────────┐
│  Enumerator  │────▶│  I/O Engine   │────▶│  Decode Pipeline│────▶│  Renderer │
│  (async dir  │     │  (overlapped, │     │  (EXIF thumb →  │     │  (wgpu /  │
│   walk)      │     │   SSD-tuned)  │     │   low-res →     │     │   egui)   │
│              │     │              │     │   full-res)     │     │           │
└─────────────┘     └──────────────┘     └────────────────┘     └───────────┘
        │                    ▲                     ▲                    │
        │                    │                     │                    │
        └────────────────────┴─────────────────────┘                    │
                        Priority Scheduler ◀────────────────────────────┘
                     (visible + near-visible first)
```

### Key Components

| Component | Role |
|---|---|
| **Enumerator** | Streams directory entries without blocking the UI. Handles folders with 100k+ files. |
| **I/O Engine** | Issues overlapped/async reads at high queue depth. Optimized for NVMe/SSD random read. |
| **Decode Pipeline** | Multi-stage: EXIF embedded thumbnail → downscaled decode → full decode. Each stage produces a usable image. |
| **Priority Scheduler** | Determines what to load next based on scroll position. Cancels/deprioritizes off-screen work. |
| **Renderer** | GPU-accelerated grid of thumbnails (folder view) and full image display (image view). |

---

## Technology Choices

| Concern | Choice | Rationale |
|---|---|---|
| Windowing + UI | `egui` + `eframe` (wgpu backend) | GPU-rendered, handles scrolling/layout, fast iteration. We manage textures ourselves for progressive rendering. |
| Image decode (JPEG) | `zune-jpeg` (pure Rust) | ~2-3x faster than `image` crate for JPEG. Pure Rust = zero C deps, cross-platform. Within 10-20% of libjpeg-turbo. |
| Image decode (other) | `image` crate | PNG, WebP, TIFF, BMP, etc. |
| RAW preview extraction | `kamadak-exif` + raw TIFF/IFD parsing | DNG and CR2 embed full-res JPEG previews. Extract those — don't demosaic sensor data. |
| EXIF thumbnail | `kamadak-exif` | Extract embedded JPEG thumbnails without decoding the full image. Works on RAW files too (DNG, CR2, NEF, ARW all use EXIF/TIFF structure). |
| Async runtime | `tokio` (multi-thread) | IOCP on Windows = true overlapped I/O. Good ecosystem. |
| Parallelism | `rayon` (for CPU-bound decode) | Work-stealing pool for image decoding, separate from I/O. |
| Channel | `crossbeam-channel` or `tokio::sync::mpsc` | Communication between I/O, decode, and render threads. |

---

## Phases

### Phase 0 — Project Skeleton
**Goal:** Compilable project, window opens, single hard-coded image displays.

- `cargo init`
- Dependencies: `eframe`, `image` (minimal set — add deps only when needed)
- Open a window with eframe
- Load one image from a CLI arg, upload as egui texture, display it
- CLI arg parsing via `std::env::args` (no framework)
- VS Code configuration: `tasks.json` (build/clippy), `launch.json` (debug), `settings.json` (rust-analyzer)
- `README.md` with build/setup instructions
- **Exit criterion:** `iv D:\Photos\test.jpg` opens a window showing the image. F5 in VS Code builds and launches the debugger.

### Phase 1 — Async Folder Enumeration + Basic Grid
**Goal:** Open a folder, stream file entries, show a grid of placeholder tiles.

- Spawn a `tokio` task that calls `tokio::fs::read_dir` in a streaming fashion
- Filter for image extensions (case-insensitive):
  - Common: `.jpg`, `.jpeg`, `.png`, `.webp`, `.tiff`, `.tif`, `.bmp`, `.gif`
  - RAW (first-class): `.dng`, `.cr2`, `.cr3`, `.nef`, `.arw`, `.orf`, `.rw2`, `.raf`
- Feed entries into a `Arc<DashMap<PathBuf, ImageEntry>>` or similar concurrent structure
- `ImageEntry` states: `Discovered → Loading → Thumbnail → FullRes`
- Render a scrollable grid in egui with gray placeholder tiles for each discovered file
- Show filename below each tile
- Grid dynamically grows as enumerator discovers more files
- **Exit criterion:** `iv.exe D:\Photos` shows a scrolling grid of gray boxes with filenames that populates progressively.

### Phase 2 — EXIF Thumbnail Extraction (First Pixels on Screen)
**Goal:** Extract embedded JPEG thumbnails for near-instant preview\s.

- For each discovered image, read just the EXIF/TIFF header (first ~64KB of file)
- Extract the embedded thumbnail JPEG (typically 160×120 or 320×240)
- This works for **both JPEG and RAW files** — DNG, CR2, NEF, ARW all embed JPEG thumbnails in standard EXIF/TIFF IFD structures
- CR2 files typically contain 3 embedded JPEGs: small thumb, medium preview, and full-res preview
- DNG files embed a JPEG preview (often full-resolution) in the EXIF data
- Decode the thumbnail JPEG with `zune-jpeg`
- Upload to GPU as egui `TextureHandle`
- Replace the gray placeholder with the EXIF thumbnail
- Processing order: visible tiles first, then near-visible (±2 screens)
- **Target:** EXIF thumbnails for visible images within **<100ms** of scroll stop on SSD.
- **Exit criterion:** Opening a folder of 1000 JPEGs/DNGs/CR2s shows blurry-but-recognizable thumbnails almost instantly.

### Phase 3 — Priority Scheduler + Progressive Quality
**Goal:** Intelligently schedule decode work, progressively improve visible thumbnails.

- Implement a `PriorityScheduler` that tracks:
  - Current scroll position / visible tile range
  - Tiles sorted by distance from viewport center
- Three decode tiers:
  1. **Tier 0 (instant):** EXIF embedded thumbnail — ~64KB read, ~1ms decode
  2. **Tier 1 (fast):** `zune-jpeg` decode at 1/8 size — full file read, but decode only produces small image
  3. **Tier 2 (quality):** Full decode, downscale in software to thumbnail resolution — best quality
- When user scrolls, cancel in-flight tier 1/2 work for tiles that left the viewport (±buffer)
- Re-queue newly visible tiles starting at tier 0
- Wire up a "generation" counter so stale results are discarded
- **Exit criterion:** Scrolling through 10,000 images feels fluid. Visible tiles sharpen progressively. Scrolling cancels off-screen work.

### Phase 4 — High-Throughput I/O (SSD + Network)
**Goal:** Maximize I/O throughput on both local SSD and network shares (SMB/NFS).

- Read files using `tokio::fs::File` with high concurrency (not sequential)
- Maintain a pool of in-flight reads, semaphore-bounded:
  - Local SSD: 16-64 concurrent reads (exploit NVMe queue depth)
  - Network/SMB: higher concurrency (32-128) to hide round-trip latency
- Auto-detect high-latency paths (UNC paths `\\server\...`, or heuristic based on first-read latency) and adjust concurrency
- For EXIF extraction: partial read (first 64KB) — critical on network where a 25MB CR2 read takes 200ms+ but 64KB takes <1ms
- For tier 1 decode: read full file into buffer, pass to zune-jpeg
- Cancellation is even more important on network — don't waste bandwidth on off-screen tiles
- Optional: experiment with memory-mapped I/O (`memmap2`) vs buffered reads (mmap may not work well over SMB)
- Measure: log throughput (files/sec, MB/sec) and latency (time-to-first-thumbnail, time-to-all-visible) for both local and network paths
- **Exit criterion:** Sustain >2 GB/s read throughput on NVMe. Saturate 1Gbps link on SMB. Visible screen of 50 thumbnails fully rendered in <200ms (local) / <2s (network).

### Phase 5 — Full Image View
**Goal:** Click a thumbnail to view the full-resolution image.

- Click a thumbnail → transition to image view
- Immediately show the best-available decoded version (EXIF thumb, stretched) while full image loads
- Decode full-resolution image:
  - JPEG: decode with `zune-jpeg`
  - RAW (DNG, CR2, etc.): extract the largest embedded JPEG preview (often full-res) — no demosaicing needed
  - Other (PNG, WebP, etc.): decode with `image` crate
- Display with GPU scaling (wgpu handles large textures well)
- Keyboard/mouse navigation:
  - `Escape` / `Backspace` → back to folder view
  - `Left` / `Right` → previous/next image
  - Mouse wheel → zoom
  - Click-drag → pan (when zoomed)
  - `F` or double-click → fit to window
- Pre-decode adjacent images (left/right neighbors) for instant navigation
- **Exit criterion:** Click-to-visible in <50ms (showing EXIF thumb), full quality in <300ms for a 24MP JPEG on SSD.

### Phase 6 — Decode Benchmark Suite
**Goal:** Establish a repeatable benchmark for measuring and comparing decode techniques.

- `examples/bench.rs` — generates high-res (4000×3000 = 12MP) test images in all supported formats
- Test variants:
  - JPEG without EXIF thumbnail
  - JPEG with embedded EXIF thumbnail (manually constructed APP1)
  - PNG, WebP, TIFF, BMP, GIF
- Measures per-format:
  - EXIF extraction time (file read + EXIF parse + thumbnail decode)
  - Full decode + downscale time (file read + decode + orientation + resize)
  - Output thumbnail dimensions
- Configurable thumbnail resolution (`--thumb-size`)
- Reusable image directory (regenerate by deleting)
- **Exit criterion:** `cargo run --release --example bench` produces a comparison table.

### Phase 7 — Scroll Virtualization + Large Folder Performance  
**Goal:** Handle 100k+ image folders without lag.

- Only allocate egui widgets for visible rows + buffer rows
- GPU texture eviction: unload textures for tiles far from viewport
- Keep decoded `Vec<u8>` in RAM (cheaper to re-upload than re-decode) — evict these when memory pressure is high
- Enumerator should handle >100k files: use streaming, don't collect into a single Vec upfront
- Profile and fix any O(n) per-frame operations
- **Exit criterion:** Folder with 100,000 images scrolls at 60fps. Memory usage stays under 4GB.

### Phase 8 — Measurement & Thumbnail Cache Decision
**Goal:** Decide whether to add a persistent thumbnail cache.

- Instrument all stages with timing:
  - Enumeration: files/sec
  - I/O: MB/sec, latency distribution
  - Decode: images/sec per tier
  - Render: frame time, texture upload time
- Create a benchmark folder set (100, 1k, 10k, 100k images)
- Measure cold-start time (first open) vs warm-start (OS page cache hot)
- **Decision point:** If cold-start time-to-all-visible-thumbnails exceeds 2 seconds for 100 images, implement a thumbnail cache:
  - SQLite or flat-file cache keyed by (path, mtime, size)
  - Store tier-1 quality JPEG thumbnails
  - Async background writer, never blocks the UI
- **Exit criterion:** Published benchmark numbers. Cache decision made with data.

### Phase 9 — Video Thumbnail Support
**Goal:** Show thumbnails for common video files without adding full video playback.

- Expand enumeration to include video extensions (case-insensitive): `.mov`, `.mp4`, `.webm`, `.mkv`, `.avi`, `.3gp`, `.mpg`, `.mpeg`, `.vob`, `.wmv`
- Treat videos as first-class grid entries with a distinct media type, but keep image and video decode paths separate
- Start with iPhone `.MOV` files, including H.264/HEVC content and rotation metadata
- Support `.mp4` containers with common H.264/HEVC streams
- Support `.webm` containers with VP8/VP9 streams where the selected decoder supports them
- Support `.mkv` containers where the selected decoder supports the contained codec
- Support observed legacy formats through the same decoder path: `.avi`, `.3gp`, `.mpg`, `.mpeg`, `.vob`, `.wmv`
- Extract a representative thumbnail frame near the beginning of the video, avoiding full-file decode
- Apply orientation/rotation metadata before uploading the thumbnail texture
- Show a classic play affordance on video thumbnails: a translucent circle with a right-facing triangle, not a text `VIDEO` tag
- Launch the OS default video player when a standalone video tile is clicked
- Detect iPhone Live Photos by matching same-stem image/video pairs that differ only by extension, such as `IMG_0001.HEIC` + `IMG_0001.MOV`
- For Live Photos, show the still image as the primary grid tile with a `Live Image` tag instead of a separate video tile
- Add a full image view affordance for Live Photos that launches or plays the paired movie
- Fail gracefully for unsupported codecs, DRM-protected files, corrupt videos, or missing decoder support
- Measure startup impact separately from image thumbnailing so video work does not regress image-folder performance
- **Exit criterion:** Folders containing iPhone MOV, MP4, WebM, and Live Photo pairs show usable thumbnails and launch the appropriate movie while image thumbnail performance remains unchanged.

### Phase 10 — Polish & Robustness
**Goal:** Handle real-world usage edge cases.

- Adjustable thumbnail/tile size (keyboard shortcut or scroll-zoom in folder view)
- Broken/corrupt images: show error icon, don't crash
- Permission denied: skip gracefully
- Non-image files: ignore without errors
- RAW sensor demosaicing via `rawloader` (stretch goal — embedded JPEG previews cover 99% of use cases)
- HEIC/AVIF support via pure-Rust decoders when available (stretch goal)  
- Window resize: reflow grid
- DPI awareness / HiDPI displays
- Dark theme (default)
- Status bar: file count, loading progress
- Smooth scrolling with momentum
- **Exit criterion:** Can open any folder on the system without crashing.

---

## Deferred Decisions

| Decision | Defer Until | Notes |
|---|---|---|
| Folder browsing / tree view | After Phase 5 | May integrate with Explorer shell extension instead |
| Thumbnail cache | Phase 8 | Measure first, decide with data |
| Metadata display (EXIF date, dimensions, etc.) | After Phase 5 | Nice-to-have for image view |
| Sorting (by name, date, size) | After Phase 6 | Requires reading metadata for all files |
| Filtering | After Phase 6 | By date range, file type, etc. |
| Multi-folder / recursive | After Phase 6 | Walk subdirectories |
| Embed EXIF thumbnails | After Phase 8 | Opt-in CLI command (`iv --embed-thumbnails <folder>`) to losslessly inject EXIF thumbnails into source JPEGs that lack them. One-time cost, permanent speedup. Never automatic. |
| RAW demosaicing | Far future | Embedded JPEG previews cover viewing. Only needed for "develop" features. |
| Full video playback | Far future | Thumbnail support is Phase 9; playback requires a separate timing/audio/UI pipeline. |

---

## File Structure (Planned)

```
iv/
├── Cargo.toml
├── src/
│   ├── main.rs              # Entry point, CLI parsing, eframe launch
│   ├── app.rs               # Top-level eframe::App, owns all state
│   ├── enumerator.rs        # Async directory walking
│   ├── io_engine.rs         # Overlapped file reads, concurrency control
│   ├── decode.rs            # EXIF extraction, RAW preview extraction, image decoding
│   ├── scheduler.rs         # Priority queue, visibility tracking
│   ├── folder_view.rs       # Grid layout, thumbnail rendering
│   ├── image_view.rs        # Full-res image display, zoom/pan
│   ├── texture_cache.rs     # GPU texture lifecycle management
│   └── types.rs             # Shared types (ImageEntry, DecodeTier, etc.)
├── tests/
│   ├── common/
│   │   └── mod.rs           # Test helpers — synthetic image generation
│   └── image_loading.rs     # Integration tests — load pipeline
├── PLAN.md
└── README.md
```

---

## Testing Strategy

Tests grow with each phase. No test fixtures are checked in — synthetic images are generated at runtime.

### Test Taxonomy

| Kind | Location | What it tests | Requires GPU? |
|---|---|---|---|
| **Unit tests** | `src/*.rs` (`#[cfg(test)]`) | Pure functions: fit_size, center_offset, is_image_file, extension matching | No |
| **Integration tests** | `tests/image_loading.rs` | Full load pipeline: file → decode → pixel data, error handling, format support | No |
| **Test helpers** | `tests/common/mod.rs` | Synthetic image generation (JPEG, PNG, BMP, GIF, corrupt, empty, batch) | No |

### Per-Phase Test Expectations

| Phase | New tests |
|---|---|
| 0 — Skeleton | Unit: fit_size, center_offset, is_image_file. Integration: load JPEG/PNG/BMP/GIF, error handling, large images, batch generation. |
| 1 — Enumeration | Unit: extension filtering. Integration: enumerate folder, verify discovered count, verify streaming behavior. |
| 2 — EXIF Thumbs | Unit: EXIF parsing. Integration: extract thumbnail from real JPEG with EXIF, verify dimensions, handle missing EXIF. |
| 3 — Scheduler | Unit: priority ordering, generation counter, cancel semantics. |
| 4 — I/O Engine | Integration: concurrent read throughput, partial read correctness, semaphore bounding. |
| 5 — Image View | Unit: zoom/pan math. Integration: pre-decode neighbors. |
| 6 — Benchmarks | Benchmarks (`cargo bench`): decode throughput, I/O throughput, EXIF extraction rate. |
| 7 — Large Folders | Stress: 100k synthetic entries, measure frame time, memory. |
| 8 — Measurement | Benchmark-folder sets, cold/warm start comparisons, thumbnail cache decision data. |
| 9 — Video Thumbs | Integration: MOV/MP4/WebM thumbnail extraction, iPhone rotation metadata, unsupported codec fallback. |
| 10 — Polish | Fuzz/edge-case: corrupt files, permission denied, zero-byte, very large dimensions. |

---

## Performance Targets

| Metric | Target | Notes |
|---|---|---|
| Time to first pixel (EXIF thumb) | <50ms | For first visible image after folder open |
| Time to all visible thumbnails (EXIF) | <100ms | For a screen of ~50 images on NVMe |
| Time to all visible thumbnails (Tier 1) | <500ms | Higher quality, after EXIF pass |
| Scroll frame time | <16ms (60fps) | Must not drop frames while scrolling |
| Cold start, 1k images folder | <2s to full screen | Including enumeration |
| Memory per thumbnail (GPU) | ~50KB | 256×256 RGBA |
| Memory per thumbnail (CPU cached) | ~15KB | Compressed JPEG in RAM |

---

## Build & Run

```bash
# Build (release for perf testing)
cargo build --release

# Run on a folder
iv "D:\Photos\Vacation"      # Windows
iv ~/Photos/Vacation           # macOS/Linux

# Run on a single image (later)
iv ~/Photos/sunset.jpg
```
