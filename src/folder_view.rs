use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::app::DecodedImage;
use crate::decode;
use crate::enumerator::{self, EnumHandle, EnumMessage};

/// Thumbnail tile size in the grid.
const TILE_SIZE: f32 = 160.0;
/// Padding between tiles.
const TILE_PADDING: f32 = 8.0;
/// Height reserved for the filename label below the tile.
const LABEL_HEIGHT: f32 = 20.0;
/// Full cell size including padding and label.
const CELL_SIZE: f32 = TILE_SIZE + TILE_PADDING + LABEL_HEIGHT;
/// Thumbnail decode resolution (pixels).
const THUMB_DECODE_SIZE: u32 = 160;
/// Number of thumbnail worker threads.
const NUM_THUMB_WORKERS: usize = 4;

/// Per-image state in the grid.
struct ImageEntry {
    path: PathBuf,
    texture: Option<egui::TextureHandle>,
    state: ThumbState,
}

/// Thumbnail loading state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Pending used when we add visibility-based loading in Phase 3
enum ThumbState {
    /// Waiting to be queued for loading.
    Pending,
    /// Queued or in-progress on a worker thread.
    Loading,
    /// Successfully decoded and uploaded to GPU.
    Loaded,
    /// Failed to decode.
    Failed,
}

/// Manages a pool of worker threads that decode thumbnails.
struct ThumbLoader {
    work_tx: mpsc::Sender<(usize, PathBuf)>,
    result_rx: mpsc::Receiver<(usize, Result<DecodedImage, String>)>,
}

impl ThumbLoader {
    fn new() -> Self {
        let (work_tx, work_rx) = mpsc::channel::<(usize, PathBuf)>();
        let (result_tx, result_rx) = mpsc::channel();

        let work_rx = Arc::new(Mutex::new(work_rx));

        for _ in 0..NUM_THUMB_WORKERS {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            thread::spawn(move || {
                while let Ok((idx, path)) = work_rx.lock().unwrap().recv() {
                    let result = decode::decode_thumbnail(&path, THUMB_DECODE_SIZE);
                    if result_tx.send((idx, result)).is_err() {
                        break; // Receiver dropped, shutting down
                    }
                }
            });
        }

        Self { work_tx, result_rx }
    }

    fn queue(&self, idx: usize, path: &PathBuf) {
        let _ = self.work_tx.send((idx, path.clone()));
    }
}

/// State for the folder grid view.
pub struct FolderView {
    /// Directory being viewed.
    folder: PathBuf,
    /// Discovered image entries with thumbnail state.
    entries: Vec<ImageEntry>,
    /// Whether enumeration has completed.
    enum_done: bool,
    /// Total count reported by enumerator (set when done).
    enum_total: Option<usize>,
    /// Handle to the background enumerator.
    enum_handle: Option<EnumHandle>,
    /// Thumbnail loader thread pool.
    thumb_loader: ThumbLoader,
    /// Whether all thumbnails have been loaded.
    thumbs_done: bool,
    /// Error from enumeration, if any.
    error: Option<String>,
}

impl FolderView {
    pub fn new(folder: PathBuf) -> Self {
        let enum_handle = Some(enumerator::enumerate_folder(folder.clone()));
        Self {
            folder,
            entries: Vec::new(),
            enum_done: false,
            enum_total: None,
            enum_handle,
            thumb_loader: ThumbLoader::new(),
            thumbs_done: false,
            error: None,
        }
    }

    /// Drain pending messages from the enumerator (non-blocking).
    fn poll_enumerator(&mut self) {
        if let Some(ref handle) = self.enum_handle {
            // Drain all available messages without blocking
            loop {
                match handle.receiver.try_recv() {
                    Ok(EnumMessage::Found(path)) => {
                        let idx = self.entries.len();
                        self.thumb_loader.queue(idx, &path);
                        self.entries.push(ImageEntry {
                            path,
                            texture: None,
                            state: ThumbState::Loading,
                        });
                    }
                    Ok(EnumMessage::Done(count)) => {
                        self.enum_done = true;
                        self.enum_total = Some(count);
                        log::info!(
                            "Enumeration complete: {} images in {}",
                            count,
                            self.folder.display()
                        );
                        break;
                    }
                    Ok(EnumMessage::Error(e)) => {
                        log::error!("Enumeration error: {e}");
                        self.error = Some(e);
                        self.enum_done = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if !self.enum_done {
                            self.enum_done = true;
                            log::warn!("Enumerator disconnected unexpectedly");
                        }
                        break;
                    }
                }
            }
        }

        // Drop the handle once done to free the thread resources
        if self.enum_done {
            self.enum_handle = None;
        }
    }

