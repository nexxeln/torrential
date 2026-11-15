//! Downloads by reference: finding what a target names, its files, and getting or playing it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use torrential_core::protocol::{Change, Event, FilesReply, GetRequest, METADATA_WAIT, PlayReply, PlayRequest, Stream};
use torrential_core::text::sanitize;
use torrential_core::{
    Error, ErrorCode, HexPrefix, Release, ReleaseId, SearchQuery, SearchResult, Target, TorrentFile, TorrentId,
};

use super::ctx::Ctx;
use super::engine_port::Metainfo;
use super::files;
use super::http;
use super::magnet::{self, Magnet};
use super::owner::plays::Destination;
use super::owner::{
    Command, FileBytes, FilesInfo, Finish, GetCommand, MetaTarget, Origin, PlayCommand, Resolved, Source, Update,
};
use super::search_port::{BoxFuture, Progress};
use crate::log::log;

const MAX_TORRENT_BYTES: u64 = 16 * 1024 * 1024;
/// How often a stream of download events with nothing new says it is still there.
const HEARTBEAT: Duration = Duration::from_secs(15);

pub(super) async fn search(ctx: &Ctx, query: SearchQuery) -> Result<SearchResult, Error> {
    let (preferences, player) = ((ctx.preferences)(), query.profile.player);
    let mut result = ctx.search.search(query, &|| ctx.machine.profile(preferences, player)).await?;
    record(ctx, &mut result);
    Ok(result)
}

/// A search as it runs. Each answer is recorded, as `search` records its results.
pub(super) struct Searching<'a> {
    ctx: &'a Ctx,
    /// `None` once every source has answered or the deadline has passed.
    running: Option<BoxFuture<'a, Result<SearchResult, Error>>>,
    answers: mpsc::UnboundedReceiver<Progress>,
}

/// What a running search has to say.
pub(super) enum Answer {
    /// What the sources that have answered found, and the sources still searching.
    SoFar(Progress),
    /// The final results, once every source has answered or the deadline has passed.
    Done(Result<SearchResult, Error>),
}

impl<'a> Searching<'a> {
    pub(super) fn start(ctx: &'a Ctx, query: SearchQuery) -> Self {
        let (progress, answers) = mpsc::unbounded_channel();
        let running = Box::pin(async move {
            let (preferences, player) = ((ctx.preferences)(), query.profile.player);
            let rank_for = || ctx.machine.profile(preferences, player);
            ctx.search.search_stream(query, &rank_for, progress).await
        });
        Self { ctx, running: Some(running), answers }
    }

    pub(super) fn over(&self) -> bool {
        self.running.is_none()
    }

    /// Waits for the next source to answer. Once the search is over it never returns.
    pub(super) async fn next(&mut self) -> Answer {
        let Some(running) = &mut self.running else { return std::future::pending().await };
        let mut answer = tokio::select! {
            biased;
            Some(progress) = self.answers.recv() => Answer::SoFar(progress),
            result = running => {
                self.running = None;
                Answer::Done(result)
            }
        };
        if let Answer::SoFar(Progress { result, .. }) | Answer::Done(Ok(result)) = &mut answer {
            record(self.ctx, result);
        }
        answer
    }
}

/// The events of some downloads: a snapshot of them, then each change, with a heartbeat while
/// nothing changes.
pub(super) struct JobEvents<'a> {
    ctx: &'a Ctx,
    /// The downloads followed, or `None` for every download, those started later too.
    followed: Option<Vec<TorrentId>>,
    updates: broadcast::Receiver<Update>,
    heartbeat: tokio::time::Interval,
    /// Changes were missed, so the next event is a fresh snapshot.
    lagged: bool,
}

