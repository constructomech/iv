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
use crate::grid::{Grid, GridConfig, GridEventKind, SortMode, TileState};
use crate::media;

/// Returns true if IV_DEBUG env var is set to a truthy value.
fn debug_mode() -> bool {
    std::env::var("IV_DEBUG").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Per-frame time budget for scheduling + result polling (ms).
/// Keeps the UI thread responsive at 60fps (~16ms frame budget).
const FRAME_WORK_BUDGET_MS: f64 = 4.0;
/// Thumbnail decode resolution (pixels).
const THUMB_SIZE: u32 = 160;

/// Probe read size — 64KB covers HEIC meta box + thumbnail data for most
/// camera originals, JPEG APP1 with embedded thumbnail, PNG chunk headers
/// before IDAT, and WebP VP8X + initial chunk scan.
const PROBE_SIZE: usize = 64 * 1024;
/// Fallback prefix size for formats without smart probing (TIFF, etc).
const EXIF_PREFIX_SIZE: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Smart I/O read (run on tokio blocking threads)
// ---------------------------------------------------------------------------

/// Unified smart I/O: small probe → format detection → targeted read.
/// Works for all formats: JPEG, HEIC, PNG, WebP, GIF, BMP, TIFF, etc.
fn io_read_embedded_smart(
    path: &std::path::Path,
    gen_counter: &Arc<AtomicU64>,
    generation: u64,
    is_heif: bool,
) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();

    // Single 8KB probe covers all format detection + HEIC meta + PNG/WebP chunk headers
    let probe_len = (file_len as usize).min(PROBE_SIZE);
    let mut probe = vec![0u8; probe_len];
    file.read_exact(&mut probe).ok()?;

    if generation < gen_counter.load(Ordering::Relaxed) {
        return None;
    }

    match decode::probe_embedded_thumbnail(&probe, file_len) {
        decode::ProbeResult::ContainedInProbe { data } => Some(data),
        decode::ProbeResult::NeedsRead { offset, length } => {
            if is_heif {
                // HEIC: libheif needs a well-formed file to decode HEVC thumbnails.
                // A truncated mdat causes green corruption. Read the full file.
                std::fs::read(path).ok()
            } else if offset == 0 {
                // TIFF/DNG: read from file start, reusing already-read probe bytes
                let mut buf = vec![0u8; length];
                let reuse = probe_len.min(length);
                buf[..reuse].copy_from_slice(&probe[..reuse]);
                if reuse < length {
                    file.read_exact(&mut buf[reuse..]).ok()?;
                }
                Some(buf)
            } else {
                // Other formats: targeted read at exact offset
                let mut buf = vec![0u8; length];
                file.seek(SeekFrom::Start(offset)).ok()?;
                file.read_exact(&mut buf).ok()?;
                Some(buf)
            }
        }
        decode::ProbeResult::NoThumbnail => {
            // Return probe so decode can confirm EmbeddedMiss
            Some(probe)
        }
        decode::ProbeResult::Inconclusive => {
            // Fall back to prefix read
            read_prefix(&mut file, file_len as usize, &probe)
        }
    }
}

/// Read up to EXIF_PREFIX_SIZE from a file, reusing already-read probe bytes.
fn read_prefix(file: &mut std::fs::File, file_len: usize, probe: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let read_len = file_len.min(EXIF_PREFIX_SIZE);
    let already = probe.len().min(read_len);
    let mut buf = vec![0u8; read_len];
    buf[..already].copy_from_slice(&probe[..already]);
    if already < read_len {
        file.read_exact(&mut buf[already..]).ok()?;
    }
    Some(buf)
}

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
    /// Resolution upgrade for undersized thumbnails. Lowest priority.
    /// Raw files: extract larger embedded JPEG preview.
    /// Other formats: full decode + downscale to tile size.
    Upscale,
    /// Read EXIF date-taken metadata only.
    /// JPEG-like files use a small prefix; TIFF/DNG uses seekable IFD traversal.
    DateScan,
    /// Extract a thumbnail frame from a video file.
    VideoThumbnail,
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
    /// Target decode size (pixels). Used by Upscale tier.
    target_size: u32,
}

