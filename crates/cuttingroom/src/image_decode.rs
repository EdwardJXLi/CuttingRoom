use std::{
    io::{BufRead, BufReader, Cursor, Read, Seek},
    path::Path,
};

use cap_std::fs::{Dir, File};
use cr_project::PhotographFormat;
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgba, RgbaImage,
    metadata::{Cicp, Orientation},
};
use jxl_image_rs_integration::JxlDecoder;

use crate::metadata::{self, Metadata};
use crate::source_file;

mod heif;
#[cfg(target_os = "macos")]
mod image_io;
mod jpeg;
mod native_library;
mod raw;

const MAX_IMAGE_EDGE: u32 = 16_384;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EXIF_BYTES: usize = 1024 * 1024;

pub(crate) enum DecodeError {
    Unavailable,
    DecoderUnavailable,
}

#[derive(Clone, Copy)]
pub(crate) enum PreviewSize {
    Thumbnail,
    Photo,
}

impl PreviewSize {
    pub(crate) fn max_edge(self) -> u32 {
        match self {
            Self::Thumbnail => 384,
            // A square RGBA8 preview fits within the 16 MiB frame upload budget.
            Self::Photo => 2048,
        }
    }
}

pub(crate) struct DecodedImage {
    pixels: DynamicImage,
    orientation: Orientation,
    premultiplied_alpha: bool,
    pub(crate) source_dimensions: (u32, u32),
    pub(crate) metadata: Metadata,
}

