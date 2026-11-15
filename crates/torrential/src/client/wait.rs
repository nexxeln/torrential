use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use torrential_core::protocol::{Event, Frame, Request, Response, read_frame};
use torrential_core::text::sanitize;
use torrential_core::{Error, ErrorCode, Job, JobStatus, TorrentId};

use super::connect::{self, Conn};
use super::rows::{job_row, next_for_stopped, short_ids};
use crate::cli::Until;
use crate::env::Env;
use crate::output::format;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, json_document, json_line};
use crate::paths::Paths;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(100);
const HUMAN_HEARTBEAT: Duration = Duration::from_secs(240);

pub fn heartbeat(timeout: Option<Duration>) -> Duration {
    timeout.map_or(HUMAN_HEARTBEAT, |t| (t / 20).clamp(Duration::from_secs(60), Duration::from_secs(240)))
}

pub fn evaluate(watched: &[Watched], until: Until) -> Option<WaitEnd> {
    let met = |job: &Job| match until {
        Until::Done => matches!(job.status, JobStatus::Done { .. }),
        Until::Metadata => job.progress.size.is_some(),
    };
    let named = |pred: &dyn Fn(&Job) -> bool| watched.iter().find(|w| pred(&w.job)).map(|w| w.id.clone());
    if let Some(id) = named(&|j| matches!(j.status, JobStatus::Error { .. })) {
        return Some(WaitEnd::Failed { id });
    }
    if watched.iter().all(|w| met(&w.job)) {
        return Some(match until {
            Until::Done => WaitEnd::Done,
            Until::Metadata => WaitEnd::Metadata,
        });
    }
    named(&|j| !met(j) && j.status == JobStatus::Paused).map(|id| WaitEnd::Stopped { id })
}

pub struct Printer {
    pub mode: Mode,
}

#[derive(serde::Serialize)]
struct ProgressLine<'a> {
    #[serde(rename = "type")]
    kind: DocKind,
    elapsed: u64,
    job: &'a Job,
}

#[derive(serde::Serialize)]
struct ResultLine<'a> {
    #[serde(rename = "type")]
    kind: DocKind,
    outcome: &'static str,
    elapsed: u64,
    jobs: Vec<&'a Job>,
}

impl Printer {
    pub fn header(&self) -> String {
        match self.mode {
            Mode::Json => String::new(),
            _ => format!("{}\n", WAIT_HEADER.join("\t")),
        }
    }

    pub fn row(&self, elapsed: Duration, id: &str, job: &Job) -> String {
        match self.mode {
            Mode::Json => json_line(&ProgressLine { kind: DocKind::Progress, elapsed: elapsed.as_secs(), job }),
            _ => {
                let cells: Vec<String> = wait_row(elapsed, id, job).into_iter().map(|c| c.text).collect();
                format!("{}\n", cells.join("\t"))
            }
        }
    }

    pub fn end(&self, end: &WaitEnd, elapsed: Duration, watched: &[Watched]) -> String {
        let (summary, next) = wait_end(end, elapsed, watched);
        match self.mode {
            Mode::Json => {
                let jobs = watched.iter().map(|w| &w.job).collect();
                let line =
                    ResultLine { kind: DocKind::Result, outcome: end.outcome(), elapsed: elapsed.as_secs(), jobs };
                json_document(&line, next.as_deref())
            }
            _ => Doc { body: Body::Table { header: Vec::new(), rows: Vec::new() }, summary, next }.plain(),
        }
    }
}

struct State {
    watched: Vec<Watched>,
}

impl State {
    fn new(jobs: Vec<Job>) -> Self {
        Self { watched: watched(jobs) }
    }

    fn update(&mut self, job: Job) -> Option<usize> {
        let i = self.watched.iter().position(|w| w.job.id == job.id)?;
        self.watched[i].job = job;
        Some(i)
    }

    fn removed(&self, id: &TorrentId) -> Option<String> {
        self.watched.iter().find(|w| w.job.id == *id).map(|w| w.id.clone())
    }
}

type Events = mpsc::Receiver<Result<Event, Error>>;

