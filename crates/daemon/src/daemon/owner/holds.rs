//! Title plays' holds on the downloads they test, and what each download is owed once no play
//! tests it.

use std::collections::{BTreeMap, BTreeSet};

use torrential_core::{Error, TorrentId};

use super::super::book::{Intent, JobEntry, Observed, Phase};
use super::Hold;

/// The downloads title plays are testing or have yet to put back. Only in memory: after a
/// restart no play is running, so nothing is under test and every download stays as it is.
#[derive(Default)]
pub(super) struct Holds(BTreeMap<TorrentId, UnderTest>);

impl Holds {
    /// `hold` tests `id`, which was `before` when the first play reached it.
    pub(super) fn test(&mut self, id: TorrentId, hold: Hold, before: Before) {
        self.0.entry(id).or_insert(UnderTest { holds: BTreeSet::new(), before }).holds.insert(hold);
    }

    /// The person took `id` over, so whatever it was before, it stays theirs.
    pub(super) fn adopt(&mut self, id: TorrentId) {
        if let Some(test) = self.0.get_mut(&id) {
            test.before = Before::Kept;
        }
    }

    /// `id` is owed nothing more: it played, it is gone, or it was put back.
    pub(super) fn end(&mut self, id: TorrentId) {
        self.0.remove(&id);
    }

    /// Gives up `hold` on every download it tests.
    pub(super) fn release(&mut self, hold: Hold) {
        for test in self.0.values_mut() {
            test.holds.remove(&hold);
        }
    }

    pub(super) fn testing(&self, id: TorrentId) -> bool {
        self.0.contains_key(&id)
    }

    pub(super) fn ids(&self) -> Vec<TorrentId> {
        self.0.keys().copied().collect()
    }

    /// What `id` is owed now that no play tests it, or `None` while one still does. A download
    /// no longer on the list is owed nothing.
    pub(super) fn owed(
        &self,
        id: TorrentId,
        job: Option<&JobEntry>,
        on_disk: impl FnOnce(&JobEntry) -> OnDisk,
    ) -> Option<PutBack> {
        let test = self.0.get(&id).filter(|test| test.holds.is_empty())?;
        Some(job.map_or(PutBack::Nothing, |job| test.before.put_back(job, || on_disk(job))))
    }
}

/// A download title plays are testing, or one the last of them gave up and that is not yet put
/// back.
struct UnderTest {
    /// The plays testing it. Empty once the last has given up.
    holds: BTreeSet<Hold>,
    before: Before,
}

/// What a download was before the first title play to test it reached it.
pub(super) enum Before {
    /// Not on this machine. The play made it.
    Absent,
    Paused,
    Failed(Error),
    /// Running or done, which a play leaves as it is.
    Kept,
}

/// What a download no title play tests any more is still owed.
pub(super) enum PutBack {
    Nothing,
    /// Nothing until the engine has stopped writing it, which shows what it left on disk.
    Wait,
    Pause,
    Fail(Error),
    /// Takes it off the list. Nothing of it is on disk.
    Remove,
}

/// What a download has on disk.
pub(super) enum OnDisk {
    /// The engine may still write it.
    Unsettled,
    Nothing,
    Something,
}

impl Before {
    /// How to put `job` back as it was. No file is ever deleted. A download that was here stays,
    /// paused or stopped again if the play resumed it. A download the play made stays once it
    /// is finished or has played. Otherwise it is stopped, and it leaves the list only when
    /// nothing of it is on disk, so no file is left without a download to show for it.
    fn put_back(&self, job: &JobEntry, on_disk: impl FnOnce() -> OnDisk) -> PutBack {
        let finished = matches!(job.record.intent, Intent::Done { .. })
            || matches!(job.observed, Observed::InEngine(Phase::Finishing { .. }));
        let running = job.record.intent == Intent::Running;
        match self {
            Before::Kept => PutBack::Nothing,
            Before::Paused if running => PutBack::Pause,
            Before::Failed(error) if running => PutBack::Fail(error.clone()),
            Before::Paused | Before::Failed(_) => PutBack::Nothing,
            Before::Absent if finished || job.record.title.as_ref().is_none_or(|linked| linked.played) => {
                PutBack::Nothing
            }
            Before::Absent => match (on_disk(), running) {
                (OnDisk::Nothing, _) => PutBack::Remove,
                (OnDisk::Unsettled | OnDisk::Something, true) => PutBack::Pause,
                (OnDisk::Unsettled, false) => PutBack::Wait,
                (OnDisk::Something, false) => PutBack::Nothing,
            },
        }
    }
}
