use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use torrential_core::protocol::{Event, Request};
use torrential_core::{Error, ErrorCode, Job, JobStatus, TorrentId};

use super::connect::Conn;
use crate::cli::Until;
use crate::output::theme::Token;
use crate::output::views::{self, WaitEnd, Watched};
use crate::output::{Body, Doc, Mode, json_document, json_line};
use crate::wire;

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
    named(&|j| !met(j) && matches!(j.status, JobStatus::Paused | JobStatus::Interrupted))
        .map(|id| WaitEnd::Stopped { id })
}

pub struct Printer {
    pub mode: Mode,
}

#[derive(serde::Serialize)]
struct ProgressLine<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    elapsed: u64,
    job: &'a Job,
}

#[derive(serde::Serialize)]
struct ResultLine<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    outcome: &'static str,
    elapsed: u64,
    jobs: Vec<&'a Job>,
}

impl Printer {
    pub fn header(&self) -> String {
        match self.mode {
            Mode::Json => String::new(),
            _ => format!("{}\n", views::WAIT_HEADER.join("\t")),
        }
    }

    pub fn row(&self, elapsed: Duration, id: &str, job: &Job) -> String {
        match self.mode {
            Mode::Json => json_line(&ProgressLine { kind: "progress", elapsed: elapsed.as_secs(), job }),
            _ => {
                let cells: Vec<String> = views::wait_row(elapsed, id, job).into_iter().map(|c| c.text).collect();
                format!("{}\n", cells.join("\t"))
            }
        }
    }

    pub fn end(&self, end: &WaitEnd, elapsed: Duration, watched: &[Watched]) -> String {
        let (summary, next) = views::wait_end(end, elapsed, watched);
        match self.mode {
            Mode::Json => {
                let jobs = watched.iter().map(|w| &w.job).collect();
                let line = ResultLine { kind: "result", outcome: end.outcome(), elapsed: elapsed.as_secs(), jobs };
                json_document(&line, next.as_deref())
            }
            _ => Doc { body: Body::Table { header: Vec::new(), rows: Vec::new() }, summary, next }.plain(),
        }
    }
}

struct State {
    watched: Vec<Watched>,
    printed: Vec<&'static str>,
}

impl State {
    fn new(jobs: Vec<Job>) -> Self {
        let printed = vec![""; jobs.len()];
        Self { watched: views::watched(jobs), printed }
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

fn events(conn: Conn, request: &Request) -> Result<(Vec<Job>, Events), Error> {
    let mut conn = conn;
    conn.send(request)?;
    let mut stream = conn.into_stream();
    let lost =
        |e: std::io::Error| Error::new(ErrorCode::DaemonUnavailable, format!("lost the connection to the engine: {e}"));
    let first: Option<serde_json::Value> = wire::read(&mut stream).map_err(lost)?;
    let first = first.ok_or_else(|| lost(std::io::ErrorKind::UnexpectedEof.into()))?;
    let jobs = match serde_json::from_value::<Event>(first.clone()) {
        Ok(Event::Snapshot { jobs }) => jobs,
        _ => match serde_json::from_value(first) {
            Ok(torrential_core::protocol::Response::Error { error }) => return Err(error),
            _ => return Err(Error::internal("the engine sent an unexpected first watch event")),
        },
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let event = match wire::read::<Event>(&mut stream) {
                Ok(Some(event)) => Ok(event),
                Ok(None) => Err(Error::new(ErrorCode::DaemonUnavailable, "the engine stopped while waiting")),
                Err(e) => Err(lost(e)),
            };
            let end = event.is_err();
            if tx.send(event).is_err() || end {
                return;
            }
        }
    });
    Ok((jobs, rx))
}

