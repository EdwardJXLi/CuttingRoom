use std::path::{Path, PathBuf};

use cap_std::fs::Dir;
use xmp_toolkit::{IterOptions, XmpMeta};

use super::{Fields, MetadataSource, add, clean_text, filename};
use crate::source_file;

const MAX_PACKET_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ROWS: usize = 128;

pub(super) fn read_sidecars(
    directory: &Dir,
    photo: &Path,
    cancelled: impl Fn() -> bool,
) -> Vec<MetadataSource> {
    let mut full_name = photo.as_os_str().to_os_string();
    full_name.push(".xmp");
    [PathBuf::from(full_name), photo.with_extension("xmp")]
        .into_iter()
        .take_while(|_| !cancelled())
        .filter_map(|path| read_sidecar(directory, path))
        .collect()
}

fn read_sidecar(directory: &Dir, mut path: PathBuf) -> Option<MetadataSource> {
    let file = source_file::open(directory, &path, MAX_PACKET_BYTES).or_else(|| {
        path.set_extension("XMP");
        source_file::open(directory, &path, MAX_PACKET_BYTES)
    })?;
    let filename = filename(&path);
    let mut fields = source_file::read(file, MAX_PACKET_BYTES)
        .and_then(|bytes| std::str::from_utf8(&bytes).ok()?.parse::<XmpMeta>().ok())
        .map(|packet| fields(&packet))
        .unwrap_or_default();
    if fields.is_empty() {
        add(&mut fields, "XMP", "No readable metadata".to_owned());
    }
    Some(MetadataSource { filename, fields })
}

fn fields(packet: &XmpMeta) -> Fields {
    let mut properties = packet.iter(IterOptions::default().leaf_nodes_only());
    let mut fields = Fields::new();
    for property in properties.by_ref().take(MAX_ROWS) {
        let label = clean_text(property.name.as_bytes());
        if !label.is_empty() {
            add(
                &mut fields,
                &label,
                clean_text(property.value.value.as_bytes()),
            );
        }
    }
    if properties.next().is_some() {
        add(&mut fields, "XMP", "More fields omitted".to_owned());
    }
    fields
}
