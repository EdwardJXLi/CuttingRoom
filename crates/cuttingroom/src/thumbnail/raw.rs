use std::{
    ffi::{c_int, c_uint, c_void},
    io::Cursor,
    mem::offset_of,
    ptr::NonNull,
    slice,
};

use image::{DynamicImage, GrayImage, RgbImage};
use libloading::Library;

use super::{Pixels, decode_jpeg, make_preview, native_library, valid_dimensions};

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

pub(super) struct RawDecoder {
    _library: Library,
    init: unsafe extern "C" fn(c_uint) -> *mut c_void,
    close: unsafe extern "C" fn(*mut c_void),
    open_buffer: unsafe extern "C" fn(*mut c_void, *const c_void, usize) -> c_int,
    unpack_thumb: unsafe extern "C" fn(*mut c_void) -> c_int,
    make_mem_thumb: unsafe extern "C" fn(*mut c_void, *mut c_int) -> *mut ProcessedImage,
    clear_mem: unsafe extern "C" fn(*mut ProcessedImage),
}

struct Decode<'a> {
    api: &'a RawDecoder,
    context: NonNull<c_void>,
    preview: Option<NonNull<ProcessedImage>>,
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

    pub(super) fn decode(&self, bytes: &[u8]) -> Option<Pixels> {
        // SAFETY: LibRaw creates a thread-local context. The Decode guard frees
        // it before the caller's read-only input can be released.
        let context = NonNull::new(unsafe { (self.init)(NO_DATA_ERROR_CALLBACK) })?;
        let mut decode = Decode {
            api: self,
            context,
            preview: None,
        };
        let mut error = 0;
        // SAFETY: The context belongs to this worker. LibRaw borrows bytes until
        // close. make_mem_thumb returns an owned buffer or null and writes error.
        unsafe {
            if (self.open_buffer)(context.as_ptr(), bytes.as_ptr().cast(), bytes.len()) != 0
                || (self.unpack_thumb)(context.as_ptr()) != 0
            {
                return None;
            }
            decode.preview = NonNull::new((self.make_mem_thumb)(context.as_ptr(), &mut error));
        }
        if error != 0 {
            return None;
        }
        decode.render()
    }
}

impl Decode<'_> {
    fn render(&self) -> Option<Pixels> {
        let preview = self.preview?;
        // SAFETY: make_mem_thumb returned a live allocation with this C header.
        let header = unsafe { preview.as_ref() };
        let len = header.data_size as usize;
        if len == 0 || len > MAX_EMBEDDED_BYTES {
            return None;
        }
        // SAFETY: LibRaw allocates data_size initialized bytes after the header.
        // The slice ends before this guard calls libraw_dcraw_clear_mem.
        let data = unsafe {
            let start = preview
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
                Some(make_preview(
                    image,
                    image::metadata::Orientation::NoTransforms,
                ))
            }
            _ => None,
        }
    }
}

impl Drop for Decode<'_> {
    fn drop(&mut self) {
        // SAFETY: Both pointers belong to this guard and are released once,
        // on the same worker that acquired them.
        unsafe {
            if let Some(preview) = self.preview {
                (self.api.clear_mem)(preview.as_ptr());
            }
            (self.api.close)(self.context.as_ptr());
        }
    }
}
