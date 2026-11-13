//! torrential's own folder for plays. Each play downloads into a folder of its own inside it.
//! Plays no one streams stop downloading, the plays watched longest ago leave once the folder is
//! over its limit, and torrential deletes only inside it, unless the person asks.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use torrential_core::protocol::{Change, Kept};
use torrential_core::{Error, ErrorCode, TorrentId};

use super::super::admission;
use super::super::book::{Home, Intent, JobEntry, Observed, Play};
use super::super::files;
use super::super::log;
use super::super::server::OwnerHandle;
use super::super::store::Pending;
use super::{Command, Finish, Owner, Reply, claim_key, now};

/// How long a play no one streams keeps downloading. Long enough that a break, or a phone that
/// locked, comes back to a play still downloading. Short enough that a film left after a few
/// minutes soon stops taking bandwidth and disk.
pub const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// How often the owner looks for plays to pause or to take out of the plays folder.
const TEND_EVERY: Duration = Duration::from_secs(1);

/// The plays folder and its limits.
#[derive(Clone, Debug)]
pub struct Plays {
    pub root: PathBuf,
    /// The most the plays that may leave take up before those watched longest ago leave.
    pub max: u64,
    /// How long a play no one streams keeps downloading.
    pub idle_after: Duration,
}

impl Plays {
    /// Makes an empty folder for a new play of `id`. It is made here and now, so no other play
    /// and no file the person put there is in it.
    pub fn new_folder(&self, id: TorrentId) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.root)?;
        let stem = &id.to_hex()[..STEM];
        for n in 0u32.. {
            let folder = self.root.join(format!("{stem}-{n}"));
            match std::fs::create_dir(&folder) {
                Ok(()) => return Ok(folder),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!("a free folder name turns up long before u32 runs out")
    }
}

const STEM: usize = 16;

/// Whether `name` is a name `Plays::new_folder` gives a folder.
fn play_folder(name: &OsStr) -> bool {
    let Some((stem, n)) = name.to_str().and_then(|name| name.split_once('-')) else { return false };
    stem.len() == STEM
        && stem.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
}

/// Where a play's new download goes. A play that joins a download leaves it where it is.
#[derive(Clone, Debug)]
pub enum Destination {
    /// A new folder in the plays folder.
    Plays,
    /// This folder, or the downloads folder.
    Folder(Option<PathBuf>),
}

/// What removing a download deletes. It stays on record until it is done, so a removal that a
/// crash cut short finishes when the engine starts again.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Deletion {
    /// A play's own folder and everything in it.
    Play { folder: PathBuf },
    /// The files a download wrote into a folder the person chose, as paths inside it, and the
    /// folders below it that they leave empty.
    Files { dir: PathBuf, files: Vec<PathBuf> },
}

/// What a deletion did.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct Deleted {
    pub deleted: Vec<PathBuf>,
    /// What it left, and why.
    pub kept: Vec<(PathBuf, &'static str)>,
}

impl Deletion {
    /// Deletes what this names and nothing else: a play's folder only when it is a folder that
    /// `Plays::new_folder` named directly inside `plays`, with links resolved, and a file only
    /// when it is a regular file reached through real folders. It never follows a link, and
    /// leaves what `used` says a download on the list uses. Run again, it deletes what is left.
    pub fn run(&self, plays: &Path, used: impl Fn(&Path) -> bool) -> Deleted {
        let mut done = Deleted::default();
        match self {
            Deletion::Play { folder } if used(folder) => done.kept.push((folder.clone(), "another download uses it")),
            Deletion::Play { folder } => match delete_play(plays, folder) {
                Ok(true) => done.deleted.push(folder.clone()),
                Ok(false) => {}
                Err(why) => done.kept.push((folder.clone(), why)),
            },
            Deletion::Files { dir, files } => {
                for file in files {
                    let path = dir.join(file);
                    if used(&path) {
                        done.kept.push((path, "another download uses it"));
                        continue;
                    }
                    match delete_file(dir, file) {
                        Ok(true) => done.deleted.push(path),
                        Ok(false) => {}
                        Err(why) => done.kept.push((path, why)),
                    }
                }
                for file in files {
                    prune(dir, file);
                }
            }
        }
        done
    }

