use std::{cell::Cell, error::Error, fs, path::Path};

use rusqlite::Connection;
use tempfile::{TempDir, tempdir};

use crate::{
    OpenError, OpenOutcome, PendingUpgrade, Project,
    schema::{APPLICATION_ID, Schema, Upgrade},
};

const STORAGE: &str = ".cuttingroom";
const DATABASE: &str = ".cuttingroom/project.sqlite";
const BACKUP: &str = ".cuttingroom/project.sqlite.v1.bak1";
const LEGACY_SCHEMA: Schema = Schema {
    version: 1,
    upgrades: &[],
};
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
fn upgrades_version_one_and_keeps_photo_identity_when_the_folder_moves() -> TestResult {
    let folder = tempdir()?;
    let original = folder.path().join("photos #1");
    fs::create_dir(&original)?;
    fs::write(original.join("original.dng"), b"photo")?;
    drop(legacy_project(&original)?);

    let db = Connection::open(original.join(DATABASE))?;
    let application_id: i32 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let tables: i64 = db.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
    assert_eq!(
        (application_id, version(&db)?, tables),
        (APPLICATION_ID, 1, 0)
    );
    drop(db);

    let before = fs::read(original.join(DATABASE))?;
    let upgrade = pending(&original, crate::schema::SCHEMA)?;
    assert_eq!(upgrade.versions(), (1, 2));
    drop(upgrade);
    assert_eq!(fs::read(original.join(DATABASE))?, before);
    assert!(!original.join(BACKUP).exists());

    let project = pending(&original, crate::schema::SCHEMA)?.apply()?;
    assert_eq!(fs::read(original.join(BACKUP))?, before);
    assert_eq!(
        project.sync_photographs(10, || false, |_| {})?.paths,
        ["original.dng"]
    );
    drop(project);

    let db = Connection::open(original.join(DATABASE))?;
    assert_eq!(version(&db)?, 2);
    let id = photo_record(&db, &["original.dng"])?.0;
    assert_eq!(uuid::Uuid::parse_str(&id)?.get_version_num(), 4);
    drop(db);

    let moved = folder.path().join("renamed photos");
    fs::rename(&original, &moved)?;
    let project = ready(&moved)?;
    assert_eq!(project.root(), moved.canonicalize()?);
    assert_eq!(
        project.sync_photographs(10, || false, |_| {})?.paths,
        ["original.dng"]
    );
    let db = Connection::open(moved.join(DATABASE))?;
    assert_eq!(photo_record(&db, &["original.dng"])?, (id, true));
    assert_eq!(fs::read(moved.join("original.dng"))?, b"photo");
    Ok(())
}

#[test]
fn rescans_photographs_and_retains_missing_records() -> TestResult {
    let folder = tempdir()?;
    let root = folder.path();
    let nested_path = Path::new("day 2").join("é.JPG");
    fs::create_dir(root.join("day 2"))?;
    fs::write(root.join(&nested_path), b"jpeg")?;
    fs::write(root.join("gone.dng"), b"raw")?;
    fs::write(root.join("notes.txt"), b"notes")?;
    for excluded in ["nested/.cuttingroom", ".Trash"] {
        fs::create_dir_all(root.join(excluded))?;
    }
    fs::write(root.join("nested/other.jpg"), b"nested project")?;
    fs::write(root.join(".Trash/removed.jpg"), b"trashed photo")?;
    #[cfg(unix)]
    let outside = tempdir()?;
    #[cfg(unix)]
    {
        let photo = outside.path().join("outside.jpg");
        fs::write(&photo, b"outside")?;
        std::os::unix::fs::symlink(&photo, root.join("linked.jpg"))?;
        std::os::unix::fs::symlink(outside.path(), root.join("linked directory"))?;
    }

    let project = ready(root)?;
    fs::write(root.join(STORAGE).join("preview.png"), b"cache")?;
    let mut found = 0;
    let listing = project.sync_photographs(1, || false, |count| found = count)?;
    assert_eq!(found, listing.total);
    assert_eq!(listing.total, 2);
    assert_eq!(listing.paths, [nested_path.to_string_lossy()]);
    drop(project);

    let db = Connection::open(root.join(DATABASE))?;
    let nested_display = nested_path.to_str().ok_or("expected a Unicode path")?;
    let nested_name = ["day 2", "é.JPG"];
    let (nested_id, nested_present) = photo_record(&db, &nested_name)?;
    let (removed_id, removed_present) = photo_record(&db, &["gone.dng"])?;
    assert!(nested_present && removed_present);

    fs::remove_file(root.join("gone.dng"))?;
    fs::write(root.join(&nested_path), b"a changed jpeg")?;
    fs::write(root.join("added.HEIC"), b"new photo")?;
    let listing = ready(root)?.sync_photographs(10, || false, |_| {})?;
    assert_eq!(listing.total, 2);
    assert_eq!(listing.paths, ["added.HEIC", nested_display]);
    assert_eq!(photo_record(&db, &nested_name)?, (nested_id, true));
    assert_eq!(
        photo_record(&db, &["gone.dng"])?,
        (removed_id.clone(), false)
    );
    assert!(photo_record(&db, &["added.HEIC"])?.1);
    assert_eq!(fs::read(root.join(&nested_path))?, b"a changed jpeg");

    fs::write(root.join("gone.dng"), b"returned photo")?;
    ready(root)?.sync_photographs(0, || false, |_| {})?;
    assert_eq!(photo_record(&db, &["gone.dng"])?, (removed_id, true));
    Ok(())
}

