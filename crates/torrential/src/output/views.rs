use std::path::Path;
use std::time::Duration;

use torrential_core::id::shortest_unique_prefix;
use torrential_core::protocol::{Change, FilesReply, GetOutcome, GetReply};
use torrential_core::text::sanitize;
use torrential_core::units::{format_limit, format_size, size_text};
use torrential_core::{
    Arch, Category, ErrorCode, Job, JobStatus, Machine, Profile, Release, ReleaseId, SearchResult, Selection,
    SourceOutcome, TorrentId,
};

use super::format::{self, NONE};
use super::theme::Token;
use super::{Body, Cell, Doc};
use crate::skill::{Change as SkillChange, Installed, State};
use crate::update::Policy;

const SHORT_ID: usize = 6;
const MAX_FILE_ROWS: usize = 200;

pub fn short_ids(ids: &[TorrentId]) -> Vec<String> {
    ids.iter().map(|id| shortest_unique_prefix(id, ids, SHORT_ID)).collect()
}

pub fn release_ids(releases: &[Release]) -> Vec<String> {
    let as_torrent = |hex: &str| hex.parse::<TorrentId>().expect("release IDs are 40 hex characters");
    let torrents: Vec<TorrentId> = releases
        .iter()
        .filter_map(|r| match r.id {
            ReleaseId::Torrent(id) => Some(id),
            ReleaseId::Locator(_) => None,
        })
        .collect();
    let locators: Vec<TorrentId> = releases
        .iter()
        .filter_map(|r| match r.id {
            ReleaseId::Locator(id) => Some(as_torrent(&id.to_hex())),
            ReleaseId::Torrent(_) => None,
        })
        .collect();
    releases
        .iter()
        .map(|r| match r.id {
            ReleaseId::Torrent(id) => shortest_unique_prefix(&id, &torrents, SHORT_ID),
            ReleaseId::Locator(id) => {
                format!("u{}", shortest_unique_prefix(&as_torrent(&id.to_hex()), &locators, SHORT_ID))
            }
        })
        .collect()
}

pub fn short_ref(id: &str) -> String {
    let (lead, hex) = id.strip_prefix('u').map_or(("", id), |rest| ("u", rest));
    format!("{lead}{}", &hex[..hex.len().min(SHORT_ID)])
}

pub fn source_outcome(outcome: &SourceOutcome) -> String {
    match outcome {
        SourceOutcome::Ok { results } => format::count(*results as usize, "result"),
        SourceOutcome::Cached { results } => format!("{}, cached", format::count(*results as usize, "result")),
        SourceOutcome::Timeout => "timeout".to_string(),
        SourceOutcome::Http { status } => format!("http {status}"),
        SourceOutcome::Connect { reason } => format!("connect failed ({})", sanitize(reason)),
        SourceOutcome::BadResponse { reason } => format!("bad response ({})", sanitize(reason)),
        SourceOutcome::Irrelevant => "irrelevant response".to_string(),
    }
}

