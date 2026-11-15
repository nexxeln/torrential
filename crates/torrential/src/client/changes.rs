//! `torrential pause`, `resume`, `rm` and `verify`: what each did to the downloads it named.

use std::time::Duration;

use torrential_core::id::shortest_unique_prefix;
use torrential_core::protocol::{Change, Request, Response};
use torrential_core::{Error, TorrentId};

use super::connect::{self, Conn};
use super::rows::{SHORT_ID, short_ref};
use super::{answer, emit};
use crate::output::format;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, Mode};

pub fn run(mut conn: Conn, request: Request, command: ChangeCommand, mode: Mode) -> Result<u8, Error> {
    let within = match command {
        ChangeCommand::Pause => Duration::from_secs(20),
        _ => connect::DEFAULT_REPLY_WITHIN,
    };
    match answer(conn.call_within(&request, within)?)? {
        Response::Changed { changes } => {
            let doc = self::changes(command, &changes);
            Ok(emit(mode, &doc, &Response::Changed { changes }))
        }
        other => Err(connect::unexpected(&other)),
    }
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
        ChangeCommand::Remove => removed_files(changes, &ids),
        _ => Vec::new(),
    };
    Doc { body: Body::Table { header, rows }, summary, next }
}

/// What `rm` did with the files of the downloads it removed, and what it left and why.
fn removed_files(changes: &[Change], ids: &[String]) -> Vec<String> {
    // Each fate, and the downloads it was, in the order they come.
    let mut fates: Vec<(&str, Vec<&str>)> = Vec::new();
    for (change, id) in changes.iter().zip(ids).filter(|(c, _)| c.result == "removed") {
        let fate = match (change.deleted.is_empty(), change.trashed.is_empty()) {
            (false, _) => "deleted",
            (true, false) => "moved to the Trash",
            (true, true) => "kept",
        };
        match fates.iter_mut().find(|(f, _)| *f == fate) {
            Some((_, of)) => of.push(id),
            None => fates.push((fate, vec![id])),
        }
    }
    let done = match &fates[..] {
        [] => "files were kept".to_string(),
        [(fate, _)] => format!("files were {fate}"),
        [rest @ .., last] => {
            let said = |(fate, of): &(&str, Vec<&str>)| format!("files of {} were {fate}", of.join(", "));
            format!("{}, and {}", rest.iter().map(said).collect::<Vec<_>>().join(", "), said(last))
        }
    };
    let left = changes.iter().flat_map(|c| &c.kept).map(|k| format!("kept {}: {}", k.path.display(), k.reason));
    std::iter::once(done).chain(left).collect()
}
