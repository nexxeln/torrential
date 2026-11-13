use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use torrential_core::{
    Error, ErrorCode, InfoHash, Kind, PieceMap, Release, Selection, TitleCard, TitleId, TitleLink, TorrentId,
};

use super::admission::Cap;
use super::book::{Home, Intent, JobRecord, Linked, Play};
use super::owner::deleter::{Identity, PlayName};
use super::search_port::SavedLink;

const MIGRATIONS: &[&str] = &[
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
    inode INTEGER NOT NULL
) STRICT;
"#,
];

#[derive(Clone, PartialEq, Debug)]
pub struct Tombstone {
    pub id: TorrentId,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub removed_at: i64,
}

pub const TOMBSTONE_SECS: i64 = 86_400;
pub const RESULTS_SECS: i64 = 7 * 86_400;
pub const METAINFO_SECS: i64 = RESULTS_SECS;
pub const METAINFO_BYTES: i64 = 64 << 20;

/// The deletion of a play's folder in the plays folder, on record until it is done.
#[derive(Clone, PartialEq, Debug)]
pub struct Recorded {
    pub key: i64,
    pub job: TorrentId,
    pub play: PlayName,
    /// The folder torrential made for the play, the only one this deletes.
    pub folder: Identity,
}

#[derive(Clone, PartialEq, Debug)]
pub struct StoredJob {
    pub record: JobRecord,
    pub metainfo: Option<Vec<u8>>,
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let existed = path.exists();
        let conn = Connection::open(path).map_err(|e| corrupt(path, &e.to_string()))?;
        let store = Self { conn, path: path.to_path_buf() };
        store.configure()?;
        if existed {
            let check: String =
                store.conn.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(|e| store.fail(e))?;
            if check != "ok" {
                return Err(corrupt(path, &check));
            }
        }
        store.migrate(MIGRATIONS)?;
        Ok(store)
    }

    fn configure(&self) -> Result<(), Error> {
        let mode: String =
            self.conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).map_err(|e| self.fail(e))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(corrupt(&self.path, &format!("SQLite could not switch to WAL mode and is in {mode} mode")));
        }
        self.conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;").map_err(|e| self.fail(e))?;
        self.conn.busy_timeout(std::time::Duration::from_secs(1)).map_err(|e| self.fail(e))
    }

    fn migrate(&self, migrations: &[&str]) -> Result<(), Error> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| self.fail(e))?;
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
            self.conn
                .execute("VACUUM INTO ?1", [backup.to_string_lossy()])
                .map_err(|e| corrupt(&self.path, &format!("backup before migrating failed: {e}")))?;
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| self.fail(e))?;
        for migration in &migrations[version..] {
            tx.execute_batch(migration).map_err(|e| corrupt(&self.path, &format!("migration failed: {e}")))?;
        }
        tx.pragma_update(None, "user_version", migrations.len() as i64).map_err(|e| self.fail(e))?;
        tx.commit().map_err(|e| self.fail(e))
    }

    fn fail(&self, e: rusqlite::Error) -> Error {
        sql_error(&self.path, e)
    }

    /// The database file, which the shelf opens a second connection to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write<T>(&mut self, f: impl FnOnce(&Transaction) -> rusqlite::Result<T>) -> Result<T, Error> {
        let Self { conn, path } = self;
        let tx = conn.transaction().map_err(|e| sql_error(path, e))?;
        let value = f(&tx).map_err(|e| sql_error(path, e))?;
        tx.commit().map_err(|e| sql_error(path, e))?;
        Ok(value)
    }

    pub fn jobs(&self) -> Result<Vec<StoredJob>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT j.id, j.info_hash, j.name, j.dir, j.selection, j.max_size, j.magnet, j.intent, j.finished_at, \
                 j.error, j.added_at, j.verified, j.piece_map, j.files_verified, m.bytes, j.max_size_from_setup, \
                 t.title, t.span, t.release, t.played, p.watched_at, p.by_hand, p.device, p.inode FROM jobs j \
                 LEFT JOIN metainfo m ON m.id = j.id \
                 LEFT JOIN job_titles t ON t.job = j.id \
                 LEFT JOIN plays p ON p.job = j.id \
                 ORDER BY j.added_at",
            )
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, Option<String>>(13)?,
                    r.get::<_, Option<Vec<u8>>>(14)?,
                    r.get::<_, bool>(15)?,
                    (
                        r.get::<_, Option<String>>(16)?,
                        r.get::<_, Option<String>>(17)?,
                        r.get::<_, Option<String>>(18)?,
                        r.get::<_, Option<bool>>(19)?,
                    ),
                    (
                        r.get::<_, Option<i64>>(20)?,
                        r.get::<_, Option<bool>>(21)?,
                        r.get::<_, Option<i64>>(22)?,
                        r.get::<_, Option<i64>>(23)?,
                    ),
                ))
            })
            .map_err(|e| self.fail(e))?;
        let mut jobs = Vec::new();
        for row in rows {
            let (
                id,
                hash,
                name,
                dir,
                selection,
                max_size,
                magnet,
                intent,
                finished_at,
                error,
                added_at,
                verified,
                piece_map,
                files_verified,
                bytes,
                from_setup,
                title,
                play,
            ) = row.map_err(|e| self.fail(e))?;
            let bad = |what: &str| corrupt(&self.path, &format!("job {id} has an unreadable {what}"));
            let intent = match (intent.as_str(), finished_at, error) {
                ("running", None, None) => Intent::Running,
                ("paused", None, None) => Intent::Paused,
                ("done", Some(finished_at), None) => Intent::Done { finished_at },
                ("failed", None, Some(error)) => {
                    Intent::Failed { error: serde_json::from_str(&error).map_err(|_| bad("error"))? }
                }
                _ => return Err(bad("intent")),
            };
            let record = JobRecord {
                id: id.parse().map_err(|_| bad("id"))?,
                info_hash: serde_json::from_str(&hash).map_err(|_| bad("info hash"))?,
                name,
                dir: PathBuf::from(std::ffi::OsString::from_vec(dir)),
                selection: serde_json::from_str::<Selection>(&selection).map_err(|_| bad("selection"))?,
                cap: Cap { bytes: max_size as u64, from_setup },
                magnet,
                added_at,
                intent,
                verified: verified as u64,
                piece_map: piece_map.map(|m| m.parse()).transpose().map_err(|_| bad("piece map"))?,
                files_verified: files_verified
                    .map(|f| serde_json::from_str(&f))
                    .transpose()
                    .map_err(|_| bad("file progress"))?,
                title: linked(title).map_err(|_| bad("title link"))?,
                home: match play {
                    (Some(watched_at), Some(by_hand), Some(device), Some(inode)) => {
                        Home::Plays(Play { watched_at, by_hand, folder: identity(device, inode) })
                    }
                    _ => Home::Folder,
                },
            };
            jobs.push(StoredJob { record, metainfo: bytes });
        }
        Ok(jobs)
    }

    pub fn insert(&mut self, record: &JobRecord, metainfo: Option<&[u8]>, now: i64) -> Result<(), Error> {
        let (intent, finished_at, error) = intent_columns(&record.intent);
        self.write(|tx| {
            tx.execute(
                "INSERT INTO jobs (id, info_hash, name, dir, selection, max_size, magnet, intent, finished_at, error, added_at, verified, \
                 piece_map, files_verified, max_size_from_setup) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    record.id.to_hex(),
                    json(&record.info_hash),
                    record.name,
                    record.dir.as_os_str().as_bytes(),
                    json(&record.selection),
                    record.cap.bytes as i64,
                    record.magnet,
                    intent,
                    finished_at,
                    error,
                    record.added_at,
                    record.verified as i64,
                    record.piece_map.map(|m| m.to_string()),
                    record.files_verified.as_ref().map(json),
                    record.cap.from_setup,
                ],
            )?;
            insert_aliases(tx, record.id, &record.info_hash)?;
            if let Some(bytes) = metainfo {
                upsert_metainfo(tx, record.id, bytes, now)?;
            }
            if let Some(linked) = &record.title {
                insert_link(tx, record.id, &linked.link, now)?;
            }
            if let Home::Plays(play) = record.home {
                tx.execute(
                    "INSERT INTO plays (job, watched_at, by_hand, device, inode) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        record.id.to_hex(),
                        play.watched_at,
                        play.by_hand,
                        play.folder.device as i64,
                        play.folder.inode as i64
                    ],
                )?;
            }
            tx.execute("DELETE FROM tombstones WHERE id = ?1", [record.id.to_hex()])?;
            Ok(())
        })
    }

    pub fn set_intent(&mut self, id: TorrentId, intent: &Intent) -> Result<(), Error> {
        let (intent, finished_at, error) = intent_columns(intent);
        self.write(|tx| {
            tx.execute(
                "UPDATE jobs SET intent = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
                params![id.to_hex(), intent, finished_at, error],
            )
            .map(drop)
        })
    }

    pub fn record_check(
        &mut self,
        id: TorrentId,
        intent: Option<&Intent>,
        verified: u64,
        piece_map: Option<PieceMap>,
        files: &[u64],
    ) -> Result<(), Error> {
        self.write(|tx| {
            if let Some(intent) = intent {
                let (intent, finished_at, error) = intent_columns(intent);
                tx.execute(
                    "UPDATE jobs SET intent = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
                    params![id.to_hex(), intent, finished_at, error],
                )?;
            }
            tx.execute(
                "UPDATE jobs SET verified = ?2, piece_map = ?3, files_verified = ?4 WHERE id = ?1",
                params![id.to_hex(), verified as i64, piece_map.map(|m| m.to_string()), json(&files)],
            )?;
            tx.execute("DELETE FROM resume WHERE job = ?1", [id.to_hex()]).map(drop)
        })
    }

    pub fn set_selection(&mut self, id: TorrentId, selection: &Selection) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("UPDATE jobs SET selection = ?2 WHERE id = ?1", params![id.to_hex(), json(selection)]).map(drop)
        })
    }

    pub fn set_cap(&mut self, id: TorrentId, cap: Cap) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE jobs SET max_size = ?2, max_size_from_setup = ?3 WHERE id = ?1",
                params![id.to_hex(), cap.bytes as i64, cap.from_setup],
            )
            .map(drop)
        })
    }

    pub fn learn(
        &mut self,
        id: TorrentId,
        name: &str,
        info_hash: &InfoHash,
        bytes: &[u8],
        now: i64,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE jobs SET name = ?2, info_hash = ?3 WHERE id = ?1",
                params![id.to_hex(), name, json(info_hash)],
            )?;
            insert_aliases(tx, id, info_hash)?;
            upsert_metainfo(tx, id, bytes, now)
        })
    }

    pub fn cache_metainfo(&mut self, id: TorrentId, bytes: &[u8], now: i64) -> Result<(), Error> {
        self.write(|tx| upsert_metainfo(tx, id, bytes, now))
    }

    pub fn metainfo(&self, id: TorrentId) -> Result<Option<Vec<u8>>, Error> {
        self.conn
            .query_row("SELECT bytes FROM metainfo WHERE id = ?1", [id.to_hex()], |r| r.get(0))
            .optional()
            .map_err(|e| self.fail(e))
    }

    pub fn resume(&self, id: TorrentId) -> Result<Option<Vec<u8>>, Error> {
        self.conn
            .query_row("SELECT bytes FROM resume WHERE job = ?1", [id.to_hex()], |r| r.get(0))
            .optional()
            .map_err(|e| self.fail(e))
    }

    pub fn save_resume(
        &mut self,
        id: TorrentId,
        bytes: &[u8],
        verified: u64,
        piece_map: Option<PieceMap>,
        files: Option<&[u64]>,
        now: i64,
    ) -> Result<(), Error> {
        self.write(|tx| save_resume(tx, id, bytes, verified, piece_map, files, now))
    }

    pub fn finish(&mut self, id: TorrentId, finished_at: i64, bytes: &[u8], verified: u64) -> Result<(), Error> {
        self.write(|tx| {
            save_resume(tx, id, bytes, verified, None, None, finished_at)?;
            tx.execute(
                "UPDATE jobs SET intent = 'done', finished_at = ?2, error = NULL WHERE id = ?1",
                params![id.to_hex(), finished_at],
            )
            .map(drop)
        })
    }

    /// Removes a download, and puts on record that its play's folder, the one with that identity,
    /// is to be deleted, both or neither. Answers the deletion's key.
    pub fn remove(&mut self, tombstone: &Tombstone, play: Option<(&PlayName, Identity)>) -> Result<Option<i64>, Error> {
        self.write(|tx| {
            let id = tombstone.id.to_hex();
            tx.execute("DELETE FROM jobs WHERE id = ?1", [&id])?;
            let key = match play {
                Some((play, folder)) => {
                    tx.execute(
                        "INSERT INTO deletions (job, play, device, inode) VALUES (?1, ?2, ?3, ?4)",
                        params![id, play.as_str(), folder.device as i64, folder.inode as i64],
                    )?;
                    Some(tx.last_insert_rowid())
                }
                None => None,
            };
            tx.execute(
                "INSERT OR REPLACE INTO tombstones (id, name, path, removed_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    id,
                    tombstone.name,
                    tombstone.path.as_ref().map(|p| p.as_os_str().as_bytes().to_vec()),
                    tombstone.removed_at
                ],
            )?;
            tx.execute("DELETE FROM tombstones WHERE removed_at < ?1", [tombstone.removed_at - TOMBSTONE_SECS])?;
            evict_metainfo(tx, tombstone.removed_at)?;
            Ok(key)
        })
    }

    /// The deletions on record that are not done, oldest first.
    pub fn deletions(&self) -> Result<Vec<Recorded>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT rowid, job, play, device, inode FROM deletions ORDER BY rowid")
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(|e| self.fail(e))?;
        let mut recorded = Vec::new();
        for row in rows {
            let (key, job, play, device, inode) = row.map_err(|e| self.fail(e))?;
            let bad = || corrupt(&self.path, &format!("deletion {key} is unreadable"));
            let play = PlayName::parse(&play).ok_or_else(bad)?;
            let job = job.parse().map_err(|_| bad())?;
            recorded.push(Recorded { key, job, play, folder: identity(device, inode) });
        }
        Ok(recorded)
    }

    pub fn deleted(&mut self, key: i64) -> Result<(), Error> {
        self.write(|tx| tx.execute("DELETE FROM deletions WHERE rowid = ?1", [key]).map(drop))
    }

    /// The plays folder the engine used last.
    pub fn plays_folder(&self) -> Result<Option<Identity>, Error> {
        self.conn
            .query_row("SELECT device, inode FROM plays_folder", [], |r| Ok(identity(r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(|e| self.fail(e))
    }

    /// Notes the plays folder the engine uses. When it used another before, the plays it had
    /// become downloads in a folder the person chose, and their folders are no longer deleted.
    pub fn use_plays_folder(&mut self, identity: Identity) -> Result<(), Error> {
        self.write(|tx| {
            let before: Option<(i64, i64)> = tx
                .query_row("SELECT device, inode FROM plays_folder", [], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if before.is_some_and(|before| before != (identity.device as i64, identity.inode as i64)) {
                tx.execute("DELETE FROM plays", [])?;
                tx.execute("DELETE FROM deletions", [])?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO plays_folder (one, device, inode) VALUES (1, ?1, ?2)",
                params![identity.device as i64, identity.inode as i64],
            )
            .map(drop)
        })
    }

    /// Notes that someone pressed play on a play, or streamed it, at `at`.
    pub fn watched(&mut self, id: TorrentId, at: i64) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("UPDATE plays SET watched_at = ?2 WHERE job = ?1", params![id.to_hex(), at]).map(drop)
        })
    }

    /// Notes that the person asked for a play by hand, so it never leaves the plays folder by itself.
    pub fn by_hand(&mut self, id: TorrentId) -> Result<(), Error> {
        self.write(|tx| tx.execute("UPDATE plays SET by_hand = 1 WHERE job = ?1", [id.to_hex()]).map(drop))
    }

    /// The titles saved to the library.
    pub fn saved_titles(&self) -> Result<std::collections::BTreeSet<TitleId>, Error> {
        let mut stmt = self.conn.prepare("SELECT title FROM library").map_err(|e| self.fail(e))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| self.fail(e))?;
        let mut saved = std::collections::BTreeSet::new();
        for row in rows {
            let title = row.map_err(|e| self.fail(e))?;
            saved
                .insert(title.parse().map_err(|_| corrupt(&self.path, &format!("saved title {title} is unreadable")))?);
        }
        Ok(saved)
    }

    /// Saves `card` in the library, keeping when it was first saved, so the library lists the
    /// title even when the catalog cannot be reached.
    pub fn save_title(&mut self, card: &TitleCard, now: i64) -> Result<(), Error> {
        let kind = match card.kind {
            Kind::Film => "film",
            Kind::Series => "series",
        };
        self.write(|tx| {
            tx.execute(
                "INSERT INTO library (title, kind, name, year, rating, saved_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (title) DO UPDATE SET kind = excluded.kind, name = excluded.name, \
                 year = excluded.year, rating = excluded.rating",
                params![card.id.as_str(), kind, card.name, card.year, card.rating, now],
            )
            .map(drop)
        })
    }

    pub fn unsave_title(&mut self, title: &TitleId) -> Result<(), Error> {
        self.write(|tx| tx.execute("DELETE FROM library WHERE title = ?1", [title.as_str()]).map(drop))
    }

    /// Links a download that already existed to the title a play joined it for.
    pub fn link(&mut self, id: TorrentId, link: &TitleLink, now: i64) -> Result<(), Error> {
        self.write(|tx| insert_link(tx, id, link, now))
    }

    pub fn played(&mut self, id: TorrentId) -> Result<(), Error> {
        self.write(|tx| tx.execute("UPDATE job_titles SET played = 1 WHERE job = ?1", [id.to_hex()]).map(drop))
    }

    pub fn remove_duplicate(&mut self, id: TorrentId) -> Result<(), Error> {
        self.write(|tx| tx.execute("DELETE FROM jobs WHERE id = ?1", [id.to_hex()]).map(drop))
    }

    pub fn tombstones(&self, now: i64) -> Result<Vec<Tombstone>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, removed_at FROM tombstones WHERE removed_at >= ?1")
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([now - TOMBSTONE_SECS], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<Vec<u8>>>(2)?, r.get(3)?))
            })
            .map_err(|e| self.fail(e))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, path, removed_at) = row.map_err(|e| self.fail(e))?;
            out.push(Tombstone {
                id: id.parse().map_err(|_| corrupt(&self.path, "a tombstone has an unreadable id"))?,
                name,
                path: path.map(|p| PathBuf::from(std::ffi::OsString::from_vec(p))),
                removed_at,
            });
        }
        Ok(out)
    }

    pub fn alias_owners(&self, info_hash: &InfoHash) -> Result<Vec<TorrentId>, Error> {
        let mut stmt =
            self.conn.prepare("SELECT DISTINCT job FROM aliases WHERE hash = ?1").map_err(|e| self.fail(e))?;
        let mut owners = Vec::new();
        for key in alias_keys(info_hash) {
            let rows = stmt.query_map([key], |r| r.get::<_, String>(0)).map_err(|e| self.fail(e))?;
            for row in rows {
                let id = row.map_err(|e| self.fail(e))?;
                let id = id.parse().map_err(|_| corrupt(&self.path, "an alias names an unreadable job id"))?;
                if !owners.contains(&id) {
                    owners.push(id);
                }
            }
        }
        Ok(owners)
    }

    pub fn record_results(&mut self, saved: &[(Release, Option<SavedLink>)], now: i64) -> Result<(), Error> {
        self.write(|tx| {
            for (release, link) in saved {
                tx.execute(
                    "INSERT OR REPLACE INTO results (id, release, seen_at, link) VALUES (?1, ?2, ?3, ?4)",
                    params![release.id.to_ref(), json(release), now, link.as_ref().map(json)],
                )?;
            }
            tx.execute("DELETE FROM results WHERE seen_at < ?1", [now - RESULTS_SECS]).map(drop)
        })
    }

    pub fn results(&self, now: i64) -> Result<Vec<(Release, Option<SavedLink>)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT release, link FROM results WHERE seen_at >= ?1 ORDER BY seen_at DESC")
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([now - RESULTS_SECS], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
            .map_err(|e| self.fail(e))?;
        let mut out = Vec::new();
        for row in rows {
            let (release, link) = row.map_err(|e| self.fail(e))?;
            let unreadable = |_| corrupt(&self.path, "a search result is unreadable");
            out.push((
                serde_json::from_str(&release).map_err(unreadable)?,
                link.map(|link| serde_json::from_str(&link)).transpose().map_err(unreadable)?,
            ));
        }
        Ok(out)
    }
}