/// Where to find the engine again when the connection to it ends.
pub struct Engine {
    pub paths: Paths,
    pub env: Env,
}

/// Follows `refs`, and follows them again when the engine restarts.
fn events(conn: Conn, refs: Vec<String>, engine: Engine) -> Result<(Vec<Job>, Events), Error> {
    let (jobs, mut stream) = watch(conn, refs)?;
    let ids: Vec<String> = jobs.iter().map(|job| job.id.to_hex()).collect();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let event = match read_frame::<Event>(&mut stream) {
                Ok(Some(event)) => Ok(event),
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    Err(Error::internal(format!("the engine sent an event this torrential cannot read: {e}")))
                }
                Ok(None) | Err(_) => match back(&engine, &ids) {
                    Ok((jobs, again)) => {
                        stream = again;
                        Ok(Event::Snapshot { jobs })
                    }
                    Err(error) => Err(error),
                },
            };
            let end = event.is_err();
            if tx.send(event).is_err() || end {
                return;
            }
        }
    });
    Ok((jobs, rx))
}

/// Asks the engine to follow `refs` and reads the snapshot it answers with.
fn watch(mut conn: Conn, refs: Vec<String>) -> Result<(Vec<Job>, UnixStream), Error> {
    conn.send(&Request::Watch { refs })?;
    let (mut stream, log) = conn.into_stream();
    let first = read_frame(&mut stream).map_err(|e| connect::lost(e, &log))?;
    let unexpected = || Error::internal("the engine sent an unexpected first watch event");
    match first.ok_or_else(|| connect::lost(std::io::ErrorKind::UnexpectedEof.into(), &log))? {
        Frame::Update(Event::Snapshot { jobs }) => Ok((jobs, stream)),
        Frame::Reply(reply) => match *reply {
            Response::Error { error } => Err(error),
            _ => Err(unexpected()),
        },
        Frame::Update(_) => Err(unexpected()),
    }
}

/// Follows the downloads `ids` again once the engine answers. An engine that is restarting leaves
/// its socket and refuses connections for a few seconds, so `back` tries again for a short while.
/// One that was stopped removes its socket, and a newer one is not for this torrential to use; either
/// ends the wait at once.
fn back(engine: &Engine, ids: &[String]) -> Result<(Vec<Job>, UnixStream), Error> {
    let stopped = || Error::new(ErrorCode::DaemonUnavailable, "the engine stopped while waiting");
    let followed = connect::poll(connect::BACK_WITHIN, || match connect::existing(&engine.paths, &engine.env) {
        Ok(Some(conn)) => match watch(conn, ids.to_vec()) {
            Err(error) if error.code == ErrorCode::DaemonUnavailable => Ok(None),
            followed => followed.map(Some),
        },
        Ok(None) if !engine.paths.socket().exists() => Err(stopped()),
        Ok(None) => Ok(None),
        Err(error) if error.code == ErrorCode::DaemonUnavailable => Ok(None),
        Err(error) => Err(error),
    })?;
    followed.ok_or_else(stopped)
}

/// Follows `refs` until `until` holds for each, one fails or stops, or `timeout` passes, and prints
/// a line each time one changes state.
pub fn run(
    following: (Conn, Engine),
    refs: Vec<String>,
    until: Until,
    timeout: Option<Duration>,
    printer: &Printer,
) -> Result<u8, Error> {
    let lines = Lines {
        printer,
        out: std::io::stdout().lock(),
        every: heartbeat(timeout),
        beat: Instant::now(),
        printed: Vec::new(),
    };
    follow(following, refs, until, timeout, lines)
}

/// Follows `refs` as `run` does, with a bar for each in a terminal.
pub fn attach(
    following: (Conn, Engine),
    refs: Vec<String>,
    until: Until,
    timeout: Option<Duration>,
) -> Result<u8, Error> {
    follow(
        following,
        refs,
        until,
        timeout,
        Bars { all: MultiProgress::with_draw_target(ProgressDrawTarget::stderr()), each: Vec::new() },
    )
}

