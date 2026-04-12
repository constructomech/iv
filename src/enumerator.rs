use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use crate::app::is_image_file;

/// Messages sent from the enumerator thread to the UI.
pub enum EnumMessage {
    /// A new image file was discovered.
    Found(PathBuf),
    /// Enumeration is complete. Contains total count.
    Done(usize),
    /// An error occurred during enumeration.
    Error(String),
}

/// Handle to a running enumeration. The receiver yields `EnumMessage`s.
pub struct EnumHandle {
    pub receiver: mpsc::Receiver<EnumMessage>,
}

/// Start enumerating image files in `folder` on a background thread.
/// Returns immediately with a handle to receive results.
pub fn enumerate_folder(folder: PathBuf) -> EnumHandle {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        enumerate_inner(&folder, &tx);
    });

    EnumHandle { receiver: rx }
}

fn enumerate_inner(folder: &Path, tx: &mpsc::Sender<EnumMessage>) {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(e) => {
            let _ = tx.send(EnumMessage::Error(format!(
                "Failed to read directory {}: {e}",
                folder.display()
            )));
            return;
        }
    };

    let mut count = 0usize;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("Skipping directory entry: {e}");
                continue;
            }
        };

        let path = entry.path();

        // Skip directories and non-image files
        if path.is_file() && is_image_file(&path) {
            count += 1;
            if tx.send(EnumMessage::Found(path)).is_err() {
                return; // Receiver dropped, app is shutting down
            }
        }
    }

    let _ = tx.send(EnumMessage::Done(count));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iv_enum_test_{name}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn enumerates_image_files() {
        let dir = make_test_dir("basic");
        // Create some image files
        fs::write(dir.join("photo.jpg"), b"fake jpg").unwrap();
        fs::write(dir.join("icon.png"), b"fake png").unwrap();
        fs::write(dir.join("readme.txt"), b"not an image").unwrap();
        fs::write(dir.join("video.mp4"), b"not an image").unwrap();

        let handle = enumerate_folder(dir.clone());
        let mut found = Vec::new();
        let mut done_count = None;

        for msg in handle.receiver {
            match msg {
                EnumMessage::Found(p) => found.push(p),
                EnumMessage::Done(c) => {
                    done_count = Some(c);
                    break;
                }
                EnumMessage::Error(e) => panic!("unexpected error: {e}"),
            }
        }

        assert_eq!(found.len(), 2);
        assert_eq!(done_count, Some(2));
        let names: Vec<_> = found.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
        assert!(names.contains(&"photo.jpg".to_string()));
        assert!(names.contains(&"icon.png".to_string()));

        cleanup(&dir);
    }

    #[test]
    fn enumerates_raw_files() {
        let dir = make_test_dir("raw");
        fs::write(dir.join("IMG_001.CR2"), b"fake cr2").unwrap();
        fs::write(dir.join("IMG_002.DNG"), b"fake dng").unwrap();
        fs::write(dir.join("IMG_003.NEF"), b"fake nef").unwrap();

        let handle = enumerate_folder(dir.clone());
        let mut found = Vec::new();

        for msg in handle.receiver {
            match msg {
                EnumMessage::Found(p) => found.push(p),
                EnumMessage::Done(_) => break,
                EnumMessage::Error(e) => panic!("unexpected error: {e}"),
            }
        }

        assert_eq!(found.len(), 3);
        cleanup(&dir);
    }

    #[test]
    fn empty_folder_produces_done_zero() {
        let dir = make_test_dir("empty");

        let handle = enumerate_folder(dir.clone());
        let mut got_done = false;

        for msg in handle.receiver {
            match msg {
                EnumMessage::Found(_) => panic!("should find nothing"),
                EnumMessage::Done(c) => {
                    assert_eq!(c, 0);
                    got_done = true;
                    break;
                }
                EnumMessage::Error(e) => panic!("unexpected error: {e}"),
            }
        }

        assert!(got_done);
        cleanup(&dir);
    }

    #[test]
    fn nonexistent_folder_produces_error() {
        let handle = enumerate_folder(PathBuf::from("/this/path/does/not/exist/at/all"));
        let mut got_error = false;

        for msg in handle.receiver {
            match msg {
                EnumMessage::Error(_) => {
                    got_error = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(got_error);
    }

    #[test]
    fn skips_subdirectories() {
        let dir = make_test_dir("subdirs");
        fs::write(dir.join("photo.jpg"), b"fake").unwrap();
        fs::create_dir_all(dir.join("subdir")).unwrap();
        fs::write(dir.join("subdir").join("nested.jpg"), b"fake").unwrap();

        let handle = enumerate_folder(dir.clone());
        let mut found = Vec::new();

        for msg in handle.receiver {
            match msg {
                EnumMessage::Found(p) => found.push(p),
                EnumMessage::Done(_) => break,
                EnumMessage::Error(e) => panic!("unexpected error: {e}"),
            }
        }

        // Only top-level files, not subdirectory contents
        assert_eq!(found.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn case_insensitive_extensions() {
        let dir = make_test_dir("caseext");
        fs::write(dir.join("a.JPG"), b"fake").unwrap();
        fs::write(dir.join("b.Png"), b"fake").unwrap();
        fs::write(dir.join("c.TIFF"), b"fake").unwrap();

        let handle = enumerate_folder(dir.clone());
        let mut found = Vec::new();

        for msg in handle.receiver {
            match msg {
                EnumMessage::Found(p) => found.push(p),
                EnumMessage::Done(_) => break,
                EnumMessage::Error(e) => panic!("unexpected error: {e}"),
            }
        }

        assert_eq!(found.len(), 3);
        cleanup(&dir);
    }
}
