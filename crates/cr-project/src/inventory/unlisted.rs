use std::{
    io,
    path::{Component, Path, PathBuf},
};

use diesel::{
    QueryDsl, RunQueryDsl,
    connection::SimpleConnection,
    sql_types::{BigInt, Binary},
    sqlite::SqliteConnection,
};
use tempfile::NamedTempFile;

use super::{BATCH_SIZE, PhotographGroup};
use crate::{OpenError, Project, schema::connect};

diesel::table! {
    unlisted_photographs (stem, path) {
        stem -> Binary,
        path -> Binary,
    }
}

const PAGE_SIZE: i64 = 32;

/// A bounded, worker-owned cursor over the rest of a committed scan.
pub struct UnlistedPhotographs {
    db: SqliteConnection,
    _file: NamedTempFile,
    total: usize,
    remaining: usize,
    after: Vec<u8>,
}

impl UnlistedPhotographs {
    /// The total number of groups outside the visible listing.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Whether the cursor has returned its last group.
    pub fn is_finished(&self) -> bool {
        self.remaining == 0
    }

    /// Read the next page of complete groups. An empty page marks the end.
    pub fn next_batch(&mut self) -> Result<Vec<PhotographGroup>, OpenError> {
        if self.is_finished() {
            return Ok(Vec::new());
        }

        let rows = diesel::sql_query(
            "SELECT stem, path FROM unlisted_photographs
             WHERE stem IN (
                 SELECT DISTINCT stem FROM unlisted_photographs
                 WHERE stem > ? ORDER BY stem LIMIT ?
             )
             ORDER BY stem, path",
        )
        .bind::<Binary, _>(&self.after)
        .bind::<BigInt, _>(PAGE_SIZE)
        .load::<UnlistedPhoto>(&mut self.db)?;
        let mut groups = Vec::<PhotographGroup>::new();
        let mut previous = &self.after;
        for UnlistedPhoto { stem: key, path } in &rows {
            if key != previous {
                groups.push(PhotographGroup {
                    stem: native_path(key)?,
                    files: Vec::new(),
                });
                previous = key;
            }
            if let Some(group) = groups.last_mut() {
                group.files.push(source_path(path, &group.stem)?);
            }
        }
        for group in &mut groups {
            group.files.sort_unstable();
        }
        self.remaining = self
            .remaining
            .checked_sub(groups.len())
            .filter(|_| !groups.is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid thumbnail inventory")
            })?;
        self.after = previous.to_vec();
        Ok(groups)
    }
}

pub(super) struct UnlistedWriter {
    db: SqliteConnection,
    file: NamedTempFile,
    batch: Vec<UnlistedPhoto>,
}

impl UnlistedWriter {
    pub(super) fn start(project: &Project) -> Result<Self, OpenError> {
        project.storage.revalidate(Some(project.file_id))?;
        let file = tempfile::Builder::new()
            .prefix(".thumbnail-inventory-")
            .suffix(".sqlite")
            .tempfile_in(&project.storage.directory)?;
        let mut db = connect(file.path())?;
        project.storage.revalidate(Some(project.file_id))?;
        file.reopen()?;
        // This scratch database has no durable state and uses a separate transaction.
        db.batch_execute(
            "PRAGMA synchronous = OFF;
             PRAGMA temp_store = MEMORY;
             BEGIN;
             CREATE TABLE unlisted_photographs (
                 stem BLOB NOT NULL,
                 path BLOB NOT NULL,
                 PRIMARY KEY (stem, path)
             ) WITHOUT ROWID;",
        )?;
        Ok(Self {
            db,
            file,
            batch: Vec::with_capacity(BATCH_SIZE),
        })
    }

    pub(super) fn append(&mut self, stem: &Path, paths: Vec<PathBuf>) -> Result<(), OpenError> {
        let stem = native_bytes(stem);
        for path in paths {
            self.batch.push(UnlistedPhoto {
                stem: stem.clone(),
                path: native_bytes(&path),
            });
            if self.batch.len() == BATCH_SIZE {
                self.flush()?;
            }
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<UnlistedPhotographs, OpenError> {
        self.flush()?;
        let count: i64 = unlisted_photographs::table
            .select(diesel::dsl::sql::<BigInt>("COUNT(DISTINCT stem)"))
            .get_result(&mut self.db)?;
        self.db.batch_execute("COMMIT;")?;
        let total = usize::try_from(count).map_err(io::Error::other)?;
        Ok(UnlistedPhotographs {
            db: self.db,
            _file: self.file,
            total,
            remaining: total,
            after: Vec::new(),
        })
    }

    fn flush(&mut self) -> Result<(), OpenError> {
        if !self.batch.is_empty() {
            diesel::insert_into(unlisted_photographs::table)
                .values(self.batch.as_slice())
                .execute(&mut self.db)?;
            self.batch.clear();
        }
        Ok(())
    }
}

#[derive(diesel::Insertable, diesel::QueryableByName)]
#[diesel(table_name = unlisted_photographs)]
#[diesel(treat_none_as_default_value = false)]
struct UnlistedPhoto {
    stem: Vec<u8>,
    path: Vec<u8>,
}

#[cfg(unix)]
fn native_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn native_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn native_path(bytes: &[u8]) -> io::Result<PathBuf> {
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes).to_os_string()
    };
    #[cfg(windows)]
    let name = {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let (units, trailing) = bytes.as_chunks::<2>();
        if !trailing.is_empty() {
            return Err(invalid_path());
        }
        OsString::from_wide(
            &units
                .iter()
                .copied()
                .map(u16::from_le_bytes)
                .collect::<Vec<_>>(),
        )
    };
    Ok(PathBuf::from(name))
}

fn source_path(bytes: &[u8], stem: &Path) -> io::Result<PathBuf> {
    let path = native_path(bytes)?;
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        || path.with_extension("").as_os_str() != stem.as_os_str()
    {
        return Err(invalid_path());
    }
    Ok(path)
}

fn invalid_path() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid inventory path")
}
