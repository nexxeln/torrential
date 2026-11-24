//! The web app's API: `/api/*` on the engine's HTTP server. web/src/api.ts is the same contract
//! from the browser's side.
//!
//! A route that starts work or changes anything takes a JSON POST. Any website can make a browser
//! send a GET here, and an old browser does not say which, but a JSON POST from another website
//! waits for a preflight the engine never grants.

pub mod assets;
mod cast;
pub mod devices;
mod online;
pub mod play;
pub(super) mod titles;
pub mod watch;
#[cfg(test)]
mod wire;

use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use torrential_core::protocol::Change;
use torrential_core::{
    Category, Error, ErrorCode, Job, Profile, Release, SearchQuery, SearchResult, Target, TorrentId,
};

use super::ctx::Ctx;
use super::devices::Asker;
use super::downloads::{self, Answer, Searching};
use super::http::{Connection, Method, Request};
use super::media::{self, Kept, Made};
use super::owner::Command;
use super::search_port::Progress;
use super::subtitle_port::SubtitlePort;
use crate::ffmpeg::fetch::{Fetching, Stage};
use torrential_core::protocol::METADATA_WAIT;

const NDJSON: &str = "application/x-ndjson";
const VTT: &str = "text/vtt; charset=utf-8";
/// Converted subtitles kept in memory. One film's is some tens of kilobytes.
const SUBTITLES_KEPT: usize = 64;
/// Files whose online subtitles are kept. Each is a short list.
const FOUND_KEPT: usize = 64;
const SEARCH_LIMIT: u16 = 50;
const MOST_RESULTS: u16 = 200;

/// What the API reads from outside the engine, and what it remembers between requests.
pub struct State {
    pub subtitles: Arc<dyn SubtitlePort>,
    /// What the online sources have for each file asked about, with where each subtitle is.
    found: Mutex<Kept<(TorrentId, u32), Arc<online::Searched>>>,
    /// Online subtitles as WebVTT, by file and subtitle ID.
    converted: Mutex<Kept<(TorrentId, u32, String), Made>>,
}

impl State {
    pub fn new(subtitles: Arc<dyn SubtitlePort>) -> Self {
        Self { subtitles, found: Mutex::new(Kept::new(FOUND_KEPT)), converted: Mutex::new(Kept::new(SUBTITLES_KEPT)) }
    }
}

/// Answers the request for `/api/{route}` from this Mac or a paired device.
pub async fn answer(conn: &mut Connection<'_>, request: &Request, route: &str, asker: &Asker) -> io::Result<bool> {
    match (request.method, route) {
        (Method::Post, "search") => return search(conn, request).await,
        (Method::Get, "events") => return watch::events(conn, request).await,
        (Method::Get, "stream") => return watch::stream(conn, request).await,
        (Method::Get, "subtitles") => return watch::subtitles(conn, request).await,
        (Method::Get, "title/sources") => return titles::sources(conn, request, asker).await,
        (Method::Post, "title/play") => return titles::play(conn, request, asker).await,
        (Method::Get, "title/art") => return titles::art(conn, request).await,
        (Method::Post, "ffmpeg") => return ffmpeg(conn, request).await,
        _ => {}
    }
    let ctx = conn.ctx;
    match (request.method, route) {
        (Method::Get, "subtitles/online/fetch") => return whole(conn, request, VTT, online::vtt(ctx, request)).await,
        (Method::Get, "thumbnail") => return whole(conn, request, "image/jpeg", thumbnail(ctx, request)).await,
        _ => {}
    }
    // A browser that leaves, or stops a request, closes the connection. The work stops with it.
    let reply = tokio::select! {
        reply = respond(ctx, request, route, asker) => reply,
        gone = conn.gone() => return Err(gone),
    };
    match reply {
        Ok(body) => conn.json("200 OK", &body, request).await,
        Err(error) => fail(conn, request, &error).await,
    }
}

