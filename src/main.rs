use std::env;
use std::path::PathBuf;
use std::process;

mod app;
mod decode;
mod enumerator;
mod folder_view;
mod image_view;
mod scheduler;

fn main() {
    env_logger::init();

    let path = match env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("Usage: iv <image-or-folder-path>");
            eprintln!("  Opens and displays an image, or browses a folder of images.");
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
