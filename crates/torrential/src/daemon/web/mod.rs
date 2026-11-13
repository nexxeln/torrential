//! The web app's API: `/api/*` on the engine's HTTP server. web/src/api.ts is the same contract
//! from the browser's side.
//!
//! A route that starts work or changes anything takes a JSON POST. Any website can make a browser
//! send a GET here, and an old browser does not say which, but a JSON POST from another website
//! waits for a preflight the engine never grants.

pub mod assets;
pub mod devices;
pub mod ffmpeg;
pub mod media;
mod online;
mod play;
mod thumbnail;
mod titles;
mod watch;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::Hash;
use std::io;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use torrential_core::{Category, Error, ErrorCode, Preferences, Profile, Release, SearchQuery, Target, TorrentId};

use super::devices::Asker;
use super::http::{Connection, Method, Request};
use super::owner::Command;
use super::search_port::Progress;
use super::server::{self, Ctx, METADATA_WAIT};
use super::subtitle_port::SubtitlePort;
use ffmpeg::Tools;
use media::Media;

const NDJSON: &str = "application/x-ndjson";
const VTT: &str = "text/vtt; charset=utf-8";
/// Converted subtitles kept in memory. One film's is some tens of kilobytes.
const SUBTITLES_KEPT: usize = 64;
const SEARCH_LIMIT: u16 = 50;
const MOST_RESULTS: u16 = 200;

/// A whole file the engine made and keeps: a converted subtitle, or a thumbnail.
type Made = Arc<Vec<u8>>;

/// What the API reads from outside the engine, and what it remembers between requests.
pub struct State {
    /// The preferences in config.yaml now, which rank searches and cap new downloads.
    pub preferences: Arc<dyn Fn() -> Preferences + Send + Sync>,
    /// Where ffmpeg and ffprobe are now. A person can install them while the engine runs.
    pub tools: Arc<dyn Fn() -> Tools + Send + Sync>,
    probes: Mutex<HashMap<(TorrentId, u32), Arc<Media>>>,
    pub subtitles: Arc<dyn SubtitlePort>,
    /// What the online sources have for each file asked about, with where each subtitle is.
    found: Mutex<HashMap<(TorrentId, u32), Arc<torrential_search::subtitles::Found>>>,
    /// Online subtitles as WebVTT, by file and subtitle ID.
    converted: Mutex<Kept<(TorrentId, u32, String), Made>>,
    /// Thumbnails, by file and second.
    thumbnails: Mutex<Kept<(TorrentId, u32, u32), Made>>,
    thumbnailing: tokio::sync::Semaphore,
}

impl State {
    pub fn new(
        preferences: Arc<dyn Fn() -> Preferences + Send + Sync>,
        tools: Arc<dyn Fn() -> Tools + Send + Sync>,
        subtitles: Arc<dyn SubtitlePort>,
    ) -> Self {
        Self {
            preferences,
            tools,
            probes: Mutex::default(),
            subtitles,
            found: Mutex::default(),
            converted: Mutex::new(Kept::new(SUBTITLES_KEPT)),
            thumbnails: Mutex::new(Kept::new(thumbnail::KEPT)),
            thumbnailing: tokio::sync::Semaphore::new(thumbnail::AT_ONCE),
        }
    }
}

