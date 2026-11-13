//! The downloads someone is streaming now, and how fast the other downloads may go meanwhile.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use torrential_core::TorrentId;

use super::super::book::{Intent, JobEntry, Observed, Phase};
use super::super::engine_port::EngineCommand;
use super::Owner;

/// How often the other downloads' limits follow the streams.
const DECIDE_EVERY: Duration = Duration::from_secs(1);
/// What a held-back download may take, so its peers stay connected.
const TRICKLE: NonZeroU32 = NonZeroU32::new(64 * 1024).unwrap();

/// Someone streaming a download, from their request for its bytes until the answer ends.
pub struct Lease {
    _held: Arc<()>,
}

#[derive(Default)]
pub struct Streams {
    /// The downloads someone streams. Each counts one more than the leases held on it.
    open: BTreeMap<TorrentId, Arc<()>>,
    /// The downloads held back to make room for the streams.
    limited: BTreeSet<TorrentId>,
    decided_at: Option<Instant>,
}

/// Whether `job` is taking pieces from peers.
fn downloading(job: &JobEntry) -> bool {
    job.record.intent == Intent::Running
        && matches!(job.observed, Observed::InEngine(Phase::Metadata | Phase::Checking | Phase::Downloading))
}

impl Owner {
    /// Someone starts streaming `id`, until they drop the lease. The other downloads make room at
    /// the next tick.
    pub(super) fn lease(&mut self, id: TorrentId) -> Lease {
        self.streams.decided_at = None;
        Lease { _held: Arc::clone(self.streams.open.entry(id).or_default()) }
    }

    /// While a download someone streams is still downloading, holds the other downloads to a
    /// trickle. Once none is, lets them go.
    pub(super) fn prioritize(&mut self, now: Instant) {
        let streams = &mut self.streams;
        if streams.decided_at.is_some_and(|at| now.duration_since(at) < DECIDE_EVERY) {
            return;
        }
        streams.decided_at = Some(now);
        streams.open.retain(|_, held| Arc::strong_count(held) > 1);
        let (fed, others): (Vec<&JobEntry>, Vec<&JobEntry>) =
            self.jobs.values().filter(|job| downloading(job)).partition(|job| streams.open.contains_key(&job.id()));
        let mut held = BTreeSet::new();
        if !fed.is_empty() {
            // Sent every time, for a download the engine adds again starts with no limit.
            for job in others {
                self.engine.send(EngineCommand::Limit(job.id(), Some(TRICKLE)));
                held.insert(job.id());
            }
        }
        for &id in streams.limited.difference(&held) {
            if self.jobs.get(&id).is_some_and(|job| job.observed.in_engine()) {
                self.engine.send(EngineCommand::Limit(id, None));
            }
        }
        streams.limited = held;
    }
}
