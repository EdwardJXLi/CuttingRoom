use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use diesel::{
    ExpressionMethods, QueryDsl, RunQueryDsl, connection::SimpleConnection,
    sqlite::SqliteConnection,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    OpenError, Project,
    scan::photographs,
    schema::{SCHEMA, connect, inspect},
};

mod unlisted;

pub use unlisted::UnlistedPhotographs;
use unlisted::UnlistedWriter;

diesel::table! {
    photos (id) {
        id -> Text,
        path -> Text,
        present -> Bool,
    }
}

const BATCH_SIZE: usize = 256;

/// The number of present source files and a bounded list of complete groups.
pub struct PhotographList {
    pub total: i64,
    pub groups: Vec<PhotographGroup>,
    pub has_more: bool,
}

/// Sources in one directory with the same exact filename stem.
pub struct PhotographGroup {
    pub stem: PathBuf,
    pub files: Vec<PathBuf>,
}

impl Project {
    /// Reconcile the inventory against a fresh folder scan on a blocking worker.
    ///
    /// Report the number of source files observed so far through `progress`.
    /// Return up to `list_limit` complete groups after commit. Cancelled and failed
    /// scans preserve the previous inventory.
    pub fn sync_photographs(
        &self,
        list_limit: u32,
        cancelled: impl Fn() -> bool,
        progress: impl FnMut(i64),
    ) -> Result<PhotographList, OpenError> {
        self.sync_inventory(list_limit, cancelled, progress, false)
            .map(|(listing, _)| listing)
    }

    /// Reconcile the inventory and return a cursor for groups outside the listing.
    ///
    /// Keep the cursor on the project worker. Replace it after a successful scan.
    /// If the listing has more groups and the cursor is absent, the spool failed.
    pub fn sync_photographs_for_thumbnails(
        &self,
        list_limit: u32,
        cancelled: impl Fn() -> bool,
        progress: impl FnMut(i64),
    ) -> Result<(PhotographList, Option<UnlistedPhotographs>), OpenError> {
        self.sync_inventory(list_limit, cancelled, progress, true)
    }

    fn sync_inventory(
        &self,
        list_limit: u32,
        cancelled: impl Fn() -> bool,
        progress: impl FnMut(i64),
        record_unlisted: bool,
    ) -> Result<(PhotographList, Option<UnlistedPhotographs>), OpenError> {
        self.storage
            .check_database_header(self.file_id, SCHEMA.version)?;
        let mut db = connect(&self.storage.database)?;
        db.batch_execute("PRAGMA synchronous = FULL;")?;
        // Serialize the scans as well as their writes so a stale scan cannot win.
        db.immediate_transaction(|db| {
            if inspect(db, SCHEMA)? != SCHEMA.version {
                return Err(OpenError::ChangedStorage);
            }
            self.storage.revalidate(Some(self.file_id))?;
            diesel::update(photos::table.filter(photos::present.eq(true)))
                .set(photos::present.eq(false))
                .execute(db)?;
            let inventory = self.record_photographs(
                db,
                list_limit as usize,
                &cancelled,
                progress,
                record_unlisted,
            )?;
            self.storage.revalidate(Some(self.file_id))?;
            if cancelled() {
                return Err(OpenError::ScanCancelled);
            }
            Ok(inventory)
        })
    }

    fn record_photographs(
        &self,
        db: &mut SqliteConnection,
        list_limit: usize,
        cancelled: impl Fn() -> bool,
        mut progress: impl FnMut(i64),
        record_unlisted: bool,
    ) -> Result<(PhotographList, Option<UnlistedPhotographs>), OpenError> {
        let mut batch = Vec::with_capacity(BATCH_SIZE);
        let mut unlisted = None;
        let mut groups: BTreeMap<_, Vec<PathBuf>> = BTreeMap::new();
        let mut total = 0;
        let mut has_more = false;
        for path in photographs(self.root(), &cancelled) {
            let path = path.map_err(OpenError::Scan)?;
            batch.push(ScannedPhoto {
                id: Uuid::new_v4().to_string(),
                path: encode_path(&path),
            });
            if batch.len() == BATCH_SIZE {
                upsert_photos(db, &batch)?;
                batch.clear();
            }
            total += 1;
            progress(total);
            if list_limit == 0 && !record_unlisted {
                has_more = true;
                continue;
            }
            let stem = path.with_extension("");
            let key = (stem.to_string_lossy().into_owned(), stem);
            groups.entry(key).or_default().push(path);
            if groups.len() > list_limit
                && let Some(((_, stem), paths)) = groups.pop_last()
            {
                // The largest listed key can only decrease once the list is full.
                if !has_more && record_unlisted {
                    unlisted = UnlistedWriter::start(self).ok();
                }
                has_more = true;
                if let Some(writer) = &mut unlisted
                    && writer.append(&stem, paths).is_err()
                {
                    unlisted = None;
                }
            }
        }
        if cancelled() {
            return Err(OpenError::ScanCancelled);
        }
        upsert_photos(db, &batch)?;
        let listing = PhotographList {
            total,
            groups: groups
                .into_iter()
                .map(|((_, stem), mut files)| {
                    files.sort_unstable();
                    PhotographGroup { stem, files }
                })
                .collect(),
            has_more,
        };
        Ok((listing, unlisted.and_then(|writer| writer.finish().ok())))
    }
}

fn upsert_photos(db: &mut SqliteConnection, batch: &[ScannedPhoto]) -> Result<(), OpenError> {
    diesel::insert_into(photos::table)
        .values(batch)
        .on_conflict(photos::path)
        .do_update()
        .set(photos::present.eq(true))
        .execute(db)?;
    Ok(())
}

#[derive(diesel::Insertable)]
#[diesel(table_name = photos)]
#[diesel(treat_none_as_default_value = false)]
struct ScannedPhoto {
    id: String,
    path: String,
}

fn encode_path(path: &Path) -> String {
    let payload = if let Some(components) = path
        .iter()
        .map(|name| name.to_str())
        .collect::<Option<Vec<_>>>()
    {
        json!({ "encoding": "unicode", "components": components })
    } else {
        encode_native_path(path)
    };
    payload.to_string()
}

#[cfg(unix)]
fn encode_native_path(path: &Path) -> Value {
    use std::os::unix::ffi::OsStrExt;

    use base64::{Engine, engine::general_purpose::STANDARD};

    let components: Vec<_> = path
        .iter()
        .map(|name| STANDARD.encode(name.as_bytes()))
        .collect();
    json!({ "encoding": "unix-bytes", "components_base64": components })
}

#[cfg(windows)]
fn encode_native_path(path: &Path) -> Value {
    use std::os::windows::ffi::OsStrExt;

    let components: Vec<_> = path
        .iter()
        .map(|name| name.encode_wide().collect::<Vec<_>>())
        .collect();
    json!({ "encoding": "windows-utf16", "components_utf16": components })
}