pub fn search(result: &SearchResult, now: i64) -> Doc {
    let ids = release_ids(&result.releases);
    let video = result.is_video();
    let rows = result
        .releases
        .iter()
        .zip(&ids)
        .map(|(r, id)| {
            let source = match r.sources.as_slice() {
                [] => NONE.to_string(),
                [one] => one.clone(),
                [first, rest @ ..] => format!("{first}+{}", rest.len()),
            };
            let flags: Vec<&str> = r.flags.iter().map(|f| f.as_str()).collect();
            let quality = r.quality().to_string();
            let unknown = if video || r.category == Some(Category::Video) { format::UNKNOWN } else { NONE };
            vec![
                Cell::styled(id, Token::Accent),
                Cell::new(r.seeders.map_or_else(|| format::UNKNOWN.to_string(), |s| s.to_string())),
                Cell::new(format::size(r.size)),
                if quality.is_empty() { Cell::styled(unknown, Token::Muted) } else { Cell::new(quality) },
                Cell::new(format::age(r.published, now)),
                Cell::styled(source, Token::Muted),
                if flags.is_empty() {
                    Cell::styled(NONE, Token::Muted)
                } else {
                    Cell::styled(flags.join(","), Token::Mint)
                },
                Cell::new(&r.name),
            ]
        })
        .collect();
    let ok = result.sources.iter().filter(|s| s.outcome.is_ok()).count();
    let found = if result.releases.is_empty() {
        "no results".to_string()
    } else {
        format!("{} of {}", result.releases.len(), format::count(result.total as usize, "result"))
    };
    let mut line = vec![found, format!("{ok} of {} ok", format::count(result.sources.len(), "source"))];
    line.extend(
        result
            .sources
            .iter()
            .filter(|s| !s.outcome.is_ok())
            .map(|s| format!("{}: {}", s.source, source_outcome(&s.outcome))),
    );
    line.push(format::seconds_tenths(result.elapsed_ms));
    if result.hidden_adult > 0 {
        line.push(format!("{} adult hidden: --adult", result.hidden_adult));
    }
    let shown = result.releases.len() as u32;
    if result.total > shown {
        line.push(format!("more: --limit {}", result.total.min(shown + 10)));
    }
    let mut summary = vec![line.join(" · ")];
    let query = &result.query;
    let mut filters = Vec::new();
    for source in &query.sources {
        filters.push(format!("--source {}", format::shell_word(&sanitize(source))));
    }
    if let Some(category) = query.category {
        filters.push(format!("--category {}", category.as_str()));
    }
    if let Some(min) = query.min_seeds {
        filters.push(format!("--min-seeds {min}"));
    }
    if let Some(max) = query.max_size {
        filters.push(format!("--max-size {}", size_text(max)));
    }
    let next = match ids.first() {
        Some(id) => format!("torrential files {id}"),
        None => {
            let words: Vec<String> = query.text.split_whitespace().map(|w| format::shell_word(&sanitize(w))).collect();
            let mut next = format!("torrential search {}", words.join(" "));
            let cached = result.sources.iter().any(|s| matches!(s.outcome, SourceOutcome::Cached { .. }));
            if !filters.is_empty() {
                summary.push(format!("filtered by {}", filters.join(" ")));
            } else if cached {
                next.push_str(" --fresh");
            }
            next
        }
    };
    let header = vec!["ID", "SEEDS", "SIZE", "QUALITY", "AGE", "SOURCE", "FLAGS", "NAME"];
    let profile = &result.query.profile;
    if video {
        summary.push(format!("ranked for {}", ranked_for(profile)));
    } else if result.releases.iter().any(|r| !Arch::named_in(&r.name).is_empty()) {
        summary.push(format!("ranked for {}", profile.describe_arch()));
    }
    Doc { body: Body::Table { header, rows }, summary, next: Some(next) }
}

pub fn ranked_for(profile: &Profile) -> String {
    match profile.overrides() {
        Some(overrides) => format!("{} · overrides: {overrides}", profile.describe()),
        None => profile.describe(),
    }
}

pub fn profile_check(profile: &Profile) -> Check {
    Check { check: "profile".into(), status: CheckStatus::Ok, detail: ranked_for(profile) }
}

#[derive(Debug, serde::Serialize)]
pub struct FileMatch {
    pub patterns: Vec<String>,
    pub total_files: usize,
    pub size: u64,
}

