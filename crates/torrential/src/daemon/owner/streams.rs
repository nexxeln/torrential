//! The downloads someone is streaming now, and how fast the other downloads may go meanwhile.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use torrential_core::TorrentId;

use super::super::book::{Intent, JobEntry, Observed, Phase};
use super::super::engine_port::EngineCommand;
use super::Owner;

/// How often the other downloads' limits follow the streams.
const DECIDE_EVERY: Duration = Duration::from_secs(1);
/// How long the fastest the downloads went together stands for what the connection can do.
const REMEMBER_FASTEST: Duration = Duration::from_secs(60 * 60);
/// The streams can always speed up into a quarter of the connection.
const ROOM: u64 = 4;
/// The other downloads always have a tenth of the connection between them, so none stalls.
const FLOOR: u64 = 10;
/// The least a download is held to, before torrential has seen the connection go fast.
const SLOWEST: NonZeroU32 = NonZeroU32::new(16 * 1024).unwrap();

/// Someone streaming a download, from their request for its bytes until the answer ends.
pub struct Lease {
    _held: Arc<()>,
}

#[derive(Default)]
pub struct Streams {
    /// The downloads someone streams. Each counts one more than the leases held on it.
    open: BTreeMap<TorrentId, Arc<()>>,
    fastest: Fastest,
    /// What the other downloads may take between them, in bytes a second, while a stream
    /// downloads.
    others: Option<u64>,
    /// The downloads held back to make room for the streams.
    limited: BTreeSet<TorrentId>,
    decided_at: Option<Instant>,
}

/// The fastest the downloads went together within the last hour.
#[derive(Default)]
struct Fastest {
    /// Each rate is faster than every rate after it, so the first is the fastest.
    rates: VecDeque<(Instant, u64)>,
}

impl Fastest {
    fn sample(&mut self, at: Instant, rate: u64) {
        while self.rates.back().is_some_and(|&(_, slower)| slower <= rate) {
            self.rates.pop_back();
        }
        self.rates.push_back((at, rate));
        while self.rates.front().is_some_and(|&(then, _)| at.duration_since(then) > REMEMBER_FASTEST) {
            self.rates.pop_front();
        }
    }

    fn rate(&self) -> u64 {
        self.rates.front().map_or(0, |&(_, rate)| rate)
    }
}

/// What the other downloads may take between them, in bytes a second, while the streamed
/// downloads take `streams` of a connection that went at `fastest`. Their share is what the
/// streams leave, less the room kept for the streams, and never less than the floor. They drop to
/// the floor when a stream opens, so it gets the connection at once, then grow by half each second
/// until they reach their share.
fn others_share(fastest: u64, streams: u64, before: Option<u64>) -> u64 {
    let floor = fastest / FLOOR;
    let share = (fastest - fastest / ROOM).saturating_sub(streams).max(floor);
    before.map_or(floor, |before| share.min(before + before / 2).max(floor))
}

