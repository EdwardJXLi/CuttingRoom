use std::{
    fmt::{self, Write},
    io::Cursor,
    path::Path,
};

use exif::{Exif, In, Tag, Value};

mod group;
mod xmp;

pub(crate) use group::{GroupMetadata, read_group};

const MAX_VALUE_BYTES: usize = 256;
const MAX_EXIF_ROWS: usize = 1024;
pub(crate) type Fields = Vec<(String, String)>;

pub(crate) struct MetadataSource {
    pub(crate) filename: String,
    pub(crate) fields: Fields,
}

#[derive(Default)]
pub(crate) struct Metadata {
    pub(crate) summary: Fields,
    pub(crate) details: Fields,
}

pub(crate) fn read_exif(bytes: Vec<u8>) -> Metadata {
    exif::Reader::new()
        .read_raw(bytes)
        .ok()
        .map_or_else(Metadata::default, fields)
}

pub(crate) fn read_container(bytes: &[u8]) -> Metadata {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .map_or_else(Metadata::default, fields)
}

fn fields(exif: Exif) -> Metadata {
    let ascii = |tag| match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Ascii(values) => values.first().map(Vec::as_slice),
        _ => None,
    };
    let text = |tag| ascii(tag).map(clean_text).unwrap_or_default();
    let mut summary = Vec::new();
    add(
        &mut summary,
        "Camera",
        camera_name(&text(Tag::Make), &text(Tag::Model)),
    );
    add(
        &mut summary,
        "Lens",
        camera_name(&text(Tag::LensMake), &text(Tag::LensModel)),
    );
    if let Some((label, date)) = [
        (Tag::DateTimeOriginal, "Captured"),
        (Tag::DateTimeDigitized, "Digitized"),
    ]
    .into_iter()
    .find_map(|(tag, label)| {
        let date = exif::DateTime::from_ascii(ascii(tag)?).ok()?;
        let datetime =
            chrono::NaiveDate::from_ymd_opt(date.year.into(), date.month.into(), date.day.into())?
                .and_hms_opt(date.hour.into(), date.minute.into(), date.second.into())?;
        Some((label, datetime))
    }) {
        add(
            &mut summary,
            label,
            date.format("%Y-%m-%d %H:%M").to_string(),
        );
    }
    let exposure = [Tag::ExposureTime, Tag::FNumber]
        .into_iter()
        .filter_map(|tag| exif.get_field(tag, In::PRIMARY))
        .filter(|field| {
            matches!(&field.value, Value::Rational(values)
                if matches!(values.as_slice(), [value] if value.num > 0 && value.denom > 0))
        })
        .map(|field| display(field, &exif))
        .collect::<Vec<_>>()
        .join(", ");
    add(&mut summary, "Exposure", exposure);
    if let Some(iso) = [Tag::ISOSpeed, Tag::PhotographicSensitivity]
        .into_iter()
        .filter_map(|tag| exif.get_field(tag, In::PRIMARY))
        .find_map(|field| {
            field
                .value
                .get_uint(0)
                .filter(|iso| *iso > 0 && *iso != 65535)
        })
    {
        add(&mut summary, "ISO", iso.to_string());
    }

    let mut details = Vec::new();
    let fields = exif.fields();
    let field_count = fields.len();
    for field in fields.take(MAX_EXIF_ROWS) {
        let value = display(field, &exif);
        if value.is_empty() {
            continue;
        }
        let label = if field.ifd_num == In::PRIMARY {
            field.tag.to_string()
        } else {
            format!("{} ({})", field.tag, field.ifd_num)
        };
        details.push((label, value));
    }
    if field_count > MAX_EXIF_ROWS {
        add(
            &mut details,
            "EXIF",
            format!("{} fields omitted", field_count - MAX_EXIF_ROWS),
        );
    }
    Metadata { summary, details }
}