impl<'a> JobEvents<'a> {
    /// Follows the downloads `refs` names, or every download when it names none, and answers the
    /// first event: a snapshot of them.
    pub(super) async fn start(ctx: &'a Ctx, refs: Vec<String>) -> Result<(Self, Event), Error> {
        let everything = refs.is_empty();
        let (jobs, updates) = ctx.owner.ask(|reply| Command::Watch { refs, reply }).await?;
        let followed = (!everything).then(|| jobs.iter().map(|job| job.id).collect());
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        Ok((Self { ctx, followed, updates, heartbeat, lagged: false }, Event::Snapshot { jobs }))
    }

    /// The next event, or `None` once the engine stops.
    pub(super) async fn next(&mut self) -> Option<Event> {
        let event = loop {
            if self.lagged {
                let refs = self.followed.iter().flatten().map(|id| id.to_hex()).collect();
                let jobs = self.ctx.owner.ask(|reply| Command::Status { refs, reply }).await.ok()?;
                self.lagged = false;
                break Event::Snapshot { jobs };
            }
            tokio::select! {
                update = self.updates.recv() => match update {
                    Ok(Update::Job(job)) if self.follows(job.id) => break Event::Job { job },
                    Ok(Update::Removed(id)) if self.follows(id) => break Event::Removed { id: id.to_hex() },
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => self.lagged = true,
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
                _ = self.heartbeat.tick() => break Event::Heartbeat { at: torrential_core::time::now() },
            }
        };
        self.heartbeat.reset();
        Some(event)
    }

    fn follows(&self, id: TorrentId) -> bool {
        self.followed.as_ref().is_none_or(|followed| followed.contains(&id))
    }
}

/// Pauses the downloads `refs` names.
pub(super) async fn pause(ctx: &Ctx, refs: Vec<String>) -> Result<Vec<Change>, Error> {
    ctx.owner.ask(|reply| Command::Pause { refs, reply }).await
}

/// Resumes the downloads `refs` names, or every paused one when `all` is set.
pub(super) async fn resume(ctx: &Ctx, refs: Vec<String>, all: bool) -> Result<Vec<Change>, Error> {
    ctx.owner.ask(|reply| Command::Resume { refs, all, reply }).await
}

/// Removes the downloads `refs` names, and with `delete` their files too.
pub(super) async fn remove(ctx: &Ctx, refs: Vec<String>, delete: bool) -> Result<Vec<Change>, Error> {
    ctx.owner.ask(|reply| Command::Remove { refs, delete, reply }).await
}

pub(super) fn record(ctx: &Ctx, result: &mut SearchResult) {
    for release in &mut result.releases {
        release.name = sanitize(&release.name);
    }
    let saved = result.releases.iter().map(|r| (r.clone(), ctx.search.saved_link(&r.id))).collect();
    ctx.owner.tell(Command::RecordResults(saved));
}

fn parse(ctx: &Ctx, bytes: &[u8]) -> Result<Metainfo, Error> {
    if bytes.len() as u64 > MAX_TORRENT_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidTorrent,
            format!("{} bytes is too large for a .torrent file", bytes.len()),
        ));
    }
    (ctx.parse)(bytes)
}

async fn read_torrent(ctx: &Ctx, path: &std::path::Path) -> Result<Metainfo, Error> {
    let unreadable =
        |e: std::io::Error| Error::new(ErrorCode::InvalidRef, format!("cannot read {}: {e}", path.display()));
    let size = tokio::fs::metadata(path).await.map_err(unreadable)?.len();
    if size > MAX_TORRENT_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidTorrent,
            format!("{} is {size} bytes, too large for a .torrent file", path.display()),
        ));
    }
    parse(ctx, &tokio::fs::read(path).await.map_err(unreadable)?)
}

async fn fetch(ctx: &Ctx, url: String) -> Result<Metainfo, Error> {
    let bytes = ctx.search.fetch_torrent(url).await?;
    let metainfo = parse(ctx, &bytes)?;
    ctx.owner.tell(Command::Cache { metainfo: metainfo.clone() });
    Ok(metainfo)
}

