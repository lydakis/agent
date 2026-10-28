//! An image file as `read` sends it to the model: as it is when it is
//! small enough, else scaled down or re-encoded, as Claude Code's Read
//! sends one.
use crate::{Error, Result, codec::Image};
use base64::Engine;
use image::{GenericImageView, ImageFormat, imageops::FilterType};

/// The longest side a picture is sent at: what Anthropic's models see at
/// most (it scales larger ones down), and under the 2000 pixels it allows
/// once a request holds more than 20 pictures.
const LONGEST: u32 = 1568;
/// The most bytes a picture is sent with as it is, or as a scaled PNG:
/// the context is measured in bytes, so a larger one goes as a JPEG, well
/// under the 5 MB Anthropic takes for one.
const MOST_BYTES: usize = 1024 * 1024;

/// The picture an image file holds, with a line for the model on what it
/// is; `None` for a file that is not one of the four kinds providers take.
pub fn read(bytes: &[u8]) -> Result<Option<(Image, String)>> {
    let (format, media_type, kind) = match bytes {
        [0x89, b'P', b'N', b'G', ..] => (ImageFormat::Png, "image/png", "PNG"),
        [0xff, 0xd8, 0xff, ..] => (ImageFormat::Jpeg, "image/jpeg", "JPEG"),
        [b'G', b'I', b'F', b'8', ..] => (ImageFormat::Gif, "image/gif", "GIF"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => (ImageFormat::WebP, "image/webp", "WebP"),
        _ => return Ok(None),
    };
    let unreadable = |e: image::ImageError| {
        Error::with(
            "invalid_image",
            format!("a {kind} image that cannot be read: {e}"),
        )
    };
    let (width, height) = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(unreadable)?;
    let size = bytes.len();
    let (media_type, data, line) = if width.max(height) <= LONGEST && size <= MOST_BYTES {
        (
            media_type,
            bytes.to_vec(),
            format!("a {kind} image, {width}×{height}, {size} bytes; it follows"),
        )
    } else {
        let picture = image::load_from_memory_with_format(bytes, format).map_err(unreadable)?;
        let scaled = match width.max(height) > LONGEST {
            true => picture.resize(LONGEST, LONGEST, FilterType::CatmullRom),
            false => picture,
        };
        let (w, h) = scaled.dimensions();
        // A photograph stays a JPEG; anything else is a PNG unless that is
        // still too large, when a JPEG keeps it under the limit.
        let mut data = Vec::new();
        let mut media_type = "image/png";
        if format != ImageFormat::Jpeg {
            scaled
                .write_to(&mut std::io::Cursor::new(&mut data), ImageFormat::Png)
                .map_err(unreadable)?;
        }
        if format == ImageFormat::Jpeg || data.len() > MOST_BYTES {
            data.clear();
            media_type = "image/jpeg";
            image::DynamicImage::ImageRgb8(scaled.to_rgb8())
                .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                    &mut std::io::Cursor::new(&mut data),
                    85,
                ))
                .map_err(unreadable)?;
        }
        let sent = match (w, h) == (width, height) {
            true => format!(
                "as a {} of {} bytes",
                media_type[6..].to_uppercase(),
                data.len()
            ),
            false => format!("scaled to {w}×{h}"),
        };
        (
            media_type,
            data,
            format!("a {kind} image, {width}×{height}, {size} bytes; it follows {sent}"),
        )
    };
    let image = Image {
        media_type: media_type.to_owned(),
        data: base64::engine::general_purpose::STANDARD.encode(data),
    };
    Ok(Some((image, line)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut data = Vec::new();
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut std::io::Cursor::new(&mut data), ImageFormat::Png)
            .unwrap();
        data
    }

    fn decoded(image: &Image) -> image::DynamicImage {
        let data = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap();
        image::load_from_memory(&data).unwrap()
    }

    #[test]
    fn a_picture_that_fits_goes_as_it_is_and_a_large_one_scaled() {
        let small = png(800, 600);
        let (image, line) = read(&small).unwrap().unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .unwrap(),
            small
        );
        assert!(line.starts_with("a PNG image, 800×600,"), "{line}");
        let (image, line) = read(&png(2880, 1800)).unwrap().unwrap();
        assert_eq!(decoded(&image).dimensions(), (1568, 980));
        assert!(line.ends_with("scaled to 1568×980"), "{line}");
        // A photograph stays a JPEG.
        let mut jpeg = Vec::new();
        image::DynamicImage::new_rgb8(3000, 2000)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        let (image, _) = read(&jpeg).unwrap().unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert_eq!(decoded(&image).dimensions(), (1568, 1045));
        // One that fits but is heavy goes as a JPEG of the same size.
        let mut seed = 1_u32;
        let noise = image::RgbImage::from_fn(1200, 900, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
        });
        let mut heavy = Vec::new();
        noise
            .write_to(&mut std::io::Cursor::new(&mut heavy), ImageFormat::Png)
            .unwrap();
        assert!(heavy.len() > MOST_BYTES);
        let (image, line) = read(&heavy).unwrap().unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert_eq!(decoded(&image).dimensions(), (1200, 900));
        assert!(line.contains("it follows as a JPEG of"), "{line}");
    }

    #[test]
    fn other_files_are_not_pictures_and_a_broken_one_says_so() {
        assert!(read(b"%PDF-1.7").unwrap().is_none());
        assert!(read(b"text").unwrap().is_none());
        let error = read(b"\x89PNG\r\n\x1a\nbroken").unwrap_err();
        assert_eq!(error.code, "invalid_image");
    }
}
