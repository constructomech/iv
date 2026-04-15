use std::env;
use std::path::PathBuf;
use std::process;

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

    // --demo [N | path]: show a grid with synthetic tiles or enumerate a real folder
    let demo_mode = args.iter().any(|a| a == "--demo");

    if demo_mode {
        let demo_arg = args
            .iter()
            .position(|a| a == "--demo")
            .and_then(|i| args.get(i + 1));

        // Is the argument a number (synthetic) or a path (real folder)?
        let (title, demo_source) = match demo_arg {
            Some(s) if s.parse::<usize>().is_ok() => {
                let count = s.parse::<usize>().unwrap();
                (
                    format!("iv — demo ({count} tiles)"),
                    DemoSource::Synthetic(count),
                )
            }
            Some(s) => {
                let p = PathBuf::from(s);
                if !p.is_dir() {
                    eprintln!("Error: not a directory: {}", p.display());
                    process::exit(1);
                }
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                (format!("iv — {name}"), DemoSource::Folder(p))
            }
            None => (
                "iv — demo (10000 tiles)".to_string(),
                DemoSource::Synthetic(10_000),
            ),
        };

        let native_options = eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([1280.0, 720.0]),
            ..Default::default()
        };

        if let Err(e) = eframe::run_native(
            "iv",
            native_options,
            Box::new(move |_cc| Ok(Box::new(DemoApp::new(demo_source)))),
        ) {
            eprintln!("Error running iv: {e}");
            process::exit(1);
        }
        return;
    }

    let path = match args.get(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("Usage: iv <image-or-folder-path>");
            eprintln!("       iv --demo [count | path]");
            process::exit(1);
        }
    };

    if !path.exists() {
        eprintln!("Error: path does not exist: {}", path.display());
        process::exit(1);
    }

    let is_folder = path.is_dir();
    let title = format!(
        "iv — {}",
        path.file_name().unwrap_or_default().to_string_lossy()
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
                Ok(Box::new(DemoApp::new(DemoSource::Folder(path))))
            } else {
                // Single image: use the old app for now
                Ok(Box::new(app::App::new_image(_cc, path)))
            }
        }),
    ) {
        eprintln!("Error running iv: {e}");
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

/// What the demo app is showing.
enum DemoSource {
    /// Synthetic grid with N tiles.
    Synthetic(usize),
    /// Enumerate a real folder.
    Folder(PathBuf),
}

/// App that shows a grid of tiles with image view support.
struct DemoApp {
    grid_view: grid_view::GridView,
    enum_handle: Option<enumerator::EnumHandle>,
    enum_done: bool,
    mode: AppMode,
}

impl DemoApp {
    fn new(source: DemoSource) -> Self {
        match source {
            DemoSource::Synthetic(count) => Self {
                grid_view: grid_view::GridView::new_demo(count),
                enum_handle: None,
                enum_done: true,
                mode: AppMode::Grid,
            },
            DemoSource::Folder(path) => {
                let mut grid = grid::Grid::new(grid::GridConfig::default());
                if std::env::var("IV_DEBUG").map_or(false, |v| v == "1") {
                    grid.enable_logging();
                }
                Self {
                    grid_view: grid_view::GridView::new(grid),
                    enum_handle: Some(enumerator::enumerate_folder(path)),
                    enum_done: false,
                    mode: AppMode::Grid,
                }
            }
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

impl eframe::App for DemoApp {
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
