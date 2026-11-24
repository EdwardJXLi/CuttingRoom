use std::{
    ffi::{c_int, c_void},
    io::Cursor,
    ptr::NonNull,
};

use image::{DynamicImage, ImageDecoder, RgbaImage, codecs::jpeg::JpegDecoder};
use turbojpeg_sys as tj;

use super::{DecodedImage, MAX_DECODE_BYTES, PreviewSize, decode_image, limits, valid_dimensions};
use crate::metadata::{self, Fields, add_dimensions};

// The C API uses size_t. The pregenerated bindings use unsigned long, which is narrower on MSVC.
unsafe extern "C" {
    fn tj3DecompressHeader(handle: tj::tjhandle, bytes: *const u8, len: usize) -> c_int;
    fn tj3Decompress8(
        handle: tj::tjhandle,
        bytes: *const u8,
        len: usize,
        pixels: *mut u8,
        pitch: c_int,
        format: c_int,
    ) -> c_int;
}

struct Decompressor<'a> {
    handle: NonNull<c_void>,
    bytes: &'a [u8],
}

impl<'a> Decompressor<'a> {
    fn open(bytes: &'a [u8]) -> Option<Self> {
        // SAFETY: TurboJPEG returns a new instance or null. This worker owns it until Drop.
        let handle = NonNull::new(unsafe { tj::tj3Init(tj::TJINIT_TJINIT_DECOMPRESS as c_int) })?;
        let decoder = Self { handle, bytes };
        // SAFETY: The handle is live and the decoder borrows the input until Drop.
        let status = unsafe { tj3DecompressHeader(handle.as_ptr(), bytes.as_ptr(), bytes.len()) };
        (status == 0).then_some(decoder)
    }

    fn get(&self, parameter: tj::TJPARAM) -> c_int {
        // SAFETY: The handle remains live on this worker. The getter does not retain any pointers.
        unsafe { tj::tj3Get(self.handle.as_ptr(), parameter as c_int) }
    }

    fn dimensions(&self) -> Option<(u32, u32)> {
        let width = u32::try_from(self.get(tj::TJPARAM_TJPARAM_JPEGWIDTH)).ok()?;
        let height = u32::try_from(self.get(tj::TJPARAM_TJPARAM_JPEGHEIGHT)).ok()?;
        (width > 0 && height > 0).then_some((width, height))
    }

    fn rgba(&mut self, size: PreviewSize) -> Option<(RgbaImage, (u32, u32))> {
        let (width, height) = self.dimensions()?;
        if !valid_dimensions(width, height) {
            return None;
        }
        let eighths = match size {
            PreviewSize::Thumbnail if self.get(tj::TJPARAM_TJPARAM_LOSSLESS) == 0 => {
                (size.max_edge() * 8).div_ceil(width.max(height)).min(8)
            }
            _ => 8,
        };
        let handle = self.handle.as_ptr();
        // TurboJPEG expects the fraction in lowest terms.
        let divisor = 1 << eighths.trailing_zeros();
        let scale = tj::tjscalingfactor {
            num: (eighths / divisor) as c_int,
            denom: (8 / divisor) as c_int,
        };
        let max_memory_mib = (MAX_DECODE_BYTES / (1024 * 1024)) as c_int;
        // SAFETY: All calls configure the live decompressor on its owning worker.
        // Match the previous decoder's 100-scan limit for progressive JPEGs.
        let configured = unsafe {
            tj::tj3SetScalingFactor(handle, scale) == 0
                && tj::tj3Set(
                    handle,
                    tj::TJPARAM_TJPARAM_MAXMEMORY as c_int,
                    max_memory_mib,
                ) == 0
                && tj::tj3Set(handle, tj::TJPARAM_TJPARAM_SCANLIMIT as c_int, 100) == 0
        };
        if !configured {
            return None;
        }

        let scaled_width = (width * eighths).div_ceil(8);
        let scaled_height = (height * eighths).div_ceil(8);
        let stride = scaled_width * 4;
        let byte_count = (stride * scaled_height) as usize;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(byte_count).ok()?;
        pixels.resize(byte_count, 0);
        // SAFETY: The same source supplied the validated dimensions. The configured factor
        // rounds dimensions up to the allocated size. Each row has stride writable bytes.
        // TurboJPEG borrows both buffers only for this synchronous call.
        let status = unsafe {
            tj3Decompress8(
                handle,
                self.bytes.as_ptr(),
                self.bytes.len(),
                pixels.as_mut_ptr(),
                stride as c_int,
                tj::TJPF_TJPF_RGBA as c_int,
            )
        };
        if status != 0 {
            return None;
        }
        Some((
            RgbaImage::from_raw(scaled_width, scaled_height, pixels)?,
            (width, height),
        ))
    }
}

impl Drop for Decompressor<'_> {
    fn drop(&mut self) {
        // SAFETY: This is the sole owner of the handle and all synchronous calls have returned.
        unsafe { tj::tj3Destroy(self.handle.as_ptr()) };
    }
}

pub(super) fn decode(bytes: &[u8], size: PreviewSize) -> Option<DecodedImage> {
    let mut decoder = Decompressor::open(bytes)?;
    if matches!(
        decoder.get(tj::TJPARAM_TJPARAM_COLORSPACE) as tj::TJCS,
        tj::TJCS_TJCS_CMYK | tj::TJCS_TJCS_YCCK
    ) {
        // TurboJPEG outputs CMYK samples for these files. Preserve the existing RGB conversion.
        let mut decoder = JpegDecoder::new(Cursor::new(bytes)).ok()?;
        decoder.set_limits(limits()).ok()?;
        return decode_image(decoder).ok();
    }

    let (pixels, source_dimensions) = decoder.rgba(size)?;
    let (metadata, orientation) = metadata::read_jpeg(bytes);
    Some(DecodedImage {
        pixels: DynamicImage::ImageRgba8(pixels),
        orientation,
        premultiplied_alpha: false,
        source_dimensions,
        metadata,
    })
}

pub(super) fn inspect(bytes: &[u8]) -> Option<Fields> {
    let dimensions = Decompressor::open(bytes)?.dimensions()?;
    let mut fields = metadata::read_container(bytes).details;
    add_dimensions(&mut fields, dimensions);
    Some(fields)
}
