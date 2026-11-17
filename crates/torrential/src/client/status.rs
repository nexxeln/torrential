//! `torrential status`: the downloads, or one in detail.

use torrential_core::text::sanitize;
use torrential_core::units::format_size;
use torrential_core::{ErrorCode, Job, JobStatus};

use super::rows::{job_row, job_state_cell, next_for_stopped, selection, short_ids, state_name, waiting_line};
use crate::output::format;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc};

fn restartable(status: &JobStatus) -> bool {
    matches!(status, JobStatus::Error { error } if error.code != ErrorCode::TooLarge)
}

fn is_active(status: &JobStatus) -> bool {
    matches!(status, JobStatus::Queued | JobStatus::Metadata | JobStatus::Checking | JobStatus::Downloading)
}

pub const JOB_HEADER: [&str; 8] = ["ID", "STATE", "PROGRESS", "SIZE", "DOWN", "PEERS", "ETA", "NAME"];

pub fn jobs(jobs: &[Job]) -> Doc {
    let ids = short_ids(&jobs.iter().map(|j| j.id).collect::<Vec<_>>());
    let rows = jobs.iter().zip(&ids).map(|(job, id)| job_row(id, job)).collect();
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for job in jobs {
        let name = state_name(job);
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    let mut line = vec![format::count(jobs.len(), "download")];
    line.extend(counts.iter().map(|(name, count)| format!("{count} {name}")));
    let mut summary = vec![line.join(" · ")];
    summary.extend(waiting_line(jobs));
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
            summary.push(format!("to retry failed downloads: torrential resume {}", restart.join(" ")));
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
        ("state", job_state_cell(job)),
    ];
    let progress = match p.size {
        Some(size) => {
            format!("{} ({} of {})", format::percent(p), format_size(p.verified.min(size)), format_size(size))
        }
        None => format::UNKNOWN.to_string(),
    };
    fields.push(("progress", Cell::new(progress)));
    let downloading = job.status == JobStatus::Downloading && job.waiting_for.is_none();
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
    let mut summary: Vec<String> = waiting_line([job]).into_iter().collect();
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