pub(super) fn sql_error(path: &Path, e: rusqlite::Error) -> Error {
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

/// A file's identity as the store keeps it, in two signed columns.
fn identity(device: i64, inode: i64) -> Identity {
    Identity { device: device as u64, inode: inode as u64 }
}

fn corrupt(path: &Path, reason: &str) -> Error {
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

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("stored values serialize")
}

fn intent_columns(intent: &Intent) -> (&'static str, Option<i64>, Option<String>) {
    match intent {
        Intent::Running => ("running", None, None),
        Intent::Paused => ("paused", None, None),
        Intent::Done { finished_at } => ("done", Some(*finished_at), None),
        Intent::Failed { error } => ("failed", None, Some(json(error))),
    }
}

fn insert_link(tx: &Transaction, id: TorrentId, link: &TitleLink, now: i64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO job_titles (job, title, span, release, played, linked_at) \
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        params![
            id.to_hex(),
            link.title.as_str(),
            link.span.to_string(),
            link.release.as_ref().map(|r| r.to_ref()),
            now
        ],
    )
    .map(drop)
}

type LinkColumns = (Option<String>, Option<String>, Option<String>, Option<bool>);

fn linked(columns: LinkColumns) -> Result<Option<Linked>, ()> {
    let (Some(title), Some(span), release, Some(played)) = columns else { return Ok(None) };
    let release = release.map(|r| serde_json::from_value(serde_json::Value::String(r))).transpose().map_err(drop)?;
    let link = TitleLink { title: title.parse().map_err(drop)?, span: span.parse().map_err(drop)?, release };
    Ok(Some(Linked { link, played }))
}

