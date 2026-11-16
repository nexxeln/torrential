//! The folders the engine has looked in since it started. The first look in a folder can wait a
//! long time: macOS has a program it has not seen before wait to open ~/Downloads, ~/Desktop or
//! ~/Documents until the person answers a prompt, and a network or removable disk can be slow to
//! answer. So the first look happens on a thread of its own, and the owner touches a folder only
//! once it has answered, which keeps the engine answering everyone else meanwhile.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use torrential_core::{Error, ErrorCode};

use crate::disk::Disk;
use crate::log::log;

/// How long a folder takes to answer before torrential says it is waiting for it.
pub const SAY_AFTER: Duration = Duration::from_secs(3);

#[cfg(target_os = "macos")]
const WHY: &str = ", check for a macOS permission prompt";
#[cfg(not(target_os = "macos"))]
const WHY: &str = "";

#[cfg(target_os = "macos")]
const HINT: &str = "check for a macOS permission prompt and allow it, then run the command again";
#[cfg(not(target_os = "macos"))]
const HINT: &str = "make sure the disk it is on is connected and answers, then run the command again";

pub struct Reach {
    disk: Arc<dyn Disk>,
    /// Folders that answered. What is inside one answers too.
    reached: Vec<PathBuf>,
    /// Folders a thread is looking in now, and when it started.
    reaching: BTreeMap<PathBuf, Looking>,
    answered: mpsc::Sender<PathBuf>,
    answers: mpsc::Receiver<PathBuf>,
}

struct Looking {
    since: Instant,
    /// The wait is in the log.
    said: bool,
}

impl Reach {
    /// `reached` are folders the engine has looked in already.
    pub fn new(disk: Arc<dyn Disk>, reached: Vec<PathBuf>) -> Self {
        let (answered, answers) = mpsc::channel();
        Self { disk, reached, reaching: BTreeMap::new(), answered, answers }
    }

    /// Whether `dir`, or a folder it is in, has answered.
    pub fn reached(&self, dir: &Path) -> bool {
        self.reached.iter().any(|folder| dir.starts_with(folder))
    }

    /// Whether `dir` has answered. If it has not, a thread starts to look in it, unless one is
    /// looking already.
    pub fn reach(&mut self, dir: &Path) -> bool {
        if self.reached(dir) {
            return true;
        }
        if self.reaching.contains_key(dir) {
            return false;
        }
        let (disk, answered, looked) = (self.disk.clone(), self.answered.clone(), dir.to_path_buf());
        let thread = std::thread::Builder::new().name("reach".into()).spawn(move || {
            // Whatever the look finds, the folder has answered, so the next look takes no time.
            let _ = disk.lineage(&looked);
            let _ = answered.send(looked);
        });
        if thread.is_err() {
            self.reached.push(dir.to_path_buf());
            return true;
        }
        self.reaching.insert(dir.to_path_buf(), Looking { since: Instant::now(), said: false });
        false
    }

    /// Notes the folders that answered since the last call, and says in the log once which of
    /// those still looked in have taken longer than `SAY_AFTER`. Answers whether any answered.
    pub fn catch_up(&mut self, now: Instant) -> bool {
        let mut any = false;
        while let Ok(dir) = self.answers.try_recv() {
            self.arrived(dir);
            any = true;
        }
        for (dir, looking) in &mut self.reaching {
            if !looking.said && now.duration_since(looking.since) >= SAY_AFTER {
                log(&format!("waiting for access to {}{WHY}", dir.display()));
                looking.said = true;
            }
        }
        any
    }

    /// Waits at most `within` for each of `dirs` to answer, as the engine starts.
    pub fn wait(&mut self, dirs: &[PathBuf], within: Duration) {
        let deadline = Instant::now() + within;
        let mut waiting: Vec<&PathBuf> = dirs.iter().filter(|dir| !self.reach(dir)).collect();
        while !waiting.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return };
            match self.answers.recv_timeout(left) {
                Ok(dir) => self.arrived(dir),
                Err(_) => return,
            }
            waiting.retain(|dir| !self.reached(dir));
        }
    }

    fn arrived(&mut self, dir: PathBuf) {
        self.reaching.remove(&dir);
        self.reached.push(dir);
    }
}

/// The answer to a request that waited `SAY_AFTER` for `dir`.
pub fn waiting(dir: &Path) -> Error {
    Error::new(ErrorCode::StorageError, format!("torrential is waiting for access to {}", dir.display()))
        .with_hint(HINT)
}
