use std::path::PathBuf;

use cap_std::fs::Dir;
use cr_project::photograph_format;

use super::{Fields, MetadataSource, add, add_format, filename, xmp::read_sidecars};
use crate::image_decode::{DecodeError, Decoder};

pub(crate) struct GroupMetadata {
    pub(crate) files: Vec<MetadataSource>,
    pub(crate) sidecars: Vec<MetadataSource>,
}

pub(crate) fn read_group(
    decoder: &Decoder,
    directory: &Dir,
    paths: &[PathBuf],
    cancelled: impl Fn() -> bool,
) -> GroupMetadata {
    let files = paths
        .iter()
        .take_while(|_| !cancelled())
        .filter_map(|path| {
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
            Some(MetadataSource {
                filename: filename(path),
                fields,
            })
        })
        .collect();
    let sidecars = read_sidecars(directory, paths, cancelled);
    GroupMetadata { files, sidecars }
}
