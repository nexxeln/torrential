//! The downloads someone is streaming now, and how fast the other downloads may go meanwhile.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use torrential_core::{Error, Slice, TorrentId};

use super::super::book::{Intent, JobEntry, Observed, Phase};
use super::super::engine_port::{EngineCommand, EngineHandle};
use super::super::playing::{Open, Playing};
use super::Owner;

/// A film with less than this on disk ahead of its player holds the other downloads back. It
/// gives the line time to turn toward the film before the player runs out.
const BEHIND: Duration = Duration::from_secs(60);
/// Once every film being watched has this much on disk ahead of its player, the other downloads
/// go. A film let go at this lead drains for a minute before it holds them back again, so the
/// caps change at most about once a minute. Its peers take up to half a minute to come back
/// once the others are held again, which `BEHIND` leaves room for.
const AHEAD: Duration = Duration::from_secs(120);
/// A film whose players have taken nothing and asked for nothing for this long is paused, and
/// holds nothing back.
const IDLE: Duration = Duration::from_secs(60);
/// What a held-back download may take, so its peers stay connected.
const TRICKLE: NonZeroU32 = NonZeroU32::new(64 * 1024).unwrap();
/// How long a film whose runtime is unknown is taken to play. Most films run longer, so their
/// lead is counted short and the other downloads wait longer rather than less.
const GUESSED_RUNTIME: Duration = Duration::from_secs(45 * 60);
/// How often the other downloads' limits follow the films, and each lead is measured.
const DECIDE_EVERY: Duration = Duration::from_secs(1);

/// What a player does with one answer's bytes.
#[derive(Clone, Copy, Default, Debug)]
enum Taking {
    /// Nothing reads through the lease yet. Nothing ever reads through a cast's.
    #[default]
    Nothing,
    /// The player waits for the engine to write the bytes it asked for.
    Waiting,
    /// The answer has handed the player bytes it has yet to take, since then.
    Since(Instant),
}

/// How far one answer has got through the file it sends a player.
#[derive(Clone, Copy, Default, Debug)]
struct Reading {
    /// The next byte the answer sends.
    at: u64,
    taking: Taking,
    /// How long the film on disk after `at` plays, once measured.
    lead: Option<Duration>,
}

impl Reading {
    /// When the player last took bytes or asked for more, once it has.
    fn read_at(&self, now: Instant) -> Option<Instant> {
        match self.taking {
            Taking::Nothing => None,
            Taking::Waiting => Some(now),
            Taking::Since(since) => Some(since),
        }
    }

    /// Whether the player has taken bytes or asked for more within `IDLE`.
    fn reading(&self, now: Instant) -> bool {
        self.read_at(now).is_some_and(|at| now.duration_since(at) < IDLE)
    }
}

/// Someone streaming a download, from their request for its bytes until the answer ends. Each one
/// also counts as playing, which holds off an update's switch.
pub struct Lease {
    reading: Arc<Mutex<Reading>>,
    /// When a player last read the download through an answer that has ended.
    ended: Arc<Mutex<Option<Instant>>>,
    _playing: Open,
}

impl Lease {
    /// The answer waits for the engine to write the bytes at `at`.
    pub fn waiting(&self, at: u64) {
        let mut reading = self.reading.lock().expect("no holder panics");
        (reading.at, reading.taking) = (at, Taking::Waiting);
    }

