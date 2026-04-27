use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

/// Lazy, in-memory folder tree for browsing the filesystem without a catalog.
pub struct FolderTree {
    root: FolderNode,
    selected: PathBuf,
    filter: String,
}

struct FolderNode {
    path: PathBuf,
    expanded: bool,
    state: FolderLoadState,
}

struct FolderEntry {
    path: PathBuf,
    has_child_folders: bool,
}

enum FolderLoadState {
    Unknown,
    Loading(mpsc::Receiver<Result<Vec<FolderEntry>, String>>),
    Loaded(Vec<FolderNode>),
    Error(String),
}

impl FolderTree {
    pub fn new(root: PathBuf) -> Self {
        let selected = root.clone();
        let mut root = FolderNode::new(root);
        root.expanded = true;
        Self {
            root,
            selected,
            filter: String::new(),
        }
    }

    pub fn set_selected(&mut self, path: PathBuf) {
        self.selected = path;
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        self.root.poll_loads(ui.ctx());
        let mut selected: Option<PathBuf> = None;
        let selected_path = self.selected.clone();

        ui.add_sized(
            [ui.available_width(), 22.0],
            egui::TextEdit::singleline(&mut self.filter).hint_text("Search folders"),
        );
        ui.add_space(4.0);

        let filter = normalized_filter(&self.filter);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Self::show_node(
                    ui,
                    &mut self.root,
                    &selected_path,
                    filter.as_deref(),
                    true,
                    &mut selected,
                );
            });
        if let Some(path) = &selected {
            self.selected = path.clone();
        }
        selected
    }

    fn show_node(
        ui: &mut egui::Ui,
        node: &mut FolderNode,
        selected_path: &Path,
        filter: Option<&str>,
        force_visible: bool,
        selected: &mut Option<PathBuf>,
    ) {
        if !force_visible && !node.is_visible_for_filter(filter) {
            return;
        }

        if node.is_leaf() {
            if Self::folder_row(ui, node, selected_path, Self::disclosure_gutter(ui)).clicked() {
                *selected = Some(node.path.clone());
            }
            return;
        }

        let id = ui.make_persistent_id(("folder_tree_node", &node.path));
        let force_open = filter.is_some_and(|filter| node.has_loaded_descendant_match(filter));
        let mut header = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            id,
            node.expanded,
        )
        .show_header(ui, |ui| Self::folder_row(ui, node, selected_path, 0.0));
        if force_open {
            header.set_open(true);
        }

        node.expanded = header.is_open();
        if node.expanded {
            node.ensure_loading();
        }

        let (_, header_response, _) = header.body(|ui| match &mut node.state {
            FolderLoadState::Unknown => {}
            FolderLoadState::Loading(_) => {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
                ui.add(egui::Spinner::new().size(12.0));
            }
            FolderLoadState::Loaded(children) => {
                for child in children {
                    Self::show_node(ui, child, selected_path, filter, false, selected);
                }
            }
            FolderLoadState::Error(error) => {
                ui.label(
                    egui::RichText::new(error.as_str())
                        .color(egui::Color32::from_rgb(220, 80, 80))
                        .size(11.0),
                );
            }
        });

        if header_response.inner.clicked() {
            *selected = Some(node.path.clone());
        }
    }

    fn folder_row(
        ui: &mut egui::Ui,
        node: &FolderNode,
        selected_path: &Path,
        leading_space: f32,
    ) -> egui::Response {
        let is_selected = same_path(&node.path, selected_path);
        let height = 22.0;
        let width = ui.available_width().max(80.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());

        let bg = if is_selected {
            egui::Color32::from_rgb(54, 86, 116)
        } else if response.hovered() {
            egui::Color32::from_rgb(42, 42, 42)
        } else {
            egui::Color32::TRANSPARENT
        };
        if bg != egui::Color32::TRANSPARENT {
            ui.painter().rect_filled(rect.shrink(1.0), 3.0, bg);
        }

        let text_color = if is_selected {
            egui::Color32::from_rgb(240, 240, 240)
        } else {
            egui::Color32::from_rgb(185, 185, 185)
        };
        ui.painter().text(
            egui::pos2(rect.left() + leading_space + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            node.display_name(),
            egui::FontId::proportional(13.0),
            text_color,
        );

        response.on_hover_text(node.path.display().to_string())
    }

    fn disclosure_gutter(ui: &egui::Ui) -> f32 {
        ui.spacing().icon_width + ui.spacing().icon_spacing
    }
}

