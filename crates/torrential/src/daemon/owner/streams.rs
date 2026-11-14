//! The downloads someone is streaming now, and how fast the other downloads may go meanwhile.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use torrential_core::{Slice, TorrentId};

use super::super::book::{Intent, JobEntry, Observed, Phase};
use super::super::engine_port::{EngineCommand, EngineHandle};
use super::Owner;

/// A film with less than this on disk ahead of its player holds the other downloads back. It
/// gives the line time to turn toward the film before the player runs out.
const BEHIND: Duration = Duration::from_secs(60);
/// Once every film being watched has this much on disk ahead of its player, the other downloads
/// go. A film let go at this lead drains for a minute before it holds them back again, so the
/// caps change at most about once a minute. Its peers take up to half a minute to come back
/// once the others are held again, which `BEHIND` leaves room for.
const AHEAD: Duration = Duration::from_secs(120);
/// A film whose player has taken nothing for this long is paused, and holds nothing back.
const IDLE: Duration = Duration::from_secs(60);
/// What a held-back download may take, so its peers stay connected.
const TRICKLE: NonZeroU32 = NonZeroU32::new(64 * 1024).unwrap();
/// How long a film whose runtime is unknown is taken to play. Most films run longer, so their
/// lead is counted short and the other downloads wait longer rather than less.
const GUESSED_RUNTIME: Duration = Duration::from_secs(45 * 60);
/// How often the other downloads' limits follow the films, and each lead is measured.
const DECIDE_EVERY: Duration = Duration::from_secs(1);

/// How far one answer has got through the file it sends a player.
#[derive(Clone, Copy, Default, Debug)]
struct Reading {
    /// The next byte the answer sends.
    at: u64,
    /// When the answer began handing the player bytes it has yet to take.
    writing_since: Option<Instant>,
    /// How long the film on disk after `at` plays, once measured.
    lead: Option<Duration>,
}

impl Reading {
    /// Whether the player has left the answer's bytes untaken so long that it must be paused.
    fn idle(&self, now: Instant) -> bool {
        self.writing_since.is_some_and(|since| now.duration_since(since) >= IDLE)
    }
}

/// Someone streaming a download, from their request for its bytes until the answer ends.
pub struct Lease {
    reading: Arc<Mutex<Reading>>,
}

impl Lease {
    /// The answer waits for the engine to write the bytes at `at`.
    pub fn waiting(&self, at: u64) {
        let mut reading = self.reading.lock().expect("no holder panics");
        (reading.at, reading.writing_since) = (at, None);
    }

    /// The answer hands the player the bytes at `at`.
    pub fn writing(&self, at: u64) {
        let mut reading = self.reading.lock().expect("no holder panics");
        (reading.at, reading.writing_since) = (at, Some(Instant::now()));
    }

    /// Measures how far the film on disk runs ahead of the answer, at once and then each second
    /// for as long as it runs. `runtime` is how long file `file` of `id` plays, once something has
    /// read it.
    pub async fn follow(
        &self,
        engine: &EngineHandle,
        id: TorrentId,
        file: u32,
        size: u64,
        runtime: impl Fn() -> Option<Duration>,
    ) -> Infallible {
        loop {
            let at = self.reading.lock().expect("no holder panics").at;
            let rate = bitrate(size, runtime());
            let window = rate.saturating_mul(AHEAD.as_secs()).min(size.saturating_sub(at));
            let measured = match engine.written(id, file, at, window, AHEAD.as_secs() as u32).await {
                Ok(runs) => lead(&runs, window, rate, at + window == size),
                // The engine lets go of a download once it is done, and has nothing past the end.
                Err(_) => Duration::MAX,
            };
            self.reading.lock().expect("no holder panics").lead = Some(measured);
            tokio::time::sleep(DECIDE_EVERY).await;
        }
    }
}

/// Bytes of a file of `size` bytes that play each second, as if it played evenly for `runtime`.
fn bitrate(size: u64, runtime: Option<Duration>) -> u64 {
    let runtime = runtime.filter(|runtime| !runtime.is_zero()).unwrap_or(GUESSED_RUNTIME);
    ((size as f64 / runtime.as_secs_f64()) as u64).max(1)
}

