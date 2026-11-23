use std::{
    io::{BufRead, BufReader, Read, Seek},
    path::Path,
};

use cap_std::fs::{Dir, OpenOptions};
use cr_project::{PhotographFormat, photograph_format};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgba,
    codecs::jpeg::JpegDecoder,
    metadata::{Cicp, Orientation},
};
use jxl_image_rs_integration::JxlDecoder;
use slint::{Rgba8Pixel, SharedPixelBuffer};

mod heif;
#[cfg(target_os = "macos")]
mod image_io;
mod native_library;
mod raw;

const THUMBNAIL_EDGE: u32 = 384;
const MAX_IMAGE_EDGE: u32 = 16_384;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) type Pixels = SharedPixelBuffer<Rgba8Pixel>;

pub(crate) enum PreviewError {
    Unavailable,
    DecoderUnavailable,
}

pub(crate) struct Decoder {
    raw: Option<raw::RawDecoder>,
    heif: Option<heif::HeifDecoder>,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Self {
            raw: raw::RawDecoder::new(),
            heif: heif::HeifDecoder::new(),
        }
    }

    pub(crate) fn load(&self, directory: &Dir, path: &Path) -> Result<Pixels, PreviewError> {
        use PhotographFormat::{Heif, Jpeg, JpegXl, OpenExr, Png, Raw, Tiff, WebP};
        use PreviewError::{DecoderUnavailable, Unavailable};

        let format = photograph_format(path).ok_or(Unavailable)?;
        let file = open_source(directory, path).ok_or(Unavailable)?;
        match format {
            Jpeg => {
                decode_jpeg(BufReader::new(file).take(MAX_SOURCE_BYTES)).map_err(|_| Unavailable)
            }
            JpegXl => decode_jpeg_xl(file).map_err(|_| Unavailable),
            Png => decode_raster(file, ImageFormat::Png).map_err(|_| Unavailable),
            Tiff => decode_raster(file, ImageFormat::Tiff).map_err(|_| Unavailable),
            WebP => decode_raster(file, ImageFormat::WebP).map_err(|_| Unavailable),
            OpenExr => decode_raster(file, ImageFormat::OpenExr).map_err(|_| Unavailable),
            Raw => {
                let decoder = self.raw.as_ref().ok_or(DecoderUnavailable)?;
                let bytes = read_source(file).ok_or(Unavailable)?;
                decoder.decode(&bytes).ok_or(Unavailable)
            }
            Heif => {
                if !cfg!(target_os = "macos") && self.heif.is_none() {
                    return Err(DecoderUnavailable);
                }
                let bytes = read_source(file).ok_or(Unavailable)?;
                #[cfg(target_os = "macos")]
                if let Some(pixels) = image_io::decode(&bytes).map_err(|()| Unavailable)? {
                    return Ok(pixels);
                }
                let decoder = self.heif.as_ref().ok_or(DecoderUnavailable)?;
                let image = decoder.decode(&bytes).ok_or(Unavailable)?;
                Ok(make_preview(image, Orientation::NoTransforms))
            }
        }
    }
}

fn open_source(directory: &Dir, path: &Path) -> Option<cap_std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    let file = directory.open_with(path, &options).ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.len() <= MAX_SOURCE_BYTES).then_some(file)
}

fn read_source(file: cap_std::fs::File) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_SOURCE_BYTES).then_some(bytes)
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);

    limits
}

fn decode_jpeg(reader: impl BufRead + Seek) -> image::ImageResult<Pixels> {
    let mut decoder = JpegDecoder::new(reader)?;
    decoder.set_limits(limits())?;
    render(decoder)
}

fn decode_jpeg_xl(file: cap_std::fs::File) -> image::ImageResult<Pixels> {
    let mut decoder = JxlDecoder::new(file.take(MAX_SOURCE_BYTES))?;
    decoder.set_limits(limits())?;
    render(decoder)
}

fn decode_raster(file: cap_std::fs::File, format: ImageFormat) -> image::ImageResult<Pixels> {
    let mut reader = ImageReader::with_format(BufReader::new(file), format);
    reader.limits(limits());
    let decoder = reader.into_decoder()?;
    if format == ImageFormat::OpenExr {
        render_open_exr(decoder)
    } else {
        render(decoder)
    }
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    (1..=MAX_IMAGE_EDGE).contains(&width)
        && (1..=MAX_IMAGE_EDGE).contains(&height)
        && u64::from(width) * u64::from(height) * 4 <= MAX_DECODE_BYTES
}

fn decode_image(mut decoder: impl ImageDecoder) -> image::ImageResult<(DynamicImage, Orientation)> {
    let (width, height) = decoder.dimensions();
    if !valid_dimensions(width, height) || decoder.total_bytes() > MAX_DECODE_BYTES {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::InsufficientMemory),
        ));
    }

    let orientation = decoder.orientation()?;
    Ok((DynamicImage::from_decoder(decoder)?, orientation))
}

fn render(decoder: impl ImageDecoder) -> image::ImageResult<Pixels> {
    let (image, orientation) = decode_image(decoder)?;
    Ok(make_preview(image, orientation))
}

fn render_open_exr(decoder: impl ImageDecoder) -> image::ImageResult<Pixels> {
    let (image, orientation) = decode_image(decoder)?;
    let mut preview = resize_preview(image, orientation);
    // OpenEXR stores premultiplied alpha. Slint expects straight RGBA.
    if let Some(pixels) = preview.as_mut_rgba32f() {
        for Rgba([red, green, blue, alpha]) in pixels.pixels_mut() {
            if *alpha > 0.0 {
                *red /= *alpha;
                *green /= *alpha;
                *blue /= *alpha;
            }
        }
    }
    // The decoder returns linear samples and does not expose the file's chromaticities.
    preview.set_color_space(Cicp::SRGB_LINEAR)?;
    preview.apply_color_space(Cicp::SRGB, Default::default())?;
    Ok(preview_pixels(preview))
}

fn make_preview(image: DynamicImage, orientation: Orientation) -> Pixels {
    preview_pixels(resize_preview(image, orientation))
}

fn resize_preview(image: DynamicImage, orientation: Orientation) -> DynamicImage {
    let mut preview = if image.width().max(image.height()) > THUMBNAIL_EDGE {
        image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE)
    } else {
        image
    };
    preview.apply_orientation(orientation);
    preview
}

fn preview_pixels(preview: DynamicImage) -> Pixels {
    let preview = preview.into_rgba8();
    let (width, height) = preview.dimensions();
    SharedPixelBuffer::clone_from_slice(preview.as_raw(), width, height)
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io::Cursor};

    use image::{
        ExtendedColorType, ImageEncoder, Rgb, RgbImage,
        codecs::{
            jpeg::JpegEncoder,
            openexr::{OpenExrDecoder, OpenExrEncoder},
        },
    };

    use super::{decode_jpeg, render_open_exr};

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

        let preview = decode_jpeg(Cursor::new(bytes))?;
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

        let preview = render_open_exr(OpenExrDecoder::new(bytes)?)?;
        let pixel = preview.as_slice().first().ok_or("empty preview")?;
        assert!((117..=119).contains(&pixel.r));
        assert_eq!((pixel.g, pixel.b, pixel.a), (255, 0, 128));
        Ok(())
    }
}
