//! The folders the engine downloads into, and whether each answers now. Any look in a folder can
//! wait a long time: macOS has a program it has not seen before wait to open ~/Downloads, ~/Desktop
//! or ~/Documents until the person answers a prompt, a network share that dropped leaves every call
//! waiting, and a sleeping disk takes seconds to wake. So the owner looks in a folder only through
//! `Reach`, on a thread of its own, and a folder that is slow to answer holds up only the downloads
//! and commands that need it, while the engine keeps answering everyone else.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use torrential_core::{Error, ErrorCode};

use crate::disk::Disk;
use crate::log::log;

/// How long a folder takes to answer before torrential says it is waiting for it.
pub const SAY_AFTER: Duration = Duration::from_secs(3);

/// How long the owner waits for a folder that answered before. One that takes longer is waited
/// for as on a first look, until what was asked of it comes back.
const ANSWER_WITHIN: Duration = Duration::from_secs(1);

#[cfg(target_os = "macos")]
const WHY: &str = ", check for a macOS permission prompt and that its disk is connected";
#[cfg(not(target_os = "macos"))]
const WHY: &str = "";

#[cfg(target_os = "macos")]
const HINT: &str = "allow access if macOS asks, and make sure the disk it is on is connected and answers, then run \
                    the command again";
#[cfg(not(target_os = "macos"))]
const HINT: &str = "make sure the disk it is on is connected and answers, then run the command again";

pub struct Reach {
    disk: Arc<dyn Disk>,
    /// Inside a cell, for any look may find that a folder stopped answering.
    folders: RefCell<Folders>,
    answered: mpsc::Sender<Answer>,
    answers: mpsc::Receiver<Answer>,
}

struct Folders {
    /// The folders looked in. What is inside one answers with the nearest of them above it.
    looked: BTreeMap<PathBuf, Answers>,
    looks: u64,
}

enum Answers {
    Yes,
    /// A look in it has not come back: the first, or one that took longer than `ANSWER_WITHIN`.
    Waiting(Looking),
}

struct Looking {
    look: u64,
    since: Instant,
    /// The wait is in the log.
    said: bool,
}

/// A look that came back: the folder it was in, and which look it was.
type Answer = (PathBuf, u64);

impl Folders {
    /// The folder `dir` answers with: the nearest one looked in at or above it.
    fn holding(&self, dir: &Path) -> Option<(&PathBuf, &Answers)> {
        self.looked
            .iter()
            .filter(|(folder, _)| dir.starts_with(folder))
            .max_by_key(|(folder, _)| folder.as_os_str().len())
    }
}

impl Reach {
    /// `reached` are folders the engine has looked in already.
    pub fn new(disk: Arc<dyn Disk>, reached: Vec<PathBuf>) -> Self {
        let (answered, answers) = mpsc::channel();
        let looked = reached.into_iter().map(|folder| (folder, Answers::Yes)).collect();
        Self { disk, folders: RefCell::new(Folders { looked, looks: 0 }), answered, answers }
    }

    /// Whether `dir` answers now.
    pub fn reached(&self, dir: &Path) -> bool {
        matches!(self.folders.borrow().holding(dir), Some((_, Answers::Yes)))
    }

    /// Whether `dir` answers now. A folder never looked in gets its first look.
    pub fn reach(&self, dir: &Path) -> bool {
        self.answering(dir).is_some()
    }

