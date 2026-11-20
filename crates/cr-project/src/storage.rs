use std::{
    fs::{self, File, Metadata, OpenOptions, TryLockError},
    io::{self, Read},
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

use crate::{OpenError, schema::APPLICATION_ID};

pub(crate) struct Storage {
    pub(crate) directory: PathBuf,
    pub(crate) database: PathBuf,
    directory_id: FileId,
}

impl Storage {
    pub(crate) fn open(root: &Path) -> Result<Self, OpenError> {
        let directory = root.join(".cuttingroom");
        let directory_id = ensure_directory(&directory)?;
        let database = directory.join("project.sqlite");
        Ok(Self {
            directory,
            database,
            directory_id,
        })
    }

    pub(crate) fn database_id(&self) -> Result<Option<FileId>, OpenError> {
        self.revalidate_directory()?;
        let database = state_file_id(&self.database)?;
        let journal = state_file_id(&self.directory.join("project.sqlite-journal"))?;
        if state_file_id(&self.directory.join("project.sqlite-wal"))?.is_some()
            || state_file_id(&self.directory.join("project.sqlite-shm"))?.is_some()
            || (database.is_none() && journal.is_some())
        {
            return Err(OpenError::UnexpectedJournal);
        }
        Ok(database)
    }

    fn revalidate_directory(&self) -> Result<(), OpenError> {
        let metadata = checked_metadata(&self.directory, true)?.ok_or(OpenError::ChangedStorage)?;
        if file_id(&self.directory, &metadata)? != self.directory_id {
            return Err(OpenError::ChangedStorage);
        }
        Ok(())
    }

    pub(crate) fn revalidate(&self, id: Option<FileId>) -> Result<(), OpenError> {
        if self.database_id()? != id {
            return Err(OpenError::ChangedStorage);
        }
        Ok(())
    }

    pub(crate) fn check_database_header(
        &self,
        id: FileId,
        supported: i32,
    ) -> Result<(), OpenError> {
        let mut file = self.open_database_file(id)?;
        // SQLite can replay a journal when it opens the database. Check the header first.
        let mut header = [0; 100];
        file.read_exact(&mut header)?;
        if &header[..16] != b"SQLite format 3\0" || header[68..72] != APPLICATION_ID.to_be_bytes() {
            return Err(OpenError::WrongApplication);
        }
        let found = i32::from_be_bytes([header[60], header[61], header[62], header[63]]);
        if found > supported {
            return Err(OpenError::UnsupportedSchema { found, supported });
        }
        Ok(())
    }

    pub(crate) fn open_database_file(&self, id: FileId) -> Result<File, OpenError> {
        self.revalidate(Some(id))?;
        let mut options = File::options();
        options.read(true);
        no_follow(&mut options);
        let file = options.open(&self.database)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || is_link(&metadata) || handle_id(&file, &metadata, false)? != id {
            return Err(OpenError::ChangedStorage);
        }
        Ok(file)
    }

    pub(crate) fn open_lock(
        &self,
        confirm: impl FnOnce() -> io::Result<bool>,
    ) -> Result<Option<ProjectLock>, OpenError> {
        let path = self.directory.join("project.lock");
        let mut options = File::options();
        options.read(true).write(true).create_new(true);
        no_follow(&mut options);
        let (file, expected_id) = match options.open(&path) {
            Ok(file) => (file, None),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let id = state_file_id(&path)?;
                if id.is_some() && !confirm()? {
                    return Ok(None);
                }
                self.revalidate_directory()?;
                match options.create_new(false).open(&path) {
                    Ok(file) if id.is_some() => (file, id),
                    Ok(_) => return Err(OpenError::ChangedStorage),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        (options.create_new(true).open(&path)?, None)
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || is_link(&metadata) {
            return Err(OpenError::UnsafeStorage);
        }
        let id = handle_id(&file, &metadata, false)?;
        if expected_id.is_some_and(|expected| expected != id) {
            return Err(OpenError::ChangedStorage);
        }
        let lock = ProjectLock { file, path, id };
        lock.lock_shared()?;
        Ok(Some(lock))
    }

    pub(crate) fn publish_backup(
        &self,
        backup: NamedTempFile,
        version: i32,
    ) -> Result<(), OpenError> {
        let prefix = format!("project.sqlite.v{version}.bak");
        let mut last = 0_u64;
        for entry in self.directory.read_dir()? {
            let filename = entry?.file_name();
            if let Some(index) = filename
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|suffix| suffix.parse::<u64>().ok())
            {
                last = last.max(index);
            }
        }
        let index = last
            .checked_add(1)
            .ok_or_else(|| io::Error::other("backup number exhausted"))?;
        publish(backup, &self.directory.join(format!("{prefix}{index}")))
    }
}

pub(crate) struct ProjectLock {
    file: File,
    path: PathBuf,
    id: FileId,
}

impl ProjectLock {
    pub(crate) fn lock_exclusive(&self) -> Result<(), OpenError> {
        self.file.unlock()?;
        self.try_lock(File::try_lock)
    }

    pub(crate) fn lock_shared(&self) -> Result<(), OpenError> {
        self.try_lock(File::try_lock_shared)
    }

    pub(crate) fn unlock(&self) -> io::Result<()> {
        self.file.unlock()
    }

    fn try_lock(&self, lock: fn(&File) -> Result<(), TryLockError>) -> Result<(), OpenError> {
        match lock(&self.file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(OpenError::LockBusy),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        if state_file_id(&self.path)? != Some(self.id) {
            return Err(OpenError::ChangedStorage);
        }
        Ok(())
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = self.unlock();
        if self.try_lock(File::try_lock).is_ok() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn publish(file: NamedTempFile, path: &Path) -> Result<(), OpenError> {
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    sync_parent(path)?;
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<FileId, OpenError> {
    if let Some(metadata) = checked_metadata(path, true)? {
        return file_id(path, &metadata);
    }
    match fs::create_dir(path) {
        Ok(()) => sync_parent(path)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = checked_metadata(path, true)?.ok_or(OpenError::ChangedStorage)?;
    file_id(path, &metadata)
}

fn checked_metadata(path: &Path, directory: bool) -> Result<Option<Metadata>, OpenError> {
    match path.symlink_metadata() {
        Ok(metadata)
            if !is_link(&metadata)
                && (directory && metadata.is_dir() || !directory && metadata.is_file()) =>
        {
            Ok(Some(metadata))
        }
        Ok(_) => Err(OpenError::UnsafeStorage),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn state_file_id(path: &Path) -> Result<Option<FileId>, OpenError> {
    checked_metadata(path, false)?
        .map(|metadata| file_id(path, &metadata))
        .transpose()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId(u64, u64);

#[cfg(unix)]
fn file_id(_path: &Path, metadata: &Metadata) -> Result<FileId, OpenError> {
    unix_id(metadata, metadata.is_dir())
}

#[cfg(unix)]
fn handle_id(_file: &File, metadata: &Metadata, directory: bool) -> Result<FileId, OpenError> {
    unix_id(metadata, directory)
}

#[cfg(unix)]
fn unix_id(metadata: &Metadata, directory: bool) -> Result<FileId, OpenError> {
    use std::os::unix::fs::MetadataExt;

    if !directory && metadata.nlink() != 1 {
        return Err(OpenError::UnsafeStorage);
    }
    Ok(FileId(metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_id(path: &Path, metadata: &Metadata) -> Result<FileId, OpenError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let mut options = File::options();
    options
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let actual = file.metadata()?;
    if is_link(&actual) || actual.is_dir() != metadata.is_dir() {
        return Err(OpenError::UnsafeStorage);
    }
    handle_id(&file, &actual, actual.is_dir())
}

#[cfg(windows)]
fn handle_id(file: &File, _metadata: &Metadata, directory: bool) -> Result<FileId, OpenError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: The file owns the handle for this call. The writable output has the required size.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    if !directory && info.nNumberOfLinks != 1 {
        return Err(OpenError::UnsafeStorage);
    }
    Ok(FileId(
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(unix)]
fn no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(unix)]
fn is_link(metadata: &Metadata) -> bool {
    metadata.is_symlink()
}

#[cfg(windows)]
fn is_link(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}
