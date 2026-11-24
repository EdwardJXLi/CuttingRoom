use std::{
    hash::Hasher,
    io::{self, Write},
    path::{Component, Path},
    time::UNIX_EPOCH,
};

use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
#[cfg(windows)]
use cap_std::fs::MetadataExt as WindowsMetadataExt;
use cap_std::fs::{Dir, Metadata, OpenOptions};
use cr_project::PhotographGroup;
use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};
use siphasher::sip128::{Hasher128, SipHasher24};
use slint::SharedPixelBuffer;
use uuid::Uuid;

use crate::{
    image_decode::{DecodeError, PreviewSize, decode_cached_thumbnail},
    metadata::Fields,
    preview::Preview,
    source_file,
};

const CACHE_DIRECTORY: &str = "thumbnails";
const JPEG_JFIF: &[u8] = &[0xff, 0xd8, 0xff, 0xe0];
const JPEG_APP15: &[u8] = &[0xff, 0xef];
const CACHE_FORMAT: &[u8] = b"CuttingRoom Thumbnail\0\x01";
const JPEG_QUALITY: u8 = 82;
const MAX_CACHE_BYTES: u64 = 512 * 1024;
const MAX_METADATA_BYTES: usize = 8 * 1024;
const CANVAS_CHANNEL: u32 = 0x1a;

pub(crate) struct ThumbnailCache {
    directory: Dir,
}

#[derive(Clone)]
pub(crate) struct LoadedThumbnail {
    pub(crate) preview: Preview,
    pub(crate) status: CacheStatus,
}

#[derive(Clone)]
pub(crate) enum CacheStatus {
    Hit,
    Write(ThumbnailEntry),
    Unavailable,
}

#[derive(Clone)]
pub(crate) struct ThumbnailEntry {
    key: String,
    fingerprint: [u8; 16],
}

impl ThumbnailCache {
    pub(crate) fn open(project: &Dir) -> io::Result<Self> {
        let storage = project.open_dir_nofollow(".cuttingroom")?;
        Ok(Self {
            directory: open_or_create(&storage, CACHE_DIRECTORY)?,
        })
    }

    pub(crate) fn clear(&self, project: &Dir, cancelled: impl Fn() -> bool) -> io::Result<()> {
        if is_link(&project.symlink_metadata(".cuttingroom")?) {
            return Err(io::Error::other("unsafe thumbnail storage"));
        }
        let directory = self.current_directory(project)?;
        let metadata = directory.dir_metadata()?;
        if is_link(&metadata) || metadata.dev() != project.dir_metadata()?.dev() {
            return Err(io::Error::other("unsafe thumbnail cache directory"));
        }
        clear_directory(directory, metadata.dev(), cancelled)
    }

    pub(crate) fn load_or_generate(
        &self,
        project: &Dir,
        group: &PhotographGroup,
        generate: impl FnOnce() -> Preview,
        cancelled: impl Fn() -> bool,
    ) -> LoadedThumbnail {
        if cancelled() {
            return LoadedThumbnail {
                preview: Preview {
                    pixels: Err(DecodeError::Unavailable),
                    filename: String::new(),
                    summary: Fields::new(),
                },
                status: CacheStatus::Unavailable,
            };
        }

        let key = group_key(group);
        let fingerprint = source_fingerprint(project, group);
        if let Some(fingerprint) = fingerprint
            && let Some(preview) = self.lookup(project, &key, fingerprint)
        {
            return LoadedThumbnail {
                preview,
                status: CacheStatus::Hit,
            };
        }

        let preview = generate();
        let status = match fingerprint {
            Some(fingerprint) if preview.pixels.is_ok() && !cancelled() => {
                CacheStatus::Write(ThumbnailEntry { key, fingerprint })
            }
            _ => CacheStatus::Unavailable,
        };
        LoadedThumbnail { preview, status }
    }

    pub(crate) fn save(
        &self,
        project: &Dir,
        group: &PhotographGroup,
        entry: ThumbnailEntry,
        preview: &Preview,
        cancelled: impl Fn() -> bool,
    ) -> bool {
        if cancelled() {
            return false;
        }
        let Ok(bytes) = encode_entry(entry.fingerprint, preview) else {
            return false;
        };
        !cancelled()
            && source_fingerprint(project, group) == Some(entry.fingerprint)
            && self.store(project, &entry.key, &bytes).is_ok()
    }

