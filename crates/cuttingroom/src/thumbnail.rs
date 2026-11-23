use std::{
    io::{BufReader, Read},
    path::Path,
};

use cap_std::fs::{Dir, OpenOptions};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, codecs::jpeg::JpegDecoder,
};
use slint::{Rgba8Pixel, SharedPixelBuffer};

const THUMBNAIL_EDGE: u32 = 384;
const MAX_IMAGE_EDGE: u32 = 16_384;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) fn load(directory: &Dir, path: &Path) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    let file = directory.open_with(path, &options).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return None;
    }

    let format = ImageFormat::from_path(path).ok()?;
    decode(BufReader::new(file), format).ok()
}

fn decode(
    reader: BufReader<cap_std::fs::File>,
    format: ImageFormat,
) -> image::ImageResult<SharedPixelBuffer<Rgba8Pixel>> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);

    if format == ImageFormat::Jpeg {
        // The JPEG decoder reads the entire source before checking its dimensions.
        let mut decoder = JpegDecoder::new(reader.take(MAX_SOURCE_BYTES))?;
        decoder.set_limits(limits)?;
        return render(decoder);
    }

    let mut reader = ImageReader::with_format(reader, format);
    reader.limits(limits);
    render(reader.into_decoder()?)
}

fn render(mut decoder: impl ImageDecoder) -> image::ImageResult<SharedPixelBuffer<Rgba8Pixel>> {
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || decoder.total_bytes() > MAX_DECODE_BYTES {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::InsufficientMemory),
        ));
    }

    let orientation = decoder.orientation()?;
    let image = DynamicImage::from_decoder(decoder)?;
    let mut preview = if width.max(height) > THUMBNAIL_EDGE {
        image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE)
    } else {
        image
    };
    preview.apply_orientation(orientation);
    let preview = preview.into_rgba8();
    let (width, height) = preview.dimensions();
    Ok(SharedPixelBuffer::clone_from_slice(
        preview.as_raw(),
        width,
        height,
    ))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io::Cursor};

    use image::{ImageEncoder, Rgb, RgbImage, codecs::jpeg::JpegEncoder};

    use super::{JpegDecoder, render};

    #[test]
    fn jpeg_thumbnail_preserves_aspect_ratio_and_applies_exif_orientation()
    -> Result<(), Box<dyn Error>> {
        let image = RgbImage::from_fn(800, 400, |x, _| {
            if x < 400 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        });
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new(&mut bytes);
        // EXIF orientation 6 rotates the image 90 degrees clockwise.
        encoder.set_exif_metadata(vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 1, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
        ])?;
        encoder.encode_image(&image)?;

        let preview = render(JpegDecoder::new(Cursor::new(bytes))?)?;
        assert_eq!((preview.width(), preview.height()), (192, 384));
        let first = preview.as_slice().first().ok_or("empty preview")?;
        let last = preview.as_slice().last().ok_or("empty preview")?;
        assert!(first.r > 240 && first.b < 15);
        assert!(last.b > 240 && last.r < 15);
        Ok(())
    }
}
