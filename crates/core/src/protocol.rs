//! Messages between a client (CLI or TUI) and the engine process over a Unix socket.
//!
//! A frame is a 4-byte big-endian length followed by that many bytes of JSON. The
//! client's first frame is `Request::Hello`. A `Watch` request turns the connection
//! into a stream of `Event` frames until the client disconnects. A title play sends a
//! `TitleFrame::Step` for each step before its reply.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::catalog::{EpisodeInfo, TitleCard, TitleDetails, TitleId, VideoRef};
use crate::error::Error;
use crate::id::Target;
use crate::job::{Job, Selection, TorrentFile};
use crate::release::{Release, SearchQuery, SearchResult};
use crate::watching::{AttemptEvent, Next, Tried, Watched};

pub const WIRE_VERSION: u32 = 5;
/// Version of every JSON document printed with `--json`.
pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum Request {
    Hello {
        client_version: String,
        wire: u32,
    },
    /// With `stream`, the engine sends an `Event::SearchUpdate` frame each time a source answers,
    /// then the final `Response::Search`.
    Search {
        query: SearchQuery,
        #[serde(default)]
        stream: bool,
    },
    /// Lists a torrent's files, fetching metadata if needed, within `timeout_ms`.
    Files {
        target: Target,
        timeout_ms: u32,
    },
    Get(GetRequest),
    /// Downloads these files first and in order, and serves them over local HTTP as they arrive.
    Play(PlayRequest),
    /// No refs means every download.
    Status {
        refs: Vec<String>,
    },
    Pause {
        refs: Vec<String>,
    },
    Resume {
        refs: Vec<String>,
        all: bool,
    },
    /// Takes downloads off the list. A play's files go with it. Files in a folder the person chose
    /// go only with `delete`.
    Remove {
        refs: Vec<String>,
        #[serde(default)]
        delete: bool,
    },
    Verify {
        reference: String,
        repair: bool,
    },
    /// Streams `Event`s for these downloads, or for all downloads when empty.
    Watch {
        refs: Vec<String>,
    },
    /// Asks the engine to save resume data and exit, for example before an upgrade.
    Shutdown,
    /// Films and series named like `words`.
    Titles {
        words: String,
    },
    /// One film or series, with where the person stopped and what plays next.
    Title {
        id: TitleId,
    },
    /// A film's or an episode's releases, in the order a play tries them.
    Sources {
        video: VideoRef,
    },
    /// Plays a film or an episode from the first of its releases that starts. The engine sends a
    /// `TitleFrame::Step` for each step, then `Response::PlayTitle`.
    PlayTitle(TitleRequest),
    /// The same play, answered as `get` answers, for a download kept without a player. Once a
    /// source starts, it downloads as `get` downloads, not in playing order.
    GetTitle(TitleRequest),
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TitleRequest {
    /// `tt0063350`, `tt0903747:3:5`, or a series for its next episode.
    pub video: VideoRef,
    /// An ID prefix of one of the title's releases, to try only that one.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct GetRequest {
    pub target: Target,
    /// `None` means the configured download folder.
    pub dir: Option<PathBuf>,
    /// Globs or exact paths. Empty means every file.
    pub files: Vec<String>,
    /// `--max-size`, which wins over `setup_max_size`. On an existing download it replaces the cap.
    pub max_size: Option<u64>,
    /// The max size from `torrential setup`, the cap of a new download without `--max-size`.
    pub setup_max_size: Option<u64>,
    pub dry_run: bool,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct PlayRequest {
    pub target: Target,
    /// `None` means the configured download folder. An existing download keeps its own.
    pub dir: Option<PathBuf>,
    /// `TorrentFile::index` of each file to play: the video first, then its subtitles. They join any
    /// files the download already has selected.
    pub files: Vec<u32>,
    /// The max size from `torrential setup`, the cap of a new download.
    pub setup_max_size: Option<u64>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Response {
    Hello {
        engine_version: String,
        wire: u32,
        pid: u32,
        /// Where the engine serves downloads to players, and the web app, on 127.0.0.1. Zero from an
        /// engine older than the web app.
        #[serde(default)]
        http_port: u16,
    },
    Search {
        result: SearchResult,
    },
    Files {
        files: FilesReply,
    },
    Get {
        reply: Box<GetReply>,
    },
    Play {
        reply: Box<PlayReply>,
    },
    Jobs {
        jobs: Vec<Job>,
    },
    Changed {
        changes: Vec<Change>,
    },
    Titles {
        titles: Vec<TitleCard>,
    },
    Title {
        title: Box<TitlePage>,
    },
    Sources {
        sources: Box<TitleSources>,
    },
    PlayTitle {
        reply: Box<PlayReply>,
        title: Box<TitlePlayed>,
    },
    GetTitle {
        reply: Box<GetReply>,
        title: Box<TitlePlayed>,
    },
    Ok,
    Error {
        error: Error,
    },
}

/// One title as the catalog describes it, with what this engine knows of it.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TitlePage {
    #[serde(flatten)]
    pub details: TitleDetails,
    pub saved: bool,
    /// Every film or episode of the title with a position, the most recently watched last.
    pub watched: Vec<Watched>,
    /// `None` when the film is finished or the series is caught up.
    pub next: Option<Next>,
    /// The downloads made by playing this title.
    pub downloads: Vec<Job>,
}

/// A film's or an episode's releases, in the order a play tries them.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TitleSources {
    /// The film or episode, which for a series alone is its next episode.
    pub video: VideoRef,
    pub title: TitleCard,
    pub episode: Option<EpisodeInfo>,
    pub result: SearchResult,
}

/// The film or episode a title play started, and the releases it moved past.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TitlePlayed {
    pub video: VideoRef,
    /// The film's name and year, or the series' name and the episode.
    pub name: String,
    /// The release that started.
    pub release: Release,
    /// The releases tried before it, and why each did not start.
    pub tried: Vec<Tried>,
    /// Seconds into the video where the person stopped, unless they finished it.
    pub resume_at: Option<f64>,
}

