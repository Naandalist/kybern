//! Bounded, static previews. Originals retain their format and animation.
use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::io::Cursor;

pub const WIDTH: u32 = 560;
pub const HEIGHT: u32 = 352;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 96 * 1024 * 1024;

pub fn make(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().context("This format has no static preview; open the original image")?;
    let (width, height) = decoder.dimensions();
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS && decoder.total_bytes() <= MAX_DECODE_BYTES,
        "This image is too large for a preview; open the original image"
    );
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let preview = image.thumbnail(WIDTH.min(image.width()), HEIGHT.min(image.height()));
    let mut output = Cursor::new(Vec::new());
    preview.write_to(&mut output, ImageFormat::Png)?;
    Ok(output.into_inner())
}

/// A PNG copy of a TIFF or BMP upload, or `None` for any other format. macOS
/// often puts only TIFF on the clipboard when an image is copied from an app,
/// and neither clients nor most providers can show TIFF or BMP.
pub fn portable_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    if !matches!(reader.format(), Some(ImageFormat::Tiff | ImageFormat::Bmp)) {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(MAX_DECODE_BYTES * 4);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png).ok()?;
    Some(output.into_inner())
}

/// Edge of the square logo derived for a space.
pub const LOGO_EDGE: u32 = 128;

/// A 128×128 PNG of the centre square of a PNG, JPEG or WebP upload, with its
/// EXIF orientation applied. Every other format (GIF, SVG, AVIF…) is refused.
pub fn square_logo(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    ensure!(matches!(reader.format(), Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP)), "unsupported logo format");
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    ensure!(u64::from(width) * u64::from(height) <= MAX_PIXELS && decoder.total_bytes() <= MAX_DECODE_BYTES, "logo image is too large");
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let edge = image.width().min(image.height());
    ensure!(edge > 0, "logo image is empty");
    let square = image.crop_imm((image.width() - edge) / 2, (image.height() - edge) / 2, edge, edge);
    let logo = square.resize_exact(LOGO_EDGE, LOGO_EDGE, image::imageops::FilterType::Lanczos3);
    let mut output = Cursor::new(Vec::new());
    logo.write_to(&mut output, ImageFormat::Png)?;
    Ok(output.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tiff_and_bmp_uploads_become_pngs_and_others_are_left_alone() {
        let source = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(30, 20, image::Rgba([200, 10, 10, 255])));
        for format in [ImageFormat::Tiff, ImageFormat::Bmp] {
            let mut original = Cursor::new(Vec::new());
            source.write_to(&mut original, format).unwrap();
            let png = portable_png(original.get_ref()).unwrap();
            assert_eq!(image::guess_format(&png).unwrap(), ImageFormat::Png, "{format:?}");
            let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
            assert_eq!((decoded.width(), decoded.height()), (30, 20));
            assert_eq!(decoded.get_pixel(5, 5), &image::Rgba([200, 10, 10, 255]));
        }
        let mut png = Cursor::new(Vec::new());
        source.write_to(&mut png, ImageFormat::Png).unwrap();
        assert!(portable_png(png.get_ref()).is_none());
        assert!(portable_png(b"II*\0 not really a tiff").is_none());
        assert!(portable_png(b"plain text").is_none());
    }
    #[test]
    fn previews_fit_bounds_preserve_alpha_and_aspect_ratio() {
        for (width, height) in [(1800, 600), (600, 1800), (80, 40)] {
            let source = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(width, height, image::Rgba([32, 64, 128, 128])));
            let mut original = Cursor::new(Vec::new());
            source.write_to(&mut original, ImageFormat::Png).unwrap();
            let bytes = make(original.get_ref()).unwrap();
            let preview = image::load_from_memory(&bytes).unwrap();
            assert!(preview.width() <= WIDTH && preview.height() <= HEIGHT);
            assert!(preview.width() <= width && preview.height() <= height);
            assert!((f64::from(preview.width()) / f64::from(preview.height()) - f64::from(width) / f64::from(height)).abs() < 0.02);
            assert_eq!(preview.to_rgba8().get_pixel(0, 0).0, [32, 64, 128, 128]);
        }
    }
    #[test]
    fn rejects_unsupported_or_invalid_data_without_decoding() {
        assert!(make(b"not an image").is_err());
        assert!(make(b"\0\0\0\x20ftypavif").is_err());
    }
    #[test]
    fn square_logo_crops_the_centre_to_128_pixels_and_rejects_other_formats() {
        // 300×200: red left third, green middle, blue right third. The centre 200×200 is green with a red/blue sliver.
        let source = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(300, 200, |x, _| match x {
            0..=49 => image::Rgba([255, 0, 0, 255]),
            50..=249 => image::Rgba([0, 255, 0, 255]),
            _ => image::Rgba([0, 0, 255, 255]),
        }));
        let mut png = Cursor::new(Vec::new());
        source.write_to(&mut png, ImageFormat::Png).unwrap();
        let logo = image::load_from_memory(&square_logo(png.get_ref()).unwrap()).unwrap().to_rgba8();
        assert_eq!((logo.width(), logo.height()), (LOGO_EDGE, LOGO_EDGE));
        assert_eq!(logo.get_pixel(2, 64).0, [0, 255, 0, 255]);
        assert_eq!(logo.get_pixel(125, 64).0, [0, 255, 0, 255]);

        let mut gif = Cursor::new(Vec::new());
        source.write_to(&mut gif, ImageFormat::Gif).unwrap();
        assert!(square_logo(gif.get_ref()).is_err());
        assert!(square_logo(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err());
        assert!(square_logo(b"not an image").is_err());
    }

    #[test]
    fn square_logo_applies_exif_orientation() {
        // A 200×100 JPEG tagged "rotate 90° clockwise" (orientation 6) reads as 100×200: the left half is on top.
        let source = image::RgbImage::from_fn(200, 100, |x, _| if x < 100 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) });
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(source).write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
        let mut bytes = jpeg.into_inner();
        // APP1 Exif segment (big-endian TIFF, one IFD entry: Orientation = 6) inserted after SOI.
        let exif: &[u8] = &[
            0xFF, 0xE1, 0x00, 0x22, b'E', b'x', b'i', b'f', 0, 0, b'M', b'M', 0, 0x2A, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0,
            6, 0, 0, 0, 0, 0, 0,
        ];
        bytes.splice(2..2, exif.iter().copied());
        let logo = image::load_from_memory(&square_logo(&bytes).unwrap()).unwrap().to_rgb8();
        let top = logo.get_pixel(64, 5).0;
        let bottom = logo.get_pixel(64, 122).0;
        assert!(top[0] > 200 && top[2] < 60, "top should be red, got {top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 60, "bottom should be blue, got {bottom:?}");
    }
}
