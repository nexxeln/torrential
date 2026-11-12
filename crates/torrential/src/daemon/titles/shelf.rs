//! Where a person stopped each film and episode, and the titles they saved.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};
use torrential_core::{Error, ErrorCode, Kind, ReleaseId, TitleCard, TitleId, VideoRef, Watched};

use super::super::store::sql_error;

const WATCHED: &str = "SELECT video, position, duration, release, updated_at FROM watched";

/// Watch positions and the library, on a connection of its own to the engine's database, which
/// `Store::open` has migrated. It writes only `watched` and `library`, so it never contends with
/// the owner for a row.
pub struct Shelf {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Shelf {
    pub fn open(path: &Path) -> Result<Shelf, Error> {
        let conn = Connection::open(path).map_err(|e| sql_error(path, e))?;
        conn.execute_batch("PRAGMA synchronous = FULL;").map_err(|e| sql_error(path, e))?;
        conn.busy_timeout(Duration::from_secs(1)).map_err(|e| sql_error(path, e))?;
        Ok(Shelf { conn: Mutex::new(conn), path: path.to_path_buf() })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fail(&self, e: rusqlite::Error) -> Error {
        sql_error(&self.path, e)
    }

    /// Saves where a film or episode stopped. Saving the same video again replaces its row, and
    /// a save that names no release keeps the one saved before.
    pub fn watch(&self, watched: &Watched) -> Result<(), Error> {
        let Watched { video, position, duration, release, at } = watched;
        self.conn()
            .execute(
                "INSERT INTO watched (video, title, position, duration, release, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (video) DO UPDATE SET position = excluded.position, duration = excluded.duration, \
                 release = coalesce(excluded.release, release), updated_at = excluded.updated_at",
                params![
                    video.to_string(),
                    video.title.as_str(),
                    position,
                    duration,
                    release.as_ref().map(ReleaseId::to_ref),
                    at
                ],
            )
            .map(drop)
            .map_err(|e| self.fail(e))
    }

    /// Every film or episode of `title` with a position, the most recently watched last. Two
    /// saved in the same second keep the order they were first saved in, so the later one counts
    /// as the latest.
    pub fn watched(&self, title: &TitleId) -> Result<Vec<Watched>, Error> {
        let sql = format!("{WATCHED} WHERE title = ?1 ORDER BY updated_at, rowid");
        self.read(&sql, params![title.as_str()])
    }

    /// The most recently watched film or episode of each title, the most recent first.
    pub fn recent(&self, most: usize) -> Result<Vec<Watched>, Error> {
        let sql = "SELECT video, position, duration, release, updated_at FROM (\
                   SELECT *, row_number() OVER (PARTITION BY title ORDER BY updated_at DESC, rowid DESC) AS n, \
                   rowid AS r FROM watched) \
                   WHERE n = 1 ORDER BY updated_at DESC, r DESC LIMIT ?1";
        self.read(sql, params![most as i64])
    }

    fn read(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Watched>, Error> {
        let conn = self.conn();
        let mut stmt = conn.prepare(sql).map_err(|e| self.fail(e))?;
        let rows = stmt.query_map(params, watched).map_err(|e| self.fail(e))?;
        rows.map(|row| row.map_err(|e| self.fail(e))?.map_err(|text| self.unreadable(&text))).collect()
    }

    fn unreadable(&self, what: &str) -> Error {
        Error::new(ErrorCode::StateCorrupt, format!("{} has an unreadable {what}", self.path.display()))
    }

    /// Saves `card` in the library, keeping when it was first saved, so the library lists the
    /// title even when the catalog cannot be reached.
    pub fn save(&self, card: &TitleCard, now: i64) -> Result<(), Error> {
        self.conn()
            .execute(
                "INSERT INTO library (title, kind, name, year, rating, saved_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (title) DO UPDATE SET kind = excluded.kind, name = excluded.name, \
                 year = excluded.year, rating = excluded.rating",
                params![card.id.as_str(), kind_text(card.kind), card.name, card.year, card.rating, now],
            )
            .map(drop)
            .map_err(|e| self.fail(e))
    }

    pub fn unsave(&self, title: &TitleId) -> Result<(), Error> {
        self.conn()
            .execute("DELETE FROM library WHERE title = ?1", [title.as_str()])
            .map(drop)
            .map_err(|e| self.fail(e))
    }

    pub fn is_saved(&self, title: &TitleId) -> Result<bool, Error> {
        self.conn()
            .query_row("SELECT 1 FROM library WHERE title = ?1", [title.as_str()], |_| Ok(()))
            .optional()
            .map(|found| found.is_some())
            .map_err(|e| self.fail(e))
    }

    /// The saved titles, the most recently saved first.
    pub fn saved(&self) -> Result<Vec<TitleCard>, Error> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT title, kind, name, year, rating FROM library ORDER BY saved_at DESC, rowid DESC")
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<u16>>(3)?,
                    r.get::<_, Option<f32>>(4)?,
                ))
            })
            .map_err(|e| self.fail(e))?;
        let mut cards = Vec::new();
        for row in rows {
            let (id, kind, name, year, rating) = row.map_err(|e| self.fail(e))?;
            let kind = match kind.as_str() {
                "film" => Kind::Film,
                _ => Kind::Series,
            };
            let id = id.parse().map_err(|_| self.unreadable("saved title"))?;
            cards.push(TitleCard { id, kind, name, year, rating });
        }
        Ok(cards)
    }
}