/// How a wait shows the downloads it follows.
trait Show {
    fn start(&mut self, watched: &[Watched]);
    /// Shows that download `i` changed, `at` into the wait.
    fn changed(&mut self, watched: &[Watched], i: usize, at: Duration);
    /// Shows the downloads again if it is time, and says when it is time next.
    fn tick(&mut self, watched: &[Watched], at: Duration) -> Instant;
    fn end(&mut self, end: &WaitEnd, at: Duration, watched: &[Watched]);
    /// Shows that the engine no longer has a download the wait follows.
    fn removed(&mut self, end: &WaitEnd, at: Duration, watched: &[Watched]) {
        self.end(end, at, watched);
    }
}

fn follow(
    (conn, engine): (Conn, Engine),
    refs: Vec<String>,
    until: Until,
    timeout: Option<Duration>,
    mut show: impl Show,
) -> Result<u8, Error> {
    let started = Instant::now();
    let (jobs, rx) = events(conn, refs, engine)?;
    let mut state = State::new(jobs);
    show.start(&state.watched);
    let deadline = timeout.map(|t| started + t);
    let finish = |show: &mut dyn Show, end: WaitEnd, state: &State| {
        show.end(&end, started.elapsed(), &state.watched);
        Ok(end.exit_code())
    };
    loop {
        if let Some(end) = evaluate(&state.watched, until) {
            return finish(&mut show, end, &state);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return finish(&mut show, WaitEnd::Timeout, &state);
        }
        let wake = show.tick(&state.watched, started.elapsed());
        let wake = deadline.map_or(wake, |d| d.min(wake));
        match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(Ok(Event::Job { job })) => {
                if let Some(i) = state.update(*job) {
                    show.changed(&state.watched, i, started.elapsed());
                }
            }
            Ok(Ok(Event::Snapshot { jobs })) => {
                for job in jobs {
                    state.update(job);
                }
            }
            Ok(Ok(Event::Removed { id })) => {
                if let Some(short) = id.parse().ok().and_then(|id| state.removed(&id)) {
                    let end = WaitEnd::Stopped { id: short };
                    show.removed(&end, started.elapsed(), &state.watched);
                    return Ok(end.exit_code());
                }
            }
            Ok(Ok(Event::Heartbeat { .. } | Event::SearchUpdate { .. })) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::new(ErrorCode::DaemonUnavailable, "the engine stopped while waiting"));
            }
        }
    }
}

/// A line for each download when the wait starts, each time one changes state, and every `every`.
struct Lines<'a> {
    printer: &'a Printer,
    out: std::io::StdoutLock<'static>,
    every: Duration,
    beat: Instant,
    /// The state each download's last line showed.
    printed: Vec<&'static str>,
}

impl Lines<'_> {
    fn print(&mut self, text: String) {
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }
}

impl Show for Lines<'_> {
    fn start(&mut self, watched: &[Watched]) {
        self.print(self.printer.header());
        for w in watched {
            self.print(self.printer.row(Duration::ZERO, &w.id, &w.job));
        }
        self.printed = watched.iter().map(|w| w.job.status.name()).collect();
    }

    fn changed(&mut self, watched: &[Watched], i: usize, at: Duration) {
        let w = &watched[i];
        if w.job.status.name() != self.printed[i] {
            self.printed[i] = w.job.status.name();
            self.print(self.printer.row(at, &w.id, &w.job));
        }
    }

    fn tick(&mut self, watched: &[Watched], at: Duration) -> Instant {
        let now = Instant::now();
        if now >= self.beat + self.every {
            self.beat = now;
            for w in watched {
                self.print(self.printer.row(at, &w.id, &w.job));
            }
        }
        self.beat + self.every
    }

    fn end(&mut self, end: &WaitEnd, at: Duration, watched: &[Watched]) {
        self.print(self.printer.end(end, at, watched));
    }
}

/// A bar for each download, on stderr, redrawn twice a second.
struct Bars {
    all: MultiProgress,
    each: Vec<ProgressBar>,
}

