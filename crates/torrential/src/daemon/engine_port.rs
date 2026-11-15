use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::log::log;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use torrential_core::{Error, PieceMap, Slice, TorrentId};

use super::metainfo::Metainfo;

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

/// What the engine does to downloads, on the engine thread.
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
    /// Adds web seeds that serve every file of a download the engine has. One it no longer has
    /// gets none.
    fn add_web_seeds(&mut self, id: TorrentId, seeds: &[String]);
}

/// What the engine thread runs: the port each request acts on, and the events it waits for.
pub trait Engine: Send {
    fn port(&mut self) -> &mut dyn EnginePort;
    fn poll(&mut self, max_wait: Duration) -> Vec<EngineEvent>;
}

type Request = Box<dyn FnOnce(&mut dyn EnginePort) -> Option<EngineEvent> + Send>;

#[derive(Clone)]
pub struct EngineHandle {
    requests: mpsc::Sender<Request>,
}

impl EngineHandle {
    /// Has the engine do `act`, whatever comes of it.
    pub fn run(&self, act: impl FnOnce(&mut dyn EnginePort) + Send + 'static) {
        self.push(Box::new(move |port| {
            act(port);
            None
        }));
    }

    /// Has the engine do `act` to download `id`. A failure stops that download with the error.
    pub fn send(&self, id: TorrentId, act: impl FnOnce(&mut dyn EnginePort) -> Result<(), Error> + Send + 'static) {
        self.push(Box::new(move |port| act(port).err().map(|error| EngineEvent::Failed { id, error })));
    }

    /// Asks the engine something, and waits for the answer.
    pub async fn ask<T: Send + 'static>(
        &self,
        ask: impl FnOnce(&mut dyn EnginePort) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        self.push(Box::new(move |port| {
            let _ = reply.send(ask(port));
            None
        }));
        answer.await.map_err(|_| Error::internal("the engine thread stopped"))?
    }

    /// Changes how fast `id` downloads. A limit the engine cannot set leaves the download running
    /// as it was, not failed.
    pub fn limit(&self, id: TorrentId, rate: Option<NonZeroU32>) {
        self.run(move |port| {
            if let Err(error) = port.limit(id, rate) {
                log(&format!("cannot change how fast {id} downloads: {}", error.message));
            }
        });
    }

    fn push(&self, request: Request) {
        let _ = self.requests.send(request);
    }
}

const POLL: Duration = Duration::from_millis(50);

pub fn spawn(
    mut engine: Box<dyn Engine>,
    events: tokio_mpsc::UnboundedSender<EngineEvent>,
) -> (EngineHandle, std::thread::JoinHandle<()>) {
    let (requests, inbox) = mpsc::channel::<Request>();
    let thread = std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            loop {
                loop {
                    match inbox.try_recv() {
                        Ok(request) => {
                            if let Some(event) = request(engine.port()) {
                                let _ = events.send(event);
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                for event in engine.poll(POLL) {
                    if events.send(event).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawning the engine thread");
    (EngineHandle { requests }, thread)
}
