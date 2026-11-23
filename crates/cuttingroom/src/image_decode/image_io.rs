use std::ptr;

use super::valid_dimensions;
use image::{DynamicImage, RgbaImage};
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

pub(super) fn decode(bytes: &[u8], max_edge: u32) -> Result<Option<DynamicImage>, ()> {
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
    if !valid_dimensions(width, height) {
        return Err(());
    }

    scaled_image(&source, primary, max_edge)
        .as_deref()
        .map(|image| draw_srgb(image, max_edge))
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

fn scaled_image(
    source: &CGImageSource,
    primary: usize,
    max_edge: u32,
) -> Option<CFRetained<CGImage>> {
    let max_size = CFNumber::new_i32(i32::try_from(max_edge).ok()?);
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

fn draw_srgb(image: &CGImage, max_edge: u32) -> Result<DynamicImage, ()> {
    let width = u32::try_from(CGImage::width(Some(image))).map_err(|_| ())?;
    let height = u32::try_from(CGImage::height(Some(image))).map_err(|_| ())?;
    if !valid_dimensions(width, height) || width.max(height) > max_edge {
        return Err(());
    }

    // SAFETY: The immutable color space name is provided by Core Graphics.
    let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or(())?;
    let stride = width as usize * 4;
    let len = stride * height as usize;
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(len).map_err(|_| ())?;
    buffer.resize(len, 0_u8);
    {
        // SAFETY: The buffer has height rows of stride bytes with four writable bytes per
        // pixel. It stays exclusively owned by this worker and outlives the bitmap context.
        let context = unsafe {
            CGBitmapContextCreate(
                buffer.as_mut_ptr().cast(),
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

    for [red, green, blue, alpha] in buffer.as_chunks_mut::<4>().0 {
        let alpha = u16::from(*alpha);
        if alpha > 0 && alpha < 255 {
            let unpremultiply =
                |channel| ((u16::from(channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            *red = unpremultiply(*red);
            *green = unpremultiply(*green);
            *blue = unpremultiply(*blue);
        }
    }
    RgbaImage::from_raw(width, height, buffer)
        .map(DynamicImage::ImageRgba8)
        .ok_or(())
}