/// How long the film on disk after a reader plays at `rate` bytes a second, from how much of each
/// of the equal runs of the `window` bytes after the reader is on disk. A film on disk all the way
/// to its end plays on for good.
fn lead(runs: &[Slice], window: u64, rate: u64, to_the_end: bool) -> Duration {
    let on_disk = runs.iter().take_while(|run| run.is_complete()).count();
    if on_disk == runs.len() && to_the_end {
        return Duration::MAX;
    }
    let bytes = window * on_disk as u64 / runs.len() as u64;
    Duration::from_secs_f64(bytes as f64 / rate as f64)
}

/// How the other downloads stand toward the films being watched.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Others {
    /// They go as fast as they can.
    #[default]
    Free,
    /// They trickle, so the films being watched have the line.
    Held,
}

/// How the other downloads stand, from how they stood and the readings of the films still
/// downloading. A lead between the two marks leaves them as they were. A lease no answer has
/// measured, like the one a cast holds on its download, counts for nothing.
fn decide(before: Others, readings: impl IntoIterator<Item = Reading>, now: Instant) -> Others {
    let mut after = Others::Free;
    for lead in readings.into_iter().filter(|reading| !reading.idle(now)).filter_map(|reading| reading.lead) {
        if lead < BEHIND {
            return Others::Held;
        }
        if lead < AHEAD {
            after = before;
        }
    }
    after
}

/// Whether someone is watching a download now: a reader an answer has measured, that has taken
/// something lately. A cast's lease, which nothing reads through, does not count, so a paused
/// cast's download is held back like any other.
fn watched(readings: &[Reading], now: Instant) -> bool {
    readings.iter().any(|reading| reading.lead.is_some() && !reading.idle(now))
}

#[derive(Default)]
pub struct Streams {
    /// The answers streaming each download. A lease holds each reading for as long as it lasts,
    /// and a download stays until the owner notes that its last stream closed.
    readers: BTreeMap<TorrentId, Vec<Weak<Mutex<Reading>>>>,
    others: Others,
    /// The downloads the engine holds to a trickle.
    held: BTreeSet<TorrentId>,
    decided_at: Option<Instant>,
}

/// Whether `job` is taking pieces from peers.
fn downloading(job: &JobEntry) -> bool {
    job.record.intent == Intent::Running
        && matches!(job.observed, Observed::InEngine(Phase::Metadata | Phase::Checking | Phase::Downloading))
}

impl Owner {
    /// Someone starts streaming `id`, until they drop the lease.
    pub(super) fn lease(&mut self, id: TorrentId) -> Lease {
        let first = !self.streamed_now(id);
        let reading = Arc::default();
        self.streams.readers.entry(id).or_default().push(Arc::downgrade(&reading));
        if first {
            self.first_stream(id);
        }
        Lease { reading }
    }

    /// Whether someone holds a lease on `id`.
    pub(super) fn streamed_now(&self, id: TorrentId) -> bool {
        self.streams.readers.get(&id).is_some_and(|readers| readers.iter().any(|reading| reading.strong_count() > 0))
    }

    /// The downloads whose last stream closed since this was last asked, which it forgets.
    pub(super) fn forget_closed_streams(&mut self) -> Vec<TorrentId> {
        let mut closed = Vec::new();
        self.streams.readers.retain(|id, readers| {
            readers.retain(|reading| reading.strong_count() > 0);
            if readers.is_empty() {
                closed.push(*id);
            }
            !readers.is_empty()
        });
        closed
    }