    /// The answer hands the player the bytes at `at`.
    pub fn writing(&self, at: u64) {
        let mut reading = self.reading.lock().expect("no holder panics");
        (reading.at, reading.taking) = (at, Taking::Since(Instant::now()));
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

impl Drop for Lease {
    fn drop(&mut self) {
        let read_at = self.reading.lock().expect("no holder panics").read_at(Instant::now());
        let mut ended = self.ended.lock().expect("no holder panics");
        *ended = (*ended).max(read_at);
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

/// How the other downloads stand, from how they stood and how far each film being watched runs
/// ahead of its players. A lead between the two marks leaves them as they were.
fn decide(before: Others, leads: impl IntoIterator<Item = Duration>) -> Others {
    let mut after = Others::Free;
    for lead in leads {
        if lead < BEHIND {
            return Others::Held;
        }
        if lead < AHEAD {
            after = before;
        }
    }
    after
}

/// How far a download runs ahead of the players watching it, if anyone is. It is watched from a
/// player's first read until its players have been idle for `IDLE`, through the gap between one
/// answer and the next, like the one between ffprobe's read and the player's. While no answer
/// reading it has measured its lead, it counts as behind. `ended` is when a player last read it
/// through an answer that has ended. A cast's lease, which nothing reads through, does not count,
/// so a paused cast's download is held back like any other.
fn watched(readings: &[Reading], ended: Option<Instant>, now: Instant) -> Option<Duration> {
    let read_at = readings.iter().filter_map(|reading| reading.read_at(now)).chain(ended).max()?;
    if now.duration_since(read_at) >= IDLE {
        return None;
    }
    let leads = readings.iter().filter(|reading| reading.reading(now)).filter_map(|reading| reading.lead);
    Some(leads.min().unwrap_or(Duration::ZERO))
}

pub struct Streams {
    /// The answers streaming each download. A lease holds each reading for as long as it lasts,
    /// and a download stays until the owner notes that its last stream closed.
    readers: BTreeMap<TorrentId, Vec<Weak<Mutex<Reading>>>>,
    /// When a player last read each download through an answer that has ended. A download stays
    /// while a lease holds it, or until that read is `IDLE` old.
    ended: BTreeMap<TorrentId, Arc<Mutex<Option<Instant>>>>,
    /// What plays across the engine, which a lease adds to.
    playing: Playing,
    others: Others,
    /// The downloads the engine holds to a trickle.
    held: BTreeSet<TorrentId>,
    decided_at: Option<Instant>,
}

impl Streams {
    pub fn new(playing: Playing) -> Self {
        Self {
            readers: BTreeMap::new(),
            ended: BTreeMap::new(),
            playing,
            others: Others::default(),
            held: BTreeSet::new(),
            decided_at: None,
        }
    }
}

/// Whether `job` is taking pieces from peers.
fn downloading(job: &JobEntry) -> bool {
    job.record.intent == Intent::Running
        && matches!(job.observed, Observed::InEngine(Phase::Metadata | Phase::Checking | Phase::Downloading))
}

impl Owner {
    /// Someone starts streaming `id`, until they drop the lease, unless the engine is stopping for
    /// an update.
    pub(super) fn lease(&mut self, id: TorrentId) -> Result<Lease, Error> {
        let playing = self.streams.playing.open()?;
        let first = !self.streamed_now(id);
        let reading = Arc::default();
        self.streams.readers.entry(id).or_default().push(Arc::downgrade(&reading));
        let ended = self.streams.ended.entry(id).or_default().clone();
        if first {
            self.first_stream(id);
        }
        Ok(Lease { reading, ended, _playing: playing })
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
        let lead = |id: &TorrentId| -> Option<Duration> {
            let readers = streams.readers.get(id).into_iter().flatten().filter_map(Weak::upgrade);
            let readings: Vec<Reading> = readers.map(|reading| *reading.lock().expect("no holder panics")).collect();
            let ended = streams.ended.get(id).and_then(|ended| *ended.lock().expect("no holder panics"));
            watched(&readings, ended, now)
        };
        let downloading: Vec<(TorrentId, Option<Duration>)> =
            self.jobs.values().filter(|job| downloading(job)).map(|job| (job.id(), lead(&job.id()))).collect();
        let before = streams.others;
        streams.others = decide(before, downloading.iter().filter_map(|(_, lead)| *lead));
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
            Others::Held => downloading.iter().filter(|(_, lead)| lead.is_none()).map(|(id, _)| *id).collect(),
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
        streams.ended.retain(|_, ended| {
            Arc::strong_count(ended) > 1
                || ended.lock().expect("no holder panics").is_some_and(|at| now.duration_since(at) < IDLE)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn ahead(seconds: u64) -> Reading {
        Reading { at: 0, taking: Taking::Waiting, lead: Some(secs(seconds)) }
    }

    #[test]
    fn a_film_less_than_60_s_ahead_of_its_player_holds_the_other_downloads_back() {
        assert_eq!(decide(Others::Free, [secs(59)]), Others::Held);
        assert_eq!(decide(Others::Free, [secs(60)]), Others::Free);
    }

    #[test]
    fn the_other_downloads_go_once_every_film_is_120_s_ahead_of_its_player() {
        assert_eq!(decide(Others::Held, [secs(119)]), Others::Held);
        assert_eq!(decide(Others::Held, [secs(120)]), Others::Free);
        assert_eq!(decide(Others::Held, [secs(120), secs(90)]), Others::Held);
        assert_eq!(decide(Others::Held, []), Others::Free);
    }

    #[test]
    fn a_second_film_opening_behind_holds_the_others_back_though_the_first_is_ahead() {
        assert_eq!(decide(Others::Free, [secs(120), secs(10)]), Others::Held);
    }

    #[test]
    fn a_film_runs_as_far_ahead_as_its_least_ahead_player() {
        let now = Instant::now();
        assert_eq!(watched(&[ahead(200), ahead(10)], None, now), Some(secs(10)));
    }

    #[test]
    fn a_download_only_a_cast_holds_is_not_watched_and_is_held_back_with_the_others() {
        let now = Instant::now();
        let cast = Reading::default();
        assert_eq!(watched(&[cast], None, now), None);
        assert_eq!(watched(&[cast, ahead(10)], None, now), Some(secs(10)));
    }

    #[test]
    fn a_film_whose_player_has_taken_nothing_for_a_minute_holds_nothing_back() {
        let wrote = Instant::now();
        let paused = Reading { taking: Taking::Since(wrote), ..ahead(0) };
        assert_eq!(watched(&[paused], None, wrote + secs(59)), Some(Duration::ZERO));
        assert_eq!(watched(&[paused], None, wrote + IDLE), None);
    }

    #[test]
    fn a_film_counts_as_behind_from_its_first_read_until_an_answer_measures_it() {
        let now = Instant::now();
        let asked = Reading { at: 0, taking: Taking::Waiting, lead: None };
        assert_eq!(watched(&[asked], None, now), Some(Duration::ZERO));
    }

    #[test]
    fn a_film_counts_as_behind_between_one_answer_and_the_next_until_a_minute_after_its_last_read() {
        let read = Instant::now();
        assert_eq!(watched(&[], Some(read), read + secs(2)), Some(Duration::ZERO));
        assert_eq!(watched(&[], Some(read), read + secs(59)), Some(Duration::ZERO));
        assert_eq!(watched(&[], Some(read), read + IDLE), None);
        assert_eq!(watched(&[ahead(200)], Some(read), read + secs(2)), Some(secs(200)));
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