    /// Drain completed thumbnails from the loader and upload to GPU.
    fn poll_thumbnails(&mut self, ctx: &egui::Context) {
        let mut got_any = false;

        while let Ok((idx, result)) = self.thumb_loader.result_rx.try_recv() {
            got_any = true;
            if idx < self.entries.len() {
                match result {
                    Ok(decoded) => {
                        let size = [decoded.width as usize, decoded.height as usize];
                        let color_image =
                            egui::ColorImage::from_rgba_unmultiplied(size, &decoded.pixels);
                        let texture = ctx.load_texture(
                            format!("thumb_{idx}"),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.entries[idx].texture = Some(texture);
                        self.entries[idx].state = ThumbState::Loaded;
                    }
                    Err(e) => {
                        log::warn!("Thumbnail failed for {}: {e}", self.entries[idx].path.display());
                        self.entries[idx].state = ThumbState::Failed;
                    }
                }
            }
        }

        // Check if all thumbnails are done
        if !got_any && self.enum_done {
            self.thumbs_done = self.entries.iter().all(|e| {
                matches!(e.state, ThumbState::Loaded | ThumbState::Failed)
            });
        }
    }

    /// Render the folder view. Returns Some(index) if a tile was clicked.
    pub fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> Option<usize> {
        self.poll_enumerator();
        self.poll_thumbnails(ctx);

        // Request repaint while work is in progress
        if !self.enum_done || !self.thumbs_done {
            ctx.request_repaint();
        }

        // Show error if enumeration failed
        if let Some(ref error) = self.error {
            ui.centered_and_justified(|ui| {
                ui.colored_label(egui::Color32::from_rgb(255, 80, 80), error);
            });
            return None;
        }

        // Status bar at top
        let loaded_count = self.entries.iter().filter(|e| e.state == ThumbState::Loaded).count();
        let status = if self.enum_done && self.thumbs_done {
            format!(
                "{} — {} images",
                self.folder.display(),
                self.entries.len()
            )
        } else if self.enum_done {
            format!(
                "{} — loading thumbnails ({}/{})…",
                self.folder.display(),
                loaded_count,
                self.entries.len()
            )
        } else {
            format!(
                "{} — scanning… ({} found)",
                self.folder.display(),
                self.entries.len()
            )
        };
        ui.label(
            egui::RichText::new(status)
                .color(egui::Color32::from_rgb(180, 180, 180))
                .size(13.0),
        );
        ui.add_space(4.0);

        let mut clicked_index = None;

        // Scrollable grid
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                let available_width = ui.available_width();
                let cols = ((available_width + TILE_PADDING) / (TILE_SIZE + TILE_PADDING))
                    .floor()
                    .max(1.0) as usize;
                let rows = (self.entries.len() + cols - 1) / cols;

                // Use a ui layout that wraps
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(TILE_PADDING, TILE_PADDING);

                    for (idx, entry) in self.entries.iter().enumerate() {
                        let _ = rows; // suppress unused warning
                        let response = Self::render_tile(ui, idx, entry);
                        if response.clicked() {
                            clicked_index = Some(idx);
                        }
                    }
                });
            });

        clicked_index
    }

    /// Render a single tile (thumbnail or placeholder + filename).
    fn render_tile(ui: &mut egui::Ui, _idx: usize, entry: &ImageEntry) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(TILE_SIZE, CELL_SIZE),
            egui::Sense::click(),
        );

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);

            let thumb_rect = egui::Rect::from_min_size(rect.min, egui::vec2(TILE_SIZE, TILE_SIZE));

            if let Some(tex) = &entry.texture {
                // Draw the decoded thumbnail, centered within the tile
                let tex_size = tex.size_vec2();
                let scale = (TILE_SIZE / tex_size.x).min(TILE_SIZE / tex_size.y);
                let display_w = tex_size.x * scale;
                let display_h = tex_size.y * scale;
                let offset_x = (TILE_SIZE - display_w) / 2.0;
                let offset_y = (TILE_SIZE - display_h) / 2.0;

                // Dark background behind thumbnail for letterboxing
                painter.rect_filled(thumb_rect, 2.0, egui::Color32::from_rgb(24, 24, 24));

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

                // Hover overlay
                if response.hovered() {
                    painter.rect_filled(
                        thumb_rect,
                        2.0,
                        egui::Color32::from_rgba_premultiplied(255, 255, 255, 20),
                    );
                }
            } else {
                // Gray placeholder
                let bg_color = if response.hovered() {
                    egui::Color32::from_rgb(70, 70, 70)
                } else {
                    egui::Color32::from_rgb(48, 48, 48)
                };
                painter.rect_filled(thumb_rect, 2.0, bg_color);
            }

            // Filename label below the thumbnail
            let filename = entry
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let label_rect = egui::Rect::from_min_size(
                egui::pos2(rect.min.x, rect.min.y + TILE_SIZE + 2.0),
                egui::vec2(TILE_SIZE, LABEL_HEIGHT),
            );
            painter.text(
                label_rect.center(),
                egui::Align2::CENTER_CENTER,
                truncate_filename(&filename, 20),
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgb(200, 200, 200),
            );
        }

        response
    }

    /// Access entries (for testing).
    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Count of loaded thumbnails (for testing).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn loaded_count(&self) -> usize {
        self.entries.iter().filter(|e| e.state == ThumbState::Loaded).count()
    }

    /// Check if enumeration is done (for testing).
    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        self.enum_done
    }

    /// Check if all thumbnails have been loaded (for testing).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn thumbs_complete(&self) -> bool {
        self.thumbs_done
    }
}

