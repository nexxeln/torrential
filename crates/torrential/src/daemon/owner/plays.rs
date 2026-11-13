//! torrential's own folder for plays. Each play downloads into a folder of its own inside it.
//! Plays no one streams stop downloading, the plays watched longest ago leave once the folder is
//! over its limit, and torrential deletes only inside it, unless the person asks.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use torrential_core::protocol::{Change, Kept};
use torrential_core::{Error, TorrentId};

use super::super::admission;
use super::super::book::{Home, Intent, JobEntry, Observed, Play};
use super::super::files;
use super::super::log;
use super::super::server::OwnerHandle;
use super::deleter::{Deleter, Doom, InUse};
use super::{Command, Finish, Owner, Reply, now};

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

/// Where a play's new download goes. A play that joins a download leaves it where it is.
#[derive(Clone, Debug)]
pub enum Destination {
    /// A new folder in the plays folder.
    Plays,
    /// This folder, or the downloads folder.
    Folder(Option<PathBuf>),
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
    pub(super) deleter: Deleter,
    /// The folder and the files of each download taken off the list that the engine has yet to
    /// let go of.
    pub(super) letting_go: BTreeMap<TorrentId, (PathBuf, Vec<PathBuf>)>,
    /// `rm`s that answer once their deletions are done.
    removals: Vec<Removal>,
    tended_at: Option<Instant>,
    /// Why no play may leave the plays folder now, once said.
    refused: Option<String>,
}

impl Tending {
    pub fn new(deleter: Deleter) -> Self {
        Self {
            viewers: BTreeMap::new(),
            deleter,
            letting_go: BTreeMap::new(),
            removals: Vec::new(),
            tended_at: None,
            refused: None,
        }
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
                let folder = self.tending.deleter.make_play(id)?;
                Ok((Some(folder), Home::Plays(Play { watched_at: now(), by_hand: false })))
            }
        }
    }

    /// Removes the folder `destination` made for a play that did not start. Nothing is in it.
    pub(super) fn unmade(&self, dir: Option<&Path>, home: Home) {
        if let (Some(dir), Home::Plays(_)) = (dir, home) {
            self.tending.deleter.unmake_play(dir);
        }
    }

    /// What removing `job` leaves to delete: a play's folder always, and the files of a download
    /// in a folder the person chose only when they `delete` them.
    pub(super) fn doom(&self, job: &JobEntry, delete: bool) -> Option<Doom> {
        match job.record.home {
            Home::Plays(_) => Some(Doom::Play { dir: job.record.dir.clone(), bytes: self.play_bytes(job) }),
            Home::Folder if delete => {
                let (metainfo, wanted) = (job.metainfo.as_deref()?, job.wanted()?);
                let chosen = metainfo.files.iter().zip(wanted).filter(|(_, wanted)| *wanted);
                let files = chosen.map(|(file, _)| files::relative(metainfo, file)).collect();
                Some(Doom::Files { dir: job.record.dir.clone(), files })
            }
            Home::Folder => None,
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
        let removed = changes.iter().filter_map(|change| change.id.parse().ok());
        let waiting = removed.filter(|id| self.tending.deleter.waiting_in(*id).is_some()).collect();
        let deadline = Instant::now() + self.config.writes_stop_within;
        self.tending.removals.push(Removal { waiting, changes, deadline, reply });
        self.finish_deletions();
    }

    /// Runs each deletion whose download the engine has let go of, and answers the `rm`s whose
    /// deletions are done.
    pub(super) fn finish_deletions(&mut self) {
        if self.tending.deleter.ready(&self.draining) {
            let in_use = self.in_use();
            let in_use = in_use.as_ref().map_err(String::as_str);
            for (job, done) in self.tending.deleter.run(&self.draining, in_use, &mut self.store) {
                let id = job.to_hex();
                for removal in &mut self.tending.removals {
                    if removal.waiting.remove(&job)
                        && let Some(change) = removal.changes.iter_mut().find(|change| change.id == id)
                    {
                        change.deleted.extend(done.deleted.iter().cloned());
                        let kept = done.kept.iter().map(|(path, why)| Kept { path: path.clone(), reason: why.clone() });
                        change.kept.extend(kept);
                    }
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
            for job in &removal.waiting {
                let id = job.to_hex();
                let Some(change) = removal.changes.iter_mut().find(|change| change.id == id) else { continue };
                let Some(place) = self.tending.deleter.waiting_in(*job) else { continue };
                let reason = "the engine still has it open, so it is deleted once the engine lets go";
                change.kept.push(Kept { path: place.to_path_buf(), reason: reason.into() });
            }
            let _ = removal.reply.send(Ok(removal.changes));
        }
    }

    /// The folder and the files `job` writes.
    pub(super) fn writes(job: &JobEntry) -> (PathBuf, Vec<PathBuf>) {
        let targets = job
            .metainfo
            .as_deref()
            .zip(job.wanted())
            .map(|(metainfo, wanted)| files::targets(metainfo, &job.record.dir, &wanted));
        (job.record.dir.clone(), targets.unwrap_or_default())
    }

    /// What the downloads on the list, and those the engine has yet to let go of, write, found by
    /// what each file and folder is rather than by its path, so no other name or link hides one.
    fn in_use(&self) -> Result<InUse, String> {
        let mut in_use = InUse::default();
        let listed = self.jobs.values().map(Self::writes);
        for (dir, targets) in listed.chain(self.tending.letting_go.values().cloned()) {
            in_use
                .add(&dir, &targets)
                .map_err(|e| format!("torrential cannot tell whether another download uses it: {e}"))?;
        }
        Ok(in_use)
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
        // What is still to be deleted takes up room too.
        let room = self.config.plays.max.saturating_sub(self.tending.deleter.play_bytes());
        let leaving = leaving(shares, room);
        if leaving.is_empty() {
            return;
        }
        if let Err(why) = self.tending.deleter.can_delete_plays() {
            if self.tending.refused.as_ref() != Some(&why) {
                log(&format!("no play leaves the plays folder, for {why}"));
            }
            self.tending.refused = Some(why);
            return;
        }
        self.tending.refused = None;
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

    fn id(n: u8) -> TorrentId {
        TorrentId::from_bytes([n; 20])
    }

    fn share(n: u8, bytes: u64, watched_at: i64, busy: bool) -> Share {
        Share { id: id(n), bytes, watched_at, busy }
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
}
