use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use torrential_core::{Error, ErrorCode, InfoHash, PieceMap, Slice, TorrentId};

#[derive(Clone, PartialEq, Debug)]
pub struct Metainfo {
    pub info_hash: InfoHash,
    pub name: String,
    pub files: Vec<MetaFile>,
    pub single_file: bool,
    pub bytes: Vec<u8>,
    checked: (),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetaFile {
    pub path: String,
    pub size: u64,
}

impl Metainfo {
    pub fn new(
        info_hash: InfoHash,
        name: String,
        files: Vec<MetaFile>,
        single_file: bool,
        bytes: Vec<u8>,
    ) -> Result<Self, Error> {
        let escapes = |p: &str| p.is_empty() || p.starts_with('/') || p.split('/').any(|c| c == ".." || c.is_empty());
        if escapes(&name) || name.contains('/') || files.iter().any(|f| escapes(&f.path)) {
            return Err(Error::new(ErrorCode::InvalidTorrent, "the torrent has a file path outside its own folder")
                .with_hint("this torrent is malformed or malicious; do not download it"));
        }
        Ok(Self { info_hash, name, files, single_file, bytes, checked: () })
    }

    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

pub type ParseTorrent = fn(&[u8]) -> Result<Metainfo, Error>;

#[derive(Clone, PartialEq, Debug)]
pub enum AddSource {
    Magnet(String),
    Metainfo(Vec<u8>),
}

#[derive(Clone, PartialEq, Debug)]
pub struct AddTorrent {
    pub id: TorrentId,
    pub source: AddSource,
    pub save_path: PathBuf,
    pub wanted: Option<Vec<bool>>,
    pub resume_data: Option<Vec<u8>>,
    pub paused: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activity {
    Metadata,
    Checking,
    Downloading,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub verified: u64,
    pub piece_map: Option<PieceMap>,
    pub missing: Option<Missing>,
    pub down_rate: u64,
    pub up_rate: u64,
    pub peers: u32,
    pub seeds: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Missing {
    pub pieces: u32,
    pub unavailable: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub enum EngineEvent {
    Metadata {
        id: TorrentId,
        metainfo: Metainfo,
    },
    Activity {
        id: TorrentId,
        activity: Activity,
    },
    Stats {
        id: TorrentId,
        stats: Stats,
    },
    Finished {
        id: TorrentId,
    },
    Paused {
        id: TorrentId,
    },
    ResumeData {
        id: TorrentId,
        data: Vec<u8>,
        verified: Option<u64>,
        piece_map: Option<PieceMap>,
        files: Option<Vec<u64>>,
    },
    Checked {
        id: TorrentId,
        damaged_pieces: u32,
        verified: u64,
        piece_map: Option<PieceMap>,
        files: Vec<u64>,
    },
    Failed {
        id: TorrentId,
        error: Error,
    },
    Removed {
        id: TorrentId,
    },
}

pub trait EnginePort: Send {
    fn add(&mut self, torrent: AddTorrent) -> Result<(), Error>;
    fn select(&mut self, id: TorrentId, wanted: &[bool]) -> Result<(), Error>;
    fn pause(&mut self, id: TorrentId) -> Result<(), Error>;
    fn resume(&mut self, id: TorrentId) -> Result<(), Error>;
    fn recheck(&mut self, id: TorrentId) -> Result<(), Error>;
    fn save_resume_data(&mut self, id: TorrentId) -> Result<(), Error>;
    fn save_resume_data_if_progressed(&mut self, id: TorrentId) -> Result<(), Error>;
    fn remove(&mut self, id: TorrentId) -> Result<(), Error>;
    fn file_progress(&mut self, id: TorrentId) -> Result<Vec<u64>, Error>;
    /// Fetches the pieces under this byte range of a file first. True when all of them are verified and on disk.
    fn want_range(&mut self, id: TorrentId, file: u32, offset: u64, length: u64) -> Result<bool, Error>;
    /// How much of each of `runs` equal runs of this byte range of a file is verified and on disk,
    /// in order. Unlike `want_range`, this asks peers for nothing.
    fn written(&mut self, id: TorrentId, file: u32, offset: u64, length: u64, runs: u32) -> Result<Vec<Slice>, Error>;
    /// Downloads pieces in file order, for a download someone watches as it arrives.
    fn stream(&mut self, id: TorrentId) -> Result<(), Error>;
    /// Caps how fast a download goes, in bytes a second. `None` lifts the cap.
    fn limit(&mut self, id: TorrentId, rate: Option<NonZeroU32>) -> Result<(), Error>;
    fn poll(&mut self, max_wait: Duration) -> Vec<EngineEvent>;
}

pub enum EngineCommand {
    Add(AddTorrent),
    Select(TorrentId, Vec<bool>),
    Pause(TorrentId),
    Resume(TorrentId),
    Recheck(TorrentId),
    SaveResume(TorrentId),
    SaveResumeIfProgressed(TorrentId),
    Remove(TorrentId),
    FileProgress(TorrentId, oneshot::Sender<Result<Vec<u64>, Error>>),
    WantRange(TorrentId, u32, u64, u64, oneshot::Sender<Result<bool, Error>>),
    Written(TorrentId, u32, u64, u64, u32, oneshot::Sender<Result<Vec<Slice>, Error>>),
    Stream(TorrentId),
    Limit(TorrentId, Option<NonZeroU32>),
}

#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<EngineCommand>,
}

impl EngineHandle {
    pub fn send(&self, command: EngineCommand) {
        let _ = self.commands.send(command);
    }

    pub async fn file_progress(&self, id: TorrentId) -> Result<Vec<u64>, Error> {
        self.ask(|reply| EngineCommand::FileProgress(id, reply)).await
    }

    pub async fn want_range(&self, id: TorrentId, file: u32, offset: u64, length: u64) -> Result<bool, Error> {
        self.ask(|reply| EngineCommand::WantRange(id, file, offset, length, reply)).await
    }

    pub async fn written(
        &self,
        id: TorrentId,
        file: u32,
        offset: u64,
        length: u64,
        runs: u32,
    ) -> Result<Vec<Slice>, Error> {
        self.ask(|reply| EngineCommand::Written(id, file, offset, length, runs, reply)).await
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> EngineCommand) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        self.send(make(reply));
        answer.await.map_err(|_| Error::internal("the engine thread stopped"))?
    }
}

const POLL: Duration = Duration::from_millis(50);

pub fn spawn(
    mut port: Box<dyn EnginePort>,
    events: tokio_mpsc::UnboundedSender<EngineEvent>,
) -> (EngineHandle, std::thread::JoinHandle<()>) {
    let (commands, inbox) = mpsc::channel::<EngineCommand>();
    let thread = std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            loop {
                loop {
                    match inbox.try_recv() {
                        Ok(command) => {
                            if let Some(event) = apply(port.as_mut(), command) {
                                let _ = events.send(event);
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                for event in port.poll(POLL) {
                    if events.send(event).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawning the engine thread");
    (EngineHandle { commands }, thread)
}

fn apply(port: &mut dyn EnginePort, command: EngineCommand) -> Option<EngineEvent> {
    let (id, result) = match command {
        EngineCommand::Add(torrent) => (torrent.id, port.add(torrent)),
        EngineCommand::Select(id, wanted) => (id, port.select(id, &wanted)),
        EngineCommand::Pause(id) => (id, port.pause(id)),
        EngineCommand::Resume(id) => (id, port.resume(id)),
        EngineCommand::Recheck(id) => (id, port.recheck(id)),
        EngineCommand::SaveResume(id) => (id, port.save_resume_data(id)),
        EngineCommand::SaveResumeIfProgressed(id) => (id, port.save_resume_data_if_progressed(id)),
        EngineCommand::Remove(id) => (id, port.remove(id)),
        EngineCommand::Stream(id) => (id, port.stream(id)),
        EngineCommand::Limit(id, rate) => {
            // A limit the engine cannot set leaves the download running as it was, not failed.
            if let Err(error) = port.limit(id, rate) {
                super::log(&format!("cannot change how fast {id} downloads: {}", error.message));
            }
            return None;
        }
        EngineCommand::FileProgress(id, reply) => {
            let _ = reply.send(port.file_progress(id));
            return None;
        }
        EngineCommand::WantRange(id, file, offset, length, reply) => {
            let _ = reply.send(port.want_range(id, file, offset, length));
            return None;
        }
        EngineCommand::Written(id, file, offset, length, runs, reply) => {
            let _ = reply.send(port.written(id, file, offset, length, runs));
            return None;
        }
    };
    result.err().map(|error| EngineEvent::Failed { id, error })
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct FakeTorrentFile {
    pub info_hash: InfoHash,
    pub name: String,
    pub files: Vec<(String, u64)>,
}

impl FakeTorrentFile {
    pub fn bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("fake torrents serialize")
    }

    pub fn metainfo(&self) -> Result<Metainfo, Error> {
        let files = self.files.iter().map(|(path, size)| MetaFile { path: path.clone(), size: *size }).collect();
        let single_file = self.files.len() == 1 && self.files[0].0 == self.name;
        Metainfo::new(self.info_hash, self.name.clone(), files, single_file, self.bytes())
    }
}

pub fn parse_fake(bytes: &[u8]) -> Result<Metainfo, Error> {
    serde_json::from_slice::<FakeTorrentFile>(bytes)
        .map_err(|e| Error::new(ErrorCode::InvalidTorrent, format!("not a torrent file: {e}")))?
        .metainfo()
}

#[derive(Clone, Default)]
pub struct Swarm(Arc<Mutex<SwarmState>>);

#[derive(Default)]
struct SwarmState {
    published: BTreeMap<TorrentId, FakeTorrentFile>,
    invent_from_magnets: bool,
    rate: u64,
    damaged: BTreeMap<TorrentId, u32>,
    unavailable: BTreeMap<TorrentId, u32>,
    unsaved: BTreeMap<TorrentId, (Error, u32)>,
    /// The torrents the engine was asked to download in playing order.
    streamed: BTreeSet<TorrentId>,
}

impl Swarm {
    pub fn new(rate: u64) -> Self {
        let swarm = Self::default();
        swarm.0.lock().unwrap().rate = rate;
        swarm
    }

    pub fn inventive(rate: u64) -> Self {
        let swarm = Self::new(rate);
        swarm.0.lock().unwrap().invent_from_magnets = true;
        swarm
    }

    pub fn publish(&self, torrent: FakeTorrentFile) {
        self.0.lock().unwrap().published.insert(torrent.info_hash.id(), torrent);
    }

    #[cfg(test)]
    pub fn set_rate(&self, rate: u64) {
        self.0.lock().unwrap().rate = rate;
    }

    #[cfg(test)]
    pub fn damage(&self, id: TorrentId, pieces: u32) {
        self.0.lock().unwrap().damaged.insert(id, pieces);
    }

    pub fn withhold(&self, id: TorrentId, pieces: u32) {
        self.0.lock().unwrap().unavailable.insert(id, pieces);
    }

    #[cfg(test)]
    pub fn refuse_saves(&self, id: TorrentId, error: Error) {
        self.0.lock().unwrap().unsaved.insert(id, (error, 0));
    }

    /// Whether the engine was asked to download `id` in playing order.
    #[cfg(test)]
    pub fn streamed(&self, id: TorrentId) -> bool {
        self.0.lock().unwrap().streamed.contains(&id)
    }

    #[cfg(test)]
    pub fn refused_saves(&self, id: TorrentId) -> u32 {
        self.0.lock().unwrap().unsaved.get(&id).map_or(0, |(_, refused)| *refused)
    }

    fn unavailable(&self, id: TorrentId) -> u32 {
        self.0.lock().unwrap().unavailable.get(&id).copied().unwrap_or(0)
    }
}

pub struct Fake {
    swarm: Swarm,
    torrents: BTreeMap<TorrentId, FakeTorrent>,
    events: VecDeque<EngineEvent>,
    last_poll: Instant,
    last_stats: Instant,
}

const FAKE_STATS_EVERY: Duration = Duration::from_millis(100);

struct FakeTorrent {
    id: TorrentId,
    swarm: Swarm,
    magnet: Option<String>,
    metainfo: Option<Metainfo>,
    wanted: Option<Vec<bool>>,
    arrived: f64,
    saved: f64,
    paused: bool,
    finished: bool,
    downloading: bool,
    limit: Option<NonZeroU32>,
}

impl FakeTorrent {
    fn landing(&self) -> Option<Landing> {
        let unavailable = self.swarm.unavailable(self.id);
        Some(fake_landing(self.metainfo.as_ref()?, self.wanted.as_deref()?, self.arrived as u64, unavailable))
    }

    fn progress(&self) -> (u64, Option<PieceMap>) {
        self.landing().map_or((0, None), |l| (l.verified, l.piece_map))
    }

    fn files(&self) -> Vec<u64> {
        self.landing().map(|l| l.files).unwrap_or_default()
    }

    fn written(&self, file: u32, offset: u64, length: u64) -> Result<bool, Error> {
        Ok(self.runs(file, offset, length, 1)?.iter().all(|run| run.is_complete()))
    }

    /// How much of each of `runs` equal runs of a byte range of a file has arrived.
    fn runs(&self, file: u32, offset: u64, length: u64, runs: u32) -> Result<Vec<Slice>, Error> {
        let Some(metainfo) = &self.metainfo else {
            return Err(Error::internal(format!("{} has no file list", self.id)));
        };
        let size = metainfo.files.get(file as usize).map(|f| f.size).unwrap_or(0);
        if length == 0 || offset >= size {
            return Err(Error::internal(format!("byte range {offset}+{length} is outside file {file} of {}", self.id)));
        }
        let start = metainfo.files[..file as usize].iter().map(|f| f.size).sum::<u64>() + offset;
        let span = length.min(size - offset);
        let piece = fake_piece_size(metainfo.total());
        let have = self.landing().map(|l| l.have).unwrap_or_default();
        let run = |r: u64| {
            let begin = start + r * span / u64::from(runs);
            let end = (begin + 1).max(start + (r + 1) * span / u64::from(runs));
            let pieces = begin / piece..end.div_ceil(piece);
            let arrived = pieces.clone().filter(|&p| have.get(p as usize) == Some(&true)).count();
            Slice::of(arrived as u32, pieces.count() as u32)
        };
        Ok((0..u64::from(runs)).map(run).collect())
    }

    fn wanted_size(&self) -> Option<u64> {
        let (m, wanted) = (self.metainfo.as_ref()?, self.wanted.as_ref()?);
        Some(m.files.iter().zip(wanted).filter(|(_, w)| **w).map(|(f, _)| f.size).sum())
    }

    fn stop_downloading(&mut self) {
        self.finished = false;
        self.downloading = false;
    }
}

impl Swarm {
    fn metadata(&self, id: TorrentId, magnet: Option<&str>) -> Option<Metainfo> {
        let state = self.0.lock().unwrap();
        if let Some(t) = state.published.get(&id) {
            return t.metainfo().ok();
        }
        if !state.invent_from_magnets {
            return None;
        }
        let magnet = super::magnet::parse(magnet?).ok()?;
        let size = magnet.exact_length.unwrap_or(1 << 30);
        let name = magnet.name.unwrap_or_else(|| id.to_hex());
        FakeTorrentFile { info_hash: magnet.info_hash, name: name.clone(), files: vec![(name, size)] }.metainfo().ok()
    }
}

impl Fake {
    pub fn new(swarm: Swarm) -> Self {
        let now = Instant::now();
        Self { swarm, torrents: BTreeMap::new(), events: VecDeque::new(), last_poll: now, last_stats: now }
    }

    fn torrent(&mut self, id: TorrentId) -> Result<&mut FakeTorrent, Error> {
        self.torrents.get_mut(&id).ok_or_else(|| Error::internal(format!("the engine has no torrent {id}")))
    }
}

impl EnginePort for Fake {
    fn add(&mut self, torrent: AddTorrent) -> Result<(), Error> {
        if self.torrents.contains_key(&torrent.id) {
            return Err(Error::internal(format!("the engine already has torrent {}", torrent.id)));
        }
        let (magnet, metainfo) = match torrent.source {
            AddSource::Magnet(uri) => (Some(uri), None),
            AddSource::Metainfo(bytes) => (None, Some(parse_fake(&bytes)?)),
        };
        let activity = if metainfo.is_some() { Activity::Checking } else { Activity::Metadata };
        let arrived =
            torrent.resume_data.as_deref().and_then(|d| std::str::from_utf8(d).ok()?.parse().ok()).unwrap_or(0.0);
        self.events.push_back(EngineEvent::Activity { id: torrent.id, activity });
        let t = FakeTorrent {
            id: torrent.id,
            swarm: self.swarm.clone(),
            magnet,
            metainfo,
            wanted: torrent.wanted,
            arrived,
            saved: arrived,
            paused: torrent.paused,
            finished: false,
            downloading: false,
            limit: None,
        };
        self.torrents.insert(torrent.id, t);
        Ok(())
    }

    fn select(&mut self, id: TorrentId, wanted: &[bool]) -> Result<(), Error> {
        let t = self.torrent(id)?;
        t.wanted = Some(wanted.to_vec());
        t.stop_downloading();
        Ok(())
    }

    fn pause(&mut self, id: TorrentId) -> Result<(), Error> {
        let t = self.torrent(id)?;
        t.paused = true;
        t.stop_downloading();
        let (verified, piece_map) = t.progress();
        self.events.push_back(EngineEvent::Stats { id, stats: Stats { verified, piece_map, ..Stats::default() } });
        self.events.push_back(EngineEvent::Paused { id });
        Ok(())
    }

    fn resume(&mut self, id: TorrentId) -> Result<(), Error> {
        let t = self.torrent(id)?;
        t.paused = false;
        t.stop_downloading();
        Ok(())
    }

    fn recheck(&mut self, id: TorrentId) -> Result<(), Error> {
        let damaged = self.swarm.0.lock().unwrap().damaged.remove(&id).unwrap_or(0);
        let t = self.torrent(id)?;
        let piece = t.metainfo.as_ref().map_or(0, |m| fake_piece_size(m.total()));
        t.arrived = (t.arrived - f64::from(damaged) * piece as f64).max(0.0);
        t.stop_downloading();
        let landing = t.landing();
        let damaged_pieces = landing.as_ref().map_or(0, |l| l.missing.pieces);
        let (verified, piece_map) = landing.as_ref().map_or((0, None), |l| (l.verified, l.piece_map));
        let files = landing.map(|l| l.files).unwrap_or_default();
        self.events.push_back(EngineEvent::Activity { id, activity: Activity::Checking });
        self.events.push_back(EngineEvent::Checked { id, damaged_pieces, verified, piece_map, files });
        Ok(())
    }

    fn save_resume_data(&mut self, id: TorrentId) -> Result<(), Error> {
        if let Some((error, refused)) = self.swarm.0.lock().unwrap().unsaved.get_mut(&id) {
            *refused += 1;
            return Err(error.clone());
        }
        let t = self.torrent(id)?;
        t.saved = t.arrived;
        let (verified, piece_map) = t.progress();
        let (data, files) = (t.arrived.to_string().into_bytes(), Some(t.files()));
        self.events.push_back(EngineEvent::ResumeData { id, data, verified: Some(verified), piece_map, files });
        Ok(())
    }

    fn save_resume_data_if_progressed(&mut self, id: TorrentId) -> Result<(), Error> {
        let t = self.torrent(id)?;
        if t.arrived != t.saved {
            self.save_resume_data(id)?;
        }
        Ok(())
    }

    fn remove(&mut self, id: TorrentId) -> Result<(), Error> {
        self.torrents.remove(&id);
        self.events.push_back(EngineEvent::Removed { id });
        Ok(())
    }

    fn file_progress(&mut self, id: TorrentId) -> Result<Vec<u64>, Error> {
        Ok(self.torrent(id)?.files())
    }

    fn want_range(&mut self, id: TorrentId, file: u32, offset: u64, length: u64) -> Result<bool, Error> {
        self.torrent(id)?.written(file, offset, length)
    }

    fn written(&mut self, id: TorrentId, file: u32, offset: u64, length: u64, runs: u32) -> Result<Vec<Slice>, Error> {
        self.torrent(id)?.runs(file, offset, length, runs)
    }

    fn stream(&mut self, id: TorrentId) -> Result<(), Error> {
        self.torrent(id)?;
        self.swarm.0.lock().unwrap().streamed.insert(id);
        Ok(())
    }

    fn limit(&mut self, id: TorrentId, rate: Option<NonZeroU32>) -> Result<(), Error> {
        self.torrent(id)?.limit = rate;
        Ok(())
    }

    fn poll(&mut self, max_wait: Duration) -> Vec<EngineEvent> {
        if self.events.is_empty() {
            std::thread::sleep(max_wait.min(Duration::from_millis(20)));
        }
        let dt = self.last_poll.elapsed().as_secs_f64();
        self.last_poll = Instant::now();
        let stats_due = self.last_stats.elapsed() >= FAKE_STATS_EVERY;
        if stats_due {
            self.last_stats = Instant::now();
        }
        let swarm_rate = self.swarm.0.lock().unwrap().rate;
        for (id, t) in &mut self.torrents {
            let id = *id;
            if t.metainfo.is_none() {
                if let Some(metainfo) = self.swarm.metadata(id, t.magnet.as_deref()) {
                    t.metainfo = Some(metainfo.clone());
                    self.events.push_back(EngineEvent::Metadata { id, metainfo });
                }
                continue;
            }
            let Some(size) = t.wanted_size() else { continue };
            if t.paused || t.finished {
                continue;
            }
            if !t.downloading {
                t.downloading = true;
                self.events.push_back(EngineEvent::Activity { id, activity: Activity::Downloading });
            }
            let rate = t.limit.map_or(swarm_rate, |limit| swarm_rate.min(u64::from(limit.get())));
            t.arrived = (t.arrived + rate as f64 * dt).min(size as f64);
            let landing = t.landing().expect("the file list and the selection are known");
            let done = landing.verified >= size;
            if stats_due || done {
                let down_rate = if done { 0 } else { rate };
                let (verified, piece_map, missing) = (landing.verified, landing.piece_map, Some(landing.missing));
                let stats = Stats { verified, piece_map, missing, down_rate, up_rate: 0, peers: 3, seeds: 2 };
                self.events.push_back(EngineEvent::Stats { id, stats });
            }
            if done {
                t.finished = true;
                self.events.push_back(EngineEvent::Finished { id });
            }
        }
        self.events.drain(..).collect()
    }
}

const FAKE_PIECES: u64 = 1024;
const FAKE_RUN: usize = 4;

#[derive(Clone, PartialEq, Debug)]
pub struct Landing {
    pub verified: u64,
    pub have: Vec<bool>,
    pub files: Vec<u64>,
    pub piece_map: Option<PieceMap>,
    pub missing: Missing,
}

fn fake_piece_size(total: u64) -> u64 {
    total.div_ceil(FAKE_PIECES)
}

pub fn fake_landing(metainfo: &Metainfo, wanted: &[bool], arrived: u64, unavailable: u32) -> Landing {
    let total = metainfo.total();
    let mut files = vec![0; metainfo.files.len()];
    if total == 0 {
        let missing = Missing { pieces: 0, unavailable: 0 };
        return Landing { verified: 0, have: Vec::new(), files, piece_map: None, missing };
    }
    let piece = fake_piece_size(total);
    let pieces = total.div_ceil(piece) as usize;
    let mut want = vec![0; pieces];
    for (file, p, bytes) in overlaps(metainfo, piece) {
        if wanted[file] {
            want[p] += bytes;
        }
    }
    let mut runs: Vec<usize> = (0..pieces.div_ceil(FAKE_RUN)).collect();
    let mut seed = u64::from_le_bytes(metainfo.info_hash.id().as_bytes()[..8].try_into().expect("an ID has 20 bytes"));
    for i in (1..runs.len()).rev() {
        runs.swap(i, (splitmix(&mut seed) % (i as u64 + 1)) as usize);
    }
    let wanted_pieces = want.iter().filter(|w| **w > 0).count() as u32;
    let unavailable = unavailable.min(wanted_pieces);
    let mut have = vec![false; pieces];
    let (mut left, mut landed) = (arrived, 0);
    let order = runs.iter().flat_map(|run| run * FAKE_RUN..((run + 1) * FAKE_RUN).min(pieces));
    for p in order.filter(|&p| want[p] > 0).take((wanted_pieces - unavailable) as usize) {
        if want[p] > left {
            break;
        }
        have[p] = true;
        left -= want[p];
        landed += 1;
    }
    for (file, p, bytes) in overlaps(metainfo, piece) {
        if have[p] {
            files[file] += bytes;
        }
    }
    let piece_map = PieceMap::new(std::array::from_fn(|s| {
        let begin = s * pieces / PieceMap::SLICES;
        let run = begin..(begin + 1).max((s + 1) * pieces / PieceMap::SLICES);
        Slice::of(run.clone().filter(|&p| have[p]).count() as u32, run.filter(|&p| want[p] > 0).count() as u32)
    }));
    let missing = Missing { pieces: wanted_pieces - landed, unavailable };
    Landing { verified: arrived - left, have, files, piece_map: Some(piece_map), missing }
}

fn overlaps(metainfo: &Metainfo, piece: u64) -> impl Iterator<Item = (usize, usize, u64)> + '_ {
    let mut start = 0;
    metainfo.files.iter().enumerate().flat_map(move |(file, f)| {
        let (begin, end) = (start, start + f.size);
        start = end;
        (begin / piece..end.div_ceil(piece))
            .map(move |p| (file, p as usize, end.min((p + 1) * piece) - begin.max(p * piece)))
    })
}

#[cfg(test)]
pub fn disagreement(
    metainfo: &Metainfo,
    wanted: &[bool],
    verified: u64,
    files: &[u64],
    map: &PieceMap,
) -> Option<String> {
    let selected: u64 = files.iter().zip(wanted).filter(|(_, w)| **w).map(|(bytes, _)| bytes).sum();
    if selected != verified {
        return Some(format!("the selected files hold {selected} bytes, but {verified} are verified"));
    }
    let total = metainfo.total();
    let piece = fake_piece_size(total);
    let pieces = total.div_ceil(piece) as usize;
    let mut start = 0;
    let spans: Vec<(u64, u64)> = metainfo
        .files
        .iter()
        .map(|f| {
            start += f.size;
            (start - f.size, start)
        })
        .collect();
    let (mut landed, mut missing) = (vec![0; files.len()], vec![0; files.len()]);
    for (s, slice) in map.slices().iter().enumerate() {
        let Slice::Wanted(eighths) = slice else { continue };
        let begin = s * pieces / PieceMap::SLICES;
        let end = (begin + 1).max((s + 1) * pieces / PieceMap::SLICES);
        let (lo, hi) = (begin as u64 * piece, (end as u64 * piece).min(total));
        let Some(file) = spans.iter().position(|&(a, b)| a <= lo && hi <= b) else { continue };
        let smallest = piece.min(hi - (end as u64 - 1) * piece);
        match eighths.get() {
            8 => landed[file] += hi - lo,
            0 => missing[file] += hi - lo,
            _ => {
                landed[file] += smallest;
                missing[file] += smallest;
            }
        }
    }
    metainfo.files.iter().enumerate().find_map(|(i, f)| {
        (files[i] < landed[i] || f.size - files[i] < missing[i]).then(|| {
            format!(
                "{} holds {} of {} bytes, but the map says at least {} landed and {} did not",
                f.path, files[i], f.size, landed[i], missing[i]
            )
        })
    })
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fake_map_fills_out_of_order_and_leaves_skipped_files_unwanted() {
        let set = FakeTorrentFile {
            info_hash: InfoHash::V1([7; 20]),
            name: "set".into(),
            files: vec![("a.bin".into(), 1 << 30), ("b.bin".into(), 1 << 30)],
        };
        let metainfo = set.metainfo().unwrap();
        let map =
            |percent: u64| fake_landing(&metainfo, &[true, false], (1 << 30) * percent / 100, 0).piece_map.unwrap();
        let half = map(50).to_string();
        assert_eq!(half[32..], "-".repeat(32), "b.bin is skipped");
        assert!(half.as_bytes()[..32].windows(2).any(|w| w[0] < w[1]), "{half} does not fill from the start");
        let eighths: u32 = half[..32].bytes().map(|b| u32::from(b - b'0')).sum();
        assert_eq!(eighths, 32 * 4, "{half} holds half of a.bin");
        let maps: Vec<String> = (0..=100).step_by(10).map(|percent| map(percent).to_string()).collect();
        for pair in maps.windows(2) {
            assert!(pair[0].bytes().zip(pair[1].bytes()).all(|(a, b)| a <= b), "a slice emptied: {pair:?}");
        }
        assert_eq!(maps[0], format!("{}{}", "0".repeat(32), "-".repeat(32)));
        assert_eq!(maps[10], format!("{}{}", "8".repeat(32), "-".repeat(32)));
    }

    #[test]
    fn a_fake_landing_counts_files_from_the_pieces_its_map_draws() {
        let set = FakeTorrentFile {
            info_hash: InfoHash::V1([9; 20]),
            name: "set".into(),
            files: vec![("a.bin".into(), 700_000_123), ("b.bin".into(), 300_000_017), ("c.bin".into(), 500_000_311)],
        };
        let metainfo = set.metainfo().unwrap();
        let wanted = [true, false, true];
        for arrived in [0, 1, 123_456_789, 600_000_000, 1_199_000_000] {
            let landing = fake_landing(&metainfo, &wanted, arrived, 0);
            assert!(landing.verified <= arrived, "{} verified from {arrived} arrived", landing.verified);
            let map = landing.piece_map.unwrap();
            assert_eq!(disagreement(&metainfo, &wanted, landing.verified, &landing.files, &map), None, "{map}");
        }
        let full = fake_landing(&metainfo, &wanted, 1_200_000_434, 0);
        assert_eq!(full.verified, 1_200_000_434);
        assert_eq!(full.files, [700_000_123, 195_787 + 975_850, 500_000_311], "b.bin shares two pieces");
    }

    #[test]
    fn paths_that_leave_the_folder_are_refused() {
        let with = |name: &str, path: &str| {
            FakeTorrentFile { info_hash: InfoHash::V1([1; 20]), name: name.into(), files: vec![(path.into(), 1)] }
                .metainfo()
        };
        assert_eq!(with("set", "a/b.mkv").unwrap().files[0].path, "a/b.mkv");
        for (name, path) in
            [("set", "../etc/passwd"), ("set", "/etc/passwd"), ("..", "a"), ("a/b", "c"), ("set", "a//b")]
        {
            assert_eq!(with(name, path).unwrap_err().code, ErrorCode::InvalidTorrent, "{name} {path}");
        }
    }
}
