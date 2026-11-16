//! An image from a transcript, decoded to draw: on a background thread, to one still frame of
//! bounded size, so that no image can take more than [`MAX_PIXELS`] to decode or more than
//! [`MAX_SIDE`] a side to keep. Only raster formats are drawn: an SVG can name files and
//! addresses to load.

use std::io::Cursor;
use std::sync::Arc;

use gpui::RenderImage;
use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder as _, ImageFormat, ImageReader, Limits, RgbaImage};

/// The most pixels an image may claim in its header and still be decoded: a 6K screenshot.
pub const MAX_PIXELS: u64 = 6144 * 4096;
/// The longest side an image is kept at once decoded: past what the column draws.
pub const MAX_SIDE: u32 = 2048;

/// An image ready to draw.
#[derive(Clone, Debug)]
pub struct Picture {
    pub image: Arc<RenderImage>,
    pub media_type: String,
    /// Its size in pixels, as the file has it.
    pub width: u32,
    pub height: u32,
    /// How many bytes the file is.
    pub bytes: usize,
}

impl PartialEq for Picture {
    fn eq(&self, other: &Self) -> bool {
        self.image.id == other.image.id
    }
}

impl Eq for Picture {}

/// The image of type `media_type` whose file is `bytes`, decoded; or why it can't be shown.
pub fn decode(media_type: &str, bytes: &[u8]) -> Result<Picture, String> {
    let format = match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        other => return Err(format!("{other} images aren't shown")),
    };
    let reader = || ImageReader::with_format(Cursor::new(bytes), format);
    let (width, height) = reader()
        .into_dimensions()
        .map_err(|error| format!("The image can't be read: {error}"))?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "The image claims {width} × {height} pixels, too many to decode"
        ));
    }
    let undecodable = |error: image::ImageError| format!("The image can't be decoded: {error}");
    let mut frame: RgbaImage = if format == ImageFormat::Gif {
        let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(undecodable)?;
        decoder
            .into_frames()
            .next()
            .ok_or("The image has no frame")?
            .map_err(undecodable)?
            .into_buffer()
    } else {
        let mut reader = reader();
        let mut limits = Limits::default();
        limits.max_alloc = Some(MAX_PIXELS * 4 * 2);
        reader.limits(limits);
        reader.decode().map_err(undecodable)?.to_rgba8()
    };
    if frame.width().max(frame.height()) > MAX_SIDE {
        let scale = f64::from(MAX_SIDE) / f64::from(frame.width().max(frame.height()));
        let fit = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
        frame = image::imageops::thumbnail(&frame, fit(frame.width()), fit(frame.height()));
    }
    /* gpui draws BGRA. */
    for pixel in frame.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Picture {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(frame)])),
        media_type: media_type.to_string(),
        width,
        height,
        bytes: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    /// PNG's chunk checksum.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        RgbaImage::from_pixel(width, height, Rgba([1, 2, 3, 255]))
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn an_image_is_decoded_to_one_bounded_frame() {
        let picture = decode("image/png", &png(4000, 100)).unwrap();
        assert_eq!((picture.width, picture.height), (4000, 100));
        let size = picture.image.size(0);
        assert_eq!((size.width.0, size.height.0), (2048, 51));
        // BGRA.
        assert_eq!(&picture.image.as_bytes(0).unwrap()[..4], &[3, 2, 1, 255]);
    }

    #[test]
    fn what_cant_or_mustnt_be_decoded_says_why() {
        let svg =
            b"<svg xmlns='http://www.w3.org/2000/svg'><image href='file:///etc/passwd'/></svg>";
        assert!(
            decode("image/svg+xml", svg)
                .unwrap_err()
                .contains("aren't shown")
        );
        let whole = png(10, 10);
        assert!(decode("image/png", &whole[..whole.len() / 2]).is_err());
        assert!(decode("image/png", b"not an image").is_err());
        /* A header claiming 60,000 × 60,000, and nothing after it. */
        let mut huge = whole.clone();
        huge[16..20].copy_from_slice(&60_000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&60_000u32.to_be_bytes());
        let crc = crc32(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        assert!(decode("image/png", &huge).unwrap_err().contains("too many"));
    }
}