pub fn files(reply: &FilesReply, matched: Option<&FileMatch>) -> Doc {
    let from_download = reply.metadata_source == "download";
    let risky = reply.files.iter().any(|f| f.risk.is_some());
    let rows = reply
        .files
        .iter()
        .take(MAX_FILE_ROWS)
        .map(|f| {
            let done = match (from_download, f.selected) {
                (false, _) | (true, false) => NONE.to_string(),
                (true, true) => format::percent(&torrential_core::Progress {
                    size: Some(f.size),
                    verified: f.verified,
                    ..Default::default()
                }),
            };
            let mut row = vec![Cell::new(format_size(f.size)), Cell::new(done), Cell::new(&f.path)];
            if risky {
                row.push(match f.risk {
                    Some(_) => Cell::styled("executable", Token::Error),
                    None => Cell::new(NONE),
                });
            }
            row
        })
        .collect();
    let mut header = vec!["SIZE", "DONE", "PATH"];
    if risky {
        header.push("RISK");
    }
    let name = reply.name.as_deref().map_or_else(|| format::UNKNOWN.to_string(), sanitize);
    let source = match reply.metadata_source.as_str() {
        "download" => "from the download",
        "cache" => "from cache",
        "torrent_file" => "from the .torrent file",
        "torrent_url" => "from the .torrent URL",
        "peers" => "from peers",
        _ => "",
    };
    let count = reply.files.len();
    let elapsed = format::seconds_tenths(reply.elapsed_ms);
    let mut summary = vec![match matched {
        Some(m) => format!(
            "{count} of {} in {name} match · {} · file list {source} in {elapsed}",
            format::count(m.total_files, "file"),
            format_size(m.size)
        ),
        None => format!(
            "{} in {name} · {} · file list {source} in {elapsed}",
            format::count(count, "file"),
            format_size(reply.total_size)
        ),
    }];
    if count > MAX_FILE_ROWS {
        summary.push(format!(
            "showing {MAX_FILE_ROWS} of {}; torrential files --files <GLOB> narrows the list",
            format::count(count, "file")
        ));
    }
    if risky {
        summary.push("executables inside a media torrent are a common malware sign".to_string());
    }
    let id = short_ref(&reply.id);
    let next = match matched {
        Some(m) => Some(rerun_get(&id, &Selection::Patterns(m.patterns.clone()), None)),
        None if from_download => {
            let pending = reply.files.iter().any(|f| f.selected && f.verified < f.size);
            pending.then(|| format!("torrential wait {id}"))
        }
        None => {
            if count > 1 {
                summary.push(format!("torrential get {id} --files <PATTERN> downloads only the matching files"));
            }
            Some(format!("torrential get {id}"))
        }
    };
    Doc { body: Body::Table { header, rows }, summary, next }
}

pub fn selection(selection: &Selection) -> String {
    match selection {
        Selection::All => "all".to_string(),
        Selection::Patterns(patterns) => patterns.iter().map(|p| sanitize(p)).collect::<Vec<_>>().join(" "),
    }
}

pub fn get(reply: &GetReply, dir: Option<&Path>) -> Doc {
    let job = &reply.job;
    let id = short_ids(&[job.id]).remove(0);
    let (word, token) = match reply.outcome {
        GetOutcome::Started => ("started", Token::Mint),
        GetOutcome::Queued => ("queued", Token::Accent),
        GetOutcome::Existing => ("existing", Token::Muted),
        GetOutcome::Updated => ("updated", Token::Accent),
        GetOutcome::Planned => ("planned", Token::Accent),
    };
    let mut title = vec![Cell::styled(word, token), Cell::styled(id.clone(), Token::Accent)];
    if let Some(name) = &job.name {
        title.push(Cell::new(name));
    }
    let mut fields = Vec::new();
    if reply.outcome != GetOutcome::Planned {
        fields.push(("state", state_cell(&job.status)));
    }
    let files = match (&reply.plan, job.progress.size) {
        (Some(plan), _) => format!(
            "{} ({} of {}, {})",
            selection(&job.selection),
            plan.selected_files,
            plan.total_files,
            format_size(plan.selected_size)
        ),
        (None, Some(size)) => format!("{} ({})", selection(&job.selection), format_size(size)),
        (None, None) => format!("{}, file list pending", selection(&job.selection)),
    };
    fields.push(("files", Cell::new(files)));
    if let Some(previous) = &reply.previous_selection {
        fields.push(("was", Cell::new(selection(previous))));
    }
    let path = job.path.as_deref().unwrap_or(&job.dir);
    fields.push(("path", Cell::new(path.display().to_string())));
    let mut summary = Vec::new();
    if reply.outcome == GetOutcome::Planned
        && let Some(plan) = &reply.plan
    {
        fields.push(("free", Cell::new(format_size(plan.free_space))));
        if let Some(cap) = plan.max_size {
            fields.push(("cap", Cell::new(format_limit(cap))));
        }
        let conflicts = if plan.conflicts.is_empty() { "none".to_string() } else { plan.conflicts.len().to_string() };
        fields.push(("conflicts", Cell::new(conflicts)));
        summary
            .extend(plan.conflicts.iter().take(5).map(|p| format!("exists: {}", sanitize(&p.display().to_string()))));
    }
    let next = match reply.outcome {
        GetOutcome::Planned if reply.plan.as_ref().is_some_and(|p| !p.conflicts.is_empty()) => {
            Some(rerun_get(&id, &job.selection, Some("<EMPTY DIR>".into())))
        }
        GetOutcome::Planned => {
            Some(rerun_get(&id, &job.selection, dir.map(|d| format::shell_word(&sanitize(&d.display().to_string())))))
        }
        _ if job.status.is_stopped() => next_for_stopped(&job.status, &id),
        _ => Some(format!("torrential wait {id}")),
    };
    Doc { body: Body::Fields { title, fields }, summary, next }
}

