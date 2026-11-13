use std::fmt::{self, Write as _};
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::Error;
use crate::id::{InfoHash, TorrentId};
use crate::watching::TitleLink;

/// A download. Its ID is the torrent's ID, so a search result keeps its ID when downloaded.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: TorrentId,
    pub info_hash: InfoHash,
    /// Unknown until metadata arrives for a bare infohash.
    pub name: Option<String>,
    pub dir: PathBuf,
    /// `dir` joined with the torrent's name, once the name is known.
    pub path: Option<PathBuf>,
    #[serde(flatten)]
    pub status: JobStatus,
    pub progress: Progress,
    pub selection: Selection,
    /// Unix seconds.
    pub added_at: i64,
    /// The title the download was made for, when playing a title made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<TitleLink>,
    #[serde(default)]
    pub place: Place,
}

/// Where a download's files live, which says who deletes them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// The downloads folder or a folder the person chose. Its files stay until the person deletes them.
    #[default]
    Folder,
    /// A folder of its own inside torrential's plays folder, which torrential empties as it fills.
    Plays,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum JobStatus {
    /// Waiting for a free download slot.
    Queued,
    /// Fetching the file list from peers.
    Metadata,
    /// Hashing data already on disk.
    Checking,
    Downloading,
    Paused,
    /// Every selected piece passed its hash check and was written. Nothing uploads after this.
    Done {
        finished_at: i64,
    },
    /// The engine stopped while the download was active, for example at a reboot.
    Interrupted,
    Error {
        error: Error,
    },
}