    /// The folder the deletion is in.
    fn place(&self) -> &Path {
        match self {
            Deletion::Play { folder } => folder,
            Deletion::Files { dir, .. } => dir,
        }
    }
}

/// What removing `job` deletes: a play's folder always, and the files of a download in a folder
/// the person chose only when they `delete` them.
pub(super) fn deletion(job: &JobEntry, delete: bool) -> Option<Deletion> {
    match job.record.home {
        Home::Plays(_) => Some(Deletion::Play { folder: job.record.dir.clone() }),
        Home::Folder if delete => {
            let (metainfo, wanted) = (job.metainfo.as_deref()?, job.wanted()?);
            let chosen = metainfo.files.iter().zip(wanted).filter(|(_, wanted)| *wanted);
            let files = chosen.map(|(file, _)| files::relative(metainfo, file)).collect();
            Some(Deletion::Files { dir: job.record.dir.clone(), files })
        }
        Home::Folder => None,
    }
}

/// Deletes a play's folder. False when it is already gone.
fn delete_play(plays: &Path, folder: &Path) -> Result<bool, &'static str> {
    let outside = "it is not a play's folder inside the plays folder";
    let (Some(name), Some(parent)) = (folder.file_name(), folder.parent()) else { return Err(outside) };
    if !play_folder(name) || folder.components().any(|part| matches!(part, Component::ParentDir | Component::CurDir)) {
        return Err(outside);
    }
    let gone = |e: io::Error| if e.kind() == io::ErrorKind::NotFound { Ok(false) } else { Err("it cannot be read") };
    let root = match plays.canonicalize() {
        Ok(root) => root,
        Err(e) => return gone(e),
    };
    match parent.canonicalize() {
        Ok(parent) if parent == root => {}
        Ok(_) => return Err(outside),
        Err(e) => return gone(e),
    }
    let path = root.join(name);
    match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err("it is a link or a file, not a folder"),
        Err(e) => return gone(e),
    }
    // std's remove_dir_all removes a link it meets, never what the link points to, and opens
    // each folder without following links, so a link swapped in while it runs is not followed.
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(_) => Err("it could not be deleted"),
    }
}

/// An open folder, closed when dropped.
struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

fn c_name(name: &OsStr) -> Option<CString> {
    CString::new(name.as_bytes()).ok()
}

/// Opens the folder `inside` leads to below `dir` and names its last part, going down one folder
/// at a time without following a link. `None` when a folder on the way is missing.
fn open_parent<'a>(dir: &Path, inside: &'a Path) -> Result<Option<(Fd, &'a OsStr)>, &'static str> {
    let leaves = "its path leaves the download's folder";
    let mut parts = Vec::new();
    for part in inside.components() {
        match part {
            Component::Normal(name) => parts.push(name),
            _ => return Err(leaves),
        }
    }
    let Some((last, folders)) = parts.split_last() else { return Err(leaves) };
    let dir = c_name(dir.as_os_str()).ok_or("its path cannot be read")?;
    // The download's folder itself may be a link, as ~/Downloads often is.
    let mut fd = Fd(unsafe { libc::open(dir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) });
    if fd.0 < 0 {
        return match errno() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err("the download's folder cannot be opened"),
        };
    }
    for folder in folders {
        let name = c_name(folder).ok_or("its path cannot be read")?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let next = Fd(unsafe { libc::openat(fd.0, name.as_ptr(), flags) });
        if next.0 < 0 {
            return match errno() {
                Some(libc::ENOENT) => Ok(None),
                Some(libc::ELOOP | libc::ENOTDIR) => Err("a folder on its path is a link"),
                _ => Err("a folder on its path cannot be opened"),
            };
        }
        fd = next;
    }
    Ok(Some((fd, last)))
}

fn errno() -> Option<i32> {
    io::Error::last_os_error().raw_os_error()
}

