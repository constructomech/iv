use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

fn pending_launches() -> &'static Mutex<HashSet<PathBuf>> {
    static PENDING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashSet::new()))
}

pub fn open_with_default_app(path: impl AsRef<Path>) {
    let path = path.as_ref().to_path_buf();
    let mut pending = pending_launches().lock().expect("launcher mutex poisoned");
    if !pending.insert(path.clone()) {
        return;
    }
    drop(pending);

    std::thread::spawn(move || {
        if let Err(err) = open::that(&path) {
            log::error!(
                "Failed to open {} with OS default app: {err}",
                path.display()
            );
        }
        if let Ok(mut pending) = pending_launches().lock() {
            pending.remove(&path);
        }
    });
}