impl FolderNode {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            expanded: false,
            state: FolderLoadState::Unknown,
        }
    }

    fn from_entry(entry: FolderEntry) -> Self {
        Self {
            path: entry.path,
            expanded: false,
            state: if entry.has_child_folders {
                FolderLoadState::Unknown
            } else {
                FolderLoadState::Loaded(Vec::new())
            },
        }
    }

    fn is_leaf(&self) -> bool {
        matches!(&self.state, FolderLoadState::Loaded(children) if children.is_empty())
    }

    fn is_visible_for_filter(&self, filter: Option<&str>) -> bool {
        let Some(filter) = filter else {
            return true;
        };
        self.matches_filter(filter) || self.has_loaded_descendant_match(filter)
    }

    fn matches_filter(&self, filter: &str) -> bool {
        self.display_name().to_lowercase().contains(filter)
    }

    fn has_loaded_descendant_match(&self, filter: &str) -> bool {
        match &self.state {
            FolderLoadState::Loaded(children) => children.iter().any(|child| {
                child.matches_filter(filter) || child.has_loaded_descendant_match(filter)
            }),
            _ => false,
        }
    }

    fn display_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.path.display().to_string())
    }

    fn ensure_loading(&mut self) {
        if !matches!(self.state, FolderLoadState::Unknown) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let path = self.path.clone();
        thread::spawn(move || {
            let _ = tx.send(list_child_folders(&path));
        });
        self.state = FolderLoadState::Loading(rx);
    }

    fn poll_loads(&mut self, ctx: &egui::Context) {
        let loaded = match &mut self.state {
            FolderLoadState::Loading(rx) => match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(50));
                    None
                }
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("Folder scan stopped".into())),
            },
            FolderLoadState::Loaded(children) => {
                for child in children {
                    child.poll_loads(ctx);
                }
                None
            }
            _ => None,
        };

        if let Some(result) = loaded {
            self.state = match result {
                Ok(entries) => FolderLoadState::Loaded(
                    entries.into_iter().map(FolderNode::from_entry).collect(),
                ),
                Err(error) => FolderLoadState::Error(error),
            };
            ctx.request_repaint();
        }
    }
}

fn list_child_folders(path: &Path) -> Result<Vec<FolderEntry>, String> {
    let entries =
        std::fs::read_dir(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let mut folders = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            let path = entry.path();
            folders.push(FolderEntry {
                has_child_folders: has_child_folders(&path),
                path,
            });
        }
    }
    folders.sort_by_key(|entry| {
        entry
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });
    Ok(folders)
}

fn has_child_folders(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return true;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            return true;
        }
    }
    false
}

fn same_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn normalized_filter(filter: &str) -> Option<String> {
    let filter = filter.trim();
    if filter.is_empty() {
        None
    } else {
        Some(filter.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn list_child_folders_returns_only_directories_sorted() {
        let dir = std::env::temp_dir().join(format!("iv_folder_tree_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("zeta")).unwrap();
        fs::create_dir_all(dir.join("Alpha").join("nested")).unwrap();
        fs::write(dir.join("image.jpg"), b"not a folder").unwrap();

        let folders = list_child_folders(&dir).unwrap();
        let names: Vec<_> = folders
            .iter()
            .filter_map(|entry| entry.path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .collect();

        assert_eq!(names, vec!["Alpha", "zeta"]);
        assert!(folders[0].has_child_folders);
        assert!(!folders[1].has_child_folders);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalized_filter_trims_and_lowercases() {
        assert_eq!(normalized_filter("  Little Si  "), Some("little si".into()));
        assert_eq!(normalized_filter("   "), None);
    }

    #[test]
    fn folder_filter_matches_names_and_loaded_descendants() {
        let mut parent = FolderNode::new(PathBuf::from("2002"));
        parent.state = FolderLoadState::Loaded(vec![
            FolderNode::new(PathBuf::from("2002-05-12 Little Si")),
            FolderNode::new(PathBuf::from("2002-05-18 Rattlesnake Ledge")),
        ]);

        assert!(parent.is_visible_for_filter(Some("little")));
        assert!(!parent.is_visible_for_filter(Some("flowers")));

        if let FolderLoadState::Loaded(children) = &parent.state {
            assert!(children[0].is_visible_for_filter(Some("little")));
            assert!(!children[1].is_visible_for_filter(Some("little")));
        } else {
            panic!("expected loaded children");
        }
    }
}