pub(super) async fn resolve(ctx: &Ctx, target: Target, command: &'static str) -> Result<Source, Error> {
    match target {
        Target::Prefix(prefix) => {
            let prefix = prefix.parse()?;
            let results = ctx.search.find(&prefix);
            resolve_prefix(ctx, prefix, results, command).await
        }
        Target::InfoHash(hash) => {
            let prefix = hash.id().to_hex().parse()?;
            match ctx.search.find(&prefix) {
                results if results.is_empty() => Ok(Source::Magnet(magnet::from_hash(hash))),
                results => resolve_prefix(ctx, prefix, results, command).await,
            }
        }
        Target::Magnet(uri) => Ok(Source::Magnet(magnet::parse(&uri)?)),
        Target::TorrentUrl(url) => Ok(Source::Torrent { metainfo: fetch(ctx, url).await?, origin: Origin::Url }),
        Target::TorrentFile(path) => {
            Ok(Source::Torrent { metainfo: read_torrent(ctx, &path).await?, origin: Origin::File })
        }
        Target::Title(video) => Err(torrential_core::id::not_a_torrent(&video, command)),
    }
}

async fn resolve_prefix(
    ctx: &Ctx,
    prefix: HexPrefix,
    results: Vec<Release>,
    command: &'static str,
) -> Result<Source, Error> {
    match ctx.owner.ask(|reply| Command::Resolve { prefix, results, command, reply }).await? {
        Resolved::Job(id) => Ok(Source::Download(id)),
        Resolved::Release { release, cached } => release_source(ctx, *release, cached).await,
    }
}

pub(super) async fn release_source(
    ctx: &Ctx,
    release: Release,
    cached: Option<Arc<Metainfo>>,
) -> Result<Source, Error> {
    if let Some(metainfo) = cached {
        return Ok(Source::Torrent { metainfo: (*metainfo).clone(), origin: Origin::Cache });
    }
    let magnet = release.magnet.as_deref().map(magnet::parse).transpose()?;
    if let Some(url) = ctx.search.torrent_url(&release)? {
        match fetch(ctx, url).await {
            Ok(metainfo) => return Ok(Source::Torrent { metainfo, origin: Origin::Url }),
            Err(error) if !magnet.as_ref().is_some_and(Magnet::has_trackers) => return Err(error),
            Err(error) => log(&format!("{} falls back to its magnet: {}", release.id.to_ref(), error.message)),
        }
    }
    match (magnet, release.id) {
        (Some(magnet), _) => Ok(Source::Magnet(magnet)),
        (None, ReleaseId::Torrent(id)) => {
            Ok(Source::Magnet(magnet::from_hash(torrential_core::InfoHash::V1(*id.as_bytes()))))
        }
        (None, ReleaseId::Locator(_)) => Err(Error::new(
            ErrorCode::InvalidRef,
            format!("{} has no magnet or .torrent link", sanitize(&release.name)),
        )),
    }
}

pub(super) async fn metadata(ctx: &Ctx, target: MetaTarget, deadline: Instant) -> Result<FilesInfo, Error> {
    ctx.owner.ask(|reply| Command::Metadata { target, deadline, reply }).await
}

pub(super) async fn get(ctx: &Ctx, request: GetRequest) -> Result<torrential_core::protocol::GetReply, Error> {
    let GetRequest { target, dir, files, max_size, dry_run } = request;
    let setup_max_size = (ctx.preferences)().max_size;
    let mut source = resolve(ctx, target, "get").await?;
    if dry_run && let Source::Magnet(magnet) = &source {
        let info = metadata(ctx, MetaTarget::Magnet(magnet.clone()), Instant::now() + METADATA_WAIT).await?;
        source = match info.download {
            Some(_) => Source::Download(info.id),
            None => Source::Torrent { metainfo: (*info.metainfo).clone(), origin: Origin::Peers },
        };
    }
    let get = GetCommand { source, dir, files, max_size, setup_max_size, dry_run };
    ctx.owner.ask(|reply| Command::Get { get: Box::new(get), reply }).await
}

