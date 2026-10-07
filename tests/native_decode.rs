use image::{ImageFormat, Rgb, RgbImage};
use iv::{
    DecodedImage, decode_from_bytes, decode_raw_libraw, decode_video_thumbnail,
    encode_heif_av1_rgb_file, try_heif_thumbnail, try_heif_thumbnail_from_bytes,
};

fn assert_rgba(image: &DecodedImage, width: u32, height: u32) {
    assert_eq!((image.width, image.height), (width, height));
    assert_eq!(image.pixels.len(), (width * height * 4) as usize);
    assert!(image.pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
}

#[test]
fn native_ffmpeg_decodes_frames_across_workers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frames.gif");
    RgbImage::from_pixel(64, 48, Rgb([50, 200, 100]))
        .save_with_format(&path, ImageFormat::Gif)
        .unwrap();

    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let frame =
                    decode_video_thumbnail(&path, 32).expect("FFmpeg should decode the GIF");
                assert_rgba(&frame, 32, 24);
                assert!(frame.pixels.chunks_exact(4).all(|pixel| pixel[1] > 150));
            });
        }
    });
}

#[test]
fn native_heif_round_trips_image_and_thumbnail_across_workers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.heic");
    let image = RgbImage::from_pixel(128, 96, Rgb([50, 200, 100]));
    encode_heif_av1_rgb_file(&path, image.as_raw(), 128, 96, 80, 60, 32)
        .expect("libheif should encode an AV1 image with a thumbnail");
    let data = std::fs::read(&path).unwrap();

    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let thumb =
                    try_heif_thumbnail(&path).expect("libheif should decode file thumbnail");
                assert_rgba(&thumb, 32, 24);
                let thumb = try_heif_thumbnail_from_bytes(&data)
                    .expect("libheif should decode memory thumbnail");
                assert_rgba(&thumb, 32, 24);
                let (full, _) =
                    decode_from_bytes(&data, 128, true).expect("libheif should decode full image");
                assert_rgba(&full, 128, 96);
            });
        }
    });
}

#[test]
fn native_libraw_rejects_invalid_data() {
    assert!(decode_raw_libraw(b"not a raw camera image").is_none());
}