fn rerun_get(id: &str, selection: &Selection, dir: Option<String>) -> String {
    let mut words = vec!["torrential".to_string(), "get".to_string(), id.to_string()];
    if let Selection::Patterns(patterns) = selection {
        for p in patterns {
            words.push("--files".to_string());
            words.push(format::shell_word(&sanitize(p)));
        }
    }
    if let Some(dir) = dir {
        words.push("--dir".to_string());
        words.push(dir);
    }
    words.join(" ")
}

pub fn state_cell(status: &JobStatus) -> Cell {
    let token = match status {
        JobStatus::Downloading | JobStatus::Metadata | JobStatus::Checking => Token::Mint,
        JobStatus::Done { .. } => Token::Mint,
        JobStatus::Queued | JobStatus::Paused | JobStatus::Interrupted => Token::Accent,
        JobStatus::Error { .. } => Token::Error,
    };
    Cell::styled(status.name(), token)
}

fn next_for_stopped(status: &JobStatus, id: &str) -> Option<String> {
    match status {
        JobStatus::Error { error } if error.next.is_some() => error.next.as_deref().cloned(),
        JobStatus::Error { error } if error.code == ErrorCode::TooLarge => None,
        JobStatus::Paused | JobStatus::Interrupted | JobStatus::Error { .. } => Some(format!("torrential resume {id}")),
        _ => None,
    }
}

fn restartable(status: &JobStatus) -> bool {
    match status {
        JobStatus::Interrupted => true,
        JobStatus::Error { error } => error.code != ErrorCode::TooLarge,
        _ => false,
    }
}

fn is_active(status: &JobStatus) -> bool {
    matches!(status, JobStatus::Queued | JobStatus::Metadata | JobStatus::Checking | JobStatus::Downloading)
}

pub fn job_row(id: &str, job: &Job) -> Vec<Cell> {
    let active = matches!(job.status, JobStatus::Metadata | JobStatus::Checking | JobStatus::Downloading);
    let downloading = job.status == JobStatus::Downloading;
    vec![
        Cell::styled(id, Token::Accent),
        state_cell(&job.status),
        Cell::new(format::percent(&job.progress)),
        Cell::new(format::size(job.progress.size)),
        Cell::new(if downloading { format::rate(job.progress.down_rate) } else { NONE.to_string() }),
        Cell::new(if active { job.progress.peers.to_string() } else { NONE.to_string() }),
        Cell::new(if downloading { format::eta(job.progress.eta) } else { NONE.to_string() }),
        job.name.as_deref().map_or_else(|| Cell::styled(format::UNKNOWN, Token::Muted), Cell::new),
    ]
}

pub const JOB_HEADER: [&str; 8] = ["ID", "STATE", "PROGRESS", "SIZE", "DOWN", "PEERS", "ETA", "NAME"];

