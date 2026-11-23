use std::{mem::size_of, ptr};

use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_image_io::{
    CGImageSource, kCGImagePropertyPixelHeight, kCGImagePropertyPixelWidth,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceShouldAllowFloat, kCGImageSourceShouldCacheImmediately,
    kCGImageSourceThumbnailMaxPixelSize,
};
use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::{MAX_DECODE_BYTES, MAX_IMAGE_EDGE, THUMBNAIL_EDGE};

pub(super) fn decode(bytes: &[u8]) -> Result<Option<SharedPixelBuffer<Rgba8Pixel>>, ()> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let len = isize::try_from(bytes.len()).map_err(|_| ())?;
    // SAFETY: The slice provides len readable bytes. CFData copies them and owns the copy.
    let data = unsafe { CFData::new(None, bytes.as_ptr(), len) }.ok_or(())?;
    // SAFETY: The retained CFData is valid and the source uses default creation options.
    let Some(source) = (unsafe { CGImageSource::with_data(&data, None) }) else {
        return Ok(None);
    };
    let Some((primary, width, height)) = primary_image(&source) else {
        return Ok(None);
    };
    if width.max(height) > MAX_IMAGE_EDGE
        || u64::from(width) * u64::from(height) * 4 > MAX_DECODE_BYTES
    {
        return Err(());
    }

    thumbnail(&source, primary)
        .as_deref()
        .map(draw_srgb)
        .transpose()
}

fn primary_image(source: &CGImageSource) -> Option<(usize, u32, u32)> {
    // SAFETY: The source remains retained on this worker throughout the ImageIO calls.
    let (primary, count) = unsafe { (source.primary_image_index(), source.count()) };
    if primary >= count {
        return None;
    }

    // SAFETY: The primary index belongs to source and the metadata query uses default options.
    let properties = unsafe { source.properties_at_index(primary, None) }?;
    // SAFETY: These ImageIO statics are immutable, framework-owned CFString keys.
    unsafe {
        Some((
            primary,
            dimension(&properties, kCGImagePropertyPixelWidth)?,
            dimension(&properties, kCGImagePropertyPixelHeight)?,
        ))
    }
}

fn thumbnail(source: &CGImageSource, primary: usize) -> Option<CFRetained<CGImage>> {
    let max_size = CFNumber::new_i32(i32::try_from(THUMBNAIL_EDGE).ok()?);
    let yes = CFBoolean::new(true);
    let no = CFBoolean::new(false);
    // SAFETY: These are framework-owned CFString keys. ImageIO expects CFNumber for the
    // pixel limit and CFBoolean for each other option. The dictionary retains its values.
    let options = unsafe {
        CFDictionary::<CFString, CFType>::from_slices(
            &[
                kCGImageSourceThumbnailMaxPixelSize,
                kCGImageSourceCreateThumbnailFromImageAlways,
                kCGImageSourceCreateThumbnailWithTransform,
                kCGImageSourceShouldCacheImmediately,
                kCGImageSourceShouldAllowFloat,
            ],
            &[
                max_size.as_ref(),
                yes.as_ref(),
                yes.as_ref(),
                yes.as_ref(),
                no.as_ref(),
            ],
        )
    };
    // SAFETY: The options dictionary has the types required by ImageIO and primary is valid.
    unsafe { source.thumbnail_at_index(primary, Some(options.as_opaque())) }
}

fn dimension(properties: &CFDictionary, key: &CFString) -> Option<u32> {
    // SAFETY: ImageIO returns a dictionary of CF objects. The result is borrowed for the
    // retained dictionary's lifetime, and its concrete type is checked before reading it.
    let object = unsafe {
        properties
            .value(ptr::from_ref(key).cast())
            .cast::<CFType>()
            .as_ref()
    }?;
    let value = u32::try_from(object.downcast_ref::<CFNumber>()?.as_i64()?).ok()?;
    (value != 0).then_some(value)
}

fn draw_srgb(image: &CGImage) -> Result<SharedPixelBuffer<Rgba8Pixel>, ()> {
    let width = u32::try_from(CGImage::width(Some(image))).map_err(|_| ())?;
    let height = u32::try_from(CGImage::height(Some(image))).map_err(|_| ())?;
    if width == 0 || height == 0 || width.max(height) > THUMBNAIL_EDGE {
        return Err(());
    }

    // SAFETY: The immutable color space name is provided by Core Graphics.
    let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or(())?;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    let stride = width as usize * size_of::<Rgba8Pixel>();
    {
        // SAFETY: The buffer has height rows of stride bytes with four writable bytes per
        // pixel. It stays exclusively owned by this worker and outlives the bitmap context.
        let context = unsafe {
            CGBitmapContextCreate(
                buffer.make_mut_bytes().as_mut_ptr().cast(),
                width as usize,
                height as usize,
                8,
                stride,
                Some(&color_space),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }
        .ok_or(())?;
        CGContext::draw_image(
            Some(&context),
            CGRect::new(
                Default::default(),
                CGSize::new(f64::from(width), f64::from(height)),
            ),
            Some(image),
        );
    }

    for pixel in buffer.make_mut_slice() {
        let alpha = u16::from(pixel.a);
        if alpha > 0 && alpha < 255 {
            let unpremultiply =
                |channel| ((u16::from(channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            pixel.r = unpremultiply(pixel.r);
            pixel.g = unpremultiply(pixel.g);
            pixel.b = unpremultiply(pixel.b);
        }
    }
    Ok(buffer)
}