    /// Holds every download no one is watching to a trickle, from when a film being watched is
    /// less than `BEHIND` ahead of its player until every one is `AHEAD` ahead.
    pub(super) fn prioritize(&mut self, now: Instant) {
        let streams = &mut self.streams;
        if streams.decided_at.is_some_and(|at| now.duration_since(at) < DECIDE_EVERY) {
            return;
        }
        streams.decided_at = Some(now);
        let readings = |id: &TorrentId| -> Vec<Reading> {
            let readers = streams.readers.get(id).into_iter().flatten().filter_map(Weak::upgrade);
            readers.map(|reading| *reading.lock().expect("no holder panics")).collect()
        };
        let downloading: Vec<(TorrentId, Vec<Reading>)> =
            self.jobs.values().filter(|job| downloading(job)).map(|job| (job.id(), readings(&job.id()))).collect();
        let before = streams.others;
        streams.others = decide(before, downloading.iter().flat_map(|(_, r)| r.iter().copied()), now);
        match (before, streams.others) {
            (Others::Free, Others::Held) => super::super::log(&format!(
                "a film is less than {}s ahead of its player, so the other downloads trickle",
                BEHIND.as_secs()
            )),
            (Others::Held, Others::Free) => super::super::log(&format!(
                "no film is less than {}s ahead of its player, so the other downloads go",
                AHEAD.as_secs()
            )),
            _ => {}
        }
        let held: BTreeSet<TorrentId> = match streams.others {
            Others::Free => BTreeSet::new(),
            Others::Held => {
                downloading.iter().filter(|(_, readings)| !watched(readings, now)).map(|(id, _)| *id).collect()
            }
        };
        // Sent every time, for a download the engine adds again starts with no limit.
        for &id in &held {
            self.engine.send(EngineCommand::Limit(id, Some(TRICKLE)));
        }
        for &id in streams.held.difference(&held) {
            if self.jobs.get(&id).is_some_and(|job| job.observed.in_engine()) {
                self.engine.send(EngineCommand::Limit(id, None));
            }
        }
        streams.held = held;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ahead(seconds: u64) -> Reading {
        Reading { at: 0, writing_since: None, lead: Some(Duration::from_secs(seconds)) }
    }

    #[test]
    fn a_film_less_than_60_s_ahead_of_its_player_holds_the_other_downloads_back() {
        let now = Instant::now();
        assert_eq!(decide(Others::Free, [ahead(59)], now), Others::Held);
        assert_eq!(decide(Others::Free, [ahead(60)], now), Others::Free);
    }

    #[test]
    fn the_other_downloads_go_once_every_film_is_120_s_ahead_of_its_player() {
        let now = Instant::now();
        assert_eq!(decide(Others::Held, [ahead(119)], now), Others::Held);
        assert_eq!(decide(Others::Held, [ahead(120)], now), Others::Free);
        assert_eq!(decide(Others::Held, [ahead(120), ahead(90)], now), Others::Held);
        assert_eq!(decide(Others::Held, [], now), Others::Free);
    }

    #[test]
    fn a_download_only_a_cast_holds_is_not_watched_and_is_held_back_with_the_others() {
        let now = Instant::now();
        let cast = Reading { at: 0, writing_since: None, lead: None };
        assert!(!watched(&[cast], now));
        assert!(watched(&[cast, ahead(10)], now));
    }

    #[test]
    fn a_film_whose_player_has_taken_nothing_for_a_minute_holds_nothing_back() {
        let wrote = Instant::now();
        let paused = Reading { writing_since: Some(wrote), ..ahead(0) };
        assert_eq!(decide(Others::Held, [paused], wrote + Duration::from_secs(59)), Others::Held);
        assert_eq!(decide(Others::Held, [paused], wrote + IDLE), Others::Free);
    }

    #[test]
    fn a_second_film_opening_behind_holds_the_others_back_though_the_first_is_ahead() {
        let now = Instant::now();
        assert_eq!(decide(Others::Free, [ahead(120), ahead(10)], now), Others::Held);
    }

    #[test]
    fn a_lease_no_answer_has_measured_holds_nothing_back() {
        let now = Instant::now();
        let holding = Reading { lead: None, ..ahead(0) };
        assert_eq!(decide(Others::Held, [holding], now), Others::Free);
        assert_eq!(decide(Others::Free, [ahead(120), holding], now), Others::Free);
    }

    #[test]
    fn a_lead_is_the_film_on_disk_in_a_row_after_the_reader() {
        let (done, half) = (Slice::of(4, 4), Slice::of(2, 4));
        // Three runs of a megabyte, of a film that plays a megabyte a second.
        let lead = |runs: &[Slice], to_the_end| lead(runs, 3_000_000, 1_000_000, to_the_end);
        assert_eq!(lead(&[done, done, half], false), Duration::from_secs(2));
        assert_eq!(lead(&[half, done, done], false), Duration::ZERO);
        assert_eq!(lead(&[done, done, done], false), Duration::from_secs(3));
        assert_eq!(lead(&[done, done, done], true), Duration::MAX);
    }

    #[test]
    fn a_film_of_unknown_runtime_is_taken_to_play_for_45_minutes() {
        assert_eq!(bitrate(2_700_000_000, None), 1_000_000);
        assert_eq!(bitrate(2_700_000_000, Some(Duration::from_secs(5400))), 500_000);
    }
}