pub fn jobs(jobs: &[Job]) -> Doc {
    let ids = short_ids(&jobs.iter().map(|j| j.id).collect::<Vec<_>>());
    let rows = jobs.iter().zip(&ids).map(|(job, id)| job_row(id, job)).collect();
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for job in jobs {
        let name = job.status.name();
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    let mut line = vec![format::count(jobs.len(), "download")];
    line.extend(counts.iter().map(|(name, count)| format!("{count} {name}")));
    let mut summary = vec![line.join(" · ")];
    for (job, id) in jobs.iter().zip(&ids) {
        if let JobStatus::Error { error } = &job.status {
            summary.push(format!("{id} error[{}]: {}", error.code, sanitize(&error.message)));
        }
    }
    let active: Vec<&str> =
        jobs.iter().zip(&ids).filter(|(j, _)| is_active(&j.status)).map(|(_, id)| id.as_str()).collect();
    let ids_where = |keep: fn(&JobStatus) -> bool| -> Vec<&str> {
        jobs.iter().zip(&ids).filter(|(j, _)| keep(&j.status)).map(|(_, id)| id.as_str()).collect()
    };
    let restart = ids_where(restartable);
    let paused = ids_where(|s| *s == JobStatus::Paused);
    let next = if jobs.is_empty() {
        Some("torrential search <words>".to_string())
    } else if !active.is_empty() {
        if !restart.is_empty() {
            summary.push(format!("torrential resume {} restarts the interrupted and failed ones", restart.join(" ")));
        }
        Some(format!("torrential wait {}", active.join(" ")))
    } else if !restart.is_empty() {
        Some(format!("torrential resume {}", restart.join(" ")))
    } else if !paused.is_empty() {
        Some(format!("torrential resume {}", paused.join(" ")))
    } else {
        None
    };
    Doc { body: Body::Table { header: JOB_HEADER.to_vec(), rows }, summary, next }
}

pub fn job(job: &Job) -> Doc {
    let id = short_ids(&[job.id]).remove(0);
    let p = &job.progress;
    let mut fields = vec![
        ("id", Cell::styled(job.id.to_hex(), Token::Accent)),
        ("name", job.name.as_deref().map_or_else(|| Cell::styled(format::UNKNOWN, Token::Muted), Cell::new)),
        ("state", state_cell(&job.status)),
    ];
    let progress = match p.size {
        Some(size) => {
            format!("{} ({} of {})", format::percent(p), format_size(p.verified.min(size)), format_size(size))
        }
        None => format::UNKNOWN.to_string(),
    };
    fields.push(("progress", Cell::new(progress)));
    let downloading = job.status == JobStatus::Downloading;
    if downloading {
        fields.push(("down", Cell::new(format::rate(p.down_rate))));
    }
    if matches!(job.status, JobStatus::Metadata | JobStatus::Checking | JobStatus::Downloading) {
        fields.push(("peers", Cell::new(format!("{} ({})", p.peers, format::count(p.seeds as usize, "seed")))));
    }
    if downloading {
        fields.push(("eta", Cell::new(format::eta(p.eta))));
    }
    fields.push(("files", Cell::new(selection(&job.selection))));
    let path = job.path.as_deref().unwrap_or(&job.dir);
    fields.push(("path", Cell::new(path.display().to_string())));
    fields.push(("added", Cell::new(format::timestamp(job.added_at))));
    let mut summary = Vec::new();
    match &job.status {
        JobStatus::Done { finished_at } => fields.push(("finished", Cell::new(format::timestamp(*finished_at)))),
        JobStatus::Error { error } => {
            fields
                .push(("error", Cell::styled(format!("[{}] {}", error.code, sanitize(&error.message)), Token::Error)));
            if let Some(hint) = &error.hint {
                summary.push(format!("hint: {hint}"));
            }
        }
        _ => {}
    }
    let next =
        if is_active(&job.status) { Some(format!("torrential wait {id}")) } else { next_for_stopped(&job.status, &id) };
    Doc { body: Body::Fields { title: Vec::new(), fields }, summary, next }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeCommand {
    Pause,
    Resume,
    Remove,
    Verify,
}

pub fn changes(command: ChangeCommand, changes: &[Change]) -> Doc {
    let full: Vec<TorrentId> = changes.iter().filter_map(|c| c.id.parse().ok()).collect();
    let short = |id: &str| match id.parse::<TorrentId>() {
        Ok(t) => shortest_unique_prefix(&t, &full, SHORT_ID),
        Err(_) => short_ref(id),
    };
    let ids: Vec<String> = changes.iter().map(|c| short(&c.id)).collect();
    let rows = changes
        .iter()
        .zip(&ids)
        .map(|(c, id)| {
            let last = match command {
                ChangeCommand::Remove => c.path.as_ref().map_or_else(
                    || Cell::styled(format::UNKNOWN, Token::Muted),
                    |p| Cell::new(p.display().to_string()),
                ),
                _ => c.name.as_deref().map_or_else(|| Cell::styled(format::UNKNOWN, Token::Muted), Cell::new),
            };
            vec![Cell::styled(id.clone(), Token::Accent), Cell::styled(c.result.clone(), Token::Mint), last]
        })
        .collect();
    let header = match command {
        ChangeCommand::Remove => vec!["ID", "RESULT", "PATH"],
        _ => vec!["ID", "RESULT", "NAME"],
    };
    let (verb, follows): (&str, &[&str]) = match command {
        ChangeCommand::Pause => ("resume", &["paused", "already_paused"]),
        ChangeCommand::Resume => ("wait", &["resumed", "already_running"]),
        ChangeCommand::Verify => ("wait", &["checking"]),
        ChangeCommand::Remove => ("", &[]),
    };
    let next_ids: Vec<&str> = changes
        .iter()
        .zip(&ids)
        .filter(|(c, _)| follows.contains(&c.result.as_str()))
        .map(|(_, id)| id.as_str())
        .collect();
    let next = (!next_ids.is_empty()).then(|| format!("torrential {verb} {}", next_ids.join(" ")));
    let summary = match command {
        ChangeCommand::Remove => vec!["files were kept".to_string()],
        _ => Vec::new(),
    };
    Doc { body: Body::Table { header, rows }, summary, next }
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
        WaitEnd::Metadata => (vec![format!("file list known after {after}")], Some(format!("torrential files {all}"))),
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
                vec![format!("{id} is {state}, so it will not finish on its own")],
                Some(format!("torrential resume {id}")),
            )
        }
    }
}

