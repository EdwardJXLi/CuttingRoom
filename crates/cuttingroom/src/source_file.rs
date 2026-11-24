use std::{io::Read, path::Path};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File, OpenOptions};

pub(crate) fn open(directory: &Dir, path: &Path, max_bytes: u64) -> Option<File> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }

    let file = directory.open_with(path, &options).ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.len() <= max_bytes).then_some(file)
}

pub(crate) fn read(file: File, max_bytes: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= max_bytes).then_some(bytes)
}
