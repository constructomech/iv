use rust_i18n::t;
use std::env;
use std::path::PathBuf;
use std::process;

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod decode;
mod enumerator;
mod folder_view;
mod grid;
mod grid_view;
mod image_view;
mod scheduler;

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
        .map(|p| PathBuf::from(p));

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
    Image(image_view::ImageView),
}

/// The iv application.
struct IvApp {
    grid_view: grid_view::GridView,
    enum_handle: Option<enumerator::EnumHandle>,
    enum_done: bool,
    mode: AppMode,
}

impl IvApp {
    fn new_folder(path: PathBuf, log_enabled: bool) -> Self {
        let mut grid = grid::Grid::new(grid::GridConfig::default());
        if log_enabled {
            grid.enable_logging();
        }
        Self {
            grid_view: grid_view::GridView::new(grid),
            enum_handle: Some(enumerator::enumerate_folder(path)),
            enum_done: false,
            mode: AppMode::Grid,
        }
    }

    fn new_image(path: PathBuf, _log_enabled: bool) -> Self {
        let mut grid = grid::Grid::new(grid::GridConfig::default());
        let idx = grid.add_tile_with_path(path);
        let paths = grid.all_paths().to_vec();
        Self {
            grid_view: grid_view::GridView::new(grid),
            enum_handle: None,
            enum_done: true,
            mode: AppMode::Image(image_view::ImageView::new(paths, idx)),
        }
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
                    if let Some(clicked_idx) = self.grid_view.show(ctx, ui) {
                        let paths: Vec<PathBuf> = self.grid_view.grid().all_paths().to_vec();
                        if !paths.is_empty() && clicked_idx < paths.len() {
                            self.mode =
                                AppMode::Image(image_view::ImageView::new(paths, clicked_idx));
                        }
                    }
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