#[derive(Clone, PartialEq, Debug, serde::Serialize)]
pub struct DaemonReport {
    pub engine: &'static str,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub socket: String,
    pub log: String,
    pub downloads: Option<usize>,
}

pub fn daemon_status(report: &DaemonReport) -> Doc {
    let running = report.pid.is_some();
    let mut fields = vec![("engine", Cell::styled(report.engine, if running { Token::Mint } else { Token::Muted }))];
    if let Some(pid) = report.pid {
        fields.push(("pid", Cell::new(pid.to_string())));
    }
    if let Some(version) = &report.version {
        fields.push(("version", Cell::new(version.clone())));
    }
    if let Some(downloads) = report.downloads {
        fields.push(("downloads", Cell::new(downloads.to_string())));
    }
    fields.push(("socket", Cell::new(report.socket.clone())));
    fields.push(("log", Cell::new(report.log.clone())));
    Doc {
        body: Body::Fields { title: Vec::new(), fields },
        summary: Vec::new(),
        next: Some("torrential status".to_string()),
    }
}

#[derive(Clone, PartialEq, Debug, serde::Serialize)]
pub struct Check {
    pub check: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skipped,
}

impl CheckStatus {
    fn cell(self) -> Cell {
        match self {
            Self::Ok => Cell::styled("ok", Token::Mint),
            Self::Warn => Cell::styled("warn", Token::Accent),
            Self::Fail => Cell::styled("fail", Token::Error),
            Self::Skipped => Cell::styled("skipped", Token::Muted),
        }
    }
}

pub fn doctor(checks: &[Check], fix: Option<String>) -> Doc {
    let rows = checks.iter().map(|c| vec![Cell::new(&c.check), c.status.cell(), Cell::new(c.detail.clone())]).collect();
    let probed = checks.iter().any(|c| c.check.starts_with("source "));
    let problems = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Warn | CheckStatus::Fail))
        .filter(|c| !(probed && c.check == "sources"))
        .count();
    Doc {
        body: Body::Table { header: vec!["CHECK", "STATUS", "DETAIL"], rows },
        summary: vec![format::count(problems, "problem")],
        next: fix,
    }
}

