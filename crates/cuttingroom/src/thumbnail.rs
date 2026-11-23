use std::path::Path;

use cap_std::fs::Dir;
use cr_project::{PhotographFormat, photograph_format};
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::image_decode::{DecodeError, DecodedImage, Decoder, read_image_file};

const THUMBNAIL_EDGE: u32 = 384;

pub(crate) type Pixels = SharedPixelBuffer<Rgba8Pixel>;

pub(crate) fn load(decoder: &Decoder, directory: &Dir, path: &Path) -> Result<Pixels, DecodeError> {
    let format = photograph_format(path).ok_or(DecodeError::Unavailable)?;
    let image = if format == PhotographFormat::Raw {
        let bytes = read_image_file(directory, path).ok_or(DecodeError::Unavailable)?;
        decoder
            .raw(&bytes)?
            .embedded_preview()
            .ok_or(DecodeError::Unavailable)?
    } else {
        decoder.raster(directory, path, format, Some(THUMBNAIL_EDGE))?
    };
    make_preview(image)
}

fn make_preview(image: DecodedImage) -> Result<Pixels, DecodeError> {
    let preview = image
        .into_display_rgba8(Some(THUMBNAIL_EDGE))
        .map_err(|_| DecodeError::Unavailable)?;
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

    use image::{
        ExtendedColorType, ImageEncoder, ImageFormat, Rgb, RgbImage,
        codecs::{jpeg::JpegEncoder, openexr::OpenExrEncoder},
    };

    use super::make_preview;
    use crate::image_decode::{decode_jpeg, decode_raster};

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

        let full_image = decode_jpeg(Cursor::new(&bytes))?.into_display_rgba8(None)?;
        assert_eq!(full_image.dimensions(), (400, 800));
        let preview =
            make_preview(decode_jpeg(Cursor::new(bytes))?).map_err(|_| "preview failed")?;
        assert_eq!((preview.width(), preview.height()), (192, 384));
        let first = preview.as_slice().first().ok_or("empty preview")?;
        let last = preview.as_slice().last().ok_or("empty preview")?;
        assert!(first.r > 240 && first.b < 15);
        assert!(last.b > 240 && last.r < 15);
        Ok(())
    }

    #[test]
    fn open_exr_thumbnail_converts_linear_rgb_and_preserves_alpha() -> Result<(), Box<dyn Error>> {
        let pixel = [0.09_f32, 0.75, -0.125, 0.5].map(f32::to_ne_bytes).concat();
        let mut bytes = Cursor::new(Vec::new());
        OpenExrEncoder::new(&mut bytes).write_image(&pixel, 1, 1, ExtendedColorType::Rgba32F)?;
        bytes.set_position(0);

        let decoded = decode_raster(bytes, ImageFormat::OpenExr)?;
        let preview = make_preview(decoded).map_err(|_| "preview failed")?;
        let pixel = preview.as_slice().first().ok_or("empty preview")?;
        assert!((117..=119).contains(&pixel.r));
        assert_eq!((pixel.g, pixel.b, pixel.a), (255, 0, 128));
        Ok(())
    }
}
