use std::{
    io::{BufRead, BufReader, Read, Seek},
    path::Path,
};

use cap_std::fs::{Dir, File, OpenOptions};
use cr_project::PhotographFormat;
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgba, RgbaImage,
    codecs::jpeg::JpegDecoder,
    metadata::{Cicp, Orientation},
};
use jxl_image_rs_integration::JxlDecoder;

mod heif;
#[cfg(target_os = "macos")]
mod image_io;
mod native_library;
mod raw;

const MAX_IMAGE_EDGE: u32 = 16_384;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) enum DecodeError {
    Unavailable,
    DecoderUnavailable,
}

pub(crate) struct DecodedImage {
    pixels: DynamicImage,
    orientation: Orientation,
    premultiplied_alpha: bool,
}

impl DecodedImage {
    fn upright(pixels: DynamicImage) -> Self {
        Self {
            pixels,
            orientation: Orientation::NoTransforms,
            premultiplied_alpha: false,
        }
    }

    /// Prepare display pixels, with an optional bound on the longest edge.
    pub(crate) fn into_display_rgba8(self, max_edge: Option<u32>) -> image::ImageResult<RgbaImage> {
        let mut pixels = self.pixels;
        let color_space = pixels.color_space();
        if let Some(edge) = max_edge
            && pixels.width().max(pixels.height()) > edge
        {
            pixels = pixels.thumbnail(edge, edge);
        }
        pixels.apply_orientation(self.orientation);
        if self.premultiplied_alpha
            && let Some(buffer) = pixels.as_mut_rgba32f()
        {
            for Rgba([red, green, blue, alpha]) in buffer.pixels_mut() {
                if *alpha > 0.0 {
                    *red /= *alpha;
                    *green /= *alpha;
                    *blue /= *alpha;
                }
            }
        }
        if color_space == Cicp::SRGB_LINEAR {
            pixels.set_color_space(color_space)?;
            pixels.apply_color_space(Cicp::SRGB, Default::default())?;
        }
        Ok(pixels.into_rgba8())
    }
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

    pub(crate) fn raster(
        &self,
        directory: &Dir,
        path: &Path,
        format: PhotographFormat,
        max_edge: Option<u32>,
    ) -> Result<DecodedImage, DecodeError> {
        use DecodeError::Unavailable;
        use PhotographFormat::{Avif, Heif, Jpeg, JpegXl, OpenExr, Png, Raw, Tiff, WebP};

        let file = open_source(directory, path).ok_or(Unavailable)?;
        match format {
            Jpeg => decode_jpeg(BufReader::new(file).take(MAX_SOURCE_BYTES)),
            JpegXl => decode_jpeg_xl(file),
            Png => decode_raster(BufReader::new(file), ImageFormat::Png),
            Tiff => decode_raster(BufReader::new(file), ImageFormat::Tiff),
            WebP => decode_raster(BufReader::new(file), ImageFormat::WebP),
            OpenExr => decode_raster(BufReader::new(file), ImageFormat::OpenExr),
            Avif | Heif => return self.heif(file, max_edge),
            Raw => return Err(Unavailable),
        }
        .map_err(|_| Unavailable)
    }

    fn heif(&self, file: File, max_edge: Option<u32>) -> Result<DecodedImage, DecodeError> {
        use DecodeError::{DecoderUnavailable, Unavailable};

        #[cfg(not(target_os = "macos"))]
        let _ = max_edge;
        if !cfg!(target_os = "macos") && self.heif.is_none() {
            return Err(DecoderUnavailable);
        }
        let bytes = read_source(file).ok_or(Unavailable)?;
        #[cfg(target_os = "macos")]
        if let Some(pixels) = image_io::decode(&bytes, max_edge.unwrap_or(MAX_IMAGE_EDGE))
            .map_err(|()| Unavailable)?
        {
            return Ok(DecodedImage::upright(pixels));
        }
        let decoder = self.heif.as_ref().ok_or(DecoderUnavailable)?;
        let pixels = decoder.decode(&bytes).ok_or(Unavailable)?;
        Ok(DecodedImage::upright(pixels))
    }

    pub(crate) fn raw<'a>(&'a self, bytes: &'a [u8]) -> Result<raw::RawImage<'a>, DecodeError> {
        self.raw
            .as_ref()
            .ok_or(DecodeError::DecoderUnavailable)?
            .open(bytes)
            .ok_or(DecodeError::Unavailable)
    }
}

pub(crate) fn read_image_file(directory: &Dir, path: &Path) -> Option<Vec<u8>> {
    read_source(open_source(directory, path)?)
}

fn open_source(directory: &Dir, path: &Path) -> Option<File> {
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

fn read_source(file: File) -> Option<Vec<u8>> {
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

pub(super) fn decode_jpeg(reader: impl BufRead + Seek) -> image::ImageResult<DecodedImage> {
    let mut decoder = JpegDecoder::new(reader)?;
    decoder.set_limits(limits())?;
    decode_image(decoder)
}

fn decode_jpeg_xl(file: File) -> image::ImageResult<DecodedImage> {
    let mut decoder = JxlDecoder::new(file.take(MAX_SOURCE_BYTES))?;
    decoder.set_limits(limits())?;
    decode_image(decoder)
}

pub(super) fn decode_raster(
    reader: impl BufRead + Seek,
    format: ImageFormat,
) -> image::ImageResult<DecodedImage> {
    let mut reader = ImageReader::with_format(reader, format);
    reader.limits(limits());
    let mut image = decode_image(reader.into_decoder()?)?;
    if format == ImageFormat::OpenExr {
        // The decoder returns linear samples and does not expose the file's chromaticities.
        image.pixels.set_color_space(Cicp::SRGB_LINEAR)?;
        image.premultiplied_alpha = true;
    }
    Ok(image)
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    (1..=MAX_IMAGE_EDGE).contains(&width)
        && (1..=MAX_IMAGE_EDGE).contains(&height)
        && u64::from(width) * u64::from(height) * 4 <= MAX_DECODE_BYTES
}

fn decode_image(mut decoder: impl ImageDecoder) -> image::ImageResult<DecodedImage> {
    let (width, height) = decoder.dimensions();
    if !valid_dimensions(width, height) || decoder.total_bytes() > MAX_DECODE_BYTES {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::InsufficientMemory),
        ));
    }

    Ok(DecodedImage {
        orientation: decoder.orientation()?,
        pixels: DynamicImage::from_decoder(decoder)?,
        premultiplied_alpha: false,
    })
}
