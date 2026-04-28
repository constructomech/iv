use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

impl MediaKind {
    pub fn is_image(self) -> bool {
        self == Self::Image
    }

    pub fn is_video(self) -> bool {
        self == Self::Video
    }
}

/// Recognized image file extensions (lowercase, without dot).
pub const IMAGE_EXTENSIONS: &[&str] = &[
    // Common raster
    "jpg", "jpeg", "png", "webp", "tiff", "tif", "bmp", "gif", // HEIF/HEIC
    "heic", "heif", // RAW (first-class — we extract embedded JPEG previews)
    "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf",
];

/// Recognized video file extensions (lowercase, without dot).
pub const VIDEO_EXTENSIONS: &[&str] = &["mov", "mp4", "webm"];

pub fn media_kind_for_path(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

pub fn is_image_file(path: &Path) -> bool {
    media_kind_for_path(path) == Some(MediaKind::Image)
}

pub fn is_video_file(path: &Path) -> bool {
    media_kind_for_path(path) == Some(MediaKind::Video)
}

pub fn is_media_file(path: &Path) -> bool {
    media_kind_for_path(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_detection_is_case_insensitive() {
        assert!(is_image_file(Path::new("photo.jpg")));
        assert!(is_image_file(Path::new("photo.JPEG")));
        assert!(is_image_file(Path::new("photo.Jpg")));
    }

    #[test]
    fn video_detection_is_case_insensitive() {
        assert!(is_video_file(Path::new("clip.mov")));
        assert!(is_video_file(Path::new("clip.MOV")));
        assert!(is_video_file(Path::new("clip.Mp4")));
        assert!(is_video_file(Path::new("clip.WEBM")));
    }

    #[test]
    fn unsupported_media_is_rejected() {
        assert!(!is_media_file(Path::new("readme.txt")));
        assert!(!is_media_file(Path::new("photo.xmp")));
        assert!(!is_media_file(Path::new("noext")));
    }
}
