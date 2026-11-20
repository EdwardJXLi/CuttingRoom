use std::{io, path::Path};

use diesel::{
    Connection, RunQueryDsl,
    connection::SimpleConnection,
    sql_types::{Integer, Text},
    sqlite::{
        SqliteConnection,
        pragma::{PerSchema, Pragma, ReadPragma, SchemaPragma, WritePragma},
    },
};
use libsqlite3_sys as sqlite;
use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use tempfile::NamedTempFile;

use crate::{
    OpenError,
    storage::{FileId, Storage},
};

pub(crate) const APPLICATION_ID: i32 = 0x4354_524d;
const BASE_VERSION: i32 = 1;
const INITIAL_SCHEMA: &str = include_str!("../migrations/1_create_project/up.sql");

struct ApplicationId;

impl Pragma for ApplicationId {
    const NAME: &'static str = "application_id";
    type Scope = PerSchema;
}

impl ReadPragma for ApplicationId {
    type SqlType = Integer;
}

struct UserVersion;

impl Pragma for UserVersion {
    const NAME: &'static str = "user_version";
    type Scope = PerSchema;
}

impl ReadPragma for UserVersion {
    type SqlType = Integer;
}

impl WritePragma for UserVersion {
    type Argument<'a> = i32;
}

#[derive(Clone, Copy)]
pub(crate) struct Schema {
    pub(crate) version: i32,
    pub(crate) upgrades: &'static [Upgrade],
}

pub(crate) const SCHEMA: Schema = Schema {
    version: BASE_VERSION,
    upgrades: &[],
};

pub(crate) struct Upgrade {
    pub(crate) from: i32,
    pub(crate) sql: &'static str,
}

impl Schema {
    fn upgrades_from(self, version: i32) -> impl Iterator<Item = &'static Upgrade> {
        (version..self.version)
            .filter_map(move |from| self.upgrades.iter().find(|upgrade| upgrade.from == from))
    }

    fn require_supported(self, version: i32) -> Result<(), OpenError> {
        if (BASE_VERSION..=self.version).contains(&version)
            && self.upgrades_from(version).count() == (self.version - version) as usize
        {
            Ok(())
        } else {
            Err(OpenError::UnsupportedSchema {
                found: version,
                supported: self.version,
            })
        }
    }
}

#[derive(diesel::QueryableByName)]
struct Check {
    #[diesel(sql_type = Text)]
    value: String,
}

#[derive(diesel::QueryableByName)]
struct IntegrityCheck {
    #[diesel(sql_type = Text)]
    integrity_check: String,
}

pub(crate) fn connect(path: &Path) -> Result<SqliteConnection, OpenError> {
    #[cfg(windows)]
    if path.to_str().is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid SQLite path").into());
    }
    let filename = percent_encode(path.as_os_str().as_encoded_bytes(), NON_ALPHANUMERIC);
    let mut db = SqliteConnection::establish(&format!("file:{filename}?mode=rw"))?;
    configure(&mut db)?;
    db.batch_execute("PRAGMA busy_timeout = 2000; PRAGMA foreign_keys = ON;")?;
    Ok(db)
}

fn configure(db: &mut SqliteConnection) -> Result<(), OpenError> {
    // SAFETY: Diesel owns the handle throughout the call. These options affect neither
    // transaction state nor statement ownership. No pointer leaves the closure.
    unsafe {
        db.with_raw_connection(|connection| {
            if sqlite::sqlite3_db_readonly(connection, c"main".as_ptr()) != 0 {
                return Err(OpenError::WriteAccessRequired);
            }
            for (option, enabled) in [
                (sqlite::SQLITE_DBCONFIG_DEFENSIVE, 1),
                (sqlite::SQLITE_DBCONFIG_TRUSTED_SCHEMA, 0),
                (sqlite::SQLITE_DBCONFIG_ENABLE_VIEW, 0),
                (sqlite::SQLITE_DBCONFIG_ENABLE_TRIGGER, 0),
            ] {
                let code = sqlite::sqlite3_db_config(
                    connection,
                    option,
                    enabled,
                    std::ptr::null_mut::<i32>(),
                );
                if code != sqlite::SQLITE_OK {
                    return Err(
                        io::Error::other(format!("could not configure SQLite ({code})")).into(),
                    );
                }
            }
            sqlite::sqlite3_limit(connection, sqlite::SQLITE_LIMIT_ATTACHED, 0);
            sqlite::sqlite3_limit(connection, sqlite::SQLITE_LIMIT_LENGTH, 1024 * 1024);
            Ok(())
        })
    }
}

pub(crate) fn initialize(path: &Path, schema: Schema) -> Result<(), OpenError> {
    schema.require_supported(BASE_VERSION)?;
    let mut db = connect(path)?;
    db.batch_execute("PRAGMA synchronous = FULL;")?;
    db.immediate_transaction(|db| {
        db.batch_execute(INITIAL_SCHEMA)?;
        for upgrade in schema.upgrades_from(BASE_VERSION) {
            apply_upgrade(db, upgrade)?;
        }
        inspect(db, schema)?;
        Ok(())
    })
}

pub(crate) fn inspect(db: &mut SqliteConnection, schema: Schema) -> Result<i32, OpenError> {
    if ApplicationId::get::<i32>(None, db)? != APPLICATION_ID {
        return Err(OpenError::WrongApplication);
    }
    let version = UserVersion::get::<i32>(None, db)?;
    schema.require_supported(version)?;
    let journal_mode = diesel::sql_query("SELECT journal_mode AS value FROM pragma_journal_mode")
        .get_result::<Check>(db)?;
    if journal_mode.value != "delete" {
        return Err(OpenError::UnexpectedJournal);
    }
    Ok(version)
}

fn apply_upgrade(db: &mut SqliteConnection, upgrade: &Upgrade) -> Result<(), OpenError> {
    db.batch_execute(upgrade.sql)?;
    UserVersion::set(upgrade.from + 1, None, db)?;
    Ok(())
}

pub(crate) fn migrate(
    storage: &Storage,
    id: FileId,
    schema: Schema,
    version: i32,
) -> Result<(), OpenError> {
    // Keep this descriptor open until the SQLite connection closes. On POSIX, closing any
    // descriptor for the same inode can release this process's SQLite record locks.
    let mut source = storage.open_database_file(id)?;
    let mut db = connect(&storage.database)?;
    db.batch_execute("PRAGMA synchronous = FULL;")?;
    db.immediate_transaction(|db| {
        if inspect(db, schema)? != version {
            return Err(OpenError::ChangedStorage);
        }
        storage.revalidate(Some(id))?;
        let mut backup = NamedTempFile::new_in(&storage.directory)?;
        io::copy(&mut source, backup.as_file_mut())?;
        verify_backup(backup.path(), schema, version)?;
        storage.publish_backup(backup, version)?;
        storage.revalidate(Some(id))?;

        for upgrade in schema.upgrades_from(version) {
            apply_upgrade(db, upgrade)?;
        }
        inspect(db, schema)?;
        Ok(())
    })
}

fn verify_backup(path: &Path, schema: Schema, version: i32) -> Result<(), OpenError> {
    let mut backup = connect(path)?;
    let integrity =
        diesel::sql_query("PRAGMA integrity_check(1)").get_result::<IntegrityCheck>(&mut backup)?;
    if integrity.integrity_check != "ok" || inspect(&mut backup, schema)? != version {
        return Err(OpenError::InvalidBackup);
    }
    Ok(())
}
