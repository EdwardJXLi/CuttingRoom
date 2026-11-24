mod inventory;
mod scan;
mod schema;
mod storage;

use std::{
    fs::canonicalize,
    io,
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{
    schema::{SCHEMA, Schema, connect, initialize, inspect, migrate},
    storage::{FileId, ProjectLock, Storage, publish},
};

pub use inventory::{PhotographGroup, PhotographList, UnlistedPhotographs};
pub use scan::{PhotographFormat, photograph_format};

#[derive(Debug, Error)]
pub enum OpenError {
    #[error("select an existing folder")]
    NotDirectory,
    #[error("project storage contains a link or an unexpected file type")]
    UnsafeStorage,
    #[error("project storage changed while opening")]
    ChangedStorage,
    #[error("the project is busy; close the other CuttingRoom sessions and try again")]
    LockBusy,
    #[error("CuttingRoom needs write access to the project database")]
    WriteAccessRequired,
    #[error("project uses an unexpected SQLite journal; preserve it for recovery")]
    UnexpectedJournal,
    #[error("project.sqlite is not a CuttingRoom database")]
    WrongApplication,
    #[error(
        "project schema version {found} is incompatible with this CuttingRoom version (supports {supported})"
    )]
    UnsupportedSchema { found: i32, supported: i32 },
    #[error("could not verify the pre-upgrade backup")]
    InvalidBackup,
    #[error("the photo folder could not be scanned. Check that it is connected and readable")]
    Scan(#[source] io::Error),
    #[error("the folder scan was cancelled")]
    ScanCancelled,
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("SQLite: {0}")]
    Connection(#[from] diesel::ConnectionError),
    #[error("SQLite: {0}")]
    Database(#[from] diesel::result::Error),
}

pub enum OpenOutcome {
    Ready(Project),
    NeedsUpgrade(PendingUpgrade),
    Cancelled,
}

pub struct PendingUpgrade {
    root: PathBuf,
    storage: Storage,
    file_id: FileId,
    version: i32,
    schema: Schema,
    lock: ProjectLock,
}

impl PendingUpgrade {
    pub fn versions(&self) -> (i32, i32) {
        (self.version, self.schema.version)
    }

    pub fn apply(self) -> Result<Project, OpenError> {
        self.storage.revalidate(Some(self.file_id))?;
        self.lock.lock_exclusive()?;
        migrate(&self.storage, self.file_id, self.schema, self.version)?;
        self.lock.unlock()?;
        self.lock.lock_shared()?;
        Ok(Project {
            root: self.root,
            storage: self.storage,
            file_id: self.file_id,
            _lock: self.lock,
        })
    }
}

pub struct Project {
    root: PathBuf,
    storage: Storage,
    file_id: FileId,
    _lock: ProjectLock,
}

impl Project {
    pub fn open(
        folder: &Path,
        confirm_lock: impl FnOnce() -> io::Result<bool>,
    ) -> Result<OpenOutcome, OpenError> {
        Self::open_with_schema(folder, SCHEMA, confirm_lock)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn open_with_schema(
        folder: &Path,
        schema: Schema,
        confirm_lock: impl FnOnce() -> io::Result<bool>,
    ) -> Result<OpenOutcome, OpenError> {
        let root = canonicalize(folder)?;
        if !root.is_dir() {
            return Err(OpenError::NotDirectory);
        }
        let storage = Storage::open(&root)?;
        let Some(lock) = storage.open_lock(confirm_lock)? else {
            return Ok(OpenOutcome::Cancelled);
        };
        if let Some(id) = storage.database_id()? {
            return open_existing(root, storage, id, schema, lock);
        }
        let database = NamedTempFile::new_in(&storage.directory)?;
        initialize(database.path(), schema)?;
        storage.revalidate(None)?;
        publish(database, &storage.database)?;
        let file_id = storage.database_id()?.ok_or(OpenError::ChangedStorage)?;
        Ok(OpenOutcome::Ready(Self {
            root,
            storage,
            file_id,
            _lock: lock,
        }))
    }
}

fn open_existing(
    root: PathBuf,
    storage: Storage,
    id: FileId,
    schema: Schema,
    lock: ProjectLock,
) -> Result<OpenOutcome, OpenError> {
    storage.check_database_header(id, schema.version)?;
    let version = inspect(&mut connect(&storage.database)?, schema)?;
    if version < schema.version {
        Ok(OpenOutcome::NeedsUpgrade(PendingUpgrade {
            root,
            storage,
            file_id: id,
            version,
            schema,
            lock,
        }))
    } else {
        tempfile::tempfile_in(&storage.directory)?;
        Ok(OpenOutcome::Ready(Project {
            root,
            storage,
            file_id: id,
            _lock: lock,
        }))
    }
}

#[cfg(test)]
mod tests;