fn alias_keys(info_hash: &InfoHash) -> Vec<String> {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    [info_hash.v1().map(|h| hex(&h)), info_hash.v2().map(|h| hex(&h))].into_iter().flatten().collect()
}

fn insert_aliases(tx: &Transaction, id: TorrentId, info_hash: &InfoHash) -> rusqlite::Result<()> {
    for hash in alias_keys(info_hash) {
        tx.execute("INSERT OR IGNORE INTO aliases (hash, job) VALUES (?1, ?2)", [hash, id.to_hex()])?;
    }
    Ok(())
}

fn upsert_metainfo(tx: &Transaction, id: TorrentId, bytes: &[u8], now: i64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO metainfo (id, bytes, saved_at) VALUES (?1, ?2, ?3)",
        params![id.to_hex(), bytes, now],
    )?;
    evict_metainfo(tx, now)
}

fn evict_metainfo(tx: &Transaction, now: i64) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM metainfo WHERE id NOT IN (SELECT id FROM jobs) AND saved_at < ?1", [now - METAINFO_SECS])?;
    tx.execute(
        "DELETE FROM metainfo WHERE id IN (\
           SELECT id FROM (\
             SELECT id, sum(length(bytes)) OVER (ORDER BY saved_at DESC, id) AS newer \
             FROM metainfo WHERE id NOT IN (SELECT id FROM jobs)\
           ) WHERE newer > ?1\
         )",
        [METAINFO_BYTES],
    )
    .map(drop)
}

