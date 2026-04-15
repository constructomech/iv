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

    // --demo N: show a grid of N tiles (default 10000) with no real images
    let demo_mode = args.iter().any(|a| a == "--demo");
    let demo_count: usize = args
        .iter()
        .position(|a| a == "--demo")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    if demo_mode {
        let title = format!("iv — demo ({demo_count} tiles)");
        let native_options = eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([1280.0, 720.0]),
            ..Default::default()
        };

        if let Err(e) = eframe::run_native(
            "iv",
            native_options,
            Box::new(move |_cc| Ok(Box::new(DemoApp::new(demo_count)))),
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
            eprintln!("       iv --demo [count]");
            process::exit(1);
        }
    };

    if !path.exists() {
        eprintln!("Error: path does not exist: {}", path.display());
        process::exit(1);
    }

    let is_folder = path.is_dir();
    let title = if is_folder {
        format!(
            "iv — {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    } else {
        format!(
            "iv — {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
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
        Box::new(move |cc| {
            if is_folder {
                Ok(Box::new(app::App::new_folder(cc, path)))
            } else {
                Ok(Box::new(app::App::new_image(cc, path)))
            }
        }),
    ) {
        eprintln!("Error running iv: {e}");
        process::exit(1);
    }
}

/// Minimal app that shows a grid of tiles with no real images.
struct DemoApp {
    grid_view: grid_view::GridView,
}

impl DemoApp {
    fn new(count: usize) -> Self {
        Self {
            grid_view: grid_view::GridView::new_demo(count),
        }
    }
}

impl eframe::App for DemoApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default()
            .frame(
                eframe::egui::Frame::NONE
                    .fill(eframe::egui::Color32::from_rgb(30, 30, 30))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                self.grid_view.show(ctx, ui);
            });
    }
}
