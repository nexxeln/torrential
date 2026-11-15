//! `torrential files`: the files of a torrent or a download.

use serde::Serialize;
use torrential_core::protocol::{FilesReply, MetadataSource};
use torrential_core::text::sanitize;
use torrential_core::units::format_size;
use torrential_core::{Error, Selection};

use super::rows::{rerun_get, short_ref};
use crate::output::format::{self, NONE};
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc};

#[derive(Serialize)]
pub struct Files {
    pub files: FilesReply,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<FileMatch>,
}

pub fn keep_matching(reply: &mut FilesReply, patterns: &[String]) -> Result<FileMatch, Error> {
    let set = crate::selection::compile(patterns)?;
    let wanted = reply.files.iter().map(|f| set.is_match(&f.path)).collect();
    let wanted = crate::selection::some_match(wanted, patterns, reply.name.as_deref().unwrap_or(&reply.id), &reply.id)?;
    let total_files = reply.files.len();
    let mut wanted = wanted.into_iter();
    reply.files.retain(|_| wanted.next().unwrap_or(false));
    Ok(FileMatch { patterns: patterns.to_vec(), total_files, size: reply.files.iter().map(|f| f.size).sum() })
}

const MAX_FILE_ROWS: usize = 200;

#[derive(Debug, serde::Serialize)]
pub struct FileMatch {
    pub patterns: Vec<String>,
    pub total_files: usize,
    pub size: u64,
}

pub fn files(reply: &FilesReply, matched: Option<&FileMatch>) -> Doc {
    let from_download = reply.metadata_source == MetadataSource::Download;
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
    let source = match reply.metadata_source {
        MetadataSource::Download => "from the download",
        MetadataSource::Cache => "from cache",
        MetadataSource::TorrentFile => "from the .torrent file",
        MetadataSource::TorrentUrl => "from the .torrent URL",
        MetadataSource::Peers => "from peers",
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
            "showing {MAX_FILE_ROWS} of {}; narrow the list with torrential files --files <GLOB>",
            format::count(count, "file")
        ));
    }
    if risky {
        summary.push("executables in a media torrent are a common sign of malware".to_string());
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
                summary.push(format!("to download only matching files: torrential get {id} --files <GLOB>"));
            }
            Some(format!("torrential get {id}"))
        }
    };
    Doc { body: Body::Table { header, rows }, summary, next }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::ErrorCode;
    use torrential_core::TorrentFile;

    fn reply() -> FilesReply {
        let file = |index, path: &str, size| TorrentFile {
            index,
            path: path.into(),
            size,
            selected: false,
            verified: 0,
            risk: None,
        };
        FilesReply {
            id: "6bc1af".to_string() + &"0".repeat(34),
            name: Some("film".into()),
            total_size: 600,
            files: vec![file(0, "Film.mp4", 400), file(1, "extras/Trailer.MP4", 150), file(2, "notes.txt", 50)],
            metadata_source: MetadataSource::TorrentFile,
            elapsed_ms: 1,
        }
    }

    fn keep(patterns: &[&str]) -> Result<(Vec<u32>, FileMatch), Error> {
        let mut reply = reply();
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        let matched = keep_matching(&mut reply, &patterns)?;
        Ok((reply.files.iter().map(|f| f.index).collect(), matched))
    }

    #[test]
    fn globs_keep_the_files_get_would_pick() {
        let (kept, matched) = keep(&["*.mp4"]).unwrap();
        assert_eq!(kept, [0, 1]);
        assert_eq!((matched.total_files, matched.size), (3, 550));
        assert_eq!(keep(&["NOTES.TXT", "film.mp4"]).unwrap().0, [0, 2]);
        assert_eq!(keep(&["extras/Trailer.MP4"]).unwrap().0, [1]);
    }

    #[test]
    fn globs_that_match_nothing_fail_like_get() {
        let err = keep(&["*.mkv"]).unwrap_err();
        assert_eq!(err.code, ErrorCode::NoSelection);
        assert_eq!(err.message, "no file in film matches \"*.mkv\"");
        assert_eq!(err.hint.as_deref(), Some("run torrential files 6bc1af to see the paths"));
        assert_eq!(keep(&["["]).unwrap_err().code, ErrorCode::Usage);
    }
}
