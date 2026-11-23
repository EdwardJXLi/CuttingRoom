use std::{
    ffi::{c_int, c_uint, c_void},
    io::Cursor,
    mem::offset_of,
    ptr::NonNull,
    slice,
};

use image::{DynamicImage, GrayImage, RgbImage};
use libloading::Library;

use super::{DecodedImage, decode_jpeg, native_library, valid_dimensions};

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

pub(super) struct RawDecoder {
    _library: Library,
    init: unsafe extern "C" fn(c_uint) -> *mut c_void,
    close: unsafe extern "C" fn(*mut c_void),
    open_buffer: unsafe extern "C" fn(*mut c_void, *const c_void, usize) -> c_int,
    unpack_thumb: unsafe extern "C" fn(*mut c_void) -> c_int,
    make_mem_thumb: MakeImage,
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
                make_mem_thumb: symbol(&library, b"libraw_dcraw_make_mem_thumb")?,
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
    pub(crate) fn embedded_preview(&mut self) -> Option<DecodedImage> {
        // SAFETY: LibRaw successfully opened the context on this worker.
        if unsafe { (self.api.unpack_thumb)(self.context.as_ptr()) } != 0 {
            return None;
        }
        self.make_image(self.api.make_mem_thumb)?.decode()
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

impl ImageMemory<'_> {
    fn decode(&self) -> Option<DecodedImage> {
        // SAFETY: The pointer denotes a live LibRaw allocation with this C header.
        let header = unsafe { self.pixels.as_ref() };
        let len = header.data_size as usize;
        if len == 0 || len > MAX_EMBEDDED_BYTES {
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
            JPEG => decode_jpeg(Cursor::new(data)).ok(),
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