    /// Runs `work` in `dir` on a thread of its own, and answers what it found when `dir` answers
    /// within `ANSWER_WITHIN`. Otherwise this, and every look in `dir` until `work` comes back,
    /// answers that torrential is waiting for the folder.
    pub fn ask<T: Send + 'static>(
        &self,
        dir: &Path,
        work: impl FnOnce(&dyn Disk) -> T + Send + 'static,
    ) -> Result<T, Error> {
        let Some(folder) = self.answering(dir) else { return Err(waiting(dir)) };
        let since = Instant::now();
        let (look, found) = self
            .look(&folder, work)
            .map_err(|e| Error::internal(format!("cannot start a look in {}: {e}", dir.display())))?;
        match found.recv_timeout(ANSWER_WITHIN) {
            Ok(found) => Ok(found),
            Err(RecvTimeoutError::Timeout) => {
                let waiting_for = Answers::Waiting(Looking { look, since, said: false });
                self.folders.borrow_mut().looked.insert(folder, waiting_for);
                Err(waiting(dir))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Error::internal(format!("the look in {} stopped before it answered", dir.display())))
            }
        }
    }

    /// Notes the folders that answered since the last call, and says in the log once which of
    /// those still waited for have taken longer than `SAY_AFTER`. Answers whether any answered.
    pub fn catch_up(&self, now: Instant) -> bool {
        let mut any = false;
        while let Ok(answer) = self.answers.try_recv() {
            any |= self.arrived(answer);
        }
        for (folder, answers) in &mut self.folders.borrow_mut().looked {
            if let Answers::Waiting(looking) = answers
                && !looking.said
                && now.duration_since(looking.since) >= SAY_AFTER
            {
                log(&format!("waiting for access to {}{WHY}", folder.display()));
                looking.said = true;
            }
        }
        any
    }

    /// Waits at most `within` for each of `dirs` to answer, as the engine starts.
    pub fn wait(&self, dirs: &[PathBuf], within: Duration) {
        let deadline = Instant::now() + within;
        let mut waiting: Vec<&PathBuf> = dirs.iter().filter(|dir| !self.reach(dir)).collect();
        while !waiting.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return };
            match self.answers.recv_timeout(left) {
                Ok(answer) => {
                    self.arrived(answer);
                }
                Err(_) => return,
            }
            waiting.retain(|dir| !self.reached(dir));
        }
    }

    /// The folder `dir` answers with, if it answers now. A folder never looked in gets its first
    /// look, and answers once that comes back.
    fn answering(&self, dir: &Path) -> Option<PathBuf> {
        match self.folders.borrow().holding(dir) {
            Some((folder, Answers::Yes)) => return Some(folder.clone()),
            Some((_, Answers::Waiting(_))) => return None,
            None => {}
        }
        let looked = dir.to_path_buf();
        let Ok((look, _)) = self.look(dir, move |disk| drop(disk.lineage(&looked))) else {
            // Without a thread to look on, the owner looks in the folder itself.
            self.folders.borrow_mut().looked.insert(dir.to_path_buf(), Answers::Yes);
            return Some(dir.to_path_buf());
        };
        let first = Answers::Waiting(Looking { look, since: Instant::now(), said: false });
        self.folders.borrow_mut().looked.insert(dir.to_path_buf(), first);
        None
    }

    /// Starts `work` in `folder` on a thread of its own, which answers what it found and then
    /// that it came back.
    fn look<T: Send + 'static>(
        &self,
        folder: &Path,
        work: impl FnOnce(&dyn Disk) -> T + Send + 'static,
    ) -> std::io::Result<(u64, mpsc::Receiver<T>)> {
        let look = {
            let mut folders = self.folders.borrow_mut();
            folders.looks += 1;
            folders.looks
        };
        let (found, answer) = mpsc::channel();
        let (disk, answered, folder) = (self.disk.clone(), self.answered.clone(), folder.to_path_buf());
        std::thread::Builder::new().name("reach".into()).spawn(move || {
            let _ = found.send(work(disk.as_ref()));
            let _ = answered.send((folder, look));
        })?;
        Ok((look, answer))
    }

    /// Notes that `look` came back from `folder`. Answers whether the folder was waiting for it.
    fn arrived(&self, (folder, look): Answer) -> bool {
        let mut folders = self.folders.borrow_mut();
        let Some(answers) = folders.looked.get_mut(&folder) else { return false };
        if !matches!(answers, Answers::Waiting(looking) if looking.look == look) {
            return false;
        }
        *answers = Answers::Yes;
        true
    }
}

/// The answer to a request that waited for `dir`.
pub fn waiting(dir: &Path) -> Error {
    Error::new(ErrorCode::StorageError, format!("torrential is waiting for access to {}", dir.display()))
        .with_hint(HINT)
}