fn kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::Film => "film",
        Kind::Series => "series",
    }
}

/// One row of `watched`, or what of it does not read.
fn watched(r: &Row) -> rusqlite::Result<Result<Watched, String>> {
    let video: String = r.get(0)?;
    let release: Option<String> = r.get(3)?;
    let Ok(video) = video.parse::<VideoRef>() else { return Ok(Err(format!("video {video}"))) };
    let release = match release {
        None => None,
        Some(text) => match serde_json::from_value(serde_json::Value::String(text.clone())) {
            Ok(id) => Some(id),
            Err(_) => return Ok(Err(format!("release {text}"))),
        },
    };
    Ok(Ok(Watched { video, position: r.get(1)?, duration: r.get(2)?, release, at: r.get(4)? }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::store::Store;

    fn shelf(dir: &tempfile::TempDir) -> Shelf {
        let path = dir.path().join("state.db");
        drop(Store::open(&path).unwrap());
        Shelf::open(&path).unwrap()
    }

    fn at(video: &str, position: f64, duration: Option<f64>, at: i64) -> Watched {
        Watched { video: video.parse().unwrap(), position, duration, release: None, at }
    }

    fn rows(watched: Vec<Watched>) -> Vec<(String, f64, i64)> {
        watched.into_iter().map(|w| (w.video.to_string(), w.position, w.at)).collect()
    }

    #[test]
    fn a_video_saved_again_keeps_one_row_with_the_newest_position_and_its_release() {
        let dir = tempfile::tempdir().unwrap();
        let shelf = shelf(&dir);
        let release: ReleaseId = serde_json::from_str("\"9b521219bc48550140a666f74a29048ed78e933b\"").unwrap();
        shelf.watch(&Watched { release: Some(release), ..at("tt9999901:1:1", 60.0, None, 10) }).unwrap();
        shelf.watch(&at("tt9999901:1:1", 120.5, Some(2640.0), 20)).unwrap();
        shelf.watch(&at("tt9999901:1:2", 30.0, Some(2640.0), 30)).unwrap();
        shelf.watch(&at("tt0063350", 5.0, None, 40)).unwrap();

        let series = shelf.watched(&"tt9999901".parse().unwrap()).unwrap();
        assert_eq!(rows(series.clone()), [("tt9999901:1:1".into(), 120.5, 20), ("tt9999901:1:2".into(), 30.0, 30)]);
        assert_eq!((series[0].duration, series[0].release.as_ref()), (Some(2640.0), Some(&release)));
        assert_eq!(rows(shelf.watched(&"tt0100258".parse().unwrap()).unwrap()), []);
    }

    #[test]
    fn recent_lists_the_newest_video_of_each_title_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let shelf = shelf(&dir);
        shelf.watch(&at("tt9999901:1:1", 60.0, None, 10)).unwrap();
        shelf.watch(&at("tt0063350", 5.0, None, 20)).unwrap();
        shelf.watch(&at("tt9999901:1:2", 30.0, None, 30)).unwrap();
        shelf.watch(&at("tt0100258", 1.0, None, 5)).unwrap();
        assert_eq!(
            rows(shelf.recent(10).unwrap()),
            [("tt9999901:1:2".into(), 30.0, 30), ("tt0063350".into(), 5.0, 20), ("tt0100258".into(), 1.0, 5)]
        );
        assert_eq!(rows(shelf.recent(1).unwrap()), [("tt9999901:1:2".into(), 30.0, 30)]);
    }

    #[test]
    fn the_library_keeps_each_saved_title_card_until_it_is_unsaved() {
        let dir = tempfile::tempdir().unwrap();
        let shelf = shelf(&dir);
        let card = |id: &str, kind, name: &str, year, rating| TitleCard {
            id: id.parse().unwrap(),
            kind,
            name: name.into(),
            year,
            rating,
        };
        let night = card("tt0063350", Kind::Film, "Night of the Living Dead", Some(1968), Some(7.8));
        let harbor = card("tt9999901", Kind::Series, "Harbor Lights", Some(2019), None);
        shelf.save(&night, 10).unwrap();
        shelf.save(&harbor, 20).unwrap();
        shelf.save(&night, 30).unwrap();
        assert_eq!(shelf.saved().unwrap(), [harbor.clone(), night.clone()]);
        assert_eq!(shelf.is_saved(&night.id), Ok(true));
        shelf.unsave(&night.id).unwrap();
        shelf.unsave(&night.id).unwrap();
        assert_eq!((shelf.saved().unwrap(), shelf.is_saved(&night.id)), (vec![harbor], Ok(false)));

        drop(shelf);
        drop(Store::open(&dir.path().join("state.db")).unwrap());
        let reopened = Shelf::open(&dir.path().join("state.db")).unwrap();
        assert_eq!(reopened.saved().unwrap().len(), 1, "the library outlives the engine");
    }
}
