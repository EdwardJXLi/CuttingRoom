use std::{
    ffi::{c_int, c_uint, c_void},
    mem::offset_of,
    ptr::NonNull,
    slice,
};

use image::{DynamicImage, GrayImage, RgbImage};
use libloading::Library;

use super::{DecodedImage, MAX_DECODE_BYTES, PreviewSize, jpeg, native_library, valid_dimensions};
use crate::metadata::{Metadata, add, camera_name, clean_text};

#[cfg(target_os = "linux")]
const LIBRARIES: &[&str] = &["libraw_r.so.25", "libraw.so.25"];
#[cfg(target_os = "macos")]
const LIBRARIES: &[&str] = &["libraw_r.25.dylib", "libraw.25.dylib"];
#[cfg(target_os = "windows")]
const LIBRARIES: &[&str] = &["libraw.dll", "raw.dll"];

const NO_DATA_ERROR_CALLBACK: c_uint = 1 << 1;
const JPEG: c_int = 1;
const BITMAP: c_int = 2;
const MIN_VERSION: c_int = (22 << 8) | 3;
const NEXT_ABI_VERSION: c_int = 23 << 8;
const MAX_EMBEDDED_BYTES: usize = 64 * 1024 * 1024;

#[repr(C)]
struct ProcessedImage {
    format: c_int,
    height: u16,
    width: u16,
    colors: u16,
    bits: u16,
    data_size: c_uint,
    data: [u8; 0],
}

type MakeImage = unsafe extern "C" fn(*mut c_void, *mut c_int) -> *mut ProcessedImage;

// LibRaw 0.22 metadata prefixes. The supported desktop targets use a 64-bit time_t.
#[repr(C)]
struct Camera {
    _guard: [u8; 4],
    make: [u8; 64],
    model: [u8; 64],
}

#[repr(C)]
struct Lens {
    _optics: [f32; 5],
    make: [u8; 128],
    model: [u8; 128],
}

#[repr(C)]
struct Capture {
    iso: f32,
    shutter: f32,
    aperture: f32,
    focal_length: f32,
    timestamp: i64,
}

pub(super) struct RawDecoder {
    _library: Library,
    init: unsafe extern "C" fn(c_uint) -> *mut c_void,
    close: unsafe extern "C" fn(*mut c_void),
    open_buffer: unsafe extern "C" fn(*mut c_void, *const c_void, usize) -> c_int,
    unpack_thumb: unsafe extern "C" fn(*mut c_void) -> c_int,
    unpack: unsafe extern "C" fn(*mut c_void) -> c_int,
    process: unsafe extern "C" fn(*mut c_void) -> c_int,
    raw_width: unsafe extern "C" fn(*mut c_void) -> c_int,
    raw_height: unsafe extern "C" fn(*mut c_void) -> c_int,
    image_width: unsafe extern "C" fn(*mut c_void) -> c_int,
    image_height: unsafe extern "C" fn(*mut c_void) -> c_int,
    camera: unsafe extern "C" fn(*mut c_void) -> *const Camera,
    lens: unsafe extern "C" fn(*mut c_void) -> *const Lens,
    capture: unsafe extern "C" fn(*mut c_void) -> *const Capture,
    make_mem_thumb: MakeImage,
    make_mem_image: MakeImage,
    clear_mem: unsafe extern "C" fn(*mut ProcessedImage),
}

pub(crate) struct RawImage<'a> {
    api: &'a RawDecoder,
    context: NonNull<c_void>,
    _source: &'a [u8],
}

struct ImageMemory<'a> {
    api: &'a RawDecoder,
    pixels: NonNull<ProcessedImage>,
}