    fn current_directory(&self, project: &Dir) -> io::Result<Dir> {
        let current = project
            .open_dir_nofollow(".cuttingroom")?
            .open_dir_nofollow(CACHE_DIRECTORY)?;
        if !same_file(&self.directory.dir_metadata()?, &current.dir_metadata()?) {
            return Err(io::Error::other("thumbnail cache directory changed"));
        }
        Ok(current)
    }

    fn lookup(&self, project: &Dir, key: &str, fingerprint: [u8; 16]) -> Option<Preview> {
        let (shard, filename) = entry_path(key);
        let bucket = self
            .current_directory(project)
            .ok()?
            .open_dir_nofollow(shard)
            .ok()?;
        let file = source_file::open(&bucket, Path::new(&filename), MAX_CACHE_BYTES)?;
        if file.metadata().ok()?.nlink() != 1 {
            return None;
        }
        let bytes = source_file::read(file, MAX_CACHE_BYTES)?;
        read_entry(&bytes, fingerprint)
    }

    fn store(&self, project: &Dir, key: &str, bytes: &[u8]) -> io::Result<()> {
        let (shard, filename) = entry_path(key);
        let bucket = open_or_create(&self.current_directory(project)?, shard)?;
        validate_destination(&bucket, &filename)?;

        let temp = format!("{}.tmp", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = bucket.open_with(&temp, &options)?;
        let identity = file.metadata()?;
        let result = (|| {
            file.write_all(bytes)?;
            let current = self.current_directory(project)?.open_dir_nofollow(shard)?;
            let staged = source_file::open(&current, Path::new(&temp), MAX_CACHE_BYTES)
                .ok_or_else(|| io::Error::other("staged thumbnail disappeared"))?;
            let staged_metadata = staged.metadata()?;
            if !same_file(&bucket.dir_metadata()?, &current.dir_metadata()?)
                || !same_file(&identity, &staged_metadata)
                || staged_metadata.nlink() != 1
            {
                return Err(io::Error::other("thumbnail storage changed"));
            }
            validate_destination(&current, &filename)?;
            drop(staged);
            drop(file);
            current.rename(&temp, &current, &filename)
        })();
        if result.is_err()
            && let Some(file) = source_file::open(&bucket, Path::new(&temp), MAX_CACHE_BYTES)
            && let Ok(metadata) = file.metadata()
            && same_file(&identity, &metadata)
        {
            drop(file);
            let _ = bucket.remove_file(temp);
        }
        result
    }
}

fn clear_directory(
    mut directory: Dir,
    device: u64,
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    let mut parents = Vec::new();
    let mut entries = directory.entries()?;
    loop {
        if cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if let Some(entry) = entries.next() {
            let name = entry?.file_name();
            let metadata = directory.symlink_metadata(&name)?;
            if metadata.is_dir() && !is_link(&metadata) {
                let child = directory.open_dir_nofollow(&name)?;
                let child_metadata = child.dir_metadata()?;
                if is_link(&child_metadata) || child_metadata.dev() != device {
                    return Err(io::Error::other(
                        "unsafe directory inside the thumbnail cache",
                    ));
                }
                let child_entries = child.entries()?;
                parents.push((directory, entries, name));
                directory = child;
                entries = child_entries;
            } else {
                remove_leaf(&directory, Path::new(&name), &metadata)?;
            }
        } else if let Some((parent, parent_entries, name)) = parents.pop() {
            let current = parent.open_dir_nofollow(&name)?;
            if !same_file(&current.dir_metadata()?, &directory.dir_metadata()?) {
                return Err(io::Error::other("thumbnail cache entry changed"));
            }
            drop(current);
            drop(entries);
            drop(directory);
            parent.remove_dir(&name)?;
            directory = parent;
            entries = parent_entries;
        } else {
            return Ok(());
        }
    }
}

#[cfg(windows)]
fn is_link(metadata: &Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn is_link(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn remove_leaf(directory: &Dir, name: &Path, metadata: &Metadata) -> io::Result<()> {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        // On Windows, RemoveDirectory unlinks a directory junction.
        directory.remove_dir(name)
    } else {
        directory.remove_file(name)
    }
}

#[cfg(unix)]
fn remove_leaf(directory: &Dir, name: &Path, _metadata: &Metadata) -> io::Result<()> {
    directory.remove_file(name)
}

fn validate_destination(directory: &Dir, name: &str) -> io::Result<()> {
    match directory.symlink_metadata(name) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let file = source_file::open(directory, Path::new(name), u64::MAX - 1)
                .ok_or_else(|| io::Error::other("unsafe thumbnail entry"))?;
            if file.metadata()?.nlink() != 1 {
                return Err(io::Error::other("linked thumbnail entry"));
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::other("unsafe thumbnail entry")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_or_create(parent: &Dir, name: &str) -> io::Result<Dir> {
    match parent.open_dir_nofollow(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match parent.create_dir(name) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error),
            }
            parent.open_dir_nofollow(name)
        }
        result => result,
    }
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn entry_path(key: &str) -> (&str, String) {
    let (shard, name) = key.split_at(2);
    (shard, format!("{name}.jpg"))
}

fn group_key(group: &PhotographGroup) -> String {
    let mut hash = SipHasher24::new();
    hash_path(&mut hash, &group.stem);
    format!("{:032x}", hash.finish128().as_u128())
}

fn source_fingerprint(project: &Dir, group: &PhotographGroup) -> Option<[u8; 16]> {
    let mut hash = SipHasher24::new();
    hash.write(CACHE_FORMAT);
    hash.write(&PreviewSize::Thumbnail.max_edge().to_le_bytes());
    hash_path(&mut hash, &group.stem);
    for path in &group.files {
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        hash_path(&mut hash, path);
        let metadata = project.symlink_metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata.modified().ok()?.into_std();
        let timestamp = match modified.duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos() as i128,
            Err(error) => -(error.duration().as_nanos() as i128),
        };
        hash.write(&metadata.len().to_le_bytes());
        hash.write(&timestamp.to_le_bytes());
    }
    Some(hash.finish128().as_bytes())
}

fn hash_path(hash: &mut SipHasher24, path: &Path) {
    // A capture stem is an opaque key. Stripping the extension can leave a dot component.
    let bytes = path.as_os_str().as_encoded_bytes();
    for bytes in bytes.split(|&byte| byte == b'/' || (cfg!(windows) && byte == b'\\')) {
        hash.write(&(bytes.len() as u64).to_le_bytes());
        hash.write(bytes);
    }
    hash.write(&u64::MAX.to_le_bytes());
}

fn checksum(bytes: &[u8]) -> [u8; 16] {
    let mut hash = SipHasher24::new();
    hash.write(bytes);
    hash.finish128().as_bytes()
}

fn encode_entry(fingerprint: [u8; 16], preview: &Preview) -> io::Result<Vec<u8>> {
    let pixels = preview
        .pixels
        .as_ref()
        .map_err(|_| io::Error::other("no thumbnail pixels"))?;
    if !valid_dimensions(pixels.width(), pixels.height()) {
        return Err(io::Error::other("invalid thumbnail dimensions"));
    }

    let mut rgb = Vec::with_capacity(pixels.as_slice().len() * 3);
    for pixel in pixels.as_slice() {
        let alpha = u32::from(pixel.a);
        for channel in [pixel.r, pixel.g, pixel.b] {
            rgb.push(
                ((u32::from(channel) * alpha + CANVAS_CHANNEL * (255 - alpha) + 127) / 255) as u8,
            );
        }
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode(
            &rgb,
            pixels.width(),
            pixels.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(io::Error::other)?;
    let (header, body) =
        split_jfif(&jpeg).ok_or_else(|| io::Error::other("invalid JPEG header"))?;
    let details = serde_json::to_vec(&(&preview.filename, &preview.summary))?;
    if details.len() > MAX_METADATA_BYTES {
        return Err(io::Error::other(
            "thumbnail metadata exceeds the cache limit",
        ));
    }

    // Put the source stamp and display metadata in a private JPEG APP15 segment.
    let length = u16::try_from(2 + CACHE_FORMAT.len() + fingerprint.len() + 16 + details.len())
        .map_err(io::Error::other)?;
    let mut output = Vec::with_capacity(jpeg.len() + JPEG_APP15.len() + usize::from(length));
    output.extend_from_slice(header);
    output.extend_from_slice(JPEG_APP15);
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(CACHE_FORMAT);
    output.extend_from_slice(&fingerprint);
    output.extend_from_slice(&checksum(body));
    output.extend_from_slice(&details);
    output.extend_from_slice(body);
    if output.len() as u64 > MAX_CACHE_BYTES {
        return Err(io::Error::other("JPEG exceeds the cache limit"));
    }
    Ok(output)
}

fn read_entry(bytes: &[u8], fingerprint: [u8; 16]) -> Option<Preview> {
    let (_, segments) = split_jfif(bytes)?;
    let (length, remaining) = segments
        .strip_prefix(JPEG_APP15)?
        .split_first_chunk::<2>()?;
    let length = usize::from(u16::from_be_bytes(*length)).checked_sub(2)?;
    let (marker, body) = remaining.split_at_checked(length)?;
    let details = marker
        .strip_prefix(CACHE_FORMAT)?
        .strip_prefix(fingerprint.as_slice())?;
    let (expected_checksum, json) = details.split_first_chunk::<16>()?;
    if json.len() > MAX_METADATA_BYTES
        || !body.ends_with(&[0xff, 0xd9])
        || *expected_checksum != checksum(body)
    {
        return None;
    }
    let (filename, summary): (String, Fields) = serde_json::from_slice(json).ok()?;

    let rgba = decode_cached_thumbnail(bytes)?;
    let (width, height) = rgba.dimensions();
    Some(Preview {
        pixels: Ok(SharedPixelBuffer::clone_from_slice(
            rgba.as_raw(),
            width,
            height,
        )),
        filename,
        summary,
    })
}

fn split_jfif(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (length, _) = bytes.strip_prefix(JPEG_JFIF)?.split_first_chunk::<2>()?;
    let length = usize::from(u16::from_be_bytes(*length));
    if length < 2 {
        return None;
    }
    bytes.split_at_checked(JPEG_JFIF.len() + length)
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    let max_edge = PreviewSize::Thumbnail.max_edge();
    (1..=max_edge).contains(&width) && (1..=max_edge).contains(&height)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, error::Error, path::PathBuf};

    use cap_std::ambient_authority;
    use slint::Rgba8Pixel;
    use tempfile::TempDir;

    use super::*;

    fn fixture() -> io::Result<(TempDir, Dir, PhotographGroup)> {
        let folder = tempfile::tempdir()?;
        let project = Dir::open_ambient_dir(folder.path(), ambient_authority())?;
        project.create_dir(".cuttingroom")?;
        project.create_dir("nested")?;
        project.write("nested/photo.png", b"source data")?;
        let group = PhotographGroup {
            stem: PathBuf::from("nested/photo"),
            files: vec![PathBuf::from("nested/photo.png")],
        };
        Ok((folder, project, group))
    }

    fn generated() -> Preview {
        let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(16, 16);
        pixels.make_mut_slice().fill(Rgba8Pixel::new(255, 0, 0, 0));
        Preview {
            pixels: Ok(pixels),
            filename: "photo.png".into(),
            summary: vec![("Camera".into(), "Test camera".into())],
        }
    }

    fn load_and_save(
        cache: &ThumbnailCache,
        project: &Dir,
        group: &PhotographGroup,
        generate: impl FnOnce() -> Preview,
        cancelled: impl Fn() -> bool,
    ) -> (Preview, bool) {
        let LoadedThumbnail { preview, status } =
            cache.load_or_generate(project, group, generate, &cancelled);
        let saved = match status {
            CacheStatus::Hit => true,
            CacheStatus::Write(entry) => cache.save(project, group, entry, &preview, cancelled),
            CacheStatus::Unavailable => false,
        };
        (preview, saved)
    }

    fn entry_file(group: &PhotographGroup) -> PathBuf {
        let key = group_key(group);
        let (shard, entry) = entry_path(&key);
        PathBuf::from(".cuttingroom")
            .join(CACHE_DIRECTORY)
            .join(shard)
            .join(entry)
    }

    #[test]
    fn returns_preview_before_saving_and_rejects_a_stale_write() -> Result<(), Box<dyn Error>> {
        let (_folder, project, group) = fixture()?;
        let cache = ThumbnailCache::open(&project)?;
        let LoadedThumbnail { preview, status } =
            cache.load_or_generate(&project, &group, generated, || false);
        assert!(preview.pixels.is_ok());
        let CacheStatus::Write(entry) = status else {
            return Err("new preview did not produce a write request".into());
        };
        assert!(!project.exists(entry_file(&group)));
        project.write("nested/photo.png", b"changed source data")?;
        assert!(!cache.save(&project, &group, entry, &preview, || false));
        assert!(!project.exists(entry_file(&group)));
        Ok(())
    }

    #[test]
    fn reuses_compressed_thumbnails_after_reopen_and_rebuilds_changed_sources()
    -> Result<(), Box<dyn Error>> {
        let (_folder, project, group) = fixture()?;
        let cache = ThumbnailCache::open(&project)?;
        let calls = Cell::new(0);
        let generate = || {
            calls.set(calls.get() + 1);
            generated()
        };
        let (first, saved) = load_and_save(&cache, &project, &group, generate, || false);
        assert!(saved && first.pixels.is_ok());
        assert_eq!(calls.get(), 1);
        assert!(
            project.read(entry_file(&group))?.starts_with(&[0xff, 0xd8]),
            "thumbnail must remain a readable JPEG"
        );
        drop(cache);

        let cache = ThumbnailCache::open(&project)?;
        let hit = cache.load_or_generate(&project, &group, generate, || false);
        assert_eq!(calls.get(), 1);
        assert!(matches!(hit.status, CacheStatus::Hit));
        assert_eq!(hit.preview.filename, first.filename);
        assert_eq!(hit.preview.summary, first.summary);
        let pixels = hit.preview.pixels.map_err(|_| "invalid cached JPEG")?;
        let pixel = pixels.as_slice().first().ok_or("empty thumbnail")?;
        assert_eq!((pixels.width(), pixels.height()), (16, 16));
        assert_eq!(pixel.a, 255);
        assert!(
            pixel.r.abs_diff(26) <= 2 && pixel.g.abs_diff(26) <= 2 && pixel.b.abs_diff(26) <= 2
        );

        project.write("nested/photo.png", b"longer source data")?;
        assert!(load_and_save(&cache, &project, &group, generate, || false).1);
        assert_eq!(calls.get(), 2);
        assert!(load_and_save(&cache, &project, &group, generate, || false).1);
        assert_eq!(calls.get(), 2);
        Ok(())
    }

    #[test]
    fn rejects_truncated_cache_entries_and_skips_writes_after_cancellation()
    -> Result<(), Box<dyn Error>> {
        let (_folder, project, group) = fixture()?;
        let cache = ThumbnailCache::open(&project)?;
        let path = entry_file(&group);
        assert!(load_and_save(&cache, &project, &group, generated, || false).1);
        let entry = project.read(&path)?;
        project.write(&path, &entry[..entry.len() / 2])?;

        let cancelled = Cell::new(false);
        let thumbnail = cache.load_or_generate(
            &project,
            &group,
            || {
                cancelled.set(true);
                generated()
            },
            || cancelled.get(),
        );
        assert!(
            thumbnail.preview.pixels.is_ok()
                && matches!(thumbnail.status, CacheStatus::Unavailable)
        );
        assert_eq!(project.read(&path)?, &entry[..entry.len() / 2]);
        assert!(load_and_save(&cache, &project, &group, generated, || false).1);
        assert_eq!(project.read(&path)?, entry);
        Ok(())
    }

    #[test]
    fn keeps_same_named_groups_separate_and_invalidates_changed_candidates()
    -> Result<(), Box<dyn Error>> {
        let (_folder, project, mut group) = fixture()?;
        let cache = ThumbnailCache::open(&project)?;
        project.create_dir("other")?;
        project.write("other/photo.png", b"source data")?;
        let other = PhotographGroup {
            stem: PathBuf::from("other/photo"),
            files: vec![PathBuf::from("other/photo.png")],
        };
        let calls = Cell::new(0);
        let generate = || {
            calls.set(calls.get() + 1);
            generated()
        };
        for group in [&group, &other, &group, &other] {
            assert!(load_and_save(&cache, &project, group, generate, || false).1);
        }
        assert_eq!(calls.get(), 2);

        project.write("nested/photo.jpg", b"paired data")?;
        group.files.push(PathBuf::from("nested/photo.jpg"));
        assert!(load_and_save(&cache, &project, &group, generate, || false).1);
        assert_eq!(calls.get(), 3);
        project.write("nested/photo.jpg", b"changed paired data")?;
        assert!(load_and_save(&cache, &project, &group, generate, || false).1);
        assert_eq!(calls.get(), 4);
        Ok(())
    }

    #[test]
    fn caches_a_group_with_an_opaque_dot_stem() -> Result<(), Box<dyn Error>> {
        let (_folder, project, _) = fixture()?;
        let source = PathBuf::from("nested/...jpg");
        project.write(&source, b"source data")?;
        let group = PhotographGroup {
            stem: source.with_extension(""),
            files: vec![source],
        };
        let cache = ThumbnailCache::open(&project)?;
        assert!(load_and_save(&cache, &project, &group, generated, || false).1);
        assert!(project.read(entry_file(&group))?.starts_with(&[0xff, 0xd8]));
        Ok(())
    }

    #[test]
    fn clears_cache_contents_and_keeps_the_project_files_and_cache_root()
    -> Result<(), Box<dyn Error>> {
        let (_folder, project, group) = fixture()?;
        let cache = ThumbnailCache::open(&project)?;
        assert!(load_and_save(&cache, &project, &group, generated, || false).1);
        project.write(".cuttingroom/project.sqlite", b"project data")?;
        project.create_dir_all(".cuttingroom/thumbnails/custom/folder")?;
        project.write(
            ".cuttingroom/thumbnails/custom/folder/debris.tmp",
            b"partial entry",
        )?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            _folder.path().join("nested"),
            _folder
                .path()
                .join(".cuttingroom/thumbnails/custom/folder/link"),
        )?;

        cache.clear(&project, || false)?;
        let directory = project.open_dir(".cuttingroom/thumbnails")?;
        assert!(directory.entries()?.next().is_none());
        assert_eq!(project.read("nested/photo.png")?, b"source data");
        assert_eq!(
            project.read(".cuttingroom/project.sqlite")?,
            b"project data"
        );
        assert!(load_and_save(&cache, &project, &group, generated, || false).1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_replaced_cache_directories_and_linked_entries() -> Result<(), Box<dyn Error>> {
        use std::{fs, os::unix::fs::symlink};

        let (folder, project, group) = fixture()?;
        let outside = tempfile::tempdir()?;
        let target = outside.path().join("protected.jpg");
        fs::write(&target, b"separate file")?;
        let cache = ThumbnailCache::open(&project)?;
        let entry = entry_file(&group);
        let parent = entry.parent().ok_or("missing cache bucket")?;
        project.create_dir(parent)?;
        symlink(&target, folder.path().join(&entry))?;
        let (preview, saved) = load_and_save(&cache, &project, &group, generated, || false);
        assert!(preview.pixels.is_ok() && !saved);
        assert_eq!(fs::read(&target)?, b"separate file");

        project.remove_file(&entry)?;
        fs::hard_link(&target, folder.path().join(&entry))?;
        let (preview, saved) = load_and_save(&cache, &project, &group, generated, || false);
        assert!(preview.pixels.is_ok() && !saved);
        assert_eq!(project.read(&entry)?, b"separate file");
        assert_eq!(fs::read(&target)?, b"separate file");

        let storage = folder.path().join(".cuttingroom");
        fs::rename(
            storage.join(CACHE_DIRECTORY),
            storage.join("prior-thumbnails"),
        )?;
        symlink(outside.path(), storage.join(CACHE_DIRECTORY))?;
        assert!(ThumbnailCache::open(&project).is_err());
        assert!(cache.clear(&project, || false).is_err());
        let (preview, saved) = load_and_save(&cache, &project, &group, generated, || false);
        assert!(preview.pixels.is_ok() && !saved);
        assert_eq!(fs::read_dir(outside.path())?.count(), 1);
        Ok(())
    }
}
