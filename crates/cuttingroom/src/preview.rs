use std::path::{Path, PathBuf};

use cap_std::fs::Dir;
use cr_project::{PhotographFormat, photograph_format};
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::image_decode::{DecodeError, DecodedImage, Decoder, PreviewSize, read_image_file};
use crate::metadata::{Fields, MetadataSource, add_dimensions, add_format, filename};

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

#[derive(Clone)]
pub(crate) struct Preview {
    pub(crate) pixels: Result<Pixels, DecodeError>,
    pub(crate) filename: String,
    pub(crate) summary: Fields,
}

struct Rendered<'a> {
    path: &'a Path,
    pixels: Pixels,
    summary: Fields,
}

pub(crate) fn load(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[PathBuf],
    size: PreviewSize,
    cancelled: impl Fn() -> bool,
) -> Preview {
    let sources = ordered_sources(paths);
    let preferred = sources.first().map(|(_, path)| *path);
    let (rasters, raws) =
        sources.split_at(sources.partition_point(|(format, _)| *format != PhotographFormat::Raw));
    match render(decoder, directory, rasters, raws, size, cancelled) {
        Ok(rendered) => Preview {
            filename: filename(rendered.path),
            pixels: Ok(rendered.pixels),
            summary: rendered.summary,
        },
        Err(error) => Preview {
            filename: preferred.map(filename).unwrap_or_default(),
            pixels: Err(error),
            summary: Fields::new(),
        },
    }
}

pub(crate) fn read_summary(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[PathBuf],
    cancelled: impl Fn() -> bool,
) -> Option<MetadataSource> {
    for (format, path) in ordered_sources(paths) {
        if cancelled() {
            break;
        }
        if let Ok(metadata) = decoder.inspect(directory, path, format) {
            let mut fields = metadata.summary;
            add_format(&mut fields, path);
            return Some(MetadataSource {
                filename: filename(path),
                fields,
            });
        }
    }
    None
}

fn ordered_sources(paths: &[PathBuf]) -> Vec<(PhotographFormat, &Path)> {
    use PhotographFormat::{Jpeg, Png, Raw, Tiff};

    let mut sources: Vec<_> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter_map(|path| Some((photograph_format(path)?, path)))
        .collect();
    sources.sort_by_key(|(format, _)| match format {
        Jpeg => 0,
        Png => 1,
        Tiff => 2,
        Raw => 4,
        _ => 3,
    });
    sources
}

fn render<'a>(
    decoder: &Decoder,
    directory: &Dir,
    rasters: &[(PhotographFormat, &'a Path)],
    raws: &[(PhotographFormat, &'a Path)],
    size: PreviewSize,
    cancelled: impl Fn() -> bool,
) -> Result<Rendered<'a>, DecodeError> {
    use DecodeError::{DecoderUnavailable, Unavailable};

    let mut error = Unavailable;
    for &(format, path) in rasters {
        if cancelled() {
            return Err(Unavailable);
        }
        match decoder
            .raster(directory, path, format, size)
            .and_then(|image| with_metadata(image, path, size, &cancelled))
        {
            Ok(rendered) => return Ok(rendered),
            Err(DecoderUnavailable) => error = DecoderUnavailable,
            Err(Unavailable) => (),
        }
    }

    match load_raw(decoder, directory, raws, size, &cancelled) {
        Err(Unavailable) => Err(error),
        result => result,
    }
}

fn load_raw<'a>(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[(PhotographFormat, &'a Path)],
    size: PreviewSize,
    cancelled: impl Fn() -> bool,
) -> Result<Rendered<'a>, DecodeError> {
    use DecodeError::{DecoderUnavailable, Unavailable};

    let mut first_source = None;
    let mut alternate_paths = Vec::new();
    for &(_, path) in paths {
        if cancelled() {
            return Err(Unavailable);
        }
        let Some(bytes) = read_image_file(directory, path) else {
            continue;
        };
        let mut raw = match decoder.raw(&bytes) {
            Ok(raw) => raw,
            Err(DecoderUnavailable) => return Err(DecoderUnavailable),
            Err(Unavailable) => continue,
        };
        if let Some(image) = raw.embedded_preview(size) {
            return with_metadata(image, path, size, &cancelled);
        }
        drop(raw);
        if first_source.is_none() {
            first_source = Some((path, bytes));
        } else {
            alternate_paths.push(path);
        }
    }

    let (path, bytes) = first_source.ok_or(Unavailable)?;
    if cancelled() {
        return Err(Unavailable);
    }
    if let Some(image) = decoder.raw(&bytes)?.render(&cancelled) {
        return with_metadata(image, path, size, &cancelled);
    }
    drop(bytes);
    for path in alternate_paths {
        if cancelled() {
            return Err(Unavailable);
        }
        let Some(bytes) = read_image_file(directory, path) else {
            continue;
        };
        if let Ok(raw) = decoder.raw(&bytes)
            && let Some(image) = raw.render(&cancelled)
        {
            return with_metadata(image, path, size, &cancelled);
        }
    }
    Err(Unavailable)
}