pub(crate) fn filename(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn add_dimensions(rows: &mut Fields, (width, height): (u32, u32)) {
    add(rows, "Dimensions", format!("{width} × {height}"));
}

pub(crate) fn add_format(rows: &mut Fields, path: &Path) {
    let format = path.extension().unwrap_or_default().to_string_lossy();
    add(rows, "Format", format.to_ascii_uppercase());
}

pub(crate) fn add(rows: &mut Fields, label: &str, value: String) {
    if !value.is_empty() {
        rows.push((label.to_owned(), value));
    }
}

pub(crate) fn camera_name(make: &str, model: &str) -> String {
    if model.is_empty() {
        make.to_owned()
    } else if make.is_empty()
        || model
            .to_ascii_lowercase()
            .starts_with(&make.to_ascii_lowercase())
    {
        model.to_owned()
    } else {
        format!("{make} {model}")
    }
}

pub(crate) fn clean_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_VALUE_BYTES)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn display(field: &exif::Field, exif: &Exif) -> String {
    if let Value::Undefined(bytes, _) | Value::Byte(bytes) = &field.value
        && bytes.len() > MAX_VALUE_BYTES
    {
        return format!("{} bytes", bytes.len());
    }
    struct BoundedText(String);
    impl Write for BoundedText {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            let remaining = MAX_VALUE_BYTES.saturating_sub(self.0.len());
            if text.len() <= remaining {
                self.0.push_str(text);
                return Ok(());
            }
            self.0
                .push_str(&text[..text.floor_char_boundary(remaining)]);
            Err(fmt::Error)
        }
    }
    let mut output = BoundedText(String::new());
    if write!(output, "{}", field.display_value().with_unit(exif)).is_err() {
        output.0.push('…');
    }
    clean_text(output.0.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use exif::{Field, In, Tag, Value, experimental::Writer};

    use super::read_exif;

    #[test]
    fn exif_summary_keeps_capture_details_and_full_metadata_keeps_thumbnail_tags()
    -> Result<(), exif::Error> {
        let ascii = |tag, ifd_num, text: &str| Field {
            tag,
            ifd_num,
            value: Value::Ascii(vec![text.as_bytes().to_vec()]),
        };
        let fields = [
            ascii(Tag::Make, In::PRIMARY, "Sony"),
            ascii(Tag::Model, In::PRIMARY, "Sony a7 IV"),
            ascii(Tag::LensModel, In::PRIMARY, "FE 24–70 mm F2.8"),
            ascii(Tag::DateTimeOriginal, In::PRIMARY, "2024:10:09 07:21:34"),
            ascii(Tag::Software, In::PRIMARY, "Photo editor"),
            Field {
                tag: Tag::ExposureTime,
                ifd_num: In::PRIMARY,
                value: Value::Rational(vec![(1, 250).into()]),
            },
            Field {
                tag: Tag::FNumber,
                ifd_num: In::PRIMARY,
                value: Value::Rational(vec![(8, 1).into()]),
            },
            Field {
                tag: Tag::PhotographicSensitivity,
                ifd_num: In::PRIMARY,
                value: Value::Short(vec![100]),
            },
            Field {
                tag: Tag::Orientation,
                ifd_num: In::THUMBNAIL,
                value: Value::Short(vec![1]),
            },
        ];
        let mut writer = Writer::new();
        for field in &fields {
            writer.push_field(field);
        }
        let mut bytes = Cursor::new(Vec::new());
        writer.write(&mut bytes, false)?;

        let metadata = read_exif(bytes.into_inner());
        let summary = metadata
            .summary
            .iter()
            .map(|(label, value)| (label.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("Camera", "Sony a7 IV"),
                ("Lens", "FE 24–70 mm F2.8"),
                ("Captured", "2024-10-09 07:21"),
                ("Exposure", "1/250 s, f/8"),
                ("ISO", "100"),
            ]
        );
        assert_eq!(metadata.details.len(), fields.len());
        assert!(metadata.details.iter().any(|(label, value)| {
            label == "DateTimeOriginal" && value == "2024-10-09 07:21:34"
        }));
        assert!(
            metadata
                .details
                .iter()
                .any(|(label, _)| label == "Orientation (thumbnail)")
        );
        Ok(())
    }
}