/// Deletes the regular file at `inside` below `dir`. False when nothing is there.
fn delete_file(dir: &Path, inside: &Path) -> Result<bool, &'static str> {
    let Some((parent, name)) = open_parent(dir, inside)? else { return Ok(false) };
    let name = c_name(name).ok_or("its path cannot be read")?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(parent.0, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return match errno() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err("it cannot be read"),
        };
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err("it is not a regular file");
    }
    if unsafe { libc::unlinkat(parent.0, name.as_ptr(), 0) } != 0 {
        return match errno() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err("it could not be deleted"),
        };
    }
    Ok(true)
}

/// Removes the folders on the way to `inside` below `dir` that are left empty, deepest first.
fn prune(dir: &Path, inside: &Path) {
    let mut folder = inside.parent();
    while let Some(path) = folder.filter(|p| !p.as_os_str().is_empty()) {
        let Ok(Some((parent, name))) = open_parent(dir, path) else { return };
        let Some(name) = c_name(name) else { return };
        // Removes only an empty folder, and never through a link.
        if unsafe { libc::unlinkat(parent.0, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return;
        }
        folder = path.parent();
    }
}

/// A play's share of the plays folder, for choosing which plays leave.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Share {
    pub id: TorrentId,
    pub bytes: u64,
    /// Unix seconds.
    pub watched_at: i64,
    /// Someone streams it, or a title play is testing it.
    pub busy: bool,
}

/// The plays that leave so the rest fit in `max`, those watched longest ago first. A busy play
/// never leaves, and neither does the one watched last, so a play larger than `max` still plays
/// and is there to go on with.
pub fn leaving(mut shares: Vec<Share>, max: u64) -> Vec<TorrentId> {
    let mut total: u64 = shares.iter().map(|s| s.bytes).sum();
    shares.sort_by_key(|s| (s.watched_at, s.id));
    shares.pop();
    let mut leaving = Vec::new();
    for share in shares.into_iter().filter(|s| !s.busy) {
        if total <= max {
            break;
        }
        total -= share.bytes;
        leaving.push(share.id);
    }
    leaving
}

/// Someone streaming a download, from their request for its bytes until the answer ends.
pub struct Viewer<'a> {
    owner: &'a OwnerHandle,
    id: TorrentId,
}

impl<'a> Viewer<'a> {
    pub fn new(owner: &'a OwnerHandle, id: TorrentId) -> Self {
        owner.tell(Command::Viewing { id, open: true });
        Self { owner, id }
    }
}

impl Drop for Viewer<'_> {
    fn drop(&mut self) {
        self.owner.tell(Command::Viewing { id: self.id, open: false });
    }
}

/// What the owner keeps track of for plays and deletions.
pub struct Tending {
    /// How many streams of each download are open now.
    viewers: BTreeMap<TorrentId, usize>,
    /// Deletions on record, each waiting until the engine lets go of its download.
    pub(super) doomed: Vec<Pending>,
    /// `rm`s that answer once their deletions are done.
    removals: Vec<Removal>,
    tended_at: Option<Instant>,
}

impl Tending {
    /// What a removal cut short left on record is still to be deleted.
    pub fn new(doomed: Vec<Pending>) -> Self {
        Self { viewers: BTreeMap::new(), doomed, removals: Vec::new(), tended_at: None }
    }
}

/// An `rm` waiting for its deletions.
struct Removal {
    waiting: BTreeSet<TorrentId>,
    changes: Vec<Change>,
    deadline: Instant,
    reply: Reply<Vec<Change>>,
}

impl Owner {
    /// The folder and the home of a new download a play makes. A play into the plays folder
    /// gets a new folder of its own.
    pub(super) fn destination(&self, id: TorrentId, to: Destination) -> Result<(Option<PathBuf>, Home), Error> {
        match to {
            Destination::Folder(dir) => Ok((dir, Home::Folder)),
            Destination::Plays => {
                let plays = &self.config.plays;
                let folder = plays.new_folder(id).map_err(|e| {
                    Error::new(
                        ErrorCode::StorageError,
                        format!("cannot make a folder in {}: {e}", plays.root.display()),
                    )
                })?;
                Ok((Some(folder), Home::Plays(Play { watched_at: now(), by_hand: false })))
            }
        }
    }