/// What the engine sends for `PlayTitle` and `GetTitle`: a step at a time, then the reply.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TitleFrame {
    Step { step: Box<AttemptEvent> },
    Reply(Box<Response>),
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct FilesReply {
    pub id: String,
    pub name: Option<String>,
    pub total_size: u64,
    pub files: Vec<TorrentFile>,
    /// Where the file list came from: `download`, `cache`, `torrent_file`, `torrent_url`, or `peers`.
    pub metadata_source: String,
    pub elapsed_ms: u32,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct GetReply {
    pub outcome: GetOutcome,
    pub job: Job,
    /// Present for `--dry-run`, and when files are already known.
    pub plan: Option<DownloadPlan>,
    /// For `updated`, and for a `--dry-run` that would change the selection: the selection before this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_selection: Option<Selection>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct PlayReply {
    pub job: Job,
    /// One per requested file, in the request's order.
    pub streams: Vec<Stream>,
}

/// A file served over local HTTP while it downloads. Range requests fetch the bytes they ask for first.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Stream {
    pub index: u32,
    /// Relative to the torrent's folder, sanitized, as in `TorrentFile`.
    pub path: String,
    pub size: u64,
    pub url: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GetOutcome {
    Started,
    Queued,
    /// The download already existed with the same selection. Any `--max-size` became its cap.
    Existing,
    /// The download already existed, and its file selection changed.
    Updated,
    /// `--dry-run`: nothing was started.
    Planned,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct DownloadPlan {
    pub selected_files: u32,
    pub total_files: u32,
    pub selected_size: u64,
    pub destination: PathBuf,
    pub free_space: u64,
    /// Paths that already exist and would block the download.
    pub conflicts: Vec<PathBuf>,
    /// The size cap that applies, which for an existing download is its own.
    #[serde(default)]
    pub max_size: Option<u64>,
}

/// The result of a pause, resume, remove, or verify for one download.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Change {
    pub id: String,
    pub name: Option<String>,
    /// For example `paused`, `already_paused`, `resumed`, `removed`, `already_removed`, `checking`.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// What `rm` deleted from disk: a play's folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<PathBuf>,
    /// What `rm --delete` moved to the Trash: the files of a download in a folder the person chose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trashed: Vec<PathBuf>,
    /// What `rm` was to delete and left.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<Kept>,
}

/// A file or folder `rm` was to delete or move to the Trash and left, and why.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Kept {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum Event {
    /// The first event of every watch: the current state of each watched download.
    Snapshot {
        jobs: Vec<Job>,
    },
    Job {
        job: Box<Job>,
    },
    Removed {
        id: String,
    },
    /// Sent when nothing changed for a while, so watchers know the engine is alive.
    Heartbeat {
        at: i64,
    },
    /// A streaming search's results so far. `pending` names the sources that have not answered.
    SearchUpdate {
        result: Box<SearchResult>,
        #[serde(default)]
        pending: Vec<String>,
    },
}

/// Encodes one frame: a 4-byte big-endian length, then the JSON body.
pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, serde_json::Error> {
    let body = serde_json::to_vec(message)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_by_op() {
        let json = serde_json::to_value(Request::Pause { refs: vec!["9b5212".into()] }).unwrap();
        assert_eq!(json, serde_json::json!({"op": "pause", "refs": ["9b5212"]}));
    }

    #[test]
    fn a_search_without_stream_is_the_non_streaming_search() {
        let json = r#"{"op":"search","query":{"text":"arch","limit":10,"timeout_ms":6000}}"#;
        let Request::Search { query, stream } = serde_json::from_str(json).unwrap() else { panic!("not a search") };
        assert_eq!((query.text.as_str(), stream), ("arch", false));
    }

    #[test]
    fn a_title_play_steps_then_replies_in_frames_a_reader_tells_apart() {
        let step = TitleFrame::Step { step: Box::new(AttemptEvent::Check { check: crate::watching::Check::FileList }) };
        let text = serde_json::to_string(&step).unwrap();
        assert_eq!(text, r#"{"step":{"type":"check","check":"file_list"}}"#);
        assert_eq!(serde_json::from_str::<TitleFrame>(&text).unwrap(), step);
        let reply: TitleFrame = serde_json::from_str(r#"{"type":"ok"}"#).unwrap();
        assert_eq!(reply, TitleFrame::Reply(Box::new(Response::Ok)));
    }

    #[test]
    fn frames_carry_a_big_endian_length() {
        let frame = encode_frame(&Response::Ok).unwrap();
        assert_eq!(&frame[..4], &[0, 0, 0, 13]);
        assert_eq!(&frame[4..], br#"{"type":"ok"}"#);
    }
}
