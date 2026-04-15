// lib.rs — expose decode functions for examples/diagnostics
#![allow(dead_code)]

mod app;
mod decode;
mod enumerator;
mod folder_view;
mod image_view;
mod scheduler;

pub use app::DecodedImage;
pub use decode::{
    DecodeTimings, decode_from_bytes, decode_thumbnail, decode_thumbnail_progressive,
    extract_exif_thumbnail, is_heif_extension, try_exif_only, try_heif_thumbnail,
};
pub use enumerator::{EnumHandle, EnumMessage, enumerate_folder};
pub use scheduler::{EntryState, Scheduler, ThumbState, WorkItem, WorkTier};

/// Register HEIF/HEIC decoder hooks so the `image` crate can decode these formats.
/// Call once at startup before decoding any HEIC files.
pub fn register_heif_hooks() {
    libheif_rs::integration::image::register_all_decoding_hooks();
}