fn with_metadata(
    mut image: DecodedImage,
    path: &Path,
    size: PreviewSize,
    cancelled: impl Fn() -> bool,
) -> Result<Rendered<'_>, DecodeError> {
    if cancelled() {
        return Err(DecodeError::Unavailable);
    }
    let mut summary = std::mem::take(&mut image.metadata.summary);
    add_dimensions(&mut summary, image.source_dimensions);
    add_format(&mut summary, path);
    Ok(Rendered {
        path,
        pixels: make_preview(image, size)?,
        summary,
    })
}

fn make_preview(image: DecodedImage, size: PreviewSize) -> Result<Pixels, DecodeError> {
    let preview = image
        .into_display_rgba8(Some(size.max_edge()))
        .map_err(|_| DecodeError::Unavailable)?;
    let (width, height) = preview.dimensions();
    Ok(SharedPixelBuffer::clone_from_slice(
        preview.as_raw(),
        width,
        height,
    ))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io::Cursor, path::PathBuf};

    use cap_std::{ambient_authority, fs::Dir};
    use cr_project::PhotographFormat;
    use exif::{Field, In, Tag, Value, experimental::Writer};
    use image::{
        ExtendedColorType, ImageEncoder, ImageFormat, Rgb, RgbImage,
        codecs::{jpeg::JpegEncoder, openexr::OpenExrEncoder, png::PngEncoder},
    };

    use super::{PreviewSize, load, make_preview, read_summary};
    use crate::image_decode::{Decoder, decode_raster};
    use crate::metadata::{Fields, read_group};

    const XMP: &str = r#"
        <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
          <rdf:Description rdf:about="" xmlns:basic="http://ns.adobe.com/xap/1.0/"
              xmlns:terms="http://purl.org/dc/elements/1.1/" basic:Rating="4">
            <terms:subject><rdf:Bag><rdf:li>Zürich &amp; Alps</rdf:li><rdf:li>Travel</rdf:li></rdf:Bag></terms:subject>
          </rdf:Description>
        </rdf:RDF>
    "#;

    #[test]
    fn group_details_preserve_each_files_metadata_and_list_the_shared_sidecar_once()
    -> Result<(), Box<dyn Error>> {
        let folder = tempfile::tempdir()?;
        let directory = Dir::open_ambient_dir(folder.path(), ambient_authority())?;
        directory.create_dir("trip")?;
        let paths = ["trip/photo.jpg", "trip/photo.png"].map(PathBuf::from);
        let image = RgbImage::new(4, 2);
        let mut jpeg = JpegEncoder::new(directory.create(&paths[0])?);
        jpeg.set_exif_metadata(model_exif("JPEG camera")?)?;
        jpeg.encode_image(&image)?;
        let mut png = PngEncoder::new(directory.create(&paths[1])?);
        png.set_exif_metadata(model_exif("PNG camera")?)?;
        png.write_image(image.as_raw(), 4, 2, ExtendedColorType::Rgb8)?;
        directory.write(
            "trip/photo.jpg.xmp",
            XMP.replace("Rating=\"4\"", "Rating=\"1\""),
        )?;
        directory.write("trip/photo.png.xmp", XMP)?;
        directory.write(
            "trip/photo.XMP",
            XMP.replace("Rating=\"4\"", "Rating=\"2\""),
        )?;
        directory.write("photo.xmp", XMP.replace("Rating=\"4\"", "Rating=\"5\""))?;

        let decoder = Decoder::new();
        let preview = load(&decoder, &directory, &paths, PreviewSize::Thumbnail, || {
            false
        });
        assert!(preview.pixels.is_ok());
        assert_eq!(preview.filename, "photo.jpg");
        assert!(has_field(&preview.summary, "Camera", "JPEG camera"));

        let metadata = read_group(&decoder, &directory, &paths, || false);
        let [jpeg, png] = metadata.files.as_slice() else {
            return Err("expected both group's photos".into());
        };
        assert_eq!(jpeg.filename, "photo.jpg");
        assert_eq!(png.filename, "photo.png");
        assert!(has_field(&jpeg.fields, "Model", r#""JPEG camera""#));
        assert!(has_field(&png.fields, "Model", r#""PNG camera""#));
        let [jpeg, png, stem] = metadata.sidecars.as_slice() else {
            return Err("expected two full-filename sidecars and one shared sidecar".into());
        };
        assert_eq!(jpeg.filename, "photo.jpg.xmp");
        assert_eq!(png.filename, "photo.png.xmp");
        assert!(stem.filename.eq_ignore_ascii_case("photo.xmp"));
        assert!(has_field(&jpeg.fields, "xmp:Rating", "1"));
        assert!(has_field(&png.fields, "xmp:Rating", "4"));
        assert!(has_field(&stem.fields, "xmp:Rating", "2"));
        assert!(has_field(&png.fields, "dc:subject[1]", "Zürich & Alps"));
        assert_eq!(directory.read_to_string("trip/photo.png.xmp")?, XMP);

        directory.write(&paths[0], "Unreadable image")?;
        let fallback = load(&decoder, &directory, &paths, PreviewSize::Photo, || false);
        assert!(fallback.pixels.is_ok());
        assert_eq!(fallback.filename, "photo.png");
        assert!(has_field(&fallback.summary, "Camera", "PNG camera"));
        Ok(())
    }

    #[test]
    fn sidecar_metadata_remains_available_when_preview_decoding_fails() -> Result<(), Box<dyn Error>>
    {
        let folder = tempfile::tempdir()?;
        let directory = Dir::open_ambient_dir(folder.path(), ambient_authority())?;
        directory.write("photo.jpg", "Unreadable image")?;
        directory.write("photo.jpg.xmp", "<rdf:RDF>")?;
        directory.write("photo.xmp", XMP)?;

        let decoder = Decoder::new();
        let paths = [PathBuf::from("photo.jpg")];
        let preview = load(&decoder, &directory, &paths, PreviewSize::Photo, || false);
        assert!(preview.pixels.is_err());
        let metadata = read_group(&decoder, &directory, &paths, || false);
        let [photo] = metadata.files.as_slice() else {
            return Err("expected unreadable photo".into());
        };
        assert!(has_field(&photo.fields, "Metadata", "Unavailable"));
        let [malformed, readable] = metadata.sidecars.as_slice() else {
            return Err("expected both sidecars".into());
        };
        assert!(has_field(&malformed.fields, "XMP", "No readable metadata"));
        assert!(has_field(&readable.fields, "xmp:Rating", "4"));
        Ok(())
    }

    #[test]
    fn jpeg_summary_is_available_before_pixel_decoding_can_succeed() -> Result<(), Box<dyn Error>> {
        let folder = tempfile::tempdir()?;
        let directory = Dir::open_ambient_dir(folder.path(), ambient_authority())?;
        let paths = [PathBuf::from("photo.jpg")];
        let mut bytes = Vec::new();
        let mut jpeg = JpegEncoder::new(&mut bytes);
        jpeg.set_exif_metadata(model_exif("JPEG camera")?)?;
        jpeg.encode_image(&RgbImage::new(40, 20))?;
        let end_of_header = bytes
            .windows(2)
            .position(|marker| marker == [0xff, 0xda])
            .ok_or("missing start of scan")?
            + 2;
        directory.write(&paths[0], &bytes[..end_of_header])?;

        let decoder = Decoder::new();
        let summary = read_summary(&decoder, &directory, &paths, || false).ok_or("no summary")?;
        assert_eq!(summary.filename, "photo.jpg");
        assert!(has_field(&summary.fields, "Camera", "JPEG camera"));
        assert!(has_field(&summary.fields, "Dimensions", "40 × 20"));
        assert!(
            load(&decoder, &directory, &paths, PreviewSize::Thumbnail, || {
                false
            })
            .pixels
            .is_err()
        );
        Ok(())
    }

    fn model_exif(model: &str) -> Result<Vec<u8>, exif::Error> {
        let field = Field {
            tag: Tag::Model,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![model.as_bytes().to_vec()]),
        };
        let mut writer = Writer::new();
        writer.push_field(&field);
        let mut bytes = Cursor::new(Vec::new());
        writer.write(&mut bytes, false)?;
        Ok(bytes.into_inner())
    }

    fn has_field(fields: &Fields, label: &str, value: &str) -> bool {
        fields
            .iter()
            .any(|(key, text)| key == label && text == value)
    }

    #[test]
    fn jpeg_thumbnail_preserves_aspect_ratio_and_applies_exif_orientation()
    -> Result<(), Box<dyn Error>> {
        let folder = tempfile::tempdir()?;
        let directory = Dir::open_ambient_dir(folder.path(), ambient_authority())?;
        let path = PathBuf::from("photo.jpg");
        let image = RgbImage::from_fn(800, 400, |x, _| {
            if x < 400 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        });
        let mut encoder = JpegEncoder::new(directory.create(&path)?);
        // EXIF orientation 6 rotates the image 90 degrees clockwise.
        encoder.set_exif_metadata(vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 1, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
        ])?;
        encoder.encode_image(&image)?;

        let decoder = Decoder::new();
        let decoded = decoder
            .raster(
                &directory,
                &path,
                PhotographFormat::Jpeg,
                PreviewSize::Thumbnail,
            )
            .map_err(|_| "thumbnail decode failed")?;
        assert_eq!(decoded.source_dimensions, (800, 400));
        assert_eq!(decoded.into_display_rgba8(None)?.dimensions(), (200, 400));

        let paths = [path];
        let full_preview = load(&decoder, &directory, &paths, PreviewSize::Photo, || false)
            .pixels
            .map_err(|_| "photo preview failed")?;
        assert_eq!((full_preview.width(), full_preview.height()), (400, 800));
        let thumbnail = load(&decoder, &directory, &paths, PreviewSize::Thumbnail, || {
            false
        });
        assert!(has_field(&thumbnail.summary, "Dimensions", "800 × 400"));
        let preview = thumbnail.pixels.map_err(|_| "thumbnail failed")?;
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

        let decoded = decode_raster(bytes, ImageFormat::OpenExr)?;
        let preview =
            make_preview(decoded, PreviewSize::Thumbnail).map_err(|_| "preview failed")?;
        let pixel = preview.as_slice().first().ok_or("empty preview")?;
        assert!((117..=119).contains(&pixel.r));
        assert_eq!((pixel.g, pixel.b, pixel.a), (255, 0, 128));
        Ok(())
    }
}
