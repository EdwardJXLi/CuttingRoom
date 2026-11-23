use std::{
    ffi::{c_char, c_int, c_uint, c_void},
    ptr::{self, NonNull},
    slice,
};

use image::{DynamicImage, RgbaImage};
use libloading::Library;

use super::{
    MAX_DECODE_BYTES,
    native_library::{open, symbol},
    valid_dimensions,
};

const MINIMUM_VERSION: u32 = 0x0117_0700;
const RGB: c_uint = 1;
const INTERLEAVED_RGBA: c_uint = 11;
const INTERLEAVED_CHANNEL: c_uint = 10;
const RGBA_BYTES: usize = 4;

enum Context {}
enum ImageHandle {}
enum DecodedImage {}

#[repr(C)]
struct HeifError {
    code: c_uint,
    subcode: c_uint,
    message: *const c_char,
}

// The fields through max_total_memory are the stable prefix of version 2.
#[repr(C)]
struct SecurityLimits {
    version: u8,
    max_image_size_pixels: u64,
    max_number_of_tiles: u64,
    max_bayer_pattern_pixels: u32,
    max_items: u32,
    max_color_profile_size: u32,
    max_memory_block_size: u64,
    max_components: u32,
    max_iloc_extents_per_item: u32,
    max_size_entity_group: u32,
    max_children_per_box: u32,
    max_total_memory: u64,
}

pub(super) struct HeifDecoder {
    _library: Library,
    functions: HeifFunctions,
}

struct HeifFunctions {
    deinit: unsafe extern "C" fn(),
    allocate_context: unsafe extern "C" fn() -> *mut Context,
    free_context: unsafe extern "C" fn(*mut Context),
    security_limits: unsafe extern "C" fn(*const Context) -> *mut SecurityLimits,
    set_decoding_threads: unsafe extern "C" fn(*mut Context, c_int),
    read_memory:
        unsafe extern "C" fn(*mut Context, *const c_void, usize, *const c_void) -> HeifError,
    primary_image: unsafe extern "C" fn(*mut Context, *mut *mut ImageHandle) -> HeifError,
    release_handle: unsafe extern "C" fn(*const ImageHandle),
    width: unsafe extern "C" fn(*const ImageHandle) -> c_int,
    height: unsafe extern "C" fn(*const ImageHandle) -> c_int,
    decode_image: unsafe extern "C" fn(
        *const ImageHandle,
        *mut *mut DecodedImage,
        c_uint,
        c_uint,
        *const c_void,
    ) -> HeifError,
    release_image: unsafe extern "C" fn(*const DecodedImage),
    plane_width: unsafe extern "C" fn(*const DecodedImage, c_uint) -> c_int,
    plane_height: unsafe extern "C" fn(*const DecodedImage, c_uint) -> c_int,
    plane_data: unsafe extern "C" fn(*const DecodedImage, c_uint, *mut usize) -> *const u8,
}

impl HeifDecoder {
    pub(super) fn new() -> Option<Self> {
        #[cfg(target_os = "linux")]
        let names = &["libheif.so.1", "libheif.so"];
        #[cfg(target_os = "macos")]
        let names = &["libheif.1.dylib", "libheif.dylib"];
        #[cfg(target_os = "windows")]
        let names = &["heif.dll", "libheif.dll"];

        let library = open(names)?;
        // SAFETY: The version signature matches libheif's C export.
        let version: unsafe extern "C" fn() -> u32 =
            unsafe { symbol(&library, b"heif_get_version_number")? };
        // SAFETY: Reading the library version requires no context or prior initialization.
        let version = unsafe { version() };
        if version >> 24 != 1 || version < MINIMUM_VERSION {
            return None;
        }
        // SAFETY: The resolved signatures match the versioned C API. The library stays
        // loaded with the function pointers, and the worker owns initialization and cleanup.
        let functions = unsafe {
            HeifFunctions {
                deinit: symbol(&library, b"heif_deinit")?,
                allocate_context: symbol(&library, b"heif_context_alloc")?,
                free_context: symbol(&library, b"heif_context_free")?,
                security_limits: symbol(&library, b"heif_context_get_security_limits")?,
                set_decoding_threads: symbol(&library, b"heif_context_set_max_decoding_threads")?,
                read_memory: symbol(&library, b"heif_context_read_from_memory_without_copy")?,
                primary_image: symbol(&library, b"heif_context_get_primary_image_handle")?,
                release_handle: symbol(&library, b"heif_image_handle_release")?,
                width: symbol(&library, b"heif_image_handle_get_width")?,
                height: symbol(&library, b"heif_image_handle_get_height")?,
                decode_image: symbol(&library, b"heif_decode_image")?,
                release_image: symbol(&library, b"heif_image_release")?,
                plane_width: symbol(&library, b"heif_image_get_width")?,
                plane_height: symbol(&library, b"heif_image_get_height")?,
                plane_data: symbol(&library, b"heif_image_get_plane_readonly2")?,
            }
        };
        // SAFETY: The initializer signature matches libheif's C export.
        let init: unsafe extern "C" fn(*mut c_void) -> HeifError =
            unsafe { symbol(&library, b"heif_init")? };
        // SAFETY: A null parameter selects libheif's defaults. Drop balances successful init.
        if unsafe { init(ptr::null_mut()) }.code != 0 {
            return None;
        }
        Some(Self {
            _library: library,
            functions,
        })
    }