impl RawDecoder {
    pub(super) fn new() -> Option<Self> {
        let library = native_library::open(LIBRARIES)?;
        use native_library::symbol;

        // SAFETY: These are the C signatures from LibRaw 0.22. The function
        // pointers remain private to the struct that owns the library.
        unsafe {
            let version: unsafe extern "C" fn() -> c_int =
                symbol(&library, b"libraw_versionNumber")?;
            if !(MIN_VERSION..NEXT_ABI_VERSION).contains(&version()) {
                return None;
            }
            Some(Self {
                init: symbol(&library, b"libraw_init")?,
                close: symbol(&library, b"libraw_close")?,
                open_buffer: symbol(&library, b"libraw_open_buffer")?,
                unpack_thumb: symbol(&library, b"libraw_unpack_thumb")?,
                unpack: symbol(&library, b"libraw_unpack")?,
                process: symbol(&library, b"libraw_dcraw_process")?,
                raw_width: symbol(&library, b"libraw_get_raw_width")?,
                raw_height: symbol(&library, b"libraw_get_raw_height")?,
                image_width: symbol(&library, b"libraw_get_iwidth")?,
                image_height: symbol(&library, b"libraw_get_iheight")?,
                camera: symbol(&library, b"libraw_get_iparams")?,
                lens: symbol(&library, b"libraw_get_lensinfo")?,
                capture: symbol(&library, b"libraw_get_imgother")?,
                make_mem_thumb: symbol(&library, b"libraw_dcraw_make_mem_thumb")?,
                make_mem_image: symbol(&library, b"libraw_dcraw_make_mem_image")?,
                clear_mem: symbol(&library, b"libraw_dcraw_clear_mem")?,
                _library: library,
            })
        }
    }

    pub(super) fn open<'a>(&'a self, bytes: &'a [u8]) -> Option<RawImage<'a>> {
        // SAFETY: The worker owns the new context and RawImage closes it once.
        let context = NonNull::new(unsafe { (self.init)(NO_DATA_ERROR_CALLBACK) })?;
        let image = RawImage {
            api: self,
            context,
            _source: bytes,
        };
        // SAFETY: The context is live. RawImage borrows the source until after close.
        let status =
            unsafe { (self.open_buffer)(context.as_ptr(), bytes.as_ptr().cast(), bytes.len()) };
        (status == 0).then_some(image)
    }
}

impl<'a> RawImage<'a> {
    pub(crate) fn embedded_preview(&mut self, size: PreviewSize) -> Option<DecodedImage> {
        // SAFETY: LibRaw successfully opened the context on this worker.
        if unsafe { (self.api.unpack_thumb)(self.context.as_ptr()) } != 0 {
            return None;
        }
        let mut image = self
            .make_image(self.api.make_mem_thumb)?
            .decode(MAX_EMBEDDED_BYTES, size)?;
        if let Some(dimensions) = self.dimensions() {
            image.source_dimensions = dimensions;
        }
        image.metadata = self.metadata();
        Some(image)
    }

    pub(crate) fn render(self, cancelled: impl Fn() -> bool) -> Option<DecodedImage> {
        let context = self.context.as_ptr();
        // SAFETY: These getters read the sizes of a live, opened RAW context.
        let (raw_width, raw_height, width, height) = unsafe {
            (
                (self.api.raw_width)(context),
                (self.api.raw_height)(context),
                (self.api.image_width)(context),
                (self.api.image_height)(context),
            )
        };
        if cancelled()
            || !valid_dimensions(
                u32::try_from(raw_width).ok()?,
                u32::try_from(raw_height).ok()?,
            )
            || !valid_dimensions(u32::try_from(width).ok()?, u32::try_from(height).ok()?)
        {
            return None;
        }
        // SAFETY: The opened context is exclusive to this worker. Unpack must succeed before processing.
        if unsafe { (self.api.unpack)(context) } != 0 || cancelled() {
            return None;
        }
        // SAFETY: LibRaw processes the unpacked pixels within the live context.
        if unsafe { (self.api.process)(context) } != 0 || cancelled() {
            return None;
        }
        let metadata = self.metadata();
        let memory = self.make_image(self.api.make_mem_image)?;
        drop(self);
        let mut image = memory.decode(MAX_DECODE_BYTES as usize, PreviewSize::Photo)?;
        image.source_dimensions = (width as u32, height as u32);
        image.metadata = metadata;
        Some(image)
    }

    pub(crate) fn dimensions(&self) -> Option<(u32, u32)> {
        // SAFETY: The getters read dimensions from the live RAW context.
        let (width, height) = unsafe {
            (
                (self.api.image_width)(self.context.as_ptr()),
                (self.api.image_height)(self.context.as_ptr()),
            )
        };
        (width > 0 && height > 0).then_some((width as u32, height as u32))
    }