#[test]
fn cancelled_rescan_preserves_the_previous_inventory() -> TestResult {
    let folder = tempdir()?;
    let root = folder.path();
    let saved = root.join("saved.jpg");
    fs::write(&saved, b"saved")?;
    let project = ready(root)?;
    project.sync_photographs(10, || false, |_| {})?;
    let before = fs::read(root.join(DATABASE))?;
    fs::remove_file(saved)?;
    for number in 0..300 {
        fs::write(root.join(format!("{number}.jpg")), b"new")?;
    }

    let found = Cell::new(0);
    let result = project.sync_photographs(10, || found.get() >= 280, |count| found.set(count));
    assert!(matches!(result, Err(OpenError::ScanCancelled)));
    assert_eq!(fs::read(root.join(DATABASE))?, before);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn retains_distinct_non_unicode_names() -> TestResult {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    use base64::{Engine, engine::general_purpose::STANDARD};

    let folder = tempdir()?;
    for name in [b"IMG_\xfe.jpg", b"IMG_\xff.jpg"] {
        fs::write(folder.path().join(OsStr::from_bytes(name)), b"native")?;
    }

    let listing = ready(folder.path())?.sync_photographs(10, || false, |_| {})?;
    assert_eq!(listing.paths, ["IMG_�.jpg", "IMG_�.jpg"]);
    let db = Connection::open(folder.path().join(DATABASE))?;
    let mut query = db.prepare("SELECT path FROM photos ORDER BY path")?;
    let payloads = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (payload, name) in payloads.iter().zip([b"IMG_\xfe.jpg", b"IMG_\xff.jpg"]) {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(payload)?,
            serde_json::json!({ "encoding": "unix-bytes", "components_base64": [STANDARD.encode(name)] })
        );
    }
    assert_eq!(payloads.len(), 2);
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
    let first = legacy_project(root)?;
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
    Connection::open(&path)?.pragma_update(None, "user_version", 3)?;
    let before = fs::read(&path)?;
    assert!(matches!(
        open(folder.path()),
        Err(OpenError::UnsupportedSchema {
            found: 3,
            supported: 2
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
    drop(legacy_project(folder.path())?);
    Connection::open(folder.path().join(DATABASE))?.execute_batch(
        "CREATE TABLE fixture (note TEXT NOT NULL);
         INSERT INTO fixture VALUES ('keep this');",
    )?;
    Ok(folder)
}

fn legacy_project(path: &Path) -> TestResult<Project> {
    let OpenOutcome::Ready(project) = Project::open_with_schema(path, LEGACY_SCHEMA, || Ok(false))?
    else {
        return Err("expected a version 1 project".into());
    };
    Ok(project)
}

fn photo_record(db: &Connection, components: &[&str]) -> rusqlite::Result<(String, bool)> {
    let path = serde_json::json!({ "encoding": "unicode", "components": components });
    db.query_row(
        "SELECT id, present FROM photos WHERE path = ?",
        [path.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

fn version(db: &Connection) -> rusqlite::Result<i32> {
    db.pragma_query_value(None, "user_version", |row| row.get(0))
}
