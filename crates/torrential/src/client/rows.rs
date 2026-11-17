//! Cells, rows and short IDs that more than one command prints.

use torrential_core::id::shortest_unique_prefix;
use torrential_core::text::sanitize;
use torrential_core::{Category, ErrorCode, Job, JobStatus, Profile, Release, ReleaseId, Selection, TorrentId};

use crate::output::Cell;
use crate::output::format::{self, NONE};
use crate::output::theme::Token;

pub use torrential_core::id::SHORT_ID;

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

/// A search result as a row: its ID, seeds, size, quality, age, source, flags, and name.
pub fn release_row(r: &Release, id: &str, video: bool, now: i64) -> Vec<Cell> {
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
        if flags.is_empty() { Cell::styled(NONE, Token::Muted) } else { Cell::styled(flags.join(","), Token::Mint) },
        Cell::new(&r.name),
    ]
}

pub fn ranked_for(profile: &Profile) -> String {
    match profile.overrides() {
        Some(overrides) => format!("{} · overrides: {overrides}", profile.describe()),
        None => profile.describe(),
    }
}

pub fn selection(selection: &Selection) -> String {
    match selection {
        Selection::All => "all".to_string(),
        Selection::Patterns(patterns) => patterns.iter().map(|p| sanitize(p)).collect::<Vec<_>>().join(" "),
    }
}

pub fn state_cell(status: &JobStatus) -> Cell {
    let token = match status {
        JobStatus::Downloading | JobStatus::Metadata | JobStatus::Checking => Token::Mint,
        JobStatus::Done { .. } => Token::Mint,
        JobStatus::Queued | JobStatus::Paused => Token::Accent,
        JobStatus::Error { .. } => Token::Error,
    };
    Cell::styled(status.name(), token)
}

/// The state a person reads for a download: `waiting` while the network interface it needs is
/// down, else its status.
pub fn state_name(job: &Job) -> &'static str {
    if job.waiting_for.is_some() { "waiting" } else { job.status.name() }
}

pub fn job_state_cell(job: &Job) -> Cell {
    if job.waiting_for.is_some() { Cell::styled(state_name(job), Token::Accent) } else { state_cell(&job.status) }
}

/// The line that says why downloads wait, once for all of them.
pub fn waiting_line<'a>(jobs: impl IntoIterator<Item = &'a Job>) -> Option<String> {
    let name = jobs.into_iter().find_map(|job| job.waiting_for.as_deref())?;
    Some(format!("waiting for {}, the torrent_interface in config.yaml, to come up", sanitize(name)))
}

pub fn next_for_stopped(status: &JobStatus, id: &str) -> Option<String> {
    match status {
        JobStatus::Error { error } if error.next.is_some() => error.next.as_deref().cloned(),
        JobStatus::Error { error } if error.code == ErrorCode::TooLarge => None,
        JobStatus::Paused | JobStatus::Error { .. } => Some(format!("torrential resume {id}")),
        _ => None,
    }
}

pub fn job_row(id: &str, job: &Job) -> Vec<Cell> {
    let active = matches!(job.status, JobStatus::Metadata | JobStatus::Checking | JobStatus::Downloading);
    let downloading = job.status == JobStatus::Downloading && job.waiting_for.is_none();
    vec![
        Cell::styled(id, Token::Accent),
        job_state_cell(job),
        Cell::new(format::percent(&job.progress)),
        Cell::new(format::size(job.progress.size)),
        Cell::new(if downloading { format::rate(job.progress.down_rate) } else { NONE.to_string() }),
        Cell::new(if active { job.progress.peers.to_string() } else { NONE.to_string() }),
        Cell::new(if downloading { format::eta(job.progress.eta) } else { NONE.to_string() }),
        job.name.as_deref().map_or_else(|| Cell::styled(format::UNKNOWN, Token::Muted), Cell::new),
    ]
}

pub fn rerun_get(id: &str, selection: &Selection, dir: Option<String>) -> String {
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
