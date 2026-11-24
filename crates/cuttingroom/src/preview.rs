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
    source_dimensions: (u32, u32),
}

pub(crate) struct PhotoPreview {
    pub(crate) preview: Preview,
    pub(crate) source_dimensions: Option<(u32, u32)>,
}

pub(crate) fn load_thumbnail(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[PathBuf],
    cancelled: impl Fn() -> bool,
) -> Preview {
    let sources = ordered_sources(paths);
    let preferred = sources.first().map(|(_, path)| *path);
    let (rasters, raws) =
        sources.split_at(sources.partition_point(|(format, _)| *format != PhotographFormat::Raw));
    finish_preview(
        render_thumbnail(decoder, directory, rasters, raws, cancelled),
        preferred,
    )
}

pub(crate) fn preferred_photo_file(paths: &[PathBuf]) -> Option<usize> {
    paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| Some((index, photograph_format(path)?)))
        .min_by_key(|(_, format)| {
            (
                *format != PhotographFormat::Raw,
                thumbnail_priority(*format),
            )
        })
        .map(|(index, _)| index)
}

pub(crate) fn load_photo(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[PathBuf],
    selected_file: usize,
    cancelled: impl Fn() -> bool,
) -> PhotoPreview {
    let path = paths.get(selected_file).map(PathBuf::as_path);
    let rendered = path
        .ok_or(DecodeError::Unavailable)
        .and_then(|path| render_photo(decoder, directory, path, cancelled));
    PhotoPreview {
        source_dimensions: rendered.as_ref().ok().map(|image| image.source_dimensions),
        preview: finish_preview(rendered, path),
    }
}

fn finish_preview(rendered: Result<Rendered<'_>, DecodeError>, source: Option<&Path>) -> Preview {
    match rendered {
        Ok(rendered) => Preview {
            filename: filename(rendered.path),
            pixels: Ok(rendered.pixels),
            summary: rendered.summary,
        },
        Err(error) => Preview {
            filename: source.map(filename).unwrap_or_default(),
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
    let mut sources: Vec<_> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter_map(|path| Some((photograph_format(path)?, path)))
        .collect();
    sources.sort_by_key(|(format, _)| thumbnail_priority(*format));
    sources
}

fn thumbnail_priority(format: PhotographFormat) -> u8 {
    use PhotographFormat::{Jpeg, Png, Raw, Tiff};

    match format {
        Jpeg => 0,
        Png => 1,
        Tiff => 2,
        Raw => 4,
        _ => 3,
    }
}

fn render_thumbnail<'a>(
    decoder: &Decoder,
    directory: &Dir,
    rasters: &[(PhotographFormat, &'a Path)],
    raws: &[(PhotographFormat, &'a Path)],
    cancelled: impl Fn() -> bool,
) -> Result<Rendered<'a>, DecodeError> {
    use DecodeError::{DecoderUnavailable, Unavailable};

    let mut error = Unavailable;
    let size = PreviewSize::Thumbnail;
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

    match load_raw_thumbnail(decoder, directory, raws, &cancelled) {
        Err(Unavailable) => Err(error),
        result => result,
    }
}

fn render_photo<'a>(
    decoder: &Decoder,
    directory: &Dir,
    path: &'a Path,
    cancelled: impl Fn() -> bool,
) -> Result<Rendered<'a>, DecodeError> {
    use DecodeError::Unavailable;
    let size = PreviewSize::Photo;
    if cancelled() {
        return Err(Unavailable);
    }
    let image = match photograph_format(path).ok_or(Unavailable)? {
        PhotographFormat::Raw => {
            let bytes = read_image_file(directory, path).ok_or(Unavailable)?;
            decoder.raw(&bytes)?.render(&cancelled).ok_or(Unavailable)?
        }
        format => decoder.raster(directory, path, format, size)?,
    };
    with_metadata(image, path, size, &cancelled)
}

fn load_raw_thumbnail<'a>(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[(PhotographFormat, &'a Path)],
    cancelled: impl Fn() -> bool,
) -> Result<Rendered<'a>, DecodeError> {
    use DecodeError::{DecoderUnavailable, Unavailable};

    let size = PreviewSize::Thumbnail;
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
    let source_dimensions = image.source_dimensions;
    add_dimensions(&mut summary, source_dimensions);
    add_format(&mut summary, path);
    Ok(Rendered {
        path,
        pixels: make_preview(image, size)?,
        summary,
        source_dimensions,
    })
}

