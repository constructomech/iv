use eframe::egui;
use std::path::PathBuf;

use crate::enumerator::{self, EnumHandle, EnumMessage};

/// Thumbnail tile size in the grid.
const TILE_SIZE: f32 = 160.0;
/// Padding between tiles.
const TILE_PADDING: f32 = 8.0;
/// Height reserved for the filename label below the tile.
const LABEL_HEIGHT: f32 = 20.0;
/// Full cell size including padding and label.
const CELL_SIZE: f32 = TILE_SIZE + TILE_PADDING + LABEL_HEIGHT;

/// State for the folder grid view.
pub struct FolderView {
    /// Directory being viewed.
    folder: PathBuf,
    /// Discovered image paths, in order found.
    entries: Vec<PathBuf>,
    /// Whether enumeration has completed.
    enum_done: bool,
    /// Total count reported by enumerator (set when done).
    enum_total: Option<usize>,
    /// Handle to the background enumerator.
    enum_handle: Option<EnumHandle>,
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
                        self.entries.push(path);
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

    /// Render the folder view. Returns Some(index) if a tile was clicked.
    pub fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> Option<usize> {
        self.poll_enumerator();

        // Request repaint while enumerating so new tiles appear
        if !self.enum_done {
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
        let status = if self.enum_done {
            format!(
                "{} — {} images",
                self.folder.display(),
                self.entries.len()
            )
        } else {
            format!(
                "{} — scanning... ({} found)",
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

                    for (idx, path) in self.entries.iter().enumerate() {
                        let _ = rows; // suppress unused warning
                        let response = self.render_tile(ui, idx, path);
                        if response.clicked() {
                            clicked_index = Some(idx);
                        }
                    }
                });
            });

        clicked_index
    }

    /// Render a single tile (placeholder + filename).
    fn render_tile(&self, ui: &mut egui::Ui, _idx: usize, path: &PathBuf) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(TILE_SIZE, CELL_SIZE),
            egui::Sense::click(),
        );

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);

            // Gray placeholder rectangle
            let thumb_rect = egui::Rect::from_min_size(rect.min, egui::vec2(TILE_SIZE, TILE_SIZE));

            // Hover highlight
            let bg_color = if response.hovered() {
                egui::Color32::from_rgb(70, 70, 70)
            } else {
                egui::Color32::from_rgb(48, 48, 48)
            };
            painter.rect_filled(thumb_rect, 2.0, bg_color);

            // Filename label below the thumbnail
            let filename = path
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

    /// Check if enumeration is done (for testing).
    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        self.enum_done
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
    fn folder_view_enumerates() {
        let dir = std::env::temp_dir().join(format!("iv_fv_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"fake").unwrap();
        std::fs::write(dir.join("b.png"), b"fake").unwrap();
        std::fs::write(dir.join("c.txt"), b"not image").unwrap();

        let mut fv = FolderView::new(dir.clone());

        // Poll until done
        for _ in 0..100 {
            fv.poll_enumerator();
            if fv.is_done() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(fv.is_done());
        assert_eq!(fv.entry_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
