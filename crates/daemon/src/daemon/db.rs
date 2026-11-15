//! The engine's SQLite database: its schema, how it migrates, and the one way to open it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use torrential_core::{Error, ErrorCode};

pub(super) const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    info_hash TEXT NOT NULL,
    name TEXT,
    dir BLOB NOT NULL,
    selection TEXT NOT NULL,
    max_size INTEGER NOT NULL,
    magnet TEXT,
    intent TEXT NOT NULL CHECK (intent IN ('running', 'paused', 'done', 'failed')),
    finished_at INTEGER,
    error TEXT,
    added_at INTEGER NOT NULL,
    verified INTEGER NOT NULL DEFAULT 0,
    CHECK ((intent = 'done') = (finished_at IS NOT NULL)),
    CHECK ((intent = 'failed') = (error IS NOT NULL))
) STRICT;
CREATE TABLE aliases (
    hash TEXT PRIMARY KEY,
    job TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE
) STRICT;
CREATE TABLE metainfo (
    id TEXT PRIMARY KEY,
    bytes BLOB NOT NULL,
    saved_at INTEGER NOT NULL
) STRICT;
CREATE TABLE resume (
    job TEXT PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
    bytes BLOB NOT NULL,
    saved_at INTEGER NOT NULL
) STRICT;
CREATE TABLE tombstones (
    id TEXT PRIMARY KEY,
    name TEXT,
    path BLOB,
    removed_at INTEGER NOT NULL
) STRICT;
CREATE TABLE results (
    id TEXT PRIMARY KEY,
    release TEXT NOT NULL,
    seen_at INTEGER NOT NULL
) STRICT;
"#,
    "ALTER TABLE jobs ADD COLUMN piece_map TEXT;",
    "ALTER TABLE jobs ADD COLUMN files_verified TEXT;",
    "ALTER TABLE jobs ADD COLUMN max_size_from_setup INTEGER NOT NULL DEFAULT 0;",
    "ALTER TABLE results ADD COLUMN link TEXT;",
    r#"
CREATE TABLE job_titles (
    job TEXT PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    span TEXT NOT NULL,
    release TEXT,
    played INTEGER NOT NULL DEFAULT 0 CHECK (played IN (0, 1)),
    linked_at INTEGER NOT NULL
) STRICT;
CREATE INDEX job_titles_by_title ON job_titles(title);
CREATE TABLE watched (
    video TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    position REAL NOT NULL CHECK (position >= 0),
    duration REAL CHECK (duration IS NULL OR duration > 0),
    release TEXT,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX watched_by_title ON watched(title, updated_at);
CREATE TABLE library (
    title TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('film', 'series')),
    name TEXT NOT NULL,
    year INTEGER,
    rating REAL,
    saved_at INTEGER NOT NULL
) STRICT;
"#,
    r#"
CREATE TABLE devices (
    id TEXT PRIMARY KEY,
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    name TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    seen_at INTEGER NOT NULL
) STRICT;
"#,
    r#"
CREATE TABLE plays (
    job TEXT PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
    watched_at INTEGER NOT NULL,
    by_hand INTEGER NOT NULL DEFAULT 0 CHECK (by_hand IN (0, 1)),
    device INTEGER NOT NULL,
    inode INTEGER NOT NULL
) STRICT;
CREATE TABLE plays_folder (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    device INTEGER NOT NULL,
    inode INTEGER NOT NULL
) STRICT;
CREATE TABLE deletions (
    job TEXT NOT NULL,
    play TEXT NOT NULL,
    device INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    bytes INTEGER NOT NULL
) STRICT;
"#,
];

/// The engine's database file, checked and migrated to this version's schema. The store, the
/// shelf and the devices each open their own connection to it, so none can reach it unmigrated.
pub struct Db {
    path: PathBuf,
}

/// Opens the database at `path`, making it if it is not there, checks one that was, and migrates
/// it. Before migrating, it keeps the old database as `state.v{N}.bak`.
pub fn connect(path: &Path) -> Result<Db, Error> {
    connect_with(path, MIGRATIONS)
}

pub(super) fn connect_with(path: &Path, migrations: &[&str]) -> Result<Db, Error> {
    let existed = path.exists();
    let db = Db { path: path.to_path_buf() };
    let conn = Connection::open(path).map_err(|e| corrupt(path, &e.to_string()))?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).map_err(|e| db.fail(e))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(corrupt(path, &format!("SQLite could not switch to WAL mode and is in {mode} mode")));
    }
    db.configure(&conn)?;
    if existed {
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(|e| db.fail(e))?;
        if check != "ok" {
            return Err(corrupt(path, &check));
        }
    }
    db.migrate(&conn, migrations)?;
    Ok(db)
}

impl Db {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A new connection, set up like every other.
    pub fn open(&self) -> Result<Connection, Error> {
        let conn = Connection::open(&self.path).map_err(|e| self.fail(e))?;
        self.configure(&conn)?;
        Ok(conn)
    }

    pub fn fail(&self, e: rusqlite::Error) -> Error {
        sql_error(&self.path, e)
    }