    pub(crate) fn metadata(&self) -> Metadata {
        let context = self.context.as_ptr();
        // SAFETY: The opened context belongs to this worker. Getters return borrowed
        // metadata prefixes that remain valid while this context is live and unchanged.
        let (camera, lens, capture) = unsafe {
            (
                (self.api.camera)(context).as_ref(),
                (self.api.lens)(context).as_ref(),
                (self.api.capture)(context).as_ref(),
            )
        };
        let mut metadata = Metadata::default();
        let summary = &mut metadata.summary;
        let details = &mut metadata.details;
        if let Some(camera) = camera {
            let make = raw_text(&camera.make);
            let model = raw_text(&camera.model);
            add(summary, "Camera", camera_name(&make, &model));
            add(details, "Camera make", make);
            add(details, "Camera model", model);
        }
        if let Some(lens) = lens {
            let make = raw_text(&lens.make);
            let model = raw_text(&lens.model);
            add(summary, "Lens", camera_name(&make, &model));
            add(details, "Lens make", make);
            add(details, "Lens model", model);
        }
        if let Some(capture) = capture {
            if capture.timestamp > 0
                && let Some(date) = chrono::DateTime::from_timestamp(capture.timestamp, 0)
            {
                let local = date.with_timezone(&chrono::Local);
                add(
                    summary,
                    "Captured",
                    local.format("%Y-%m-%d %H:%M").to_string(),
                );
                add(
                    details,
                    "Captured",
                    local.format("%Y-%m-%d %H:%M:%S").to_string(),
                );
            }
            let mut exposure = Vec::new();
            if positive(capture.shutter) {
                let shutter = if capture.shutter < 0.5 {
                    format!("1/{:.0} s", f64::from(capture.shutter).recip())
                } else {
                    format!("{} s", capture.shutter)
                };
                add(details, "Shutter speed", shutter.clone());
                exposure.push(shutter);
            }
            if positive(capture.aperture) {
                let aperture = format!("f/{:.1}", capture.aperture);
                add(details, "Aperture", aperture.clone());
                exposure.push(aperture);
            }
            add(summary, "Exposure", exposure.join(", "));
            if positive(capture.iso) {
                let iso = format!("{:.0}", capture.iso);
                add(summary, "ISO", iso.clone());
                add(details, "ISO", iso);
            }
            if positive(capture.focal_length) {
                add(
                    details,
                    "Focal length",
                    format!("{} mm", capture.focal_length),
                );
            }
        }
        metadata
    }

    fn make_image(&self, make: MakeImage) -> Option<ImageMemory<'a>> {
        let mut error = 0;
        // SAFETY: The context stays live for this call. LibRaw writes the status and
        // returns a separate allocation that must be freed with dcraw_clear_mem.
        let pixels = NonNull::new(unsafe { make(self.context.as_ptr(), &mut error) })?;
        (error == 0).then_some(ImageMemory {
            api: self.api,
            pixels,
        })
    }
}

fn raw_text(bytes: &[u8]) -> String {
    clean_text(bytes.split(|byte| *byte == 0).next().unwrap_or_default())
}

fn positive(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

impl ImageMemory<'_> {
    fn decode(&self, max_bytes: usize, size: PreviewSize) -> Option<DecodedImage> {
        // SAFETY: The pointer denotes a live LibRaw allocation with this C header.
        let header = unsafe { self.pixels.as_ref() };
        let len = header.data_size as usize;
        if len == 0 || len > max_bytes {
            return None;
        }
        // SAFETY: LibRaw allocates data_size initialized bytes after the header.
        // The slice ends before this guard calls libraw_dcraw_clear_mem.
        let data = unsafe {
            let start = self
                .pixels
                .as_ptr()
                .cast::<u8>()
                .add(offset_of!(ProcessedImage, data));
            slice::from_raw_parts(start, len)
        };
        match header.format {
            JPEG => jpeg::decode(data, size),
            BITMAP if header.bits == 8 => {
                let width = u32::from(header.width);
                let height = u32::from(header.height);
                if !valid_dimensions(width, height)
                    || u64::from(width) * u64::from(height) * u64::from(header.colors) != len as u64
                {
                    return None;
                }
                let image = match header.colors {
                    1 => {
                        DynamicImage::ImageLuma8(GrayImage::from_raw(width, height, data.to_vec())?)
                    }
                    3 => DynamicImage::ImageRgb8(RgbImage::from_raw(width, height, data.to_vec())?),
                    _ => return None,
                };
                Some(DecodedImage::upright(image))
            }
            _ => None,
        }
    }
}

impl Drop for RawImage<'_> {
    fn drop(&mut self) {
        // SAFETY: The context is released once, on its owning worker, before the source is released.
        unsafe { (self.api.close)(self.context.as_ptr()) };
    }
}

impl Drop for ImageMemory<'_> {
    fn drop(&mut self) {
        // SAFETY: The separate allocation is released once on the worker that received it.
        unsafe { (self.api.clear_mem)(self.pixels.as_ptr()) };
    }
}