pub(super) async fn play(ctx: &Ctx, request: PlayRequest) -> Result<PlayReply, Error> {
    let _playing = ctx.playing.open()?;
    let PlayRequest { target, dir, files } = request;
    let setup_max_size = (ctx.preferences)().max_size;
    let source = resolve(ctx, target, "play").await?;
    let to = Destination::Folder(dir);
    let play = PlayCommand { source, to, files: files.clone(), setup_max_size, title: None, finish: Finish::Stream };
    let (job, metainfo) = ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await?;
    let streams = streams(ctx, job.id, &metainfo, files);
    Ok(PlayReply { job, streams })
}

/// Where `files` of the download `id` are served, in the order given.
pub(super) fn streams(ctx: &Ctx, id: TorrentId, metainfo: &Metainfo, files: Vec<u32>) -> Vec<Stream> {
    files
        .into_iter()
        .map(|index| {
            let Served { index, path, size, .. } = served(id, metainfo, index);
            let url = http::url(ctx.http_port, id, index, &path);
            Stream { index, path, size, url }
        })
        .collect()
}

/// A file the engine serves while it downloads.
#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "StreamedFile"))]
pub struct Served {
    pub index: u32,
    pub path: String,
    pub size: u64,
    /// The path of `http::url`, for a page on the engine's own origin.
    pub url: String,
    /// Where the file starts in the torrent, in bytes.
    pub offset: u64,
}

/// File `index` of the download `id`, as the engine serves it.
pub(super) fn served(id: TorrentId, metainfo: &Metainfo, index: u32) -> Served {
    let file = &metainfo.files[index as usize];
    let path = sanitize(&file.path);
    Served {
        index,
        url: http::route_of(id, index, &path),
        path,
        size: file.size,
        offset: metainfo.files[..index as usize].iter().map(|f| f.size).sum(),
    }
}

pub(super) async fn files(ctx: &Ctx, target: Target, timeout: Duration) -> Result<FilesReply, Error> {
    let started = Instant::now();
    let deadline = started + timeout;
    let info = match resolve(ctx, target, "files").await? {
        Source::Download(id) => metadata(ctx, MetaTarget::Job(id), deadline).await?,
        Source::Magnet(magnet) => metadata(ctx, MetaTarget::Magnet(magnet), deadline).await?,
        Source::Torrent { metainfo, origin } => FilesInfo {
            id: metainfo.info_hash.id(),
            metainfo: Arc::new(metainfo),
            source: origin.source(),
            download: None,
        },
    };
    let m = &info.metainfo;
    Ok(FilesReply {
        id: info.id.to_hex(),
        name: Some(sanitize(&m.name)),
        total_size: m.total(),
        files: listed(ctx, &info).await,
        metadata_source: info.source,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
    })
}

/// A torrent's files, with what its download has selected and verified of each.
pub(super) async fn listed(ctx: &Ctx, info: &FilesInfo) -> Vec<TorrentFile> {
    let m = &info.metainfo;
    let verified: Vec<u64> = match info.download.as_ref().map(|d| &d.verified) {
        Some(FileBytes::Known(bytes)) => bytes.clone(),
        Some(FileBytes::Engine { saved }) => ctx.engine.file_progress(info.id).await.unwrap_or_else(|_| saved.clone()),
        None => Vec::new(),
    };
    let risks = files::risks(m);
    m.files
        .iter()
        .enumerate()
        .map(|(i, f)| TorrentFile {
            index: i as u32,
            path: sanitize(&f.path),
            size: f.size,
            selected: info.download.as_ref().is_some_and(|d| d.wanted[i]),
            verified: verified.get(i).copied().unwrap_or(0),
            risk: risks[i],
        })
        .collect()
}
