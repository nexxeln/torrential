use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::log::log;
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
                log(&format!("cannot change how fast {id} downloads: {}", error.message));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake;

    #[test]
    fn paths_that_leave_the_folder_are_refused() {
        let with = |name: &str, path: &str| {
            fake::TorrentFile { info_hash: InfoHash::V1([1; 20]), name: name.into(), files: vec![(path.into(), 1)] }
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
