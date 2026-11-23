use std::{collections::BinaryHeap, path::Path};

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

diesel::table! {
    photos (id) {
        id -> Text,
        path -> Text,
        present -> Bool,
    }
}

const BATCH_SIZE: usize = 256;

/// The available photograph count and a bounded list of relative display paths.
pub struct PhotographList {
    pub total: i64,
    pub paths: Vec<String>,
}

impl Project {
    /// Reconcile the inventory against a fresh folder scan on a blocking worker.
    ///
    /// Report the number of photos observed so far through `progress`.
    /// Return up to `list_limit` available paths after commit. Cancelled and failed
    /// scans preserve the previous inventory.
    pub fn sync_photographs(
        &self,
        list_limit: u32,
        cancelled: impl Fn() -> bool,
        progress: impl FnMut(i64),
    ) -> Result<PhotographList, OpenError> {
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
            let listing = self.record_photographs(db, list_limit as usize, &cancelled, progress)?;
            self.storage.revalidate(Some(self.file_id))?;
            if cancelled() {
                return Err(OpenError::ScanCancelled);
            }
            Ok(listing)
        })
    }

    fn record_photographs(
        &self,
        db: &mut SqliteConnection,
        list_limit: usize,
        cancelled: impl Fn() -> bool,
        mut progress: impl FnMut(i64),
    ) -> Result<PhotographList, OpenError> {
        let mut batch = Vec::with_capacity(BATCH_SIZE);
        let mut paths = BinaryHeap::new();
        let mut total = 0;
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
            let display_path = path.to_string_lossy();
            if paths.len() < list_limit {
                paths.push(display_path.into_owned());
            } else if let Some(mut last) = paths.peek_mut()
                && display_path.as_ref() < last.as_str()
            {
                *last = display_path.into_owned();
            }
        }
        if cancelled() {
            return Err(OpenError::ScanCancelled);
        }
        upsert_photos(db, &batch)?;
        Ok(PhotographList {
            total,
            paths: paths.into_sorted_vec(),
        })
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