async fn respond(ctx: &Ctx, request: &Request, route: &str, asker: &Asker) -> Result<Value, Error> {
    let owner = &ctx.owner;
    match (request.method, route) {
        (Method::Get, "me") => devices::me(ctx, asker),
        (Method::Get, "devices") => devices::list(ctx, asker),
        (Method::Post, "devices/add") => devices::add(ctx, asker),
        (Method::Post, "devices/remove") => devices::remove(ctx, asker, body(request)?),
        (Method::Get, "tvs") => value(ctx.casts.list()),
        (Method::Post, "tvs") => cast::search(ctx, body(request)?).await,
        (Method::Post, "cast") => cast::start(ctx, body(request)?).await,
        (Method::Post, "cast/control") => cast::control(ctx, body(request)?).await,
        (Method::Post, "play") => value(play::play(ctx, body(request)?).await?),
        (Method::Get, "titles") => titles::search(ctx, request).await,
        (Method::Get, "title") => titles::page(ctx, request).await,
        (Method::Post, "watched") => titles::watched(ctx, body(request)?).await,
        (Method::Post, "watched/forget") => titles::forget(ctx, body(request)?).await,
        (Method::Post, "library") => titles::library(ctx, body(request)?).await,
        (Method::Post, "continue/dismiss") => titles::dismiss(ctx, body(request)?).await,
        (Method::Get, "home") => titles::home(ctx).await,
        (Method::Get, "version") => value(VersionReply { version: &ctx.version, notice: ctx.news.line() }),
        (Method::Get, "media") => {
            let (id, file) = file_of(request)?;
            value(&*media::probe(ctx, id, file).await?)
        }
        (Method::Get, "subtitles/online") => {
            let (id, file) = file_of(request)?;
            online::list(ctx, id, file).await
        }
        (Method::Get, "pieces") => {
            let (id, file) = file_of(request)?;
            value(Pieces { pieces: downloads::pieces(ctx, id, file).await? })
        }
        (Method::Get, "jobs") => {
            let jobs = owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
            value(Jobs { jobs })
        }
        (Method::Post, "files") => {
            let Ref { reference } = body(request)?;
            value(downloads::files(ctx, target(&reference)?, METADATA_WAIT).await?)
        }
        (Method::Post, "pause" | "resume" | "remove") => {
            let Refs { refs, delete } = body(request)?;
            if refs.is_empty() {
                return Err(Error::new(ErrorCode::Usage, "refs names no download"));
            }
            let changes = match route {
                "pause" => downloads::pause(ctx, refs).await?,
                "resume" => downloads::resume(ctx, refs, false).await?,
                _ => downloads::remove(ctx, refs, delete).await?,
            };
            value(Changes { changes })
        }
        _ => Err(Error::new(ErrorCode::NotFound, format!("the engine has no {} /api/{route}", method(request)))),
    }
}

/// `GET /api/version`.
#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "Version"))]
struct VersionReply<'a> {
    version: &'a str,
    /// What the update board says, if anything.
    notice: Option<String>,
}

/// `GET /api/pieces`: one character a run of the file, as `downloads::pieces` writes them.
#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "FilePieces"))]
struct Pieces {
    pieces: String,
}

/// `GET /api/jobs`.
#[derive(Serialize)]
struct Jobs {
    jobs: Vec<Job>,
}

/// `POST /api/pause`, `/api/resume` and `/api/remove`.
#[derive(Serialize)]
struct Changes {
    changes: Vec<Change>,
}

/// The body of every failed request.
#[derive(Serialize)]
pub struct ErrorBody<'a> {
    pub error: &'a Error,
}

/// A line of `POST /api/search`: the results so far as each source answers, then the last.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "SearchMessage"))]
enum SearchLine {
    Update { result: SearchResult, pending: Vec<String> },
    Done { result: SearchResult },
    Error { error: Error },
}

/// A line of `POST /api/ffmpeg`.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "FfmpegMessage"))]
enum FfmpegLine {
    Progress { done: u64, size: u64 },
    Ready,
    Error { error: Error },
}

/// `GET /api/thumbnail`: a JPEG of the frame at `t` seconds.
async fn thumbnail(ctx: &Ctx, request: &Request) -> Result<Made, Error> {
    let (id, index) = file_of(request)?;
    let t = number::<f64>(request, "t")?.filter(|t| t.is_finite() && *t >= 0.0);
    let t = t.ok_or_else(|| Error::new(ErrorCode::Usage, "the request has no t parameter of zero or more seconds"))?;
    media::thumbnail::jpeg(ctx, id, index, t).await
}

#[derive(Deserialize)]
struct Refs {
    refs: Vec<String>,
    /// With `remove`, deletes the files of a download in a folder the person chose too.
    #[serde(default)]
    delete: bool,
}

#[derive(Deserialize)]
struct Ref {
    #[serde(rename = "ref")]
    reference: String,
}

fn method(request: &Request) -> &'static str {
    match request.method {
        Method::Get => "GET",
        Method::Head => "HEAD",
        Method::Post => "POST",
    }
}

fn value(body: impl serde::Serialize) -> Result<Value, Error> {
    serde_json::to_value(body).map_err(|e| Error::internal(format!("cannot write the answer as JSON: {e}")))
}

fn body<T: DeserializeOwned>(request: &Request) -> Result<T, Error> {
    serde_json::from_slice(&request.body)
        .map_err(|e| Error::new(ErrorCode::Usage, format!("the request body is not the JSON this route takes: {e}")))
}

