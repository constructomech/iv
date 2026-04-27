#![allow(dead_code)]

use rust_i18n::t;
use std::env;
use std::path::PathBuf;
use std::process;

use eframe::egui;

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod decode;
mod develop;
mod enumerator;
mod folder_tree;
mod grid;
mod grid_view;
mod image_view;

fn main() {
    env_logger::init();

    // Register HEIF/HEIC decoder so the `image` crate can decode these formats.
    libheif_rs::integration::image::register_all_decoding_hooks();

    let args: Vec<String> = env::args().collect();
    let log_enabled = args.iter().any(|a| a == "--log");

    // Find the path argument (skip flags)
    let path = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);

    let path = match path {
        Some(p) => p,
        None => {
            eprintln!("{}", t!("usage"));
            process::exit(1);
        }
    };

    if !path.exists() {
        eprintln!(
            "{}",
            t!("error.path_not_found", path = path.display().to_string())
        );
        process::exit(1);
    }

    let is_folder = path.is_dir();
    let title = t!(
        "window.title",
        name = path.file_name().unwrap_or_default().to_string_lossy()
    );

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "iv",
        native_options,
        Box::new(move |_cc| {
            if is_folder {
                Ok(Box::new(IvApp::new_folder(path, log_enabled)))
            } else {
                Ok(Box::new(IvApp::new_image(path, log_enabled)))
            }
        }),
    ) {
        eprintln!("{}", t!("error.app_failed", err = e.to_string()));
        process::exit(1);
    }
}

/// What the app is viewing.
enum AppMode {
    /// Grid/folder view.
    Grid,
    /// Full-resolution image view.
    Image(Box<image_view::ImageView>),
}

/// The iv application.
struct IvApp {
    grid_view: grid_view::GridView,
    folder_tree: folder_tree::FolderTree,
    enum_handle: Option<enumerator::EnumHandle>,
    enum_done: bool,
    current_folder: PathBuf,
    folder_pane_open: bool,
    log_enabled: bool,
    mode: AppMode,
}

impl IvApp {
    fn new_folder(path: PathBuf, log_enabled: bool) -> Self {
        let grid = Self::new_grid(log_enabled);
        Self {
            grid_view: grid_view::GridView::new(grid),
            folder_tree: folder_tree::FolderTree::new(path.clone()),
            enum_handle: Some(enumerator::enumerate_folder(path.clone())),
            enum_done: false,
            current_folder: path,
            folder_pane_open: false,
            log_enabled,
            mode: AppMode::Grid,
        }
    }

    fn new_image(path: PathBuf, log_enabled: bool) -> Self {
        let mut grid = Self::new_grid(log_enabled);
        let idx = grid.add_tile_with_path(path);
        let paths = grid.all_paths();
        let current_folder = paths
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            grid_view: grid_view::GridView::new(grid),
            folder_tree: folder_tree::FolderTree::new(current_folder.clone()),
            enum_handle: None,
            enum_done: true,
            current_folder,
            folder_pane_open: false,
            log_enabled,
            mode: AppMode::Image(Box::new(image_view::ImageView::new(paths, idx))),
        }
    }

    fn new_grid(log_enabled: bool) -> grid::Grid {
        let mut grid = grid::Grid::new(grid::GridConfig::default());
        if log_enabled {
            grid.enable_logging();
        }
        grid
    }

    fn open_folder(&mut self, path: PathBuf) {
        if path == self.current_folder {
            return;
        }
        let sort_mode = self.grid_view.grid().sort_mode();
        let mut grid = Self::new_grid(self.log_enabled);
        grid.set_sort_mode(sort_mode);
        self.grid_view.replace_grid(grid);
        self.enum_handle = Some(enumerator::enumerate_folder(path.clone()));
        self.enum_done = false;
        self.current_folder = path.clone();
        self.folder_tree.set_selected(path);
        self.mode = AppMode::Grid;
    }

    fn poll_enumerator(&mut self) {
        if let Some(ref handle) = self.enum_handle {
            loop {
                match handle.receiver.try_recv() {
                    Ok(enumerator::EnumMessage::Found(path)) => {
                        self.grid_view.grid_mut().add_tile_with_path(path);
                    }
                    Ok(enumerator::EnumMessage::Done(_)) => {
                        self.enum_done = true;
                        break;
                    }
                    Ok(enumerator::EnumMessage::Error(e)) => {
                        log::error!("Enumeration error: {e}");
                        self.enum_done = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.enum_done = true;
                        break;
                    }
                }
            }
        }
        if self.enum_done {
            self.enum_handle = None;
        }
    }

    fn show_grid_mode(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        if self.folder_pane_open {
            egui::SidePanel::left("iv_folder_tree_pane")
                .resizable(true)
                .default_width(260.0)
                .width_range(180.0..=420.0)
                .show_inside(ui, |ui| {
                    self.show_folder_pane(ui);
                });
        }

        self.show_folder_bar(ui);

        if let Some(clicked_idx) = self.grid_view.show(ctx, ui) {
            let paths = self.grid_view.grid().all_paths();
            if !paths.is_empty() && clicked_idx < paths.len() {
                self.mode =
                    AppMode::Image(Box::new(image_view::ImageView::new(paths, clicked_idx)));
            }
        }
    }

    fn show_folder_pane(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Folders")
                    .color(egui::Color32::from_rgb(210, 210, 210))
                    .size(14.0)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_sized([22.0, 20.0], egui::Button::new("‹"))
                    .on_hover_text("Hide folders")
                    .clicked()
                {
                    self.folder_pane_open = false;
                }
            });
        });
        ui.separator();
        if let Some(path) = self.folder_tree.show(ui) {
            self.open_folder(path);
        }
    }

    fn show_folder_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if !self.folder_pane_open
                && ui
                    .add_sized([22.0, 20.0], egui::Button::new("›"))
                    .on_hover_text("Show folders")
                    .clicked()
            {
                self.folder_pane_open = true;
            }
            ui.label(
                egui::RichText::new(self.current_folder.display().to_string())
                    .color(egui::Color32::from_rgb(160, 160, 160))
                    .size(12.0),
            );
        });
        ui.add_space(4.0);
    }
}

impl eframe::App for IvApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.poll_enumerator();

        if !self.enum_done {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        eframe::egui::CentralPanel::default()
            .frame(
                eframe::egui::Frame::NONE
                    .fill(eframe::egui::Color32::from_rgb(30, 30, 30))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| match &mut self.mode {
                AppMode::Grid => {
                    self.show_grid_mode(ctx, ui);
                }
                AppMode::Image(view) => {
                    let go_back = view.show(ctx, ui);
                    if go_back {
                        self.mode = AppMode::Grid;
                    }
                }
            });
    }
}

impl Drop for IvApp {
    fn drop(&mut self) {
        let log_path = std::env::temp_dir().join("iv_grid_log.json");
        if let Some(path) = self.grid_view.grid().dump_log(&log_path) {
            eprintln!("{}", t!("log.written", path = path.display().to_string()));
        }
    }
}
