// lib.rs — expose decode functions for examples/diagnostics
#![allow(dead_code)]

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod decode;
mod enumerator;
pub mod grid;
mod grid_view;
mod image_view;

pub use app::{DecodedImage, load_image};
pub use decode::{
    DecodeTimings, ExifMetadata, ProbeResult, decode_for_upscale, decode_from_bytes,
    decode_raw_libraw, decode_thumbnail, decode_thumbnail_progressive, extract_exif_thumbnail,
    is_heif_extension, is_raw_extension, load_raw_preview, needs_upscale, probe_embedded_thumbnail,
    read_date_taken, read_exif_metadata, try_embedded_from_bytes, try_exif_only,
    try_heif_thumbnail, try_heif_thumbnail_from_bytes,
};
pub use enumerator::{EnumHandle, EnumMessage, enumerate_folder};
pub use grid::{Grid, GridConfig, GridEvent, GridEventKind, SortMode, TileState, VisibleRows};

/// Register HEIF/HEIC decoder hooks so the `image` crate can decode these formats.
/// Call once at startup before decoding any HEIC files.
pub fn register_heif_hooks() {
    libheif_rs::integration::image::register_all_decoding_hooks();
}