/// Truncate a filename for display, keeping extension visible.
fn truncate_filename(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        return name.to_string();
    }
    // Keep extension visible: "longfilena….jpg"
    if let Some(dot_pos) = name.rfind('.') {
        let ext = &name[dot_pos..];
        let keep = max_len.saturating_sub(ext.len() + 1);
        if keep > 0 {
            return format!("{}…{}", &name[..keep], ext);
        }
    }
    format!("{}…", &name[..max_len - 1])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_name() {
        assert_eq!(truncate_filename("photo.jpg", 20), "photo.jpg");
    }

    #[test]
    fn truncate_long_name() {
        let name = "a_very_long_filename_that_exceeds_limit.jpg";
        let result = truncate_filename(name, 20);
        assert!(result.len() <= 24); // allow for multi-byte ellipsis
        assert!(result.ends_with(".jpg"));
        assert!(result.contains('…'));
    }

    #[test]
    fn truncate_no_extension() {
        let result = truncate_filename("a_very_long_filename_without_ext", 10);
        assert!(result.contains('…'));
    }

    #[test]
    fn folder_view_enumerates_and_queues_thumbs() {
        let dir = std::env::temp_dir().join(format!("iv_fv_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create real image files so thumbnails can decode
        let img = image::RgbImage::from_fn(100, 100, |_, _| image::Rgb([128, 64, 32]));
        img.save(dir.join("a.jpg")).unwrap();
        img.save(dir.join("b.png")).unwrap();
        std::fs::write(dir.join("c.txt"), b"not image").unwrap();

        let mut fv = FolderView::new(dir.clone());

        // Poll until enumeration done
        for _ in 0..100 {
            fv.poll_enumerator();
            if fv.is_done() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(fv.is_done());
        assert_eq!(fv.entry_count(), 2);

        // All entries should be in Loading state (queued for thumbnail)
        assert!(fv.entries.iter().all(|e| e.state == ThumbState::Loading));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