fn required(request: &Request, name: &str) -> Result<String, Error> {
    request.param(name).ok_or_else(|| Error::new(ErrorCode::Usage, format!("the request has no {name} parameter")))
}

fn number<T: std::str::FromStr>(request: &Request, name: &str) -> Result<Option<T>, Error> {
    let invalid =
        |value: String| Error::new(ErrorCode::Usage, format!("{name}={value} is not a number this route takes"));
    request.param(name).map(|value| value.parse().map_err(|_| invalid(value))).transpose()
}

/// The `id` and `file` parameters: a download and the index of one of its files.
fn file_of(request: &Request) -> Result<(TorrentId, u32), Error> {
    let id = required(request, "id")?;
    let id = id.parse().map_err(|_| Error::new(ErrorCode::InvalidRef, format!("{id} is not a download's full ID")))?;
    let file =
        number(request, "file")?.ok_or_else(|| Error::new(ErrorCode::Usage, "the request has no file parameter"))?;
    Ok((id, file))
}

/// What a page may name: a download or search result ID, an infohash, a magnet link, or the URL of
/// a .torrent file. Never a file on this machine.
fn target(reference: &str) -> Result<Target, Error> {
    match Target::parse(reference, std::path::Path::new("/"))? {
        Target::TorrentFile(_) => {
            Err(Error::new(ErrorCode::InvalidRef, "the web app cannot open a .torrent file by its path")
                .with_hint("use the torrent's magnet link, or run torrential get with the file"))
        }
        Target::Title(video) => {
            Err(Error::new(ErrorCode::InvalidRef, format!("{video} is a film or a series, not a torrent"))
                .with_hint("play a film or a series with POST /api/title/play"))
        }
        target => Ok(target),
    }
}

/// The HTTP status that goes with each error code.
fn status(code: ErrorCode) -> &'static str {
    use ErrorCode::*;
    match code {
        Usage | ConfirmationRequired => "400 Bad Request",
        NotPaired => "401 Unauthorized",
        LocalOnly => "403 Forbidden",
        InvalidRef | InvalidTorrent | NotFound | NoSelection => "404 Not Found",
        AmbiguousRef | JobConflict | PathConflict => "409 Conflict",
        TooLarge => "413 Content Too Large",
        FetchFailed => "502 Bad Gateway",
        MetadataTimeout => "504 Gateway Timeout",
        ProvidersUnavailable | NetworkUnavailable | NoSource | Interrupted => "503 Service Unavailable",
        SandboxNetworkBlocked | SandboxWriteBlocked | DaemonUnavailable | DaemonVersion => "503 Service Unavailable",
        DiskFull => "507 Insufficient Storage",
        StorageError | StateCorrupt | VerificationFailed | Internal => "500 Internal Server Error",
    }
}

async fn fail(conn: &mut Connection<'_>, request: &Request, error: &Error) -> io::Result<bool> {
    conn.json(status(error.code), &ErrorBody { error }, request).await
}

/// Answers with a whole file the engine made, which the browser may keep for an hour: the same
/// request gives the same bytes. The work stops if the browser leaves first.
async fn whole(
    conn: &mut Connection<'_>,
    request: &Request,
    content_type: &str,
    work: impl Future<Output = Result<Arc<Vec<u8>>, Error>>,
) -> io::Result<bool> {
    let made = tokio::select! {
        made = work => made,
        gone = conn.gone() => return Err(gone),
    };
    match made {
        Ok(bytes) => {
            conn.send("200 OK", content_type, "Cache-Control: private, max-age=3600\r\n", &bytes, request).await
        }
        Err(error) => fail(conn, request, &error).await,
    }
}

async fn search(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    match search_query(conn.ctx, request) {
        Ok(query) => results(conn, request, query, |releases| releases).await,
        Err(error) => fail(conn, request, &error).await,
    }
}

/// Sends the results so far as a line of JSON each time a source answers, then the final results,
/// with the releases in the order `order` puts them.
async fn results(
    conn: &mut Connection<'_>,
    request: &Request,
    query: SearchQuery,
    order: impl Fn(Vec<Release>) -> Vec<Release>,
) -> io::Result<bool> {
    conn.begin(NDJSON, request).await?;
    let mut search = Searching::start(conn.ctx, query);
    let last = loop {
        let answer = tokio::select! {
            biased;
            answer = search.next() => answer,
            gone = conn.gone() => return Err(gone),
        };
        match answer {
            Answer::SoFar(Progress { mut result, pending }) => {
                result.releases = order(result.releases);
                line(conn, &SearchLine::Update { result, pending }).await?;
            }
            Answer::Done(Ok(mut result)) => {
                result.releases = order(result.releases);
                break SearchLine::Done { result };
            }
            Answer::Done(Err(error)) => break SearchLine::Error { error },
        }
    };
    line(conn, &last).await?;
    conn.end().await
}