/// The last `most` values put in, by key. Putting one more drops the oldest.
struct Kept<K, V> {
    most: usize,
    values: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Clone + Eq + Hash, V: Clone> Kept<K, V> {
    fn new(most: usize) -> Self {
        Self { most, values: HashMap::new(), order: VecDeque::new() }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.values.get(key).cloned()
    }

    fn put(&mut self, key: K, value: V) {
        if self.values.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.most {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
    }
}

/// Answers the request for `/api/{route}` from this Mac or a paired device.
pub async fn answer(conn: &mut Connection<'_>, request: &Request, route: &str, asker: &Asker) -> io::Result<bool> {
    match (request.method, route) {
        (Method::Get, "search") => return search(conn, request).await,
        (Method::Get, "events") => return watch::events(conn, request).await,
        (Method::Get, "stream") => return watch::stream(conn, request).await,
        (Method::Get, "subtitles") => return watch::subtitles(conn, request).await,
        (Method::Get, "title/sources") => return titles::sources(conn, request).await,
        (Method::Post, "title/play") => return titles::play(conn, request).await,
        (Method::Get, "title/art") => return titles::art(conn, request).await,
        _ => {}
    }
    let ctx = conn.ctx;
    match (request.method, route) {
        (Method::Get, "subtitles/online/fetch") => return whole(conn, request, VTT, online::vtt(ctx, request)).await,
        (Method::Get, "thumbnail") => return whole(conn, request, "image/jpeg", thumbnail::jpeg(ctx, request)).await,
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
        (Method::Post, "play") => value(play::play(ctx, body(request)?).await?),
        (Method::Get, "titles") => titles::search(ctx, request).await,
        (Method::Get, "title") => titles::page(ctx, request).await,
        (Method::Post, "watched") => titles::watched(ctx, body(request)?).await,
        (Method::Post, "library") => titles::library(ctx, body(request)?).await,
        (Method::Get, "home") => titles::home(ctx).await,
        (Method::Get, "media") => {
            let (id, file) = file_of(request)?;
            value(&*watch::media(ctx, id, file).await?)
        }
        (Method::Get, "subtitles/online") => {
            let (id, file) = file_of(request)?;
            online::list(ctx, id, file).await
        }
        (Method::Get, "pieces") => {
            let (id, file) = file_of(request)?;
            Ok(json!({ "pieces": watch::pieces(ctx, id, file).await? }))
        }
        (Method::Get, "jobs") => {
            let jobs = owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
            Ok(json!({ "jobs": jobs }))
        }
        (Method::Post, "files") => {
            let Ref { reference } = body(request)?;
            value(server::files(ctx, target(&reference)?, METADATA_WAIT).await?)
        }
        (Method::Post, "pause" | "resume" | "remove") => {
            let Refs { refs } = body(request)?;
            if refs.is_empty() {
                return Err(Error::new(ErrorCode::Usage, "refs names no download"));
            }
            let changes = match route {
                "pause" => owner.ask(|reply| Command::Pause { refs, reply }).await?,
                "resume" => owner.ask(|reply| Command::Resume { refs, all: false, reply }).await?,
                _ => owner.ask(|reply| Command::Remove { refs, reply }).await?,
            };
            Ok(json!({ "changes": changes }))
        }
        _ => Err(Error::new(ErrorCode::NotFound, format!("the engine has no {} /api/{route}", method(request)))),
    }
}

#[derive(Deserialize)]
struct Refs {
    refs: Vec<String>,
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
    conn.json(status(error.code), &json!({ "error": error }), request).await
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
    let ctx = conn.ctx;
    conn.begin(NDJSON, request).await?;
    let (progress_tx, mut progress) = mpsc::unbounded_channel();
    let preferences = query.profile.preferences;
    let rank_for = || ctx.machine.profile(preferences);
    let search = ctx.search.search_stream(query, &rank_for, progress_tx);
    tokio::pin!(search);
    let result = loop {
        tokio::select! {
            biased;
            Some(Progress { mut result, pending }) = progress.recv() => {
                server::record(ctx, &mut result);
                result.releases = order(result.releases);
                line(conn, &json!({ "type": "update", "result": result, "pending": pending })).await?;
            }
            result = &mut search => break result,
            gone = conn.gone() => return Err(gone),
        }
    };
    let last = match result {
        Ok(mut result) => {
            server::record(ctx, &mut result);
            result.releases = order(result.releases);
            json!({ "type": "done", "result": result })
        }
        Err(error) => json!({ "type": "error", "error": error }),
    };
    line(conn, &last).await?;
    conn.end().await
}

fn search_query(ctx: &Ctx, request: &Request) -> Result<SearchQuery, Error> {
    let text = request.param("q").map(|q| q.trim().to_string()).filter(|q| !q.is_empty());
    let text = text.ok_or_else(|| Error::new(ErrorCode::Usage, "the search has no words: the q parameter is empty"))?;
    let category = match request.param("category") {
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
        limit: number(request, "limit")?.unwrap_or(SEARCH_LIMIT).clamp(1, MOST_RESULTS),
        fresh: request.param("fresh").is_some_and(|fresh| fresh != "0" && fresh != "false"),
        profile: Profile { preferences: (ctx.web.preferences)(), ..Profile::default() },
        ..SearchQuery::new(text)
    })
}

async fn line(conn: &mut Connection<'_>, message: &Value) -> io::Result<()> {
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

    #[test]
    fn every_error_code_has_a_failing_status() {
        for code in ErrorCode::ALL {
            let number: u16 = status(code).split(' ').next().unwrap().parse().unwrap();
            assert!((400..600).contains(&number), "{code} answers {number}");
        }
        assert_eq!(status(ErrorCode::NotFound), "404 Not Found");
        assert_eq!(status(ErrorCode::MetadataTimeout), "504 Gateway Timeout");
        assert_eq!(status(ErrorCode::PathConflict), "409 Conflict");
    }
}