pub fn setup(profile: &Profile, updates: Policy, file: &Path, result: &str) -> Doc {
    let detected = profile.detected.as_ref().map_or_else(|| "nothing".to_string(), machine);
    let fields = vec![
        ("detected", Cell::new(detected)),
        ("updates", Cell::new(updates.as_str())),
        ("file", Cell::new(file.display().to_string())),
    ];
    let title = match result {
        "shown" => Vec::new(),
        word => vec![Cell::styled(word, Token::Mint)],
    };
    let overrides = profile.overrides().unwrap_or_else(|| "none".to_string());
    let mut summary = vec![format!("ranking for {} · overrides: {overrides}", profile.describe())];
    if let Some(max) = profile.preferences.max_size {
        summary.push(format!("get refuses a download over {} unless --max-size allows it", format_limit(max)));
    }
    let next = if result == "shown" { crate::cli::SETUP_FLAGS } else { "torrential search <words>" };
    Doc { body: Body::Fields { title, fields }, summary, next: Some(next.to_string()) }
}

fn machine(machine: &Machine) -> String {
    let display = match machine.display {
        Some(d) => format!("{}x{} display{}", d.width, d.height, if d.hdr { " with HDR" } else { "" }),
        None => "no display".to_string(),
    };
    let arch = machine.arch.map(|a| a.as_str().to_string());
    let language = machine.language.as_ref().map(|l| l.name().to_string());
    [Some(display), arch, language].into_iter().flatten().collect::<Vec<_>>().join(", ")
}

fn skill_state_cell(state: State) -> Cell {
    let token = match state {
        State::UpToDate => Token::Mint,
        State::Missing => Token::Muted,
        State::Outdated | State::Modified | State::Foreign => Token::Accent,
    };
    Cell::styled(state.name(), token)
}

pub fn skill_status(installed: &[Installed], version: &str, install: &str) -> Doc {
    let rows = installed
        .iter()
        .map(|i| {
            vec![
                Cell::styled(i.target.name(), Token::Accent),
                skill_state_cell(i.state),
                Cell::new(i.version.as_deref().unwrap_or(NONE)),
                Cell::new(i.path.display().to_string()),
            ]
        })
        .collect();
    let mut summary = vec![format!("this binary carries the skill from torrential {version}")];
    for i in installed {
        match i.state {
            State::Modified => summary.push(format!("{} was edited after torrential wrote it", i.target.name())),
            State::Foreign => summary.push(format!("{} was not written by torrential", i.target.name())),
            _ => {}
        }
    }
    let kept = installed.iter().any(|i| matches!(i.state, State::Modified | State::Foreign));
    if kept {
        summary.push(format!("{install} --force replaces them"));
    }
    let stale = installed.iter().any(|i| matches!(i.state, State::Missing | State::Outdated));
    let next = (stale && !kept).then(|| install.to_string());
    Doc { body: Body::Table { header: vec!["TARGET", "STATE", "VERSION", "PATH"], rows }, summary, next }
}

pub fn skill_changes(changes: &[SkillChange]) -> Doc {
    let rows = changes
        .iter()
        .map(|c| {
            vec![
                Cell::styled(c.target.name(), Token::Accent),
                Cell::styled(c.result, Token::Mint),
                Cell::new(c.path.display().to_string()),
            ]
        })
        .collect();
    Doc { body: Body::Table { header: vec!["TARGET", "RESULT", "PATH"], rows }, summary: Vec::new(), next: None }
}

pub fn skill_check(installed: &[Installed]) -> (Check, Option<String>) {
    let states: Vec<State> = installed.iter().map(|i| i.state).collect();
    let detail = installed.iter().map(|i| format!("{} {}", i.target.name(), i.state.name())).collect::<Vec<_>>();
    let status = if states.iter().all(|s| *s == State::UpToDate) {
        CheckStatus::Ok
    } else if states.iter().all(|s| *s == State::Missing) {
        CheckStatus::Skipped
    } else {
        CheckStatus::Warn
    };
    let fixable = states.iter().all(|s| matches!(s, State::UpToDate | State::Missing | State::Outdated));
    let fix = (fixable && status != CheckStatus::Ok).then(|| "torrential skill install".to_string());
    (Check { check: "skill".into(), status, detail: detail.join(", ") }, fix)
}

#[cfg(test)]
mod tests;