impl Show for Bars {
    fn start(&mut self, watched: &[Watched]) {
        let _ = ctrlc::set_handler(|| {
            eprintln!("\nThe download continues in the background.");
            std::process::exit(130);
        });
        let style = ProgressStyle::with_template(&format!(
            "{{prefix:.{}}} {{bar:32.{}/{}}} {{msg}}",
            Token::Accent.bar_color(),
            Token::Mint.bar_color(),
            Token::Muted.bar_color()
        ))
        .expect("the bar template is valid")
        .progress_chars("━━─");
        self.each = watched
            .iter()
            .map(|w| self.all.add(ProgressBar::new(1000).with_style(style.clone()).with_prefix(w.id.clone())))
            .collect();
    }

    fn changed(&mut self, _: &[Watched], _: usize, _: Duration) {}

    fn tick(&mut self, watched: &[Watched], _: Duration) -> Instant {
        for (bar, job) in self.each.iter().zip(watched.iter().map(|w| &w.job)) {
            bar.set_position(job.progress.fraction().map_or(0, |f| (f * 1000.0) as u64));
            bar.set_message(live_message(job));
        }
        Instant::now() + Duration::from_millis(500)
    }

    fn removed(&mut self, end: &WaitEnd, _: Duration, _: &[Watched]) {
        if let WaitEnd::Stopped { id } = end {
            eprintln!("{id} was removed");
        }
    }

    fn end(&mut self, end: &WaitEnd, at: Duration, watched: &[Watched]) {
        let _ = self.all.clear();
        let (summary, next) = wait_end(end, at, watched);
        anstream::print!(
            "{}",
            Doc { body: Body::Fields { title: Vec::new(), fields: Vec::new() }, summary, next }.human()
        );
    }
}

fn live_message(job: &Job) -> String {
    use crate::output::format;
    let name = job.name.as_deref().map(torrential_core::text::sanitize).unwrap_or_default();
    match job.status {
        JobStatus::Downloading => format!(
            "{}  {}  {}  eta {}  {name}",
            format::percent(&job.progress),
            format::rate(job.progress.down_rate),
            format::count(job.progress.peers as usize, "peer"),
            format::eta(job.progress.eta)
        ),
        ref status => format!("{}  {}  {name}", status.name(), format::percent(&job.progress)),
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum WaitEnd {
    Done,
    Metadata,
    Timeout,
    Failed { id: String },
    Stopped { id: String },
}

impl WaitEnd {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Done | Self::Metadata => 0,
            Self::Timeout | Self::Stopped { .. } => torrential_core::error::EXIT_PENDING,
            Self::Failed { .. } => 7,
        }
    }

    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Metadata => "metadata",
            Self::Timeout => "timeout",
            Self::Failed { .. } => "error",
            Self::Stopped { .. } => "stopped",
        }
    }
}

pub const WAIT_HEADER: [&str; 9] = ["ELAPSED", "ID", "STATE", "PROGRESS", "SIZE", "DOWN", "PEERS", "ETA", "NAME"];

pub fn wait_row(elapsed: Duration, id: &str, job: &Job) -> Vec<Cell> {
    let mut row = vec![Cell::styled(format::elapsed(elapsed), Token::Muted)];
    row.extend(job_row(id, job));
    row
}

#[derive(Clone, PartialEq, Debug)]
pub struct Watched {
    pub id: String,
    pub job: Job,
}

pub fn watched(jobs: Vec<Job>) -> Vec<Watched> {
    let ids = short_ids(&jobs.iter().map(|j| j.id).collect::<Vec<_>>());
    ids.into_iter().zip(jobs).map(|(id, job)| Watched { id, job }).collect()
}