    fn configure(&self, conn: &Connection) -> Result<(), Error> {
        conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;").map_err(|e| self.fail(e))?;
        conn.busy_timeout(Duration::from_secs(1)).map_err(|e| self.fail(e))
    }

    fn migrate(&self, conn: &Connection, migrations: &[&str]) -> Result<(), Error> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| self.fail(e))?;
        let version = usize::try_from(version)
            .map_err(|_| corrupt(&self.path, &format!("schema version {version} is not valid")))?;
        if version > migrations.len() {
            return Err(newer(&self.path, version, migrations.len()));
        }
        if version == migrations.len() {
            return Ok(());
        }
        if version > 0 {
            let backup = self.path.with_extension(format!("v{version}.bak"));
            let _ = std::fs::remove_file(&backup);
            conn.execute("VACUUM INTO ?1", [backup.to_string_lossy()])
                .map_err(|e| corrupt(&self.path, &format!("backup before migrating failed: {e}")))?;
        }
        let tx = conn.unchecked_transaction().map_err(|e| self.fail(e))?;
        for migration in &migrations[version..] {
            tx.execute_batch(migration).map_err(|e| corrupt(&self.path, &format!("migration failed: {e}")))?;
        }
        tx.pragma_update(None, "user_version", migrations.len() as i64).map_err(|e| self.fail(e))?;
        tx.commit().map_err(|e| self.fail(e))
    }
}

pub fn sql_error(path: &Path, e: rusqlite::Error) -> Error {
    use rusqlite::ErrorCode as Sql;
    match e.sqlite_error_code() {
        Some(Sql::DatabaseCorrupt | Sql::NotADatabase) => corrupt(path, &e.to_string()),
        Some(Sql::DiskFull) => {
            Error::new(ErrorCode::StorageError, format!("the disk containing {} is full", path.display()))
                .with_hint("free some space on that disk")
        }
        _ => Error::new(ErrorCode::StateCorrupt, format!("{}: {e}", path.display()))
            .with_hint(format!("see {}", path.with_file_name("engine.log").display())),
    }
}

pub fn corrupt(path: &Path, reason: &str) -> Error {
    Error::new(ErrorCode::StateCorrupt, format!("the state database {} is damaged: {reason}", path.display()))
        .with_hint(format!(
            "the state is never reset automatically; move {} aside to start fresh, or restore a backup",
            path.display()
        ))
}

/// A newer torrential migrated the database past `known`. It kept the database it migrated from
/// as `state.v{known}.bak`, the state this version last wrote.
fn newer(path: &Path, version: usize, known: usize) -> Error {
    let backup = path.with_extension(format!("v{known}.bak"));
    let error = Error::new(
        ErrorCode::StateCorrupt,
        format!(
            "the state database {} is from a newer torrential (schema version {version}; this version reads up to {known})",
            path.display()
        ),
    );
    if backup.is_file() {
        error.with_hint(format!(
            "install the newer torrential again, or stop it and replace {} with {}, the state from before the update",
            path.display(),
            backup.display()
        ))
    } else {
        error.with_hint("install the newer torrential again")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_state_is_reported_never_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        std::fs::write(&path, b"this is not a database, but it is somebody's data").unwrap();
        let err = connect(&path).err().unwrap();
        assert_eq!(err.code, ErrorCode::StateCorrupt);
        assert_eq!(std::fs::read(&path).unwrap(), b"this is not a database, but it is somebody's data");
    }

    #[test]
    fn migrating_backs_up_the_old_database_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = connect(&path).unwrap();
        db.open().unwrap().execute("INSERT INTO tombstones (id, removed_at) VALUES ('a', 100)", []).unwrap();
        drop(connect_with(&path, &[MIGRATIONS, &["ALTER TABLE jobs ADD COLUMN note TEXT"]].concat()).unwrap());
        let shipped = MIGRATIONS.len() as i64;
        let version: i64 = db.open().unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, shipped + 1);
        let backup = Connection::open(dir.path().join(format!("state.v{shipped}.bak"))).unwrap();
        let rows: i64 = backup.query_row("SELECT count(*) FROM tombstones", [], |r| r.get(0)).unwrap();
        let backup_version: i64 = backup.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!((rows, backup_version), (1, shipped));
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(connect(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        let known = MIGRATIONS.len();
        let err = connect(&path).err().unwrap();
        assert_eq!(err.code, ErrorCode::StateCorrupt);
        assert_eq!(
            err.message,
            format!(
                "the state database {} is from a newer torrential (schema version 99; this version reads up to {known})",
                path.display()
            )
        );
        assert_eq!(err.hint.as_deref(), Some("install the newer torrential again"));

        let backup = dir.path().join(format!("state.v{known}.bak"));
        std::fs::write(&backup, b"the state before the update").unwrap();
        let err = connect(&path).err().unwrap();
        assert_eq!(
            err.hint,
            Some(format!(
                "install the newer torrential again, or stop it and replace {} with {}, the state from before the update",
                path.display(),
                backup.display()
            ))
        );
    }
}