impl DecodedImage {
    fn upright(pixels: DynamicImage) -> Self {
        Self {
            source_dimensions: (pixels.width(), pixels.height()),
            pixels,
            orientation: Orientation::NoTransforms,
            premultiplied_alpha: false,
            metadata: Metadata::default(),
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
        size: PreviewSize,
    ) -> Result<DecodedImage, DecodeError> {
        use DecodeError::Unavailable;
        use PhotographFormat::{Avif, Heif, Jpeg, JpegXl, OpenExr, Png, Raw, Tiff, WebP};

        let file = source_file::open(directory, path, MAX_SOURCE_BYTES).ok_or(Unavailable)?;
        match format {
            Jpeg => {
                let bytes = source_file::read(file, MAX_SOURCE_BYTES).ok_or(Unavailable)?;
                return jpeg::decode(&bytes, size).ok_or(Unavailable);
            }
            JpegXl => jpeg_xl_decoder(file).and_then(decode_image),
            Png | WebP | OpenExr => decode_raster(BufReader::new(file), raster_format(format)),
            Tiff => {
                let bytes = source_file::read(file, MAX_SOURCE_BYTES).ok_or(Unavailable)?;
                let mut image = decode_raster(Cursor::new(&bytes), ImageFormat::Tiff)
                    .map_err(|_| Unavailable)?;
                image.metadata = metadata::read_exif(bytes);
                return Ok(image);
            }
            Avif | Heif => return self.heif(file, size.max_edge()),
            Raw => return Err(Unavailable),
        }
        .map_err(|_| Unavailable)
    }

    pub(crate) fn inspect(
        &self,
        directory: &Dir,
        path: &Path,
        format: PhotographFormat,
    ) -> Result<Metadata, DecodeError> {
        use DecodeError::Unavailable;
        use PhotographFormat::{Avif, Heif, Jpeg, JpegXl, OpenExr, Png, Raw, Tiff, WebP};

        let file = source_file::open(directory, path, MAX_SOURCE_BYTES).ok_or(Unavailable)?;
        match format {
            Raw => self
                .raw
                .as_ref()
                .ok_or(DecodeError::DecoderUnavailable)?
                .inspect(file)
                .ok_or(Unavailable),
            Tiff => {
                let bytes = source_file::read(file, MAX_SOURCE_BYTES).ok_or(Unavailable)?;
                let dimensions = ImageReader::with_format(Cursor::new(&bytes), ImageFormat::Tiff)
                    .into_dimensions()
                    .map_err(|_| Unavailable)?;
                let mut metadata = metadata::read_exif(bytes);
                metadata.add_dimensions(dimensions);
                Ok(metadata)
            }
            Avif | Heif => Ok(metadata::read_container_from(
                BufReader::new(file).take(MAX_SOURCE_BYTES),
            )),
            Jpeg => jpeg::inspect(BufReader::new(file).take(MAX_SOURCE_BYTES)).ok_or(Unavailable),
            JpegXl => jpeg_xl_decoder(file)
                .map(inspect_decoder)
                .map_err(|_| Unavailable),
            Png | WebP | OpenExr => raster_decoder(BufReader::new(file), raster_format(format))
                .map(inspect_decoder)
                .map_err(|_| Unavailable),
        }
    }

    fn heif(&self, file: File, max_edge: u32) -> Result<DecodedImage, DecodeError> {
        use DecodeError::{DecoderUnavailable, Unavailable};

        #[cfg(not(target_os = "macos"))]
        let _ = max_edge;
        if !cfg!(target_os = "macos") && self.heif.is_none() {
            return Err(DecoderUnavailable);
        }
        let bytes = source_file::read(file, MAX_SOURCE_BYTES).ok_or(Unavailable)?;
        #[cfg(target_os = "macos")]
        if let Some(mut image) = image_io::decode(&bytes, max_edge).map_err(|()| Unavailable)? {
            image.metadata = metadata::read_container(&bytes);
            return Ok(image);
        }
        let decoder = self.heif.as_ref().ok_or(DecoderUnavailable)?;
        let pixels = decoder.decode(&bytes).ok_or(Unavailable)?;
        let mut image = DecodedImage::upright(pixels);
        image.metadata = metadata::read_container(&bytes);
        Ok(image)
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
    source_file::read(
        source_file::open(directory, path, MAX_SOURCE_BYTES)?,
        MAX_SOURCE_BYTES,
    )
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

fn jpeg_xl_decoder(file: File) -> image::ImageResult<impl ImageDecoder> {
    let mut decoder = JxlDecoder::new(file.take(MAX_SOURCE_BYTES))?;
    decoder.set_limits(limits())?;
    Ok(decoder)
}

fn raster_format(format: PhotographFormat) -> ImageFormat {
    match format {
        PhotographFormat::Png => ImageFormat::Png,
        PhotographFormat::WebP => ImageFormat::WebP,
        PhotographFormat::OpenExr => ImageFormat::OpenExr,
        _ => unreachable!("format handled by its own decoder"),
    }
}

fn raster_decoder<'a>(
    reader: impl BufRead + Seek + 'a,
    format: ImageFormat,
) -> image::ImageResult<impl ImageDecoder + 'a> {
    let mut reader = ImageReader::with_format(reader, format);
    reader.limits(limits());
    reader.into_decoder()
}

pub(super) fn decode_raster(
    reader: impl BufRead + Seek,
    format: ImageFormat,
) -> image::ImageResult<DecodedImage> {
    let mut image = decode_image(raster_decoder(reader, format)?)?;
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

fn decoder_metadata(decoder: &mut impl ImageDecoder) -> Metadata {
    decoder
        .exif_metadata()
        .ok()
        .flatten()
        .filter(|bytes| bytes.len() <= MAX_EXIF_BYTES)
        .map_or_else(Metadata::default, metadata::read_exif)
}

fn inspect_decoder(mut decoder: impl ImageDecoder) -> Metadata {
    let mut metadata = decoder_metadata(&mut decoder);
    metadata.add_dimensions(decoder.dimensions());
    metadata
}

fn decode_image(mut decoder: impl ImageDecoder) -> image::ImageResult<DecodedImage> {
    let (width, height) = decoder.dimensions();
    if !valid_dimensions(width, height) || decoder.total_bytes() > MAX_DECODE_BYTES {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::InsufficientMemory),
        ));
    }

    let metadata = decoder_metadata(&mut decoder);
    Ok(DecodedImage {
        orientation: decoder.orientation()?,
        pixels: DynamicImage::from_decoder(decoder)?,
        premultiplied_alpha: false,
        source_dimensions: (width, height),
        metadata,
    })
}