    /// Removes the folder `destination` made for a play that did not start. Nothing is in it.
    pub(super) fn unmade(&self, dir: Option<&Path>, home: Home) {
        if let (Some(dir), Home::Plays(_)) = (dir, home) {
            let _ = std::fs::remove_dir(dir);
        }
    }

    /// Someone pressed play on `id`. A play is watched now, and one played to keep is the
    /// person's.
    pub(super) fn pressed_play(&mut self, id: TorrentId, finish: Finish) -> Result<(), Error> {
        self.watched_now(id);
        if finish == Finish::Keep {
            return self.keep_play(id);
        }
        Ok(())
    }

    /// Makes a play the person's, as one they started by hand, so it never leaves the plays
    /// folder or stops by itself. It is theirs from now on even when that cannot be put on
    /// record, until the engine stops.
    pub(super) fn keep_play(&mut self, id: TorrentId) -> Result<(), Error> {
        let Some(job) = self.jobs.get_mut(&id) else { return Ok(()) };
        let Home::Plays(play) = &mut job.record.home else { return Ok(()) };
        if play.by_hand {
            return Ok(());
        }
        play.by_hand = true;
        self.store.by_hand(id).map_err(|error| {
            let message = format!(
                "{} is yours now, but the engine cannot note that, so it may leave the plays folder once the engine \
                 starts again: {}",
                self.named(id),
                error.message
            );
            Error { message, ..error }
        })
    }

    /// Starts the idle clock again for the plays a `resume` resumed.
    pub(super) fn touch_plays(&mut self, changes: &[Change]) {
        for change in changes.iter().filter(|change| change.result == "resumed") {
            if let Ok(id) = change.id.parse() {
                self.watched_now(id);
            }
        }
    }