fn save_resume(
    tx: &Transaction,
    id: TorrentId,
    bytes: &[u8],
    verified: u64,
    piece_map: Option<PieceMap>,
    files: Option<&[u64]>,
    now: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO resume (job, bytes, saved_at) VALUES (?1, ?2, ?3)",
        params![id.to_hex(), bytes, now],
    )?;
    let piece_map = piece_map.map(|m| m.to_string());
    tx.execute(
        "UPDATE jobs SET verified = ?2, piece_map = ?3, files_verified = ?4 WHERE id = ?1",
        params![id.to_hex(), verified as i64, piece_map, files.map(|f| json(&f))],
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::ReleaseId;

    fn record(n: u8) -> JobRecord {
        JobRecord {
            id: TorrentId::from_bytes([n; 20]),
            info_hash: InfoHash::Hybrid { v1: [n; 20], v2: [n + 1; 32] },
            name: Some(format!("job {n}")),
            dir: PathBuf::from("/dl"),
            selection: Selection::Patterns(vec!["*.mkv".into()]),
            cap: Cap { bytes: 7, from_setup: true },
            magnet: Some("magnet:?xt=urn:btih:x".into()),
            added_at: 100,
            intent: Intent::Running,
            verified: 0,
            piece_map: None,
            files_verified: None,
            title: None,
            home: Home::Folder,
        }
    }

    #[test]
    fn jobs_round_trip_through_every_intent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        store.insert(&record(1), Some(b"torrent"), 100).unwrap();
        store
            .set_intent(
                TorrentId::from_bytes([1; 20]),
                &Intent::Failed { error: Error::new(ErrorCode::DiskFull, "full") },
            )
            .unwrap();
        store.insert(&record(3), None, 100).unwrap();
        store.finish(TorrentId::from_bytes([3; 20]), 200, b"resume", 42).unwrap();
        let map: PieceMap = format!("{}{}", "8".repeat(32), "-".repeat(32)).parse().unwrap();
        store.insert(&record(5), None, 100).unwrap();
        store.save_resume(TorrentId::from_bytes([5; 20]), b"resume", 9, Some(map), Some(&[4, 5]), 150).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let jobs = store.jobs().unwrap();
        assert_eq!(jobs[0].record.intent, Intent::Failed { error: Error::new(ErrorCode::DiskFull, "full") });
        assert_eq!(jobs[0].metainfo.as_deref(), Some(&b"torrent"[..]));
        assert_eq!(jobs[1].record, JobRecord { intent: Intent::Done { finished_at: 200 }, verified: 42, ..record(3) });
        let saved = JobRecord { verified: 9, piece_map: Some(map), files_verified: Some(vec![4, 5]), ..record(5) };
        assert_eq!(jobs[2].record, saved);
        assert_eq!(store.resume(TorrentId::from_bytes([3; 20])).unwrap().as_deref(), Some(&b"resume"[..]));
    }

    #[test]
    fn a_title_link_keeps_whether_it_played_and_goes_with_its_job() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let link = TitleLink {
            title: "tt9999901".parse().unwrap(),
            span: "s1".parse().unwrap(),
            release: Some(ReleaseId::Torrent(TorrentId::from_bytes([1; 20]))),
        };
        let made = Linked { link: link.clone(), played: false };
        store.insert(&JobRecord { title: Some(made.clone()), ..record(1) }, None, 100).unwrap();
        store.insert(&record(3), None, 100).unwrap();
        store.link(TorrentId::from_bytes([3; 20]), &link, 110).unwrap();
        store.played(TorrentId::from_bytes([3; 20])).unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let titles: Vec<Option<Linked>> = store.jobs().unwrap().into_iter().map(|j| j.record.title).collect();
        assert_eq!(titles, [Some(made), Some(Linked { link, played: true })]);

        let gone = Tombstone { id: TorrentId::from_bytes([1; 20]), name: None, path: None, removed_at: 120 };
        store.remove(&gone, None).unwrap();
        let left: Vec<TorrentId> = store.jobs().unwrap().into_iter().map(|j| j.record.id).collect();
        assert_eq!(left, [TorrentId::from_bytes([3; 20])]);
        let links: i64 = store.conn.query_row("SELECT count(*) FROM job_titles", [], |r| r.get(0)).unwrap();
        assert_eq!(links, 1);
    }

    #[test]
    fn a_failed_check_replaces_the_progress_and_drops_the_resume_data_it_contradicts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        let id = TorrentId::from_bytes([6; 20]);
        store.insert(&record(6), None, 100).unwrap();
        store.finish(id, 200, b"every piece", 42).unwrap();
        let failed = Intent::Failed { error: Error::new(ErrorCode::VerificationFailed, "1 damaged piece") };
        store.record_check(id, Some(&failed), 30, None, &[20, 10]).unwrap();
        let jobs = store.jobs().unwrap();
        let checked = JobRecord { intent: failed, verified: 30, files_verified: Some(vec![20, 10]), ..record(6) };
        assert_eq!(jobs[0].record, checked);
        assert_eq!(store.resume(id).unwrap(), None);
    }

    #[test]
    fn both_hashes_of_a_hybrid_resolve_to_one_job() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        store.insert(&record(1), None, 100).unwrap();
        let id = TorrentId::from_bytes([1; 20]);
        assert_eq!(store.alias_owners(&InfoHash::V1([1; 20])).unwrap(), [id]);
        assert_eq!(store.alias_owners(&InfoHash::V2([2; 32])).unwrap(), [id]);
        assert_eq!(store.alias_owners(&InfoHash::V1([3; 20])).unwrap(), []);
    }

    #[test]
    fn removal_leaves_a_short_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        store.insert(&record(1), None, 100).unwrap();
        let tomb = Tombstone {
            id: TorrentId::from_bytes([1; 20]),
            name: Some("job 1".into()),
            path: Some("/dl/job 1".into()),
            removed_at: 1_000,
        };
        store.remove(&tomb, None).unwrap();
        assert!(store.jobs().unwrap().is_empty());
        assert_eq!(store.tombstones(1_000 + 60).unwrap(), [tomb]);
        assert!(store.tombstones(1_000 + TOMBSTONE_SECS + 1).unwrap().is_empty());
    }

    #[test]
    fn a_play_keeps_its_place_and_its_deletion_outlives_the_engine_until_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let folder = Identity { device: u64::MAX, inode: 7 };
        let play = JobRecord {
            dir: "/th/data/plays/0101010101010101-0".into(),
            home: Home::Plays(Play { watched_at: 100, by_hand: false, folder }),
            ..record(1)
        };
        store.insert(&play, None, 100).unwrap();
        store.insert(&record(2), None, 100).unwrap();
        store.watched(play.id, 250).unwrap();
        store.by_hand(play.id).unwrap();
        let homes: Vec<Home> = store.jobs().unwrap().into_iter().map(|j| j.record.home).collect();
        assert_eq!(homes, [Home::Plays(Play { watched_at: 250, by_hand: true, folder }), Home::Folder]);
        let tomb = Tombstone { id: play.id, name: None, path: None, removed_at: 300 };
        let name = PlayName::parse("0101010101010101-0").unwrap();
        let key = store.remove(&tomb, Some((&name, folder))).unwrap().unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.deletions().unwrap(), [Recorded { key, job: play.id, play: name, folder }]);
        assert_eq!(store.jobs().unwrap().len(), 1);
        store.deleted(key).unwrap();
        assert_eq!(store.deletions().unwrap(), []);
    }

    #[test]
    fn a_plays_folder_other_than_the_one_used_before_turns_its_plays_into_downloads_in_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        let first = Identity { device: 1, inode: 2 };
        assert_eq!(store.plays_folder().unwrap(), None);
        store.use_plays_folder(first).unwrap();
        let folder = Identity { device: 1, inode: 9 };
        let play = JobRecord { home: Home::Plays(Play { watched_at: 100, by_hand: false, folder }), ..record(1) };
        store.insert(&play, None, 100).unwrap();
        store.use_plays_folder(first).unwrap();
        assert_eq!(store.jobs().unwrap()[0].record.home, play.home, "the same folder keeps its plays");
        let moved = Identity { device: 1, inode: 3 };
        store.use_plays_folder(moved).unwrap();
        assert_eq!((store.plays_folder().unwrap(), store.jobs().unwrap()[0].record.home), (Some(moved), Home::Folder));
    }

    #[test]
    fn cached_file_lists_expire_and_fit_a_budget_but_a_downloads_stays() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        let id = TorrentId::from_bytes;
        let largest = vec![7; 16 << 20];
        let t0 = 1_000_000;
        let later = t0 + METAINFO_SECS + 1;

        store.insert(&record(1), Some(b"tracked"), t0).unwrap();
        store.insert(&record(3), Some(b"removed"), t0).unwrap();
        store.cache_metainfo(id([2; 20]), b"old", t0).unwrap();
        store.cache_metainfo(id([4; 20]), b"recent", later).unwrap();
        for n in 5..10 {
            store.cache_metainfo(id([n; 20]), &largest, later + i64::from(n)).unwrap();
        }
        store.remove(&Tombstone { id: id([3; 20]), name: None, path: None, removed_at: later + 10 }, None).unwrap();

        let kept: Vec<u8> = (1..10).filter(|n| store.metainfo(id([*n; 20])).unwrap().is_some()).collect();
        assert_eq!(kept, [1, 6, 7, 8, 9]);
        assert_eq!(store.jobs().unwrap()[0].metainfo.as_deref(), Some(&b"tracked"[..]));
    }

    #[test]
    fn damaged_state_is_reported_never_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        std::fs::write(&path, b"this is not a database, but it is somebody's data").unwrap();
        let err = Store::open(&path).err().unwrap();
        assert_eq!(err.code, ErrorCode::StateCorrupt);
        assert_eq!(std::fs::read(&path).unwrap(), b"this is not a database, but it is somebody's data");
    }

    #[test]
    fn migrating_backs_up_the_old_database_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        store.insert(&record(1), None, 100).unwrap();
        store.migrate(&[MIGRATIONS, &["ALTER TABLE jobs ADD COLUMN note TEXT"]].concat()).unwrap();
        let shipped = MIGRATIONS.len() as i64;
        let version: i64 = store.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, shipped + 1);
        let backup = Connection::open(dir.path().join(format!("state.v{shipped}.bak"))).unwrap();
        let rows: i64 = backup.query_row("SELECT count(*) FROM jobs", [], |r| r.get(0)).unwrap();
        let backup_version: i64 = backup.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!((rows, backup_version), (1, shipped));
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Store::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        let known = MIGRATIONS.len();
        let err = Store::open(&path).err().unwrap();
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
        let err = Store::open(&path).err().unwrap();
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
