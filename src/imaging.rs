//! Bounded image decoding for everything dak draws from outside data: `image` files,
//! `image_exec` program output and the colour bitmaps of configured fonts.
//!
//! A button is 60x60 pixels, but a compressed image may claim almost any size: a PNG of
//! a few hundred bytes can declare 16000x16000 pixels and make an unbounded decoder
//! allocate gigabytes (and then spend seconds resizing them). Every decode therefore goes
//! through [`decode`]/[`open`], which refuse images larger than [`MAX_DIMENSION`] on a side
//! or needing more than [`MAX_ALLOC`] bytes, and [`shrink`] brings what passes down to at
//! most [`WORKING_SIZE`] before any further processing. Only the formats enabled in
//! `Cargo.toml` are decoded at all (see [`FORMATS`]).

use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageFormat, ImageReader, ImageResult, Limits};

/// The largest width or height an image may declare.
pub const MAX_DIMENSION: u32 = 4096;

/// The most memory one decode may allocate, in bytes.
pub const MAX_ALLOC: u64 = 96 * 1024 * 1024;

/// The size (on either side) decoded images are shrunk to before compositing and
/// resizing: a few times the 60x60 button, so the final resize still has detail to work
/// with, but small enough that the extra copies cost next to nothing.
pub const WORKING_SIZE: u32 = 240;

/// The image formats dak decodes, as named in the docs. `tests/man_pages.rs` checks
/// `dak-config.5` lists every one, and a unit test checks each is really enabled.
pub const FORMATS: &[&str] = &["PNG", "JPEG", "GIF", "BMP", "ICO", "WebP", "PNM"];

/// The decoding limits: [`MAX_DIMENSION`] and [`MAX_ALLOC`].
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    limits
}

/// Decodes an image of any enabled format from `bytes`, within the limits, and shrinks
/// it to at most [`WORKING_SIZE`].
pub fn decode(bytes: &[u8]) -> ImageResult<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(limits());
    Ok(shrink(reader.decode()?))
}

/// Decodes `bytes` as an image of the given `format`, within the limits, and shrinks it
/// to at most [`WORKING_SIZE`].
pub fn decode_as(bytes: &[u8], format: ImageFormat) -> ImageResult<DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits());
    Ok(shrink(reader.decode()?))
}

/// Opens and decodes the image file at `path` (format from its content, else its
/// extension), within the limits, and shrinks it to at most [`WORKING_SIZE`]. Blocking:
/// call it off the async runtime's worker threads.
pub fn open(path: &Path) -> ImageResult<DynamicImage> {
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    reader.limits(limits());
    Ok(shrink(reader.decode()?))
}

/// `image` unchanged when it fits in [`WORKING_SIZE`], else scaled down (keeping its
/// aspect ratio) until it does.
pub fn shrink(image: DynamicImage) -> DynamicImage {
    if image.width() <= WORKING_SIZE && image.height() <= WORKING_SIZE {
        image
    } else {
        image.thumbnail(WORKING_SIZE, WORKING_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, RgbImage};

    /// A tiny valid PNG claiming `width` x `height` pixels: the header is honest, the
    /// data is not, which is all a size bomb needs.
    fn png_claiming(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        // IHDR data starts at byte 16: width, height (big endian), then the CRC at 29.
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    /// CRC-32 (IEEE), as PNG chunks use.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Encodes `image` in `format`.
    fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    /// An image declaring a side larger than the limit is refused before any pixel
    /// buffer is allocated.
    #[test]
    fn oversized_images_are_refused() {
        for (width, height) in [(16000, 16000), (MAX_DIMENSION + 1, 1), (1, 100_000)] {
            let error = decode(&png_claiming(width, height)).unwrap_err();
            assert!(
                matches!(error, image::ImageError::Limits(_)),
                "{width}x{height}: {error:?}"
            );
        }
    }

    /// Big but allowed images decode and come back shrunk to the working size, keeping
    /// their aspect ratio; small ones are untouched.
    #[test]
    fn large_images_are_shrunk_small_ones_kept() {
        let big = DynamicImage::ImageRgb8(RgbImage::new(1200, 600));
        let decoded = decode(&encoded(&big, ImageFormat::Png)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (240, 120));
        let small = DynamicImage::ImageRgb8(RgbImage::new(60, 30));
        let decoded = decode(&encoded(&small, ImageFormat::Png)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (60, 30));
    }

    /// Every documented format really is enabled: each round-trips through `decode`.
    #[test]
    fn documented_formats_decode() {
        let rgb = DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])));
        // ICO keeps its pictures as RGBA PNGs.
        let rgba = DynamicImage::ImageRgba8(rgb.to_rgba8());
        for (name, format) in [
            ("PNG", ImageFormat::Png),
            ("JPEG", ImageFormat::Jpeg),
            ("GIF", ImageFormat::Gif),
            ("BMP", ImageFormat::Bmp),
            ("ICO", ImageFormat::Ico),
            ("WebP", ImageFormat::WebP),
            ("PNM", ImageFormat::Pnm),
        ] {
            assert!(FORMATS.contains(&name), "{name}");
            let image = if format == ImageFormat::Ico {
                &rgba
            } else {
                &rgb
            };
            let decoded =
                decode(&encoded(image, format)).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(decoded.width(), 8, "{name}");
        }
        assert_eq!(FORMATS.len(), 7);
    }

    /// Formats that are not enabled are not decoded (TIFF as an example of a large,
    /// rarely needed decoder that was dropped).
    #[test]
    fn disabled_formats_are_refused() {
        // A little-endian TIFF header with nothing after it.
        let tiff = b"II*\0\x08\0\0\0\0\0";
        assert!(decode(tiff).is_err());
        assert!(!ImageFormat::Tiff.reading_enabled());
        assert!(!ImageFormat::OpenExr.reading_enabled());
        assert!(!ImageFormat::Avif.reading_enabled());
    }

    /// `decode_as` and `open` apply the same limits and shrinking.
    #[test]
    fn decode_as_and_open_are_bounded_too() {
        let bomb = png_claiming(20000, 20000);
        assert!(decode_as(&bomb, ImageFormat::Png).is_err());
        let dir = std::env::temp_dir().join(format!("dak_imaging_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bomb.png");
        std::fs::write(&path, &bomb).unwrap();
        assert!(matches!(open(&path), Err(image::ImageError::Limits(_))));
        let big = DynamicImage::ImageRgb8(RgbImage::new(500, 500));
        let path = dir.join("big.png");
        std::fs::write(&path, encoded(&big, ImageFormat::Png)).unwrap();
        assert_eq!(open(&path).unwrap().width(), WORKING_SIZE);
        let decoded = decode_as(&encoded(&big, ImageFormat::Png), ImageFormat::Png).unwrap();
        assert_eq!(decoded.width(), WORKING_SIZE);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