impl JobStatus {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Metadata => "metadata",
            Self::Checking => "checking",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Done { .. } => "done",
            Self::Interrupted => "interrupted",
            Self::Error { .. } => "error",
        }
    }

    /// No further progress happens without a user action.
    pub const fn is_stopped(&self) -> bool {
        matches!(self, Self::Paused | Self::Done { .. } | Self::Interrupted | Self::Error { .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    /// Bytes selected for download. `None` until metadata arrives.
    pub size: Option<u64>,
    /// Selected bytes in pieces that passed their hash check.
    pub verified: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub piece_map: Option<PieceMap>,
    /// Payload bytes per second.
    pub down_rate: u64,
    pub up_rate: u64,
    pub peers: u32,
    pub seeds: u32,
    /// Seconds to completion at the current rate. `None` until the rate has stayed
    /// above zero for ten seconds.
    pub eta: Option<u64>,
}

impl Progress {
    pub fn fraction(&self) -> Option<f64> {
        match self.size {
            Some(0) => Some(1.0),
            Some(size) => Some(self.verified as f64 / size as f64),
            None => None,
        }
    }
}

/// Where a download's data has arrived. The torrent's pieces are cut into [`PieceMap::SLICES`]
/// equal runs in torrent order, and each [`Slice`] says how much of its run passed the hash
/// check. With fewer pieces than slices, one piece covers several slices.
///
/// `None` until the engine first samples it, which needs the file list, and once the download
/// is done, when every selected piece has arrived. A paused or failed download keeps the map
/// from its last save.
///
/// In JSON it is a string with one character per slice: `0` to `8` for the eighths of the run's
/// selected pieces that arrived, and `-` for a run with no selected piece.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PieceMap([Slice; PieceMap::SLICES]);

impl PieceMap {
    pub const SLICES: usize = 64;

    pub const fn new(slices: [Slice; Self::SLICES]) -> Self {
        Self(slices)
    }

    pub const fn slices(&self) -> &[Slice; Self::SLICES] {
        &self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slice {
    /// No piece in the run belongs to a selected file, so nothing will arrive there.
    Unwanted,
    /// The share of the run's selected pieces that passed their hash check.
    Wanted(Eighths),
}

impl Slice {
    /// Rounds to the nearest eighth, except that only a run with no verified piece reads 0
    /// and only a complete run reads 8.
    pub fn of(verified: u32, wanted: u32) -> Self {
        if wanted == 0 {
            return Self::Unwanted;
        }
        let eighths = match verified {
            0 => 0,
            v if v >= wanted => 8,
            v => ((u64::from(v) * 8 + u64::from(wanted) / 2) / u64::from(wanted)).clamp(1, 7) as u8,
        };
        Self::Wanted(Eighths(eighths))
    }

    /// Every selected piece of the run has arrived.
    pub fn is_complete(self) -> bool {
        self == Self::Wanted(Eighths(8))
    }

    /// The character for this slice in a piece map: `0` to `8`, or `-`.
    pub fn symbol(self) -> char {
        match self {
            Self::Unwanted => '-',
            Self::Wanted(Eighths(n)) => char::from(b'0' + n),
        }
    }
}

/// A count of eighths, from 0 to 8.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Eighths(u8);

impl Eighths {
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl fmt::Display for PieceMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|slice| f.write_char(slice.symbol()))
    }
}

impl FromStr for PieceMap {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let invalid = || Error::internal(format!("a piece map is 64 characters of 0 to 8 or -, not {s:?}"));
        let bytes: &[u8; Self::SLICES] = s.as_bytes().try_into().map_err(|_| invalid())?;
        let mut slices = [Slice::Unwanted; Self::SLICES];
        for (slice, byte) in slices.iter_mut().zip(bytes) {
            *slice = match byte {
                b'-' => Slice::Unwanted,
                b'0'..=b'8' => Slice::Wanted(Eighths(byte - b'0')),
                _ => return Err(invalid()),
            };
        }
        Ok(Self(slices))
    }
}

impl Serialize for PieceMap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PieceMap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

/// Which files to download. Patterns apply once the file list is known.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Selection {
    #[default]
    All,
    /// Globs or exact relative paths. `*` also matches `/`.
    Patterns(Vec<String>),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TorrentFile {
    pub index: u32,
    /// Relative to the torrent's folder, sanitized.
    pub path: String,
    pub size: u64,
    pub selected: bool,
    pub verified: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<FileRisk>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRisk {
    /// A program or script inside a torrent that otherwise holds media.
    Executable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn status_flattens_into_the_job_document() {
        let status = JobStatus::Error { error: Error::new(ErrorCode::DiskFull, "disk full") };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "error");
        assert_eq!(json["error"]["code"], "disk_full");
        let done = serde_json::to_value(JobStatus::Done { finished_at: 7 }).unwrap();
        assert_eq!(done, serde_json::json!({"state": "done", "finished_at": 7}));
    }

    #[test]
    fn a_slice_reads_empty_or_full_only_when_it_is() {
        let counts = [(0, 0), (0, 10), (1, 100), (23, 100), (50, 100), (99, 100), (100, 100)];
        let symbols: String = counts.iter().map(|&(verified, wanted)| Slice::of(verified, wanted).symbol()).collect();
        assert_eq!(symbols, "-012478");
    }

    #[test]
    fn a_piece_map_is_one_character_per_slice_in_json() {
        let text = format!("{}{}{}", "8".repeat(20), "-".repeat(12), "03".repeat(16));
        let progress = Progress { piece_map: Some(text.parse().unwrap()), ..Progress::default() };
        let json = serde_json::to_value(progress).unwrap();
        assert_eq!(json["piece_map"], text.as_str());
        assert_eq!(serde_json::from_value::<Progress>(json).unwrap(), progress);
        let map = progress.piece_map.unwrap();
        assert_eq!(map.slices()[19..21], [Slice::Wanted(Eighths(8)), Slice::Unwanted]);

        let none = serde_json::to_value(Progress::default()).unwrap();
        assert_eq!(none.get("piece_map"), None);
        for bad in ["8".repeat(63), "9".repeat(64), format!("{}x", "0".repeat(63))] {
            assert!(bad.parse::<PieceMap>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_job_names_a_title_only_when_playing_one_made_it() {
        let id: TorrentId = "9b521219bc48550140a666f74a29048ed78e933b".parse().unwrap();
        let job = Job {
            id,
            info_hash: InfoHash::V1(*id.as_bytes()),
            name: Some("Night.of.the.Living.Dead.1968.1080p.BluRay.x264".into()),
            dir: "/downloads".into(),
            path: None,
            status: JobStatus::Queued,
            progress: Progress::default(),
            selection: Selection::All,
            added_at: 7,
            title: None,
            place: Place::Folder,
        };
        let json = serde_json::to_value(&job).unwrap();
        let keys: std::collections::BTreeSet<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["added_at", "dir", "id", "info_hash", "name", "path", "place", "progress", "selection", "state"].into()
        );
        assert_eq!(json["place"], "folder");
        assert_eq!(serde_json::from_value::<Job>(json).unwrap(), job);

        let link = TitleLink { title: "tt0063350".parse().unwrap(), span: crate::title::Span::Film, release: None };
        let linked = Job { title: Some(link), ..job };
        let json = serde_json::to_value(&linked).unwrap();
        assert_eq!(json["title"], serde_json::json!({"title": "tt0063350", "span": "film", "release": null}));
        assert_eq!(serde_json::from_value::<Job>(json).unwrap(), linked);
    }

    #[test]
    fn a_job_from_an_engine_that_knows_no_places_is_in_a_folder() {
        let id: TorrentId = "9b521219bc48550140a666f74a29048ed78e933b".parse().unwrap();
        let mut json = serde_json::json!({
            "id": id, "info_hash": InfoHash::V1(*id.as_bytes()), "name": null, "dir": "/downloads", "path": null,
            "state": "queued", "progress": Progress::default(), "selection": Selection::All, "added_at": 7,
        });
        assert_eq!(serde_json::from_value::<Job>(json.clone()).unwrap().place, Place::Folder);
        json["place"] = "plays".into();
        assert_eq!(serde_json::from_value::<Job>(json).unwrap().place, Place::Plays);
    }

    #[test]
    fn fraction_is_unknown_until_size_is_known() {
        let mut p = Progress { verified: 50, ..Progress::default() };
        assert_eq!(p.fraction(), None);
        p.size = Some(200);
        assert_eq!(p.fraction(), Some(0.25));
    }
}
