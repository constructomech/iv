use std::env;
use std::path::PathBuf;
use std::process;

mod app;

fn main() {
    env_logger::init();

    let path = match env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("Usage: iv <image-path>");
            eprintln!("  Opens and displays the specified image.");
            process::exit(1);
        }
    };

    if !path.exists() {
        eprintln!("Error: path does not exist: {}", path.display());
        process::exit(1);
    }

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(format!(
                "iv — {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ))
            .with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "iv",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, path)))),
    ) {
        eprintln!("Error running iv: {e}");
        process::exit(1);
    }
}