pub fn run(
    conn: Conn,
    refs: Vec<String>,
    until: Until,
    timeout: Option<Duration>,
    printer: &Printer,
) -> Result<u8, Error> {
    let started = Instant::now();
    let (jobs, rx) = events(conn, &Request::Watch { refs })?;
    let mut state = State::new(jobs);
    let mut out = std::io::stdout().lock();
    let print = |out: &mut std::io::StdoutLock, text: String| {
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    };
    print(&mut out, printer.header());
    let heartbeat = heartbeat(timeout);
    let mut last_beat = started;
    for (i, w) in state.watched.iter().enumerate() {
        print(&mut out, printer.row(Duration::ZERO, &w.id, &w.job));
        state.printed[i] = w.job.status.name();
    }
    let deadline = timeout.map(|t| started + t);
    loop {
        if let Some(end) = evaluate(&state.watched, until) {
            print(&mut out, printer.end(&end, started.elapsed(), &state.watched));
            return Ok(end.exit_code());
        }
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            let end = WaitEnd::Timeout;
            print(&mut out, printer.end(&end, started.elapsed(), &state.watched));
            return Ok(end.exit_code());
        }
        if now >= last_beat + heartbeat {
            last_beat = now;
            for w in &state.watched {
                print(&mut out, printer.row(started.elapsed(), &w.id, &w.job));
            }
        }
        let wake = deadline.map_or(last_beat + heartbeat, |d| d.min(last_beat + heartbeat));
        match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(Ok(Event::Job { job })) => {
                if let Some(i) = state.update(*job) {
                    let w = &state.watched[i];
                    let name = w.job.status.name();
                    if name != state.printed[i] {
                        state.printed[i] = name;
                        print(&mut out, printer.row(started.elapsed(), &w.id, &w.job));
                    }
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
                    print(&mut out, printer.end(&end, started.elapsed(), &state.watched));
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

pub fn attach(conn: Conn, refs: Vec<String>, until: Until, timeout: Option<Duration>) -> Result<u8, Error> {
    let started = Instant::now();
    let (jobs, rx) = events(conn, &Request::Watch { refs })?;
    let mut state = State::new(jobs);
    let bars = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
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
    let handles: Vec<ProgressBar> = state
        .watched
        .iter()
        .map(|w| bars.add(ProgressBar::new(1000).with_style(style.clone()).with_prefix(w.id.clone())))
        .collect();
    let deadline = timeout.map(|t| started + t);
    loop {
        for (bar, job) in handles.iter().zip(state.watched.iter().map(|w| &w.job)) {
            bar.set_position(job.progress.fraction().map_or(0, |f| (f * 1000.0) as u64));
            bar.set_message(live_message(job));
        }
        let end = evaluate(&state.watched, until)
            .or_else(|| deadline.is_some_and(|d| Instant::now() >= d).then_some(WaitEnd::Timeout));
        if let Some(end) = end {
            let _ = bars.clear();
            let (summary, next) = views::wait_end(&end, started.elapsed(), &state.watched);
            anstream::print!(
                "{}",
                Doc { body: Body::Fields { title: Vec::new(), fields: Vec::new() }, summary, next }.human()
            );
            return Ok(end.exit_code());
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(Event::Job { job })) => {
                state.update(*job);
            }
            Ok(Ok(Event::Snapshot { jobs })) => jobs.into_iter().for_each(|job| {
                state.update(job);
            }),
            Ok(Ok(Event::Removed { id })) => {
                if let Some(short) = id.parse().ok().and_then(|id| state.removed(&id)) {
                    eprintln!("{short} was removed");
                    return Ok(WaitEnd::Stopped { id: short }.exit_code());
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
        assert_eq!(evaluate(&views::watched(both_done.to_vec()), Until::Done), Some(WaitEnd::Done));
        let running = [job(1, done.clone(), Some(5)), job(2, JobStatus::Downloading, Some(5))];
        assert_eq!(evaluate(&views::watched(running.to_vec()), Until::Done), None);
        assert_eq!(evaluate(&views::watched(running.to_vec()), Until::Metadata), Some(WaitEnd::Metadata));
        let one_failed = [job(1, JobStatus::Downloading, None), job(2, failed, Some(5))];
        assert_eq!(
            evaluate(&views::watched(one_failed.to_vec()), Until::Done),
            Some(WaitEnd::Failed { id: "020202".into() })
        );
        let paused = [job(1, JobStatus::Paused, Some(5)), job(2, done, Some(5))];
        assert_eq!(
            evaluate(&views::watched(paused.to_vec()), Until::Done),
            Some(WaitEnd::Stopped { id: "010101".into() })
        );
        assert_eq!(evaluate(&views::watched(paused.to_vec()), Until::Metadata), Some(WaitEnd::Metadata));
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        assert_eq!(WaitEnd::Done.exit_code(), 0);
        assert_eq!(WaitEnd::Timeout.exit_code(), 8);
        assert_eq!(WaitEnd::Failed { id: "x".into() }.exit_code(), 7);
        assert_eq!(WaitEnd::Stopped { id: "x".into() }.exit_code(), 8);
    }
}
