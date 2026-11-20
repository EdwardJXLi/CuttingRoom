use std::{error::Error, fs, path::Path};

use rusqlite::Connection;
use tempfile::{TempDir, tempdir};

use crate::{
    OpenError, OpenOutcome, PendingUpgrade, Project,
    schema::{APPLICATION_ID, Schema, Upgrade},
};

const STORAGE: &str = ".cuttingroom";
const DATABASE: &str = ".cuttingroom/project.sqlite";
const BACKUP: &str = ".cuttingroom/project.sqlite.v1.bak1";
const UPGRADE: Upgrade = Upgrade {
    from: 1,
    sql: "ALTER TABLE fixture ADD COLUMN migrated INTEGER NOT NULL DEFAULT 1;",
};
const NEXT_SCHEMA: Schema = Schema {
    version: 2,
    upgrades: &[UPGRADE],
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[test]
fn creates_version_one_and_reopens_a_moved_folder() -> TestResult {
    let folder = tempdir()?;
    let original = folder.path().join("photos #1");
    fs::create_dir(&original)?;
    fs::write(original.join("original.dng"), b"photo")?;
    drop(ready(&original)?);

    let db = Connection::open(original.join(DATABASE))?;
    let application_id: i32 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let tables: i64 = db.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
    assert_eq!(
        (application_id, version(&db)?, tables),
        (APPLICATION_ID, 1, 0)
    );
    drop(db);

    let before = fs::read(original.join(DATABASE))?;
    let moved = folder.path().join("renamed photos");
    fs::rename(&original, &moved)?;
    assert_eq!(ready(&moved)?.root(), moved.canonicalize()?);
    assert_eq!(fs::read(moved.join(DATABASE))?, before);
    assert_eq!(fs::read(moved.join("original.dng"))?, b"photo");
    Ok(())
}

#[test]
fn lock_requires_confirmation_and_survives_until_last_close() -> TestResult {
    let folder = tempdir()?;
    let root = folder.path();
    let lock = root.join(STORAGE).join("project.lock");
    let first = ready(root)?;
    assert!(matches!(open(root)?, OpenOutcome::Cancelled));

    let OpenOutcome::Ready(second) = Project::open(root, || Ok(true))? else {
        return Err("expected confirmed project open".into());
    };
    drop(first);
    assert!(lock.exists());
    drop(second);
    assert!(!lock.exists());
    drop(ready(root)?);
    Ok(())
}

#[test]
fn stale_lock_requires_confirmation_and_is_cleaned_on_close() -> TestResult {
    let folder = tempdir()?;
    let root = folder.path();
    drop(ready(root)?);
    let lock = root.join(STORAGE).join("project.lock");
    fs::write(&lock, "")?;
    assert!(matches!(open(root)?, OpenOutcome::Cancelled));
    assert!(lock.exists());

    let OpenOutcome::Ready(project) = Project::open(root, || Ok(true))? else {
        return Err("expected confirmed project open".into());
    };
    drop(project);
    assert!(!lock.exists());
    Ok(())
}

#[test]
fn upgrade_requires_other_sessions_to_close() -> TestResult {
    let folder = upgrade_fixture()?;
    let root = folder.path();
    let first = ready(root)?;
    let OpenOutcome::NeedsUpgrade(upgrade) =
        Project::open_with_schema(root, NEXT_SCHEMA, || Ok(true))?
    else {
        return Err("expected an upgrade prompt".into());
    };
    assert!(matches!(upgrade.apply(), Err(OpenError::LockBusy)));
    assert!(!root.join(BACKUP).exists());
    drop(first);
    Ok(())
}

#[test]
fn unknown_version_is_rejected_before_writes() -> TestResult {
    let folder = tempdir()?;
    drop(ready(folder.path())?);
    let path = folder.path().join(DATABASE);
    Connection::open(&path)?.pragma_update(None, "user_version", 2)?;
    let before = fs::read(&path)?;
    assert!(matches!(
        open(folder.path()),
        Err(OpenError::UnsupportedSchema {
            found: 2,
            supported: 1
        })
    ));
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

#[test]
fn unrelated_sqlite_database_is_preserved() -> TestResult {
    let folder = tempdir()?;
    fs::create_dir(folder.path().join(STORAGE))?;
    let path = folder.path().join(DATABASE);
    Connection::open(&path)?.execute_batch("CREATE TABLE unrelated (data TEXT);")?;
    let before = fs::read(&path)?;
    assert!(matches!(
        open(folder.path()),
        Err(OpenError::WrongApplication)
    ));
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

#[test]
fn externally_hard_linked_database_is_rejected() -> TestResult {
    let folder = tempdir()?;
    drop(ready(folder.path())?);
    fs::hard_link(folder.path().join(DATABASE), folder.path().join("alias"))?;
    assert!(matches!(open(folder.path()), Err(OpenError::UnsafeStorage)));
    Ok(())
}

#[cfg(unix)]
#[test]
fn linked_storage_and_journal_are_rejected() -> TestResult {
    use std::os::unix::fs::symlink;

    let folder = tempdir()?;
    let other = tempdir()?;
    let storage = folder.path().join(STORAGE);
    symlink(other.path(), &storage)?;
    assert!(matches!(open(folder.path()), Err(OpenError::UnsafeStorage)));
    assert!(fs::read_dir(other.path())?.next().is_none());

    fs::remove_file(&storage)?;
    drop(ready(folder.path())?);
    let target = other.path().join("journal");
    symlink(&target, storage.join("project.sqlite-journal"))?;
    assert!(matches!(open(folder.path()), Err(OpenError::UnsafeStorage)));
    assert!(!target.exists());
    Ok(())
}

#[test]
fn declining_upgrade_leaves_the_project_untouched() -> TestResult {
    let folder = upgrade_fixture()?;
    let path = folder.path().join(DATABASE);
    let before = fs::read(&path)?;
    let upgrade = pending(folder.path(), NEXT_SCHEMA)?;
    assert_eq!(upgrade.versions(), (1, 2));
    drop(upgrade);
    assert_eq!(fs::read(&path)?, before);
    assert!(!folder.path().join(BACKUP).exists());
    Ok(())
}

#[test]
fn confirmed_upgrade_copies_the_original_bytes_and_preserves_data() -> TestResult {
    let folder = upgrade_fixture()?;
    let path = folder.path().join(DATABASE);
    let original = fs::read(&path)?;
    let backup = folder.path().join(BACKUP);
    fs::write(&backup, b"previous backup")?;
    drop(pending(folder.path(), NEXT_SCHEMA)?.apply()?);
    assert_eq!(fs::read(&backup)?, b"previous backup");
    let new_backup = folder.path().join(".cuttingroom/project.sqlite.v1.bak2");
    assert_eq!(fs::read(&new_backup)?, original);
    let db = Connection::open(&path)?;
    assert_eq!(version(&db)?, 2);
    let row: (String, i32) = db.query_row("SELECT note, migrated FROM fixture", [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    assert_eq!(row, ("keep this".into(), 1));
    Ok(())
}

#[test]
fn failure_in_later_upgrade_rolls_back_every_step() -> TestResult {
    let folder = upgrade_fixture()?;
    let path = folder.path().join(DATABASE);
    let original = fs::read(&path)?;
    let schema = Schema {
        version: 3,
        upgrades: &[
            UPGRADE,
            Upgrade {
                from: 2,
                sql: "DELETE FROM fixture; ALTER TABLE missing RENAME TO other;",
            },
        ],
    };
    assert!(matches!(
        pending(folder.path(), schema)?.apply(),
        Err(OpenError::Database(_))
    ));

    assert_eq!(fs::read(&path)?, original);
    assert_eq!(fs::read(folder.path().join(BACKUP))?, original);
    drop(pending(folder.path(), NEXT_SCHEMA)?.apply()?);
    Ok(())
}

#[test]
fn unavailable_backup_prevents_upgrade() -> TestResult {
    let folder = upgrade_fixture()?;
    let last_backup = format!("{STORAGE}/project.sqlite.v1.bak{}", u64::MAX);
    fs::write(folder.path().join(last_backup), b"occupied")?;
    let path = folder.path().join(DATABASE);
    let before = fs::read(&path)?;
    assert!(matches!(
        pending(folder.path(), NEXT_SCHEMA)?.apply(),
        Err(OpenError::Io(_))
    ));
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

fn ready(path: &Path) -> TestResult<Project> {
    let OpenOutcome::Ready(project) = open(path)? else {
        return Err("expected a current project".into());
    };
    Ok(project)
}

fn open(path: &Path) -> Result<OpenOutcome, OpenError> {
    Project::open(path, || Ok(false))
}

fn pending(path: &Path, schema: Schema) -> TestResult<PendingUpgrade> {
    let OpenOutcome::NeedsUpgrade(upgrade) = Project::open_with_schema(path, schema, || Ok(false))?
    else {
        return Err("expected an upgrade prompt".into());
    };
    Ok(upgrade)
}

fn upgrade_fixture() -> TestResult<TempDir> {
    let folder = tempdir()?;
    drop(ready(folder.path())?);
    Connection::open(folder.path().join(DATABASE))?.execute_batch(
        "CREATE TABLE fixture (note TEXT NOT NULL);
         INSERT INTO fixture VALUES ('keep this');",
    )?;
    Ok(folder)
}

fn version(db: &Connection) -> rusqlite::Result<i32> {
    db.pragma_query_value(None, "user_version", |row| row.get(0))
}
