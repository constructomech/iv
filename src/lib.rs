// lib.rs — expose decode functions for examples/diagnostics
#![allow(dead_code)]

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod decode;
mod develop;
mod enumerator;
pub mod grid;
mod grid_view;
mod image_view;
mod launcher;
pub mod load_bench;
pub mod media;

pub use app::{DecodedImage, load_image};
pub use decode::{
    DecodeTimings, ExifMetadata, ProbeResult, decode_for_upscale, decode_from_bytes,
    decode_raw_libraw, decode_thumbnail, decode_thumbnail_progressive, decode_video_thumbnail,
    encode_heif_av1_rgb_file, extract_exif_thumbnail, is_heif_extension, is_raw_extension,
    load_raw_preview, needs_upscale, probe_embedded_thumbnail, read_date_taken,
    read_date_taken_from_path, read_exif_metadata, read_exif_metadata_from_path,
    try_embedded_from_bytes, try_exif_only, try_heif_thumbnail, try_heif_thumbnail_from_bytes,
};
pub use develop::{
    XmpDevelopSetting, XmpDevelopSettings, XmpDevelopSource, apply_xmp_develop_settings,
    read_xmp_develop_settings_for_image, read_xmp_develop_settings_from_path,
};
pub use enumerator::{EnumHandle, EnumMessage, enumerate_folder};
pub use grid::{Grid, GridConfig, GridEvent, GridEventKind, SortMode, TileState, VisibleRows};
pub use grid_view::thumbnail_decode_worker_count;
pub use media::{
    IMAGE_EXTENSIONS, MediaKind, VIDEO_EXTENSIONS, is_image_file, is_media_file, is_video_file,
    media_kind_for_path,
};
