use std::path::{Path, PathBuf};

use cap_std::fs::Dir;
use cr_project::{PhotographFormat, photograph_format};
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::image_decode::{DecodeError, DecodedImage, Decoder, read_image_file};
use crate::metadata::{Metadata, add};

#[derive(Clone, Copy)]
pub(crate) enum PreviewSize {
    Thumbnail,
    Photo,
}

impl PreviewSize {
    fn max_edge(self) -> u32 {
        match self {
            Self::Thumbnail => 384,
            // A square RGBA8 preview fits within the 16 MiB frame upload budget.
            Self::Photo => 2048,
        }
    }
}

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

pub(crate) struct Preview {
    pub(crate) pixels: Pixels,
    pub(crate) metadata: Metadata,
}

pub(crate) fn load(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[PathBuf],
    size: PreviewSize,
    cancelled: impl Fn() -> bool,
) -> Result<Preview, DecodeError> {
    use DecodeError::{DecoderUnavailable, Unavailable};
    use PhotographFormat::{Jpeg, Png, Raw, Tiff};

    let mut rasters = Vec::new();
    let mut raws = Vec::new();
    for path in paths {
        match photograph_format(path) {
            Some(Raw) => raws.push(path),
            Some(format) => rasters.push((format, path)),
            None => (),
        }
    }
    rasters.sort_by_key(|(format, _)| match format {
        Jpeg => 0,
        Png => 1,
        Tiff => 2,
        _ => 3,
    });

    let mut error = Unavailable;
    for (format, path) in rasters {
        if cancelled() {
            return Err(Unavailable);
        }
        match decoder
            .raster(directory, path, format, Some(size.max_edge()))
            .and_then(|image| with_metadata(image, path, size, &cancelled))
        {
            Ok(pixels) => return Ok(pixels),
            Err(DecoderUnavailable) => error = DecoderUnavailable,
            Err(Unavailable) => (),
        }
    }

    match load_raw(decoder, directory, &raws, size, &cancelled) {
        Err(Unavailable) => Err(error),
        result => result,
    }
}

fn load_raw(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[&PathBuf],
    size: PreviewSize,
    cancelled: impl Fn() -> bool,
) -> Result<Preview, DecodeError> {
    use DecodeError::{DecoderUnavailable, Unavailable};

    let mut first_source = None;
    let mut alternate_paths = Vec::new();
    for &path in paths {
        if cancelled() {
            return Err(Unavailable);
        }
        let Some(bytes) = read_image_file(directory, path) else {
            continue;
        };
        let mut raw = match decoder.raw(&bytes) {
            Ok(raw) => raw,
            Err(DecoderUnavailable) => return Err(DecoderUnavailable),
            Err(Unavailable) => continue,
        };
        if let Some(image) = raw.embedded_preview() {
            return with_metadata(image, path, size, &cancelled);
        }
        drop(raw);
        if first_source.is_none() {
            first_source = Some((path, bytes));
        } else {
            alternate_paths.push(path);
        }
    }

    let (path, bytes) = first_source.ok_or(Unavailable)?;
    if cancelled() {
        return Err(Unavailable);
    }
    if let Some(image) = decoder.raw(&bytes)?.render(&cancelled) {
        return with_metadata(image, path, size, &cancelled);
    }
    drop(bytes);
    for path in alternate_paths {
        if cancelled() {
            return Err(Unavailable);
        }
        let Some(bytes) = read_image_file(directory, path) else {
            continue;
        };
        if let Ok(raw) = decoder.raw(&bytes)
            && let Some(image) = raw.render(&cancelled)
        {
            return with_metadata(image, path, size, &cancelled);
        }
    }
    Err(Unavailable)
}

fn with_metadata(
    mut image: DecodedImage,
    path: &Path,
    size: PreviewSize,
    cancelled: impl Fn() -> bool,
) -> Result<Preview, DecodeError> {
    if cancelled() {
        return Err(DecodeError::Unavailable);
    }
    let mut metadata = std::mem::take(&mut image.metadata);
    let (width, height) = image.source_dimensions;
    let format = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_uppercase();
    add(
        &mut metadata.summary,
        "Dimensions",
        format!("{width} × {height}"),
    );
    add(&mut metadata.summary, "Format", format);
    Ok(Preview {
        pixels: make_preview(image, size)?,
        metadata,
    })
}

fn make_preview(image: DecodedImage, size: PreviewSize) -> Result<Pixels, DecodeError> {
    let preview = image
        .into_display_rgba8(Some(size.max_edge()))
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

    use super::{PreviewSize, make_preview};
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

        let full_preview = make_preview(decode_jpeg(Cursor::new(&bytes))?, PreviewSize::Photo)
            .map_err(|_| "preview failed")?;
        assert_eq!((full_preview.width(), full_preview.height()), (400, 800));
        let preview = make_preview(decode_jpeg(Cursor::new(bytes))?, PreviewSize::Thumbnail)
            .map_err(|_| "preview failed")?;
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
        let preview =
            make_preview(decoded, PreviewSize::Thumbnail).map_err(|_| "preview failed")?;
        let pixel = preview.as_slice().first().ok_or("empty preview")?;
        assert!((117..=119).contains(&pixel.r));
        assert_eq!((pixel.g, pixel.b, pixel.a), (255, 0, 128));
        Ok(())
    }
}
