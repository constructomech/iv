// lib.rs — expose decode functions for examples/diagnostics
#![allow(dead_code)]

mod app;
mod decode;
mod enumerator;
mod folder_view;
mod scheduler;

pub use app::DecodedImage;
pub use decode::{decode_thumbnail, decode_thumbnail_progressive, extract_exif_thumbnail};