/// A completed result from a decode worker.
enum WorkResult {
    /// Embedded thumbnail extracted successfully.
    EmbeddedOk {
        idx: usize,
        image: DecodedImage,
        ms: f64,
        generation: u64,
    },
    /// No embedded thumbnail found. Includes file bytes for reuse.
    EmbeddedMiss {
        idx: usize,
        ms: f64,
        data: Option<Vec<u8>>,
        generation: u64,
    },
    /// Full decode completed.
    FullOk {
        idx: usize,
        image: DecodedImage,
        ms: f64,
        generation: u64,
    },
    /// Upscale decode completed.
    UpscaleOk {
        idx: usize,
        image: DecodedImage,
        ms: f64,
        generation: u64,
    },
    /// Date-taken metadata extracted.
    DateScanned {
        idx: usize,
        date: Option<String>,
        generation: u64,
    },
    /// Decode failed.
    Failed { idx: usize, generation: u64 },
}

impl WorkResult {
    fn generation(&self) -> u64 {
        match self {
            WorkResult::EmbeddedOk { generation, .. }
            | WorkResult::EmbeddedMiss { generation, .. }
            | WorkResult::FullOk { generation, .. }
            | WorkResult::UpscaleOk { generation, .. }
            | WorkResult::DateScanned { generation, .. }
            | WorkResult::Failed { generation, .. } => *generation,
        }
    }
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
    /// Decoded image dimensions per tile (for resolution-aware upscale).
    decoded_sizes: Vec<Option<(u32, u32)>>,
    /// Indices of tiles with in-flight upscale decodes.
    upgrading: std::collections::HashSet<usize>,
    /// Indices of tiles with in-flight date scans.
    date_scanning: std::collections::HashSet<usize>,
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
                                            generation: req.generation,
                                        });
                                    }
                                    None => {
                                        // For HEIC, pass file bytes for FullDecode reuse —
                                        // but only if we have enough data (not a small probe).
                                        let data = if req.is_heif && req.data.len() > PROBE_SIZE {
                                            Some(req.data)
                                        } else {
                                            None
                                        };
                                        let _ = result_tx.send(WorkResult::EmbeddedMiss {
                                            idx: req.idx,
                                            ms,
                                            data,
                                            generation: req.generation,
                                        });
                                    }
                                }
                            }
                            WorkTier::FullDecode => {
                                let result = decode::decode_from_bytes(
                                    &req.data,
                                    req.target_size,
                                    req.is_heif,
                                );
                                let ms = start.elapsed().as_secs_f64() * 1000.0;
                                match result {
                                    Ok((image, _)) => {
                                        let _ = result_tx.send(WorkResult::FullOk {
                                            idx: req.idx,
                                            image,
                                            ms,
                                            generation: req.generation,
                                        });
                                    }
                                    Err(_) => {
                                        let _ = result_tx.send(WorkResult::Failed {
                                            idx: req.idx,
                                            generation: req.generation,
                                        });
                                    }
                                }
                            }
                            WorkTier::Upscale => {
                                let is_raw = decode::is_raw_extension(&req.path);
                                let result = decode::decode_for_upscale(
                                    &req.data,
                                    req.target_size,
                                    is_raw,
                                    req.is_heif,
                                );
                                let ms = start.elapsed().as_secs_f64() * 1000.0;
                                match result {
                                    Some(image) => {
                                        let _ = result_tx.send(WorkResult::UpscaleOk {
                                            idx: req.idx,
                                            image,
                                            ms,
                                            generation: req.generation,
                                        });
                                    }
                                    None => {
                                        let _ = result_tx.send(WorkResult::Failed {
                                            idx: req.idx,
                                            generation: req.generation,
                                        });
                                    }
                                }
                            }
                            WorkTier::DateScan => {
                                let date = decode::read_date_taken_from_path(&req.path, &req.data);
                                let _ = result_tx.send(WorkResult::DateScanned {
                                    idx: req.idx,
                                    date,
                                    generation: req.generation,
                                });
                            }
                            WorkTier::VideoThumbnail => {
                                let result =
                                    decode::decode_video_thumbnail(&req.path, req.target_size);
                                let ms = start.elapsed().as_secs_f64() * 1000.0;
                                match result {
                                    Ok(image) => {
                                        let _ = result_tx.send(WorkResult::FullOk {
                                            idx: req.idx,
                                            image,
                                            ms,
                                            generation: req.generation,
                                        });
                                    }
                                    Err(err) => {
                                        log::warn!(
                                            "Failed to extract video thumbnail for {}: {err}",
                                            req.path.display()
                                        );
                                        let _ = result_tx.send(WorkResult::Failed {
                                            idx: req.idx,
                                            generation: req.generation,
                                        });
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
            decoded_sizes: Vec::new(),
            upgrading: std::collections::HashSet::new(),
            date_scanning: std::collections::HashSet::new(),
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

    /// Replace the displayed folder contents while keeping worker threads alive.
    pub fn replace_grid(&mut self, mut grid: Grid) {
        let new_generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        while self.decode_rx.try_recv().is_ok() {}
        while self.result_rx.try_recv().is_ok() {}

        grid.set_tile_size(self.tile_size, self.tile_size);
        self.grid = grid;
        self.textures.clear();
        self.decoded_sizes.clear();
        self.upgrading.clear();
        self.date_scanning.clear();
        self.timings.clear();
        self.cached_data.clear();
        self.last_scroll_y = 0.0;
        self.grid.record_event(GridEventKind::GenerationBump {
            generation: new_generation,
        });
    }

    // -- Result polling -----------------------------------------------------

    fn ensure_vecs(&mut self, idx: usize) {
        while self.textures.len() <= idx {
            self.textures.push(None);
        }
        while self.decoded_sizes.len() <= idx {
            self.decoded_sizes.push(None);
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

            if result.generation() < self.generation.load(Ordering::Relaxed) {
                continue;
            }

            match result {
                WorkResult::EmbeddedOk { idx, image, ms, .. } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        let size = [image.width as usize, image.height as usize];
                        self.decoded_sizes[idx] = Some((image.width, image.height));
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
                WorkResult::EmbeddedMiss { idx, ms, data, .. } => {
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
                WorkResult::FullOk { idx, image, ms, .. } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        let size = [image.width as usize, image.height as usize];
                        self.decoded_sizes[idx] = Some((image.width, image.height));
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
                WorkResult::Failed { idx, .. } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        self.upgrading.remove(&idx);
                        self.grid.record_event(GridEventKind::ResultReceived {
                            idx,
                            kind: "failed".into(),
                            ms: 0.0,
                        });
                        self.grid.set_tile_state(idx, TileState::Failed);
                    }
                }
                WorkResult::UpscaleOk { idx, image, ms, .. } => {
                    if idx < self.grid.tile_count() {
                        self.ensure_vecs(idx);
                        let size = [image.width as usize, image.height as usize];
                        self.decoded_sizes[idx] = Some((image.width, image.height));
                        let ci = egui::ColorImage::from_rgba_unmultiplied(size, &image.pixels);
                        self.textures[idx] = Some(ctx.load_texture(
                            format!("t{idx}"),
                            ci,
                            egui::TextureOptions::LINEAR,
                        ));
                        self.upgrading.remove(&idx);
                        self.grid.record_event(GridEventKind::ResultReceived {
                            idx,
                            kind: "upscale_ok".into(),
                            ms,
                        });
                    }
                }
                WorkResult::DateScanned { idx, date, .. } => {
                    if idx < self.grid.tile_count() {
                        self.date_scanning.remove(&idx);
                        if date.is_some() {
                            self.grid.set_tile_date(idx, date);
                        } else {
                            self.grid.set_tile_no_date(idx);
                        }
                    }
                }
            }
        }
        processed
    }

    // -- Scheduling ---------------------------------------------------------

    /// Four-phase scheduling:
    /// 1. All visible NotLoaded tiles → EmbeddedOnly (highest priority)
    /// 2. All visible EmbeddedMissed tiles → FullDecode
    /// 3. Visible Loaded tiles with undersized thumbnails → Upscale
    /// 4. Off-screen NotLoaded tiles → EmbeddedOnly (preloading, nearest first)
    ///
    /// I/O is dispatched to tokio's blocking pool; decoded bytes flow to decode workers.
    /// Stops scheduling if the frame time budget is exceeded.
    fn schedule_work(&mut self, deadline: &std::time::Instant) {
        let current_gen = self.generation.load(Ordering::Relaxed);

        // Phase 1: embedded thumbnails for visible NotLoaded tiles
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
                let tier = if media::is_video_file(&path) {
                    WorkTier::VideoThumbnail
                } else {
                    WorkTier::EmbeddedOnly
                };
                self.grid.set_tile_state(idx, TileState::LoadingEmbedded);
                let is_heif = decode::is_heif_extension(&path);
                self.spawn_io_read(idx, path, current_gen, tier, is_heif, THUMB_SIZE);
                scheduled_indices.push(idx);
            }
            if !scheduled_indices.is_empty() {
                self.grid.record_event(GridEventKind::WorkScheduled {
                    indices: scheduled_indices,
                    tier: "visible_thumb".into(),
                });
            }
            return;
        }

        // Phase 2: full decode for visible tiles where embedded failed
        let needs_full = self.grid.visible_in_state(TileState::EmbeddedMissed);
        if !needs_full.is_empty() {
            let mut scheduled_indices = Vec::new();
            for idx in needs_full {
                if std::time::Instant::now() >= *deadline {
                    break;
                }
                let path = self.grid.tile_path(idx).to_path_buf();
                if path.as_os_str().is_empty() {
                    continue;
                }
                if media::is_video_file(&path) {
                    continue;
                }
                self.grid.set_tile_state(idx, TileState::CreatingThumbnail);
                let is_heif = decode::is_heif_extension(&path);
                let cached = if idx < self.cached_data.len() {
                    self.cached_data[idx].take()
                } else {
                    None
                };
                if let Some(data) = cached {
                    let _ = self.decode_tx.send(DecodeRequest {
                        idx,
                        path,
                        data,
                        generation: current_gen,
                        tier: WorkTier::FullDecode,
                        is_heif,
                        target_size: THUMB_SIZE,
                    });
                } else {
                    self.spawn_io_read(
                        idx,
                        path,
                        current_gen,
                        WorkTier::FullDecode,
                        is_heif,
                        THUMB_SIZE,
                    );
                }
                scheduled_indices.push(idx);
            }
            if !scheduled_indices.is_empty() {
                self.grid.record_event(GridEventKind::WorkScheduled {
                    indices: scheduled_indices,
                    tier: "full_decode".into(),
                });
            }
            return;
        }

        // Phase 3: upscale visible tiles with undersized thumbnails
        if std::time::Instant::now() < *deadline {
            self.schedule_upscales(current_gen, deadline);
        }

        // Phase 4: preload off-screen NotLoaded tiles (nearest to viewport first)
        // Only runs when all visible tiles are in-flight or loaded.
        if std::time::Instant::now() >= *deadline {
            return;
        }
        self.schedule_offscreen_embedded(current_gen, deadline);

        // Phase 5: date-taken metadata scan (only in DateTaken sort mode)
        if std::time::Instant::now() < *deadline {
            self.schedule_date_scans(current_gen, deadline);
        }
    }

    /// Schedule upscale decodes for visible tiles whose thumbnails are
    /// smaller than the current tile display size.
    fn schedule_upscales(&mut self, current_gen: u64, deadline: &std::time::Instant) {
        let tile_w = self.grid.config().tile_width;
        let tile_h = self.grid.config().tile_height;
        let visible = self.grid.visible_in_state(TileState::Loaded);

        let mut scheduled_indices = Vec::new();
        for idx in visible {
            if std::time::Instant::now() >= *deadline {
                break;
            }
            if self.upgrading.contains(&idx) {
                continue;
            }
            let Some(&Some((dw, dh))) = self.decoded_sizes.get(idx) else {
                continue;
            };
            if !decode::needs_upscale(dw, dh, tile_w, tile_h) {
                continue;
            }
            let path = self.grid.tile_path(idx).to_path_buf();
            if path.as_os_str().is_empty() {
                continue;
            }
            if media::is_video_file(&path) {
                continue;
            }
            self.upgrading.insert(idx);
            let is_heif = decode::is_heif_extension(&path);
            self.spawn_io_read(
                idx,
                path,
                current_gen,
                WorkTier::Upscale,
                is_heif,
                tile_w as u32,
            );
            scheduled_indices.push(idx);
        }
        if !scheduled_indices.is_empty() {
            self.grid.record_event(GridEventKind::WorkScheduled {
                indices: scheduled_indices,
                tier: "upscale".into(),
            });
        }
    }

    /// Schedule EXIF date-taken scans for tiles that don't have dates yet.
    /// Only active in DateTaken sort mode.
    fn schedule_date_scans(&mut self, current_gen: u64, deadline: &std::time::Instant) {
        let needs_scan = self.grid.tiles_needing_date_scan();
        if needs_scan.is_empty() {
            return;
        }

        let mut scheduled_indices = Vec::new();
        for idx in needs_scan {
            if std::time::Instant::now() >= *deadline {
                break;
            }
            if self.date_scanning.contains(&idx) {
                continue;
            }
            let path = self.grid.tile_path(idx).to_path_buf();
            if path.as_os_str().is_empty() {
                continue;
            }
            if media::is_video_file(&path) {
                self.grid.set_tile_no_date(idx);
                continue;
            }
            self.date_scanning.insert(idx);
            let is_heif = decode::is_heif_extension(&path);
            self.spawn_io_read(idx, path, current_gen, WorkTier::DateScan, is_heif, 0);
            scheduled_indices.push(idx);
        }
        if !scheduled_indices.is_empty() {
            self.grid.record_event(GridEventKind::WorkScheduled {
                indices: scheduled_indices,
                tier: "date_scan".into(),
            });
        }
    }

    /// Schedule embedded thumbnail extraction for off-screen tiles,
    /// expanding outward from the visible range so nearby tiles load first.
    /// Iterates display positions and maps through display_order.
    fn schedule_offscreen_embedded(&mut self, current_gen: u64, deadline: &std::time::Instant) {
        let (vis_start, vis_end) = self.grid.visible_tile_range();
        let total = self.grid.tile_count();
        if total == 0 {
            return;
        }

        // Interleave: one tile below viewport, one above, expanding outward
        // These are display-order positions, not tile indices.
        let mut below = vis_end;
        let mut above = vis_start.wrapping_sub(1); // will wrap to usize::MAX if vis_start == 0
        let mut scheduled_count = 0;

        loop {
            if std::time::Instant::now() >= *deadline {
                break;
            }

            let mut found = false;

            // Try below viewport
            while below < total {
                let idx = self.grid.display_to_tile(below);
                if self.grid.tile_state(idx) == TileState::NotLoaded {
                    let path = self.grid.tile_path(idx).to_path_buf();
                    if !path.as_os_str().is_empty() && !media::is_video_file(&path) {
                        self.grid.set_tile_state(idx, TileState::LoadingEmbedded);
                        let is_heif = decode::is_heif_extension(&path);
                        self.spawn_io_read(
                            idx,
                            path,
                            current_gen,
                            WorkTier::EmbeddedOnly,
                            is_heif,
                            THUMB_SIZE,
                        );
                        scheduled_count += 1;
                        found = true;
                        below += 1;
                        break;
                    }
                }
                below += 1;
            }

            // Try above viewport
            while above < total {
                // above wraps to usize::MAX when it underflows, which is >= total
                let idx = self.grid.display_to_tile(above);
                if self.grid.tile_state(idx) == TileState::NotLoaded {
                    let path = self.grid.tile_path(idx).to_path_buf();
                    if !path.as_os_str().is_empty() && !media::is_video_file(&path) {
                        self.grid.set_tile_state(idx, TileState::LoadingEmbedded);
                        let is_heif = decode::is_heif_extension(&path);
                        self.spawn_io_read(
                            idx,
                            path,
                            current_gen,
                            WorkTier::EmbeddedOnly,
                            is_heif,
                            THUMB_SIZE,
                        );
                        scheduled_count += 1;
                        found = true;
                    }
                }
                above = above.wrapping_sub(1);
                if found {
                    break;
                }
            }

            if !found {
                break; // no more NotLoaded tiles in either direction
            }
        }

        let _ = scheduled_count;
    }

    /// Spawn a tokio blocking task to read a file and push bytes to the decode pool.
    fn spawn_io_read(
        &self,
        idx: usize,
        path: PathBuf,
        generation: u64,
        tier: WorkTier,
        is_heif: bool,
        target_size: u32,
    ) {
        let decode_tx = self.decode_tx.clone();
        let gen_counter = self.generation.clone();
        self.io_runtime.spawn_blocking(move || {
            if generation < gen_counter.load(Ordering::Relaxed) {
                return;
            }

            let data = match tier {
                WorkTier::EmbeddedOnly => {
                    io_read_embedded_smart(&path, &gen_counter, generation, is_heif)
                }
                WorkTier::FullDecode | WorkTier::Upscale => {
                    // Full decode / upscale always needs the entire file
                    std::fs::read(&path).ok()
                }
                WorkTier::VideoThumbnail => Some(Vec::new()),
                WorkTier::DateScan => {
                    // JPEG-like files usually only need the EXIF header. TIFF/DNG
                    // may need offset-following reads from the decode worker.
                    use std::io::Read;
                    (|| -> Option<Vec<u8>> {
                        let mut file = std::fs::File::open(&path).ok()?;
                        let file_len = file.metadata().ok()?.len() as usize;
                        let read_len = file_len.min(PROBE_SIZE);
                        let mut buf = vec![0u8; read_len];
                        file.read_exact(&mut buf).ok()?;
                        Some(buf)
                    })()
                }
            };

            let Some(data) = data else { return };
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
                target_size,
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
            self.upgrading.clear();
            self.date_scanning.clear();
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

        let keyboard_scroll_y = self.handle_keyboard_navigation(ctx);
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
        let scroll_area = egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .max_height(available_for_grid);
        let scroll_area = if let Some(scroll_y) = keyboard_scroll_y {
            scroll_area.vertical_scroll_offset(scroll_y)
        } else {
            scroll_area
        };
        scroll_area.show(ui, |ui| {
            self.grid
                .set_viewport_size(available_width, ui.clip_rect().height());
            let scroll_offset = ui.clip_rect().min.y - ui.min_rect().min.y;
            self.grid.set_scroll(scroll_offset);

            self.check_scroll_generation();
            self.schedule_work(&deadline);

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
            let sep_row = self.grid.separator_row();
            let sorted_count = self.grid.sorted_count();
            let display_order = self.grid.display_order();

            for row in render_first..render_last {
                // Check if this is the separator row
                if sep_row == Some(row) {
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Loading order metadata…")
                                .color(egui::Color32::from_rgb(140, 140, 140))
                                .size(12.0)
                                .italics(),
                        );
                    });
                    ui.allocate_space(egui::vec2(0.0, padding));
                    continue;
                }

                // Map visual row to display-order position range
                let (disp_start, disp_end) = match sep_row {
                    Some(sep) if row > sep => {
                        let offset = (row - sep - 1) * cols;
                        let start = sorted_count + offset;
                        let end = (start + cols).min(tile_count);
                        (start, end)
                    }
                    _ => {
                        let start = row * cols;
                        let end = if sep_row.is_some() {
                            (start + cols).min(sorted_count)
                        } else {
                            (start + cols).min(tile_count)
                        };
                        (start, end)
                    }
                };

                if disp_start >= disp_end {
                    ui.allocate_space(egui::vec2(0.0, cell_h));
                    continue;
                }

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(padding, 0.0);
                    for (pos, &idx) in display_order
                        .iter()
                        .enumerate()
                        .take(disp_end)
                        .skip(disp_start)
                    {
                        let state = self.grid.tile_state(idx);
                        let name = self.grid.tile_name(idx);
                        let is_video = media::is_video_file(self.grid.tile_path(idx));
                        let texture = self.textures.get(idx).and_then(|t| t.as_ref());
                        let timing = self.timings.get(idx);
                        let response = Self::render_tile(
                            ui, idx, name, state, texture, timing, tile_w, tile_h, is_video, debug,
                        );
                        if response.clicked() {
                            clicked = Some(pos);
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
                self.sort_mode_dropdown(ui);
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

        // Repaint while any tiles are pending (visible or off-screen preloading)
        if self.grid.tile_count() > 0 {
            let has_visible_pending = !self.grid.visible_in_state(TileState::NotLoaded).is_empty()
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
            let has_offscreen_pending = !self.result_rx.is_empty();
            let has_date_scan_pending = !self.grid.date_scan_complete();
            if has_visible_pending {
                // Visible work: repaint at 60fps for responsiveness
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            } else if has_offscreen_pending || has_date_scan_pending {
                // Off-screen work or date scanning: repaint at 10fps to process results
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        clicked
    }

    fn handle_keyboard_navigation(&mut self, ctx: &egui::Context) -> Option<f32> {
        let page = self
            .grid
            .viewport()
            .height
            .max(self.grid.config().cell_height());
        let current = self.grid.scroll_y();
        let target = ctx.input(|input| {
            if input.key_pressed(egui::Key::Home) {
                Some(0.0)
            } else if input.key_pressed(egui::Key::End) {
                Some(self.grid.content_height())
            } else if input.key_pressed(egui::Key::PageUp) {
                Some(current - page)
            } else if input.key_pressed(egui::Key::PageDown) {
                Some(current + page)
            } else {
                None
            }
        });

        target.map(|scroll_y| {
            self.grid.set_scroll(scroll_y);
            ctx.request_repaint();
            self.grid.scroll_y()
        })
    }

    fn sort_mode_dropdown(&mut self, ui: &mut egui::Ui) {
        let current = self.grid.sort_mode();
        let mut mode = self.grid.sort_mode();
        egui::ComboBox::from_id_salt("grid_sort_mode")
            .selected_text(format!("Sort: {}", Self::sort_mode_label(mode)))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut mode,
                    SortMode::Name,
                    Self::sort_mode_label(SortMode::Name),
                );
                ui.selectable_value(
                    &mut mode,
                    SortMode::DateTaken,
                    Self::sort_mode_label(SortMode::DateTaken),
                );
            });
        if mode != current {
            self.grid.set_sort_mode(mode);
            ui.ctx().request_repaint();
        }
    }

    fn sort_mode_label(mode: SortMode) -> &'static str {
        match mode {
            SortMode::Name => "Name",
            SortMode::DateTaken => "Date taken",
        }
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
        is_video: bool,
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
                    TileState::Failed => egui::Color32::from_rgb(65, 35, 35),
                };
                painter.rect_filled(rect, 2.0, bg);
            }

            if is_video {
                let radius = 18.0;
                let center = egui::pos2(rect.max.x - radius - 8.0, rect.min.y + radius + 8.0);
                painter.circle_filled(
                    center,
                    radius,
                    egui::Color32::from_rgba_premultiplied(0, 0, 0, 160),
                );
                painter.circle_stroke(
                    center,
                    radius,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(235, 235, 235)),
                );
                let triangle = vec![
                    egui::pos2(center.x - 5.0, center.y - 8.0),
                    egui::pos2(center.x - 5.0, center.y + 8.0),
                    egui::pos2(center.x + 8.0, center.y),
                ];
                painter.add(egui::Shape::convex_polygon(
                    triangle,
                    egui::Color32::from_rgb(245, 245, 245),
                    egui::Stroke::NONE,
                ));
                if state == TileState::Failed && texture.is_none() {
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "No thumbnail",
                        egui::FontId::proportional(12.0),
                        egui::Color32::from_rgb(210, 160, 160),
                    );
                }
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