    fn watched_now(&mut self, id: TorrentId) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let Home::Plays(play) = &mut job.record.home else { return };
        play.watched_at = now();
        if let Err(error) = self.store.watched(id, play.watched_at) {
            log(&format!("cannot note when {id} was watched: {}", error.message));
        }
    }

    /// Someone opened or closed a stream of `id`. A play someone opens resumes if it stopped.
    pub(super) fn viewing(&mut self, id: TorrentId, open: bool) {
        let count = self.tending.viewers.entry(id).or_default();
        let before = *count;
        *count = if open { before + 1 } else { before.saturating_sub(1) };
        let after = *count;
        if after == 0 {
            self.tending.viewers.remove(&id);
        }
        // The first stream to open and the last to close mark when the play was watched.
        if (open && before == 0) || (!open && after == 0) {
            self.watched_now(id);
        }
        let Some(job) = self.jobs.get(&id) else { return };
        let stopped = job.record.intent == Intent::Paused || job.observed == Observed::Parked;
        if open
            && stopped
            && matches!(job.record.home, Home::Plays(_))
            && let Err(error) = self.resume(&[id.to_hex()], false)
        {
            log(&format!("{id} is streaming but cannot resume: {}", error.message));
        }
    }

    /// `rm`: takes downloads off the list, and answers once what it deletes is deleted, or once
    /// the engine has had as long to let go as a pause has to stop writing.
    pub(super) fn rm(&mut self, refs: Vec<String>, delete: bool, reply: Reply<Vec<Change>>) {
        let changes = match self.remove(&refs, delete) {
            Ok(changes) => changes,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let removed: BTreeSet<String> = changes.iter().map(|change| change.id.clone()).collect();
        let waiting = self.tending.doomed.iter().map(|p| p.job).filter(|id| removed.contains(&id.to_hex())).collect();
        let deadline = Instant::now() + self.config.writes_stop_within;
        self.tending.removals.push(Removal { waiting, changes, deadline, reply });
        self.finish_deletions();
    }

    /// Runs each deletion on record whose download the engine has let go of.
    pub(super) fn finish_deletions(&mut self) {
        let doomed = std::mem::take(&mut self.tending.doomed);
        let (ready, waiting): (Vec<Pending>, Vec<Pending>) =
            doomed.into_iter().partition(|pending| !self.draining.contains(&pending.job));
        self.tending.doomed = waiting;
        for pending in ready {
            let done = pending.deletion.run(&self.config.plays.root, |path| self.uses(path));
            for path in &done.deleted {
                log(&format!("deleted {}", path.display()));
            }
            for (path, why) in &done.kept {
                log(&format!("kept {}, for {why}", path.display()));
            }
            // A deletion left on record runs again at the next start, and finds done what is done.
            if let Err(error) = self.store.deleted(pending.key) {
                log(&format!("cannot note that a deletion is done: {}", error.message));
            }
            let id = pending.job.to_hex();
            for removal in &mut self.tending.removals {
                if removal.waiting.remove(&pending.job)
                    && let Some(change) = removal.changes.iter_mut().find(|change| change.id == id)
                {
                    change.deleted.extend(done.deleted.iter().cloned());
                    let kept = done.kept.iter().map(|(path, why)| Kept { path: path.clone(), reason: why.to_string() });
                    change.kept.extend(kept);
                }
            }
        }
        self.finish_removals();
    }

    fn finish_removals(&mut self) {
        let now = Instant::now();
        let (done, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.tending.removals)
            .into_iter()
            .partition(|removal| removal.waiting.is_empty() || removal.deadline <= now);
        self.tending.removals = rest;
        for mut removal in done {
            for pending in self.tending.doomed.iter().filter(|pending| removal.waiting.contains(&pending.job)) {
                let id = pending.job.to_hex();
                let Some(change) = removal.changes.iter_mut().find(|change| change.id == id) else { continue };
                let reason = "the engine still has it open, so it is deleted once the engine lets go";
                change.kept.push(Kept { path: pending.deletion.place().to_path_buf(), reason: reason.into() });
            }
            let _ = removal.reply.send(Ok(removal.changes));
        }
    }

    /// Whether a download on the list writes at or below `path`.
    fn uses(&self, path: &Path) -> bool {
        let key = claim_key(path);
        self.jobs.values().any(|job| {
            let writes = job
                .metainfo
                .as_deref()
                .zip(job.wanted())
                .map(|(metainfo, wanted)| files::targets(metainfo, &job.record.dir, &wanted));
            job.record.dir.starts_with(path)
                || writes.unwrap_or_default().iter().any(|target| target.starts_with(path) || claim_key(target) == key)
        })
    }

    /// Pauses the plays no one has streamed for a while, and takes the plays watched longest ago
    /// out of the plays folder while it is over its limit. A play the person started by hand, or
    /// whose title is in the library, is the person's: it does neither, and does not count.
    pub(super) fn tend_plays(&mut self, at: Instant) {
        if self.stopping.is_some() || self.tending.tended_at.is_some_and(|last| at.duration_since(last) < TEND_EVERY) {
            return;
        }
        self.tending.tended_at = Some(at);
        let saved = match self.store.saved_titles() {
            Ok(saved) => saved,
            Err(error) => {
                log(&format!("cannot read the library, so no play leaves or pauses: {}", error.message));
                return;
            }
        };
        let (clock, idle_after) = (now(), self.config.plays.idle_after);
        let mut idle = Vec::new();
        let mut shares = Vec::new();
        for job in self.jobs.values() {
            let Home::Plays(play) = job.record.home else { continue };
            let in_library = job.record.title.as_ref().is_some_and(|linked| saved.contains(&linked.link.title));
            if play.by_hand || in_library {
                continue;
            }
            let id = job.id();
            let busy = self.tending.viewers.contains_key(&id) || self.under_test.contains_key(&id);
            let unwatched = Duration::from_secs(clock.saturating_sub(play.watched_at).max(0) as u64);
            if !busy && job.record.intent == Intent::Running && unwatched >= idle_after {
                idle.push(id);
            }
            shares.push(Share { id, bytes: self.play_bytes(job), watched_at: play.watched_at, busy });
        }
        let after = crate::output::format::elapsed(idle_after);
        for id in idle {
            log(&format!("{} paused, for no one has streamed it for {after}", self.named(id)));
            self.pause(vec![id.to_hex()], oneshot::channel().0);
        }
        let leaving = leaving(shares, self.config.plays.max);
        if leaving.is_empty() {
            return;
        }
        let max = crate::output::format::size(Some(self.config.plays.max));
        for id in &leaving {
            log(&format!("{} left the plays folder to keep it under {max}", self.named(*id)));
        }
        let refs: Vec<String> = leaving.iter().map(TorrentId::to_hex).collect();
        if let Err(error) = self.remove(&refs, false) {
            log(&format!("the plays watched longest ago stay, for they cannot be removed: {}", error.message));
        }
        self.finish_deletions();
    }

    /// What a play takes up on disk now.
    fn play_bytes(&self, job: &JobEntry) -> u64 {
        let allocated = job.metainfo.as_deref().zip(job.wanted()).map_or(0, |(metainfo, wanted)| {
            admission::allocated_bytes(metainfo, &job.record.dir, &wanted, self.disk.as_ref())
        });
        allocated.max(job.verified())
    }

    fn named(&self, id: TorrentId) -> String {
        let name = self.jobs.get(&id).and_then(|job| job.record.name.clone());
        name.unwrap_or_else(|| id.to_hex()[..6].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn id(n: u8) -> TorrentId {
        TorrentId::from_bytes([n; 20])
    }

    fn plays(dir: &Path) -> Plays {
        Plays { root: dir.join("plays"), max: 1 << 30, idle_after: IDLE_AFTER }
    }

    fn share(n: u8, bytes: u64, watched_at: i64, busy: bool) -> Share {
        Share { id: id(n), bytes, watched_at, busy }
    }

    #[test]
    fn each_play_gets_a_new_folder_even_for_the_same_torrent() {
        let tmp = tempfile::tempdir().unwrap();
        let plays = plays(tmp.path());
        let first = plays.new_folder(id(0xab)).unwrap();
        let second = plays.new_folder(id(0xab)).unwrap();
        assert_eq!(first, tmp.path().join("plays/abababababababab-0"));
        assert_eq!(second, tmp.path().join("plays/abababababababab-1"));
        assert!(play_folder(first.file_name().unwrap()));
        for name in ["abababababababab", "abababababababab-", "ABABABABABABABAB-0", "abab-0", "..", "notes.txt"] {
            assert!(!play_folder(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn the_plays_watched_longest_ago_leave_until_the_rest_fit() {
        let shares =
            vec![share(1, 6, 30, false), share(2, 6, 10, false), share(3, 6, 20, true), share(4, 6, 40, false)];
        assert_eq!(leaving(shares.clone(), 24), Vec::<TorrentId>::new());
        assert_eq!(leaving(shares.clone(), 18), [id(2)]);
        assert_eq!(leaving(shares.clone(), 12), [id(2), id(1)], "3 is busy");
        assert_eq!(leaving(shares, 0), [id(2), id(1)], "and the play watched last never leaves");
        assert_eq!(leaving(vec![share(9, 50, 1, false)], 20), Vec::<TorrentId>::new(), "one play over the cap stays");
    }

    #[test]
    fn a_play_folder_is_deleted_and_a_link_inside_it_is_removed_without_touching_what_it_points_to() {
        let tmp = tempfile::tempdir().unwrap();
        let plays = plays(tmp.path());
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("films")).unwrap();
        std::fs::write(outside.join("films/keep.mp4"), b"keep").unwrap();
        std::fs::write(outside.join("keep.mp4"), b"keep").unwrap();
        let folder = plays.new_folder(id(1)).unwrap();
        std::fs::create_dir(folder.join("Film")).unwrap();
        std::fs::write(folder.join("Film/film.mp4"), b"film").unwrap();
        symlink(outside.join("keep.mp4"), folder.join("Film/film.srt")).unwrap();
        symlink(outside.join("films"), folder.join("Film/extras")).unwrap();
        let deletion = Deletion::Play { folder: folder.clone() };
        assert_eq!(deletion.run(&plays.root, |_| false), Deleted { deleted: vec![folder.clone()], kept: vec![] });
        assert!(!folder.exists());
        assert_eq!(std::fs::read(outside.join("keep.mp4")).unwrap(), b"keep");
        assert_eq!(std::fs::read(outside.join("films/keep.mp4")).unwrap(), b"keep");
        assert_eq!(deletion.run(&plays.root, |_| false), Deleted::default(), "run again, it finds nothing to do");
    }

    #[test]
    fn nothing_outside_the_plays_folder_is_deleted_whatever_the_record_says() {
        let tmp = tempfile::tempdir().unwrap();
        let plays = plays(tmp.path());
        std::fs::create_dir_all(&plays.root).unwrap();
        let outside = tmp.path().join("outside");
        let elsewhere = outside.join("0101010101010101-0");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.mp4"), b"keep").unwrap();
        let planted = plays.root.join("0202020202020202-0");
        symlink(&elsewhere, &planted).unwrap();
        let named = plays.root.join("0303030303030303-0/../../outside/0101010101010101-0");
        let cases = [
            (elsewhere.clone(), "it is not a play's folder inside the plays folder"),
            (outside.clone(), "it is not a play's folder inside the plays folder"),
            (named, "it is not a play's folder inside the plays folder"),
            (planted.clone(), "it is a link or a file, not a folder"),
        ];
        for (folder, why) in cases {
            let done = Deletion::Play { folder: folder.clone() }.run(&plays.root, |_| false);
            assert_eq!(done, Deleted { deleted: vec![], kept: vec![(folder, why)] });
        }
        assert_eq!(std::fs::read(elsewhere.join("keep.mp4")).unwrap(), b"keep");
        assert!(planted.symlink_metadata().is_ok(), "the link itself is left too");
    }

    #[test]
    fn a_plays_folder_that_is_itself_a_link_still_deletes_its_plays() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("disk/plays");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir(tmp.path().join("data")).unwrap();
        symlink(&real, tmp.path().join("data/plays")).unwrap();
        let plays = Plays { root: tmp.path().join("data/plays"), ..plays(tmp.path()) };
        let folder = plays.new_folder(id(4)).unwrap();
        std::fs::write(folder.join("film.mp4"), b"film").unwrap();
        let done = Deletion::Play { folder: folder.clone() }.run(&plays.root, |_| false);
        assert_eq!(done.deleted, [folder]);
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
    }

    #[test]
    fn files_in_a_chosen_folder_go_but_links_and_what_another_download_uses_stay() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Downloads");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(dir.join("Set/docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(dir.join("Set/a.mkv"), b"a").unwrap();
        std::fs::write(dir.join("Set/docs/b.txt"), b"b").unwrap();
        std::fs::write(dir.join("Set/shared.nfo"), b"s").unwrap();
        std::fs::write(dir.join("mine.txt"), b"mine").unwrap();
        std::fs::write(outside.join("c.srt"), b"keep").unwrap();
        std::fs::create_dir(outside.join("extras")).unwrap();
        std::fs::write(outside.join("extras/d.txt"), b"keep").unwrap();
        symlink(outside.join("c.srt"), dir.join("Set/c.srt")).unwrap();
        symlink(outside.join("extras"), dir.join("Set/extras")).unwrap();
        let files: Vec<PathBuf> = [
            "Set/a.mkv",
            "Set/docs/b.txt",
            "Set/c.srt",
            "Set/extras/d.txt",
            "Set/shared.nfo",
            "Set/gone.mkv",
            "../mine.txt",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let deletion = Deletion::Files { dir: dir.clone(), files };
        let shared = dir.join("Set/shared.nfo");
        let done = deletion.run(Path::new("/nowhere"), |path| path == shared);
        assert_eq!(
            done,
            Deleted {
                deleted: vec![dir.join("Set/a.mkv"), dir.join("Set/docs/b.txt")],
                kept: vec![
                    (dir.join("Set/c.srt"), "it is not a regular file"),
                    (dir.join("Set/extras/d.txt"), "a folder on its path is a link"),
                    (shared.clone(), "another download uses it"),
                    (dir.join("../mine.txt"), "its path leaves the download's folder"),
                ]
            }
        );
        assert!(!dir.join("Set/docs").exists(), "a folder it left empty goes");
        assert!(dir.join("Set").exists(), "a folder with something left in it stays");
        assert_eq!(std::fs::read(outside.join("c.srt")).unwrap(), b"keep");
        assert_eq!(std::fs::read(outside.join("extras/d.txt")).unwrap(), b"keep");
        assert_eq!(std::fs::read(dir.join("mine.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(&shared).unwrap(), b"s");
    }
}