    pub(super) fn decode(&self, bytes: &[u8]) -> Option<DynamicImage> {
        if bytes.is_empty() {
            return None;
        }
        let functions = &self.functions;
        // SAFETY: The worker owns the initialized library. The returned context is owned.
        let context = NonNull::new(unsafe { (functions.allocate_context)() })?;
        let mut decode = Decode {
            functions,
            context,
            handle: None,
            image: None,
        };
        // SAFETY: Libheif owns these limits for the context's lifetime. This context is
        // exclusive to this call, which updates only the fields supported by its version.
        let limits = unsafe { (functions.security_limits)(context.as_ptr()).as_mut() }?;
        if limits.version < 2 {
            return None;
        }
        limits.max_image_size_pixels = MAX_DECODE_BYTES / RGBA_BYTES as u64;
        limits.max_memory_block_size = MAX_DECODE_BYTES;
        limits.max_total_memory = MAX_DECODE_BYTES;
        // SAFETY: The context remains live on this worker until Decode drops it. The input
        // slice stays live through cleanup because libheif borrows it without copying.
        let result = unsafe {
            (functions.set_decoding_threads)(context.as_ptr(), 0);
            (functions.read_memory)(
                context.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                ptr::null(),
            )
        };
        if result.code != 0 {
            return None;
        }

        let mut handle = ptr::null_mut();
        // SAFETY: The loaded context is live. The output slot is writable for one pointer.
        if unsafe { (functions.primary_image)(context.as_ptr(), &mut handle) }.code != 0 {
            return None;
        }
        let handle = NonNull::new(handle)?;
        decode.handle = Some(handle);
        // SAFETY: The handle is a live, owned primary-image handle.
        let (width, height) = unsafe {
            (
                (functions.width)(handle.as_ptr()),
                (functions.height)(handle.as_ptr()),
            )
        };
        dimensions(width, height)?;

        let mut image = ptr::null_mut();
        // SAFETY: The handle is live and the output slot is writable. Libheif applies
        // the file's geometric transformations and returns interleaved 8-bit RGBA.
        let result = unsafe {
            (functions.decode_image)(
                handle.as_ptr(),
                &mut image,
                RGB,
                INTERLEAVED_RGBA,
                ptr::null(),
            )
        };
        if result.code != 0 {
            return None;
        }
        let image = NonNull::new(image)?;
        decode.image = Some(image);
        self.copy_pixels(image)
    }

    fn copy_pixels(&self, image: NonNull<DecodedImage>) -> Option<DynamicImage> {
        let functions = &self.functions;
        let mut stride = 0;
        // SAFETY: The decoder retains the image until the copy completes. The requested
        // interleaved plane has four 8-bit channels and a byte stride returned by libheif.
        let (width, height, plane) = unsafe {
            (
                (functions.plane_width)(image.as_ptr(), INTERLEAVED_CHANNEL),
                (functions.plane_height)(image.as_ptr(), INTERLEAVED_CHANNEL),
                (functions.plane_data)(image.as_ptr(), INTERLEAVED_CHANNEL, &mut stride),
            )
        };
        let (width, height) = dimensions(width, height)?;
        let row_bytes = width as usize * RGBA_BYTES;
        let rows = height as usize;
        let extent = (rows - 1).checked_mul(stride)?.checked_add(row_bytes)?;
        if plane.is_null()
            || stride < row_bytes
            || extent > isize::MAX as usize
            || extent as u64 > MAX_DECODE_BYTES
        {
            return None;
        }

        let mut pixels = Vec::new();
        pixels.try_reserve_exact(rows * row_bytes).ok()?;
        for row in 0..rows {
            // SAFETY: This row lies in the live interleaved plane, using the actual
            // decoded plane bounds and validated stride. We copy only its pixel data.
            let source = unsafe { slice::from_raw_parts(plane.add(row * stride), row_bytes) };
            pixels.extend_from_slice(source);
        }
        Some(DynamicImage::ImageRgba8(RgbaImage::from_raw(
            width, height, pixels,
        )?))
    }
}

impl Drop for HeifDecoder {
    fn drop(&mut self) {
        // SAFETY: Init succeeded once. All contexts are freed before the worker drops the library.
        unsafe { (self.functions.deinit)() };
    }
}

struct Decode<'a> {
    functions: &'a HeifFunctions,
    context: NonNull<Context>,
    handle: Option<NonNull<ImageHandle>>,
    image: Option<NonNull<DecodedImage>>,
}

impl Drop for Decode<'_> {
    fn drop(&mut self) {
        // SAFETY: These pointers are owned libheif objects returned by successful calls.
        // Release dependents first while the input buffer and decoder remain live.
        unsafe {
            if let Some(image) = self.image {
                (self.functions.release_image)(image.as_ptr());
            }
            if let Some(handle) = self.handle {
                (self.functions.release_handle)(handle.as_ptr());
            }
            (self.functions.free_context)(self.context.as_ptr());
        }
    }
}

fn dimensions(width: c_int, height: c_int) -> Option<(u32, u32)> {
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    valid_dimensions(width, height).then_some((width, height))
}