pub fn wait_end(end: &WaitEnd, elapsed: Duration, watched: &[Watched]) -> (Vec<String>, Option<String>) {
    let after = format::elapsed(elapsed);
    let all = watched.iter().map(|w| w.id.as_str()).collect::<Vec<_>>().join(" ");
    let find = |id: &str| watched.iter().find(|w| w.id == id).map(|w| &w.job);
    match end {
        WaitEnd::Done => {
            let paths: Vec<String> = watched
                .iter()
                .map(|w| sanitize(&w.job.path.as_deref().unwrap_or(&w.job.dir).display().to_string()))
                .collect();
            (vec![format!("done after {after} · path {}", paths.join(", "))], None)
        }
        WaitEnd::Metadata => (vec![format!("file list ready after {after}")], Some(format!("torrential files {all}"))),
        WaitEnd::Timeout => {
            let eta = watched.iter().filter_map(|w| w.job.progress.eta).max();
            let mut line = format!("still running after {after}");
            if let Some(eta) = eta {
                line.push_str(&format!(" · eta {}", format::eta(Some(eta))));
            }
            (vec![line], Some(format!("torrential wait {all}")))
        }
        WaitEnd::Failed { id } => {
            let mut lines = Vec::new();
            let status = find(id).map(|j| &j.status);
            if let Some(JobStatus::Error { error }) = status {
                lines.push(format!(
                    "{id} stopped after {after} with error[{}]: {}",
                    error.code,
                    sanitize(&error.message)
                ));
                if let Some(hint) = &error.hint {
                    lines.push(format!("hint: {hint}"));
                }
            }
            (lines, status.and_then(|s| next_for_stopped(s, id)))
        }
        WaitEnd::Stopped { id } => {
            let state = find(id).map_or("stopped", |j| j.status.name());
            (
                vec![format!("{id} is {state} and will not finish until resumed")],
                Some(format!("torrential resume {id}")),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use torrential_core::{InfoHash, Progress, Selection};

    fn job(n: u8, status: JobStatus, size: Option<u64>) -> Job {
        Job {
            id: TorrentId::from_bytes([n; 20]),
            info_hash: InfoHash::V1([n; 20]),
            name: Some(format!("job {n}")),
            dir: PathBuf::from("/d"),
            path: None,
            status,
            progress: Progress { size, ..Progress::default() },
            selection: Selection::All,
            added_at: 0,
            title: None,
            place: torrential_core::Place::Folder,
        }
    }

    #[test]
    fn heartbeats_scale_with_the_timeout_within_bounds() {
        assert_eq!(heartbeat(Some(Duration::from_secs(100))), Duration::from_secs(60));
        assert_eq!(heartbeat(Some(Duration::from_secs(45 * 60))), Duration::from_secs(135));
        assert_eq!(heartbeat(Some(Duration::from_secs(110 * 60))), Duration::from_secs(240));
        assert_eq!(heartbeat(None), Duration::from_secs(240));
    }

    #[test]
    fn a_wait_ends_on_done_error_or_a_stop() {
        let done = JobStatus::Done { finished_at: 1 };
        let failed = JobStatus::Error { error: Error::new(ErrorCode::DiskFull, "full") };
        let both_done = [job(1, done.clone(), Some(5)), job(2, done.clone(), Some(5))];
        assert_eq!(evaluate(&watched(both_done.to_vec()), Until::Done), Some(WaitEnd::Done));
        let running = [job(1, done.clone(), Some(5)), job(2, JobStatus::Downloading, Some(5))];
        assert_eq!(evaluate(&watched(running.to_vec()), Until::Done), None);
        assert_eq!(evaluate(&watched(running.to_vec()), Until::Metadata), Some(WaitEnd::Metadata));
        let one_failed = [job(1, JobStatus::Downloading, None), job(2, failed, Some(5))];
        assert_eq!(evaluate(&watched(one_failed.to_vec()), Until::Done), Some(WaitEnd::Failed { id: "020202".into() }));
        let paused = [job(1, JobStatus::Paused, Some(5)), job(2, done, Some(5))];
        assert_eq!(evaluate(&watched(paused.to_vec()), Until::Done), Some(WaitEnd::Stopped { id: "010101".into() }));
        assert_eq!(evaluate(&watched(paused.to_vec()), Until::Metadata), Some(WaitEnd::Metadata));
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        assert_eq!(WaitEnd::Done.exit_code(), 0);
        assert_eq!(WaitEnd::Timeout.exit_code(), 8);
        assert_eq!(WaitEnd::Failed { id: "x".into() }.exit_code(), 7);
        assert_eq!(WaitEnd::Stopped { id: "x".into() }.exit_code(), 8);
    }
}