/// `POST /api/ffmpeg`: fetches ffmpeg when the engine has none, with a line of JSON as the download
/// goes and a last line `ready` or `error`. The fetch carries on when the browser leaves.
async fn ffmpeg(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let running = match conn.ctx.media.ffmpeg.fetch() {
        Ok(Fetching::Running(stage)) => Some(stage),
        Ok(Fetching::Here) => None,
        Err(error) => return fail(conn, request, &error).await,
    };
    conn.begin(NDJSON, request).await?;
    let last = match running {
        Some(stage) => follow(conn, stage).await?,
        None => FfmpegLine::Ready,
    };
    line(conn, &last).await?;
    conn.end().await
}

/// Sends a line for each step of a fetch, and answers its last line.
async fn follow(conn: &mut Connection<'_>, mut stage: tokio::sync::watch::Receiver<Stage>) -> io::Result<FfmpegLine> {
    loop {
        let now = stage.borrow_and_update().clone();
        match now {
            Stage::Downloading { done, size } => {
                line(conn, &FfmpegLine::Progress { done, size }).await?;
            }
            Stage::Done(Ok(())) => return Ok(FfmpegLine::Ready),
            Stage::Done(Err(error)) => return Ok(FfmpegLine::Error { error }),
        }
        tokio::select! {
            changed = stage.changed() => if changed.is_err() {
                let error = Error::internal("the ffmpeg fetch stopped before it finished");
                return Ok(FfmpegLine::Error { error });
            },
            gone = conn.gone() => return Err(gone),
        }
    }
}

/// What `POST /api/search` takes. A search can fetch .torrent files and keep fetching after it
/// answers, so it is a JSON POST, which another website cannot make without a preflight.
#[derive(Deserialize)]
struct Search {
    q: String,
    category: Option<String>,
    limit: Option<u16>,
    #[serde(default)]
    fresh: bool,
}

fn search_query(ctx: &Ctx, request: &Request) -> Result<SearchQuery, Error> {
    let Search { q, category, limit, fresh } = body(request)?;
    let text = q.trim();
    if text.is_empty() {
        return Err(Error::new(ErrorCode::Usage, "the search has no words: q is empty"));
    }
    let category = match category {
        Some(name) => Some(
            Category::ALL
                .into_iter()
                .find(|category| category.as_str() == name)
                .ok_or_else(|| Error::new(ErrorCode::Usage, format!("{name} is not a category")))?,
        ),
        None => None,
    };
    Ok(SearchQuery {
        category,
        limit: limit.unwrap_or(SEARCH_LIMIT).clamp(1, MOST_RESULTS),
        fresh,
        profile: Profile { preferences: (ctx.preferences)(), ..Profile::default() },
        ..SearchQuery::new(text.to_string())
    })
}

async fn line(conn: &mut Connection<'_>, message: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    conn.chunk(&line).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_cannot_name_a_file_on_this_machine() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("film.torrent");
        std::fs::write(&file, "d4:infod4:name1:aee").unwrap();
        let path = file.display().to_string();
        assert_eq!(target(&path).unwrap_err().code, ErrorCode::InvalidRef);
        assert_eq!(target("9b5212").unwrap(), Target::Prefix("9b5212".into()));
        assert!(matches!(target("magnet:?xt=urn:btih:9b5212").unwrap(), Target::Magnet(_)));
        assert!(matches!(target("https://example.org/a.torrent").unwrap(), Target::TorrentUrl(_)));
        assert_eq!(target("film").unwrap_err().code, ErrorCode::InvalidRef);
        let title = target("tt0063350").unwrap_err();
        assert_eq!(
            (title.code, title.message, title.hint),
            (
                ErrorCode::InvalidRef,
                "tt0063350 is a film or a series, not a torrent".into(),
                Some("play a film or a series with POST /api/title/play".into())
            )
        );
    }

    #[test]
    fn only_the_newest_values_are_kept() {
        let mut kept = Kept::new(2);
        kept.put("a", 1);
        kept.put("b", 2);
        kept.put("a", 3);
        assert_eq!((kept.get(&"a"), kept.get(&"b")), (Some(3), Some(2)));
        kept.put("c", 4);
        assert_eq!((kept.get(&"a"), kept.get(&"b"), kept.get(&"c")), (None, Some(2), Some(4)));
    }
}