/// The limit for one of `count` downloads that share `share` bytes a second, while together they
/// take `together` and this one takes `own`. It has an even part of the share, and more when the
/// others leave theirs unused.
fn limit(share: u64, count: usize, together: u64, own: u64) -> NonZeroU32 {
    let even = share / count.max(1) as u64;
    let bytes = even.max(share.saturating_sub(together.saturating_sub(own)));
    NonZeroU32::new(u32::try_from(bytes).unwrap_or(u32::MAX)).map_or(SLOWEST, |bytes| bytes.max(SLOWEST))
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

    /// While a download someone streams is still downloading, holds the other downloads to what
    /// it leaves of the connection. Once none is, lets them go.
    pub(super) fn prioritize(&mut self, now: Instant) {
        let streams = &mut self.streams;
        if streams.decided_at.is_some_and(|at| now.duration_since(at) < DECIDE_EVERY) {
            return;
        }
        streams.decided_at = Some(now);
        streams.open.retain(|_, held| Arc::strong_count(held) > 1);
        let downloading: Vec<&JobEntry> = self.jobs.values().filter(|job| downloading(job)).collect();
        streams.fastest.sample(now, downloading.iter().map(|job| job.stats.down_rate).sum());
        let (fed, others): (Vec<&JobEntry>, Vec<&JobEntry>) =
            downloading.into_iter().partition(|job| streams.open.contains_key(&job.id()));
        let fed_rate = fed.iter().map(|job| job.stats.down_rate).sum();
        streams.others = (!fed.is_empty()).then(|| others_share(streams.fastest.rate(), fed_rate, streams.others));
        let mut held = BTreeSet::new();
        if let Some(share) = streams.others {
            let (count, together) = (others.len(), others.iter().map(|job| job.stats.down_rate).sum());
            // Sent every time, for a download the engine adds again starts with no limit.
            for job in others {
                let limit = limit(share, count, together, job.stats.down_rate);
                self.engine.send(EngineCommand::Limit(job.id(), Some(limit)));
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

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_other_downloads_drop_to_a_tenth_when_a_stream_opens() {
        assert_eq!(others_share(100 * MIB, 0, None), 10 * MIB);
    }

    #[test]
    fn the_other_downloads_grow_back_by_half_to_what_a_slow_stream_leaves_less_a_quarter() {
        assert_eq!(others_share(100 * MIB, 20 * MIB, Some(10 * MIB)), 15 * MIB);
        assert_eq!(others_share(100 * MIB, 20 * MIB, Some(50 * MIB)), 55 * MIB);
        assert_eq!(others_share(100 * MIB, 20 * MIB, Some(55 * MIB)), 55 * MIB);
    }

    #[test]
    fn a_stream_that_fills_the_connection_leaves_the_others_a_tenth() {
        assert_eq!(others_share(100 * MIB, 90 * MIB, Some(55 * MIB)), 10 * MIB);
    }

    fn bytes(limit: NonZeroU32) -> u64 {
        u64::from(limit.get())
    }

    #[test]
    fn the_other_downloads_split_their_share_evenly_when_each_uses_its_part() {
        assert_eq!(bytes(limit(55 * MIB, 5, 55 * MIB, 11 * MIB)), 11 * MIB);
    }

    #[test]
    fn a_download_may_take_what_a_slower_one_leaves_of_the_share() {
        assert_eq!(bytes(limit(20 * MIB, 2, 12 * MIB, 10 * MIB)), 18 * MIB);
        assert_eq!(bytes(limit(20 * MIB, 2, 12 * MIB, 2 * MIB)), 10 * MIB);
    }

    #[test]
    fn a_download_is_never_held_below_sixteen_kib_a_second() {
        assert_eq!(bytes(limit(0, 3, 0, 0)), 16 * 1024);
        assert_eq!(bytes(limit(30 * 1024, 2, 30 * 1024, 15 * 1024)), 16 * 1024);
    }

    #[test]
    fn a_share_past_what_a_limit_holds_becomes_the_largest_limit() {
        assert_eq!(bytes(limit(48 * 1024 * MIB, 1, 0, 0)), u64::from(u32::MAX));
    }

    #[test]
    fn the_fastest_rate_is_forgotten_after_an_hour() {
        let start = Instant::now();
        let mut fastest = Fastest::default();
        fastest.sample(start, 50);
        fastest.sample(start + Duration::from_secs(1), 80);
        fastest.sample(start + Duration::from_secs(2), 30);
        assert_eq!(fastest.rate(), 80);
        fastest.sample(start + Duration::from_secs(3601), 10);
        assert_eq!(fastest.rate(), 80, "80 is exactly an hour old");
        fastest.sample(start + Duration::from_secs(3602), 10);
        assert_eq!(fastest.rate(), 30);
        fastest.sample(start + Duration::from_secs(3603), 10);
        assert_eq!(fastest.rate(), 10);
    }
}