fn make_preview(image: DecodedImage, size: PreviewSize) -> Result<Pixels, DecodeError> {
    let preview = image
        .into_display_rgba8(Some(size))
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

    use super::{
        PreviewSize, load_photo, load_thumbnail, make_preview, preferred_photo_file, read_summary,
    };
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
        let preview = load_thumbnail(&decoder, &directory, &paths, || false);
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
        let fallback = load_thumbnail(&decoder, &directory, &paths, || false);
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
        let preview = load_photo(&decoder, &directory, &paths, 0, || false).preview;
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
            load_thumbnail(&decoder, &directory, &paths, || false)
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
        let full_preview = load_photo(&decoder, &directory, &paths, 0, || false)
            .preview
            .pixels
            .map_err(|_| "photo preview failed")?;
        assert_eq!((full_preview.width(), full_preview.height()), (400, 800));
        let thumbnail = load_thumbnail(&decoder, &directory, &paths, || false);
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
    fn photo_preview_reports_original_and_rendered_dimensions() -> Result<(), Box<dyn Error>> {
        let folder = tempfile::tempdir()?;
        let directory = Dir::open_ambient_dir(folder.path(), ambient_authority())?;
        let paths = [PathBuf::from("photo.jpg")];
        JpegEncoder::new(directory.create(&paths[0])?).encode_image(&RgbImage::new(9000, 450))?;

        let decoder = Decoder::new();
        let photo = load_photo(&decoder, &directory, &paths, 0, || false);
        assert_eq!(photo.source_dimensions, Some((9000, 450)));
        let photo = photo.preview;
        assert_eq!(photo.filename, "photo.jpg");
        assert!(has_field(&photo.summary, "Dimensions", "9000 × 450"));
        let pixels = photo.pixels.map_err(|_| "JPEG preview failed")?;
        assert_eq!((pixels.width(), pixels.height()), (8192, 409));
        Ok(())
    }

    #[test]
    fn failed_default_preview_stays_on_the_raw_file_until_another_is_selected()
    -> Result<(), Box<dyn Error>> {
        let folder = tempfile::tempdir()?;
        let directory = Dir::open_ambient_dir(folder.path(), ambient_authority())?;
        let paths = [
            PathBuf::from("photo.jpg"),
            PathBuf::from("photo.png"),
            PathBuf::from("photo.dng"),
        ];
        JpegEncoder::new(directory.create(&paths[0])?).encode_image(&RgbImage::new(4, 2))?;
        let pixels = RgbImage::new(12, 8);
        PngEncoder::new(directory.create(&paths[1])?).write_image(
            pixels.as_raw(),
            12,
            8,
            ExtendedColorType::Rgb8,
        )?;
        directory.write(&paths[2], b"unreadable RAW")?;

        let decoder = Decoder::new();
        assert_eq!(preferred_photo_file(&paths[..2]), Some(0));
        let default = preferred_photo_file(&paths).ok_or("missing default preview source")?;
        let failed = load_photo(&decoder, &directory, &paths, default, || false);
        assert_eq!(failed.preview.filename, "photo.dng");
        assert!(failed.preview.pixels.is_err());

        let selected = load_photo(&decoder, &directory, &paths, 1, || false);
        assert_eq!(selected.preview.filename, "photo.png");
        assert!(has_field(&selected.preview.summary, "Dimensions", "12 × 8"));
        let pixels = selected.preview.pixels.map_err(|_| "selected PNG failed")?;
        assert_eq!((pixels.width(), pixels.height()), (12, 8));
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
