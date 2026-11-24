use std::path::Path;

use cap_std::fs::Dir;
use cr_project::photograph_format;

use super::{Fields, MetadataSource, add, add_format, filename, xmp::read_sidecars};
use crate::image_decode::{DecodeError, Decoder};

pub(crate) struct FileMetadata {
    pub(crate) source: MetadataSource,
    pub(crate) sidecars: Vec<MetadataSource>,
}

pub(crate) fn read_file(
    decoder: &Decoder,
    directory: &Dir,
    path: &Path,
    cancelled: impl Fn() -> bool,
) -> Option<FileMetadata> {
    if cancelled() {
        return None;
    }
    let format = photograph_format(path)?;
    let mut fields = Fields::new();
    add_format(&mut fields, path);
    match decoder.inspect(directory, path, format) {
        Ok(mut embedded) => fields.append(&mut embedded.details),
        Err(error) => {
            let message = match error {
                DecodeError::Unavailable => "Unavailable",
                DecodeError::DecoderUnavailable => "Decoder unavailable",
            };
            add(&mut fields, "Metadata", message.to_owned());
        }
    }
    Some(FileMetadata {
        source: MetadataSource {
            filename: filename(path),
            fields,
        },
        sidecars: read_sidecars(directory, path, cancelled),
    })
}
