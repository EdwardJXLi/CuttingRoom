//! Read-only photograph discovery for folder projects.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

use walkdir::{DirEntry, IntoIter, WalkDir};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotographFormat {
    Jpeg,
    JpegXl,
    Png,
    Tiff,
    Heif,
    OpenExr,
    Raw,
}

/// Identify the decoder family from a photograph's filename extension.
pub fn photograph_format(path: &Path) -> Option<PhotographFormat> {
    use PhotographFormat::{Heif, Jpeg, JpegXl, OpenExr, Png, Raw, Tiff};

    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => Some(Jpeg),
        "jxl" => Some(JpegXl),
        "png" => Some(Png),
        "tif" | "tiff" => Some(Tiff),
        "heic" | "heif" => Some(Heif),
        "exr" => Some(OpenExr),
        "3fr" | "arw" | "cr2" | "cr3" | "crw" | "dcr" | "dng" | "erf" | "fff" | "iiq" | "kdc"
        | "mef" | "mos" | "mrw" | "nef" | "nrw" | "orf" | "pef" | "raf" | "rw2" | "rwl" | "sr2"
        | "srf" | "srw" | "x3f" => Some(Raw),
        _ => None,
    }
}

/// Scan a project root without following directory links or entering nested projects.
///
/// The iterator yields errors for unreadable entries. Callers must distinguish a
/// cancelled or incomplete scan from a successful scan before marking files missing.
pub(crate) fn photographs(
    root: &Path,
    cancelled: impl Fn() -> bool,
) -> impl Iterator<Item = io::Result<PathBuf>> {
    let mut entries = WalkDir::new(root)
        .min_depth(1)
        .follow_root_links(false)
        .into_iter();
    std::iter::from_fn(move || {
        while !cancelled() {
            let entry = match entries.next()? {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error.into())),
            };
            if let Some(photo) = inspect_entry(root, &entry, &mut entries).transpose() {
                return Some(photo);
            }
        }
        None
    })
}

fn inspect_entry(
    root: &Path,
    entry: &DirEntry,
    entries: &mut IntoIter,
) -> io::Result<Option<PathBuf>> {
    let file_type = entry.file_type();
    if file_type.is_dir() {
        if excluded_directory(entry.file_name()) || is_nested_project(entry.path())? {
            entries.skip_current_dir();
        }
        return Ok(None);
    }
    if !file_type.is_file() || photograph_format(entry.path()).is_none() {
        return Ok(None);
    }
    let metadata = entry.metadata()?;
    if !metadata.is_file() || crate::storage::is_link(&metadata) {
        return Ok(None);
    }
    let relative_path = entry.path().strip_prefix(root).map_err(io::Error::other)?;
    Ok(Some(relative_path.to_path_buf()))
}

fn excluded_directory(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        let name = name.to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".cuttingroom" | ".trash" | ".trashes" | "$recycle.bin" | ".temporaryitems"
        ) || name
            .strip_prefix(".trash-")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn is_nested_project(directory: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    if crate::storage::is_link(&directory.symlink_metadata()?) {
        return Ok(true);
    }
    match fs::symlink_metadata(directory.join(".cuttingroom")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
