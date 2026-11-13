use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use torrential_core::{Error, InfoHash, Job, JobStatus, PieceMap, Progress, Selection, TitleLink, TorrentId};

use super::admission::Cap;
use super::engine_port::{Metainfo, Stats};
use super::files;

const ETA_AFTER: Duration = Duration::from_secs(10);

#[derive(Clone, PartialEq, Debug)]
pub enum Intent {
    Running,
    Paused,
    Done { finished_at: i64 },
    Failed { error: Error },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Observed {
    Absent,
    InEngine(Phase),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Metadata,
    Checking,
    Downloading,
    Pausing,
    Paused,
    Finishing { writes_stopped: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verify {
    Report,
    Repair,
}

pub fn status(intent: &Intent, observed: Observed) -> JobStatus {
    match (intent, observed) {
        (_, Observed::InEngine(Phase::Checking)) => JobStatus::Checking,
        (Intent::Done { finished_at }, _) => JobStatus::Done { finished_at: *finished_at },
        (Intent::Failed { error }, _) => JobStatus::Error { error: error.clone() },
        (Intent::Paused, _) => JobStatus::Paused,
        (Intent::Running, Observed::Absent | Observed::InEngine(Phase::Paused)) => JobStatus::Queued,
        (Intent::Running, Observed::InEngine(Phase::Metadata)) => JobStatus::Metadata,
        (Intent::Running, Observed::InEngine(Phase::Downloading | Phase::Pausing | Phase::Finishing { .. })) => {
            JobStatus::Downloading
        }
    }
}

impl Observed {
    pub fn occupies_slot(self) -> bool {
        matches!(
            self,
            Observed::InEngine(Phase::Metadata | Phase::Checking | Phase::Downloading | Phase::Finishing { .. })
        )
    }

    pub fn in_engine(self) -> bool {
        matches!(self, Observed::InEngine(_))
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct JobRecord {
    pub id: TorrentId,
    pub info_hash: InfoHash,
    pub name: Option<String>,
    pub dir: PathBuf,
    pub selection: Selection,
    pub cap: Cap,
    pub magnet: Option<String>,
    pub added_at: i64,
    pub intent: Intent,
    pub verified: u64,
    pub piece_map: Option<PieceMap>,
    pub files_verified: Option<Vec<u64>>,
    pub title: Option<Linked>,
}

/// The title a download was made for, and whether it ever reached playing.
#[derive(Clone, PartialEq, Debug)]
pub struct Linked {
    pub link: TitleLink,
    pub played: bool,
}

#[derive(Clone, Debug)]
pub struct JobEntry {
    pub record: JobRecord,
    pub metainfo: Option<Arc<Metainfo>>,
    pub observed: Observed,
    pub stats: Stats,
    receiving_since: Option<Instant>,
    rate_settled: bool,
    unsourced: Duration,
    unsourced_at: Option<Instant>,
    pub verify: Option<Verify>,
}

impl JobEntry {
    pub fn new(record: JobRecord, metainfo: Option<Arc<Metainfo>>, observed: Observed) -> Self {
        Self {
            record,
            metainfo,
            observed,
            stats: Stats::default(),
            receiving_since: None,
            rate_settled: false,
            unsourced: Duration::ZERO,
            unsourced_at: None,
            verify: None,
        }
    }

    pub fn sample(&mut self, stats: Stats, at: Instant) {
        self.receiving_since = if stats.down_rate > 0 { self.receiving_since.or(Some(at)) } else { None };
        self.rate_settled = self.receiving_since.is_some_and(|since| at - since >= ETA_AFTER);
        let downloading = self.observed == Observed::InEngine(Phase::Downloading);
        let unsourced = stats.missing.is_some_and(|m| m.pieces > 0 && m.unavailable == m.pieces);
        match (downloading, stats.peers > 0, unsourced) {
            (false, ..) | (true, true, false) => self.forget_sources(),
            (true, false, _) => self.unsourced_at = None,
            (true, true, true) => {
                self.unsourced += self.unsourced_at.map_or(Duration::ZERO, |last| at - last);
                self.unsourced_at = Some(at);
            }
        }
        self.stats = stats;
    }

    pub fn unsourced_for(&self) -> Option<Duration> {
        self.unsourced_at.map(|_| self.unsourced)
    }

    pub fn forget_sources(&mut self) {
        self.unsourced = Duration::ZERO;
        self.unsourced_at = None;
    }

    pub fn held_for_save(&self) -> bool {
        matches!(self.record.intent, Intent::Failed { .. }) && self.observed == Observed::InEngine(Phase::Paused)
    }

    pub fn id(&self) -> TorrentId {
        self.record.id
    }

    pub fn wanted(&self) -> Option<Vec<bool>> {
        files::wanted(self.metainfo.as_deref()?, &self.record.selection).ok()
    }

    fn path(&self) -> Option<PathBuf> {
        let targets =
            self.metainfo.as_deref().zip(self.wanted()).map(|(m, wanted)| files::targets(m, &self.record.dir, &wanted));
        match targets.as_deref() {
            Some([only]) => Some(only.clone()),
            _ => self.record.name.as_ref().map(|n| self.record.dir.join(n)),
        }
    }

    pub fn selected_size(&self) -> Option<u64> {
        let metainfo = self.metainfo.as_deref()?;
        let wanted = self.wanted()?;
        Some(metainfo.files.iter().zip(wanted).filter(|(_, w)| *w).map(|(f, _)| f.size).sum())
    }

    pub fn verified(&self) -> u64 {
        match (&self.record.intent, self.observed) {
            (Intent::Done { .. }, _) => self.selected_size().unwrap_or(self.record.verified),
            (_, Observed::InEngine(_)) => self.stats.verified.max(self.record.verified),
            _ => self.record.verified,
        }
    }

    pub fn piece_map(&self) -> Option<PieceMap> {
        match (&self.record.intent, self.observed) {
            (_, Observed::InEngine(_)) => self.stats.piece_map.or(self.record.piece_map),
            (Intent::Done { .. }, _) => None,
            _ => self.record.piece_map,
        }
    }

    pub fn remaining(&self) -> u64 {
        match self.record.intent {
            Intent::Running => self.selected_size().map_or(0, |size| size.saturating_sub(self.verified())),
            _ => 0,
        }
    }

    pub fn view(&self) -> Job {
        let status = status(&self.record.intent, self.observed);
        let transferring = matches!(status, JobStatus::Metadata | JobStatus::Downloading);
        let size = self.selected_size();
        let verified = self.verified();
        let s = if transferring { self.stats } else { Stats::default() };
        let eta = match size {
            Some(size) if s.down_rate > 0 && self.rate_settled && self.unsourced_at.is_none() => {
                Some(size.saturating_sub(verified).div_ceil(s.down_rate))
            }
            _ => None,
        };
        Job {
            id: self.record.id,
            info_hash: self.record.info_hash,
            name: self.record.name.as_deref().map(torrential_core::text::sanitize),
            dir: self.record.dir.clone(),
            path: self.path(),
            status,
            progress: Progress {
                size,
                verified,
                piece_map: self.piece_map(),
                down_rate: s.down_rate,
                up_rate: s.up_rate,
                peers: s.peers,
                seeds: s.seeds,
                eta,
            },
            selection: self.record.selection.clone(),
            added_at: self.record.added_at,
            title: self.record.title.as_ref().map(|linked| linked.link.clone()),
        }
    }
}

/// Streamed jobs start at once and take no slot.
pub fn to_start<'a>(
    jobs: impl Iterator<Item = &'a JobEntry>,
    max_active: usize,
    streamed: &BTreeSet<TorrentId>,
) -> Vec<TorrentId> {
    let (mut now, mut waiting) = (Vec::new(), Vec::new());
    let mut active = 0;
    for job in jobs {
        let streamed = streamed.contains(&job.id());
        if job.observed.occupies_slot() {
            active += usize::from(!streamed);
        } else if job.record.intent == Intent::Running
            && matches!(job.observed, Observed::Absent | Observed::InEngine(Phase::Paused))
        {
            if streamed {
                now.push(job.id());
            } else {
                waiting.push((job.record.added_at, job.id()));
            }
        }
    }
    waiting.sort();
    now.extend(waiting.into_iter().take(max_active.saturating_sub(active)).map(|(_, id)| id));
    now
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::engine_port::{MetaFile, Missing};
    use torrential_core::ErrorCode;

    #[test]
    fn status_derivation_table() {
        let failed = Intent::Failed { error: Error::new(ErrorCode::DiskFull, "disk full") };
        let done = Intent::Done { finished_at: 7 };
        let engine = Observed::InEngine;
        let table: [(&Intent, Observed, &str); 17] = [
            (&Intent::Running, Observed::Absent, "queued"),
            (&Intent::Running, engine(Phase::Metadata), "metadata"),
            (&Intent::Running, engine(Phase::Checking), "checking"),
            (&Intent::Running, engine(Phase::Downloading), "downloading"),
            (&Intent::Running, engine(Phase::Pausing), "downloading"),
            (&Intent::Running, engine(Phase::Paused), "queued"),
            (&Intent::Running, engine(Phase::Finishing { writes_stopped: false }), "downloading"),
            (&Intent::Paused, Observed::Absent, "paused"),
            (&Intent::Paused, engine(Phase::Pausing), "paused"),
            (&Intent::Paused, engine(Phase::Paused), "paused"),
            (&Intent::Paused, engine(Phase::Checking), "checking"),
            (&done, Observed::Absent, "done"),
            (&done, engine(Phase::Checking), "checking"),
            (&done, engine(Phase::Paused), "done"),
            (&failed, Observed::Absent, "error"),
            (&failed, engine(Phase::Checking), "checking"),
            (&failed, engine(Phase::Downloading), "error"),
        ];
        for (intent, observed, expected) in table {
            assert_eq!(status(intent, observed).name(), expected, "{intent:?} {observed:?}");
        }
        assert_eq!(status(&done, Observed::Absent), JobStatus::Done { finished_at: 7 });
    }

    fn entry(n: u8, added_at: i64, intent: Intent, observed: Observed) -> JobEntry {
        let id = TorrentId::from_bytes([n; 20]);
        let record = JobRecord {
            id,
            info_hash: InfoHash::V1([n; 20]),
            name: None,
            dir: PathBuf::from("/d"),
            selection: Selection::All,
            cap: Cap { bytes: 1, from_setup: false },
            magnet: None,
            added_at,
            intent,
            verified: 0,
            piece_map: None,
            files_verified: None,
            title: None,
        };
        JobEntry::new(record, None, observed)
    }

    #[test]
    fn an_eta_waits_until_the_rate_has_held_for_ten_seconds() {
        let mut job = entry(1, 0, Intent::Running, Observed::InEngine(Phase::Downloading));
        let film = MetaFile { path: "film.mp4".into(), size: 600 << 20 };
        let metainfo = Metainfo::new(job.record.info_hash, "film.mp4".into(), vec![film], true, Vec::new()).unwrap();
        job.metainfo = Some(Arc::new(metainfo));
        let start = Instant::now();
        let mut etas = Vec::new();
        for (second, down_rate) in [(0, 1 << 20), (9, 2 << 20), (10, 10 << 20), (11, 0), (12, 10 << 20), (22, 10 << 20)]
        {
            job.sample(Stats { down_rate, ..Stats::default() }, start + Duration::from_secs(second));
            etas.push(job.view().progress.eta);
        }
        assert_eq!(etas, [None, None, Some(60), None, None, Some(60)]);
        let missing = Some(Missing { pieces: 2, unavailable: 2 });
        let stats = Stats { down_rate: 10 << 20, missing, peers: 2, ..Stats::default() };
        job.sample(stats, start + Duration::from_secs(23));
        assert_eq!(job.view().progress.eta, None, "bytes arrive, but no source has the pieces still missing");
    }

    #[test]
    fn the_path_is_the_file_when_one_file_is_picked() {
        let mut job = entry(1, 0, Intent::Running, Observed::Absent);
        let files = vec![MetaFile { path: "bbb.mp4".into(), size: 1 }, MetaFile { path: "bbb.mov".into(), size: 1 }];
        let metainfo = Metainfo::new(job.record.info_hash, "Big Buck Bunny".into(), files, false, Vec::new()).unwrap();
        job.metainfo = Some(Arc::new(metainfo));
        job.record.name = Some("Big Buck Bunny".into());
        assert_eq!(job.view().path, Some(PathBuf::from("/d/Big Buck Bunny")));
        job.record.selection = Selection::Patterns(vec!["*.mp4".into()]);
        assert_eq!(job.view().path, Some(PathBuf::from("/d/Big Buck Bunny/bbb.mp4")));
    }

    #[test]
    fn only_connected_time_without_a_source_for_any_missing_piece_counts() {
        let mut job = entry(1, 0, Intent::Running, Observed::InEngine(Phase::Downloading));
        let start = Instant::now();
        let sample = |job: &mut JobEntry, second, peers, unavailable| {
            let missing = Some(Missing { pieces: 2, unavailable });
            job.sample(Stats { peers, missing, ..Stats::default() }, start + Duration::from_secs(second));
            job.unsourced_for().map(|d| d.as_secs())
        };
        assert_eq!(sample(&mut job, 0, 0, 2), None, "nothing connected");
        assert_eq!(sample(&mut job, 1, 2, 1), None, "a peer has one of the pieces");
        assert_eq!(sample(&mut job, 2, 2, 2), Some(0));
        assert_eq!(sample(&mut job, 5, 2, 2), Some(3));
        assert_eq!(sample(&mut job, 6, 0, 2), None, "nothing connected to ask");
        assert_eq!(sample(&mut job, 900, 2, 2), Some(3), "the outage and the first sample after it earn nothing");
        assert_eq!(sample(&mut job, 901, 2, 2), Some(4));
        assert_eq!(sample(&mut job, 902, 2, 1), None, "a source turned up");
        assert_eq!(sample(&mut job, 903, 2, 2), Some(0), "and the count starts over");
        job.observed = Observed::InEngine(Phase::Pausing);
        assert_eq!(sample(&mut job, 904, 2, 2), None, "only downloading counts");
    }

    #[test]
    fn starts_the_oldest_waiting_jobs_into_free_slots() {
        let jobs = [
            entry(1, 10, Intent::Running, Observed::InEngine(Phase::Downloading)),
            entry(2, 30, Intent::Running, Observed::Absent),
            entry(3, 20, Intent::Running, Observed::Absent),
            entry(4, 5, Intent::Paused, Observed::Absent),
            entry(6, 40, Intent::Running, Observed::InEngine(Phase::Paused)),
        ];
        let none = BTreeSet::new();
        let ids: Vec<u8> = to_start(jobs.iter(), 3, &none).iter().map(|id| id.as_bytes()[0]).collect();
        assert_eq!(ids, [3, 2]);
        let ids: Vec<u8> = to_start(jobs.iter(), 4, &none).iter().map(|id| id.as_bytes()[0]).collect();
        assert_eq!(ids, [3, 2, 6]);
    }

    #[test]
    fn the_piece_map_is_the_engines_then_the_last_saves_and_ends_when_done() {
        let map = |c: &str| Some(c.repeat(64).parse::<PieceMap>().unwrap());
        let mut job = entry(1, 0, Intent::Running, Observed::InEngine(Phase::Downloading));
        job.record.piece_map = map("2");
        assert_eq!(job.view().progress.piece_map, map("2"), "before the engine's first sample");
        job.stats.piece_map = map("5");
        assert_eq!(job.view().progress.piece_map, map("5"));
        job.record.intent = Intent::Failed { error: Error::new(ErrorCode::DiskFull, "disk full") };
        job.observed = Observed::Absent;
        assert_eq!(job.view().progress.piece_map, map("2"), "out of the engine, the last save's");
        job.record.intent = Intent::Done { finished_at: 7 };
        assert_eq!(job.view().progress.piece_map, None);
    }
}
