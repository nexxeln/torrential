use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, oneshot};
use torrential_core::error::Details;
use torrential_core::protocol::{
    Continue, Event, FilesReply, GetOutcome, GetReply, GetRequest, Home, PlayReply, PlayRequest, Request, Response,
    Stream, TitleFrame, TitlePage, TitlePlayed, TitleRequest, TitleSources, WIRE_VERSION,
};
use torrential_core::text::sanitize;
use torrential_core::watching::next;
use torrential_core::{
    AttemptEvent, Error, ErrorCode, HexPrefix, Next, Player, Profile, Release, ReleaseId, SearchQuery, SearchResult,
    Target, TitleCard, TitleId, TorrentFile, TorrentId, Tried, VideoRef,
};

use super::engine_port::{EngineHandle, Metainfo, ParseTorrent};
use super::files;
use super::http;
use super::magnet::{self, Magnet};
use super::owner::plays::Destination;
use super::owner::{
    Command, FileBytes, FilesInfo, Finish, GetCommand, MetaTarget, Origin, PlayCommand, Reply, Resolved, Source, Update,
};
use super::search_port::{Progress, SearchPort};
use super::titles::attempt::{self, Containers, PlayTitle};
use crate::machine::Detector;
use crate::output::views::title_name;
use crate::update::Version;
use crate::wire;

const MAX_TORRENT_BYTES: u64 = 16 * 1024 * 1024;
/// Titles listed under Continue on the home screens.
const CONTINUE_MOST: usize = 20;
pub const METADATA_WAIT: Duration = Duration::from_secs(60);
pub(super) const HEARTBEAT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct OwnerHandle(pub mpsc::UnboundedSender<Command>);

impl OwnerHandle {
    pub async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        self.0.send(make(reply)).map_err(|_| stopping())?;
        answer.await.map_err(|_| stopping())?
    }

    pub fn tell(&self, command: Command) {
        let _ = self.0.send(command);
    }
}

fn stopping() -> Error {
    Error::new(ErrorCode::DaemonUnavailable, "the engine is shutting down")
        .with_hint("run the command again in a moment")
}

pub struct Ctx {
    pub owner: OwnerHandle,
    pub search: Arc<dyn SearchPort>,
    pub machine: Arc<Detector>,
    pub engine: EngineHandle,
    pub parse: ParseTorrent,
    pub version: String,
    /// Where `http` serves downloads to players, and the web app.
    pub http_port: u16,
    pub web: super::web::State,
    pub titles: super::titles::Titles,
    pub playing: super::playing::Playing,
    pub news: crate::update::Board,
    /// The phones and TVs this Mac let in, and the casts playing.
    pub devices: Arc<super::devices::Devices>,
}

pub async fn handle(ctx: &Ctx, request: Request) -> Response {
    let owner = &ctx.owner;
    let result = match request {
        Request::Hello { .. } | Request::Watch { .. } => {
            Err(Error::new(ErrorCode::Usage, "hello and watch are not single requests"))
        }
        Request::Search { query, .. } => search(ctx, query).await.map(|result| Response::Search { result }),
        Request::Files { target, timeout_ms } => {
            files(ctx, target, Duration::from_millis(timeout_ms.into())).await.map(|files| Response::Files { files })
        }
        Request::Get(request) => get(ctx, request).await.map(|reply| Response::Get { reply: Box::new(reply) }),
        Request::Play(request) => play(ctx, request).await.map(|reply| Response::Play { reply: Box::new(reply) }),
        Request::Status { refs } => {
            owner.ask(|reply| Command::Status { refs, reply }).await.map(|jobs| Response::Jobs { jobs })
        }
        Request::Pause { refs } => {
            owner.ask(|reply| Command::Pause { refs, reply }).await.map(|changes| Response::Changed { changes })
        }
        Request::Resume { refs, all } => {
            owner.ask(|reply| Command::Resume { refs, all, reply }).await.map(|changes| Response::Changed { changes })
        }
        Request::Remove { refs, delete } => owner
            .ask(|reply| Command::Remove { refs, delete, reply })
            .await
            .map(|changes| Response::Changed { changes }),
        Request::Verify { reference, repair } => owner
            .ask(|reply| Command::Verify { reference, repair, reply })
            .await
            .map(|changes| Response::Changed { changes }),
        Request::Shutdown { .. } => {
            let (reply, done) = oneshot::channel();
            owner.tell(Command::Shutdown { reply });
            let _ = done.await;
            Ok(Response::Ok)
        }
        Request::Titles { words } => {
            ctx.titles.catalog.search(words.trim()).await.map(|titles| Response::Titles { titles })
        }
        Request::Title { id } => title_page(ctx, id).await.map(|page| Response::Title { title: Box::new(page) }),
        Request::Sources { video } => {
            sources(ctx, video).await.map(|sources| Response::Sources { sources: Box::new(sources) })
        }
        Request::PlayTitle(request) => play_title(ctx, request, Finish::Stream, mpsc::unbounded_channel().0).await,
        Request::GetTitle(request) => play_title(ctx, request, Finish::Keep, mpsc::unbounded_channel().0).await,
        Request::Devices => Ok(Response::Devices { list: ctx.devices.list(ctx.http_port) }),
        Request::AddDevice => {
            ctx.devices.add(ctx.http_port, super::owner::now()).map(|list| Response::Devices { list })
        }
        Request::RemoveDevice { id } => ctx.devices.remove(&id, ctx.http_port).map(|list| Response::Devices { list }),
        Request::Home => home(ctx).await.map(|home| Response::Home { home: Box::new(home) }),
    };
    result.unwrap_or_else(|error| Response::Error { error })
}

async fn search(ctx: &Ctx, query: SearchQuery) -> Result<SearchResult, Error> {
    let Profile { preferences, player, .. } = query.profile;
    let mut result = ctx.search.search(query, &|| ctx.machine.profile(preferences, player)).await?;
    record(ctx, &mut result);
    Ok(result)
}

pub(super) fn record(ctx: &Ctx, result: &mut SearchResult) {
    for release in &mut result.releases {
        release.name = sanitize(&release.name);
    }
    let saved = result.releases.iter().map(|r| (r.clone(), ctx.search.saved_link(&r.id))).collect();
    ctx.owner.tell(Command::RecordResults(saved));
}

async fn stream_search(ctx: &Ctx, query: SearchQuery, wr: &mut OwnedWriteHalf) -> std::io::Result<()> {
    let (progress_tx, mut progress) = mpsc::unbounded_channel();
    let Profile { preferences, player, .. } = query.profile;
    let rank_for = || ctx.machine.profile(preferences, player);
    let search = ctx.search.search_stream(query, &rank_for, progress_tx);
    tokio::pin!(search);
    let result = loop {
        tokio::select! {
            biased;
            Some(Progress { mut result, pending }) = progress.recv() => {
                record(ctx, &mut result);
                wire::write_async(wr, &Event::SearchUpdate { result: Box::new(result), pending }).await?;
            }
            result = &mut search => break result,
        }
    };
    let response = match result {
        Ok(mut result) => {
            record(ctx, &mut result);
            Response::Search { result }
        }
        Err(error) => Response::Error { error },
    };
    wire::write_async(wr, &response).await
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
            Err(error) => super::log(&format!("{} falls back to its magnet: {}", release.id.to_ref(), error.message)),
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

async fn get(ctx: &Ctx, request: GetRequest) -> Result<torrential_core::protocol::GetReply, Error> {
    let GetRequest { target, dir, files, max_size, setup_max_size, dry_run } = request;
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

async fn play(ctx: &Ctx, request: PlayRequest) -> Result<PlayReply, Error> {
    let PlayRequest { target, dir, files, setup_max_size } = request;
    let source = resolve(ctx, target, "play").await?;
    let to = Destination::Folder(dir);
    let play = PlayCommand { source, to, files: files.clone(), setup_max_size, title: None, finish: Finish::Stream };
    let (job, metainfo) = ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await?;
    let streams = streams(ctx, job.id, &metainfo, files);
    Ok(PlayReply { job, streams })
}

/// Where `files` of the download `id` are served, in the order given.
fn streams(ctx: &Ctx, id: TorrentId, metainfo: &Metainfo, files: Vec<u32>) -> Vec<Stream> {
    files
        .into_iter()
        .map(|index| {
            let file = &metainfo.files[index as usize];
            let path = sanitize(&file.path);
            let url = http::url(ctx.http_port, id, index, &path);
            Stream { index, path, size: file.size, url }
        })
        .collect()
}

/// One title as the catalog describes it, with where the person stopped, what plays next, and
/// the downloads made by playing it. The socket's `title` and `GET /api/title` both answer with it.
pub(super) async fn title_page(ctx: &Ctx, id: TitleId) -> Result<TitlePage, Error> {
    let details = ctx.titles.catalog.title(&id).await?;
    let watched = ctx.titles.shelf.watched(&id)?;
    let jobs = ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
    Ok(TitlePage {
        next: next(&details, &watched, super::owner::now()),
        saved: ctx.titles.shelf.is_saved(&id)?,
        downloads: jobs.into_iter().filter(|job| job.title.as_ref().is_some_and(|link| link.title == id)).collect(),
        watched,
        details,
    })
}

/// For each title watched lately, its video to go on with, and the saved titles. A finished film,
/// and a series the person has caught up with, are left out. The catalog lookups share one
/// deadline, so an unreachable catalog cannot hold the lists up. Past it, each title's details
/// come from the copy kept on this machine, and a title with none is left out.
pub(super) async fn home(ctx: &Ctx) -> Result<Home, Error> {
    let now = super::owner::now();
    let deadline = tokio::time::Instant::now() + ctx.titles.timing.home;
    let catalog = &ctx.titles.catalog;
    let mut going_on = Vec::new();
    for recent in ctx.titles.shelf.recent(CONTINUE_MOST)? {
        let id = &recent.video.title;
        let details = match tokio::time::timeout_at(deadline, catalog.title(id)).await {
            Ok(looked_up) => looked_up.ok(),
            Err(_) => catalog.kept(id).await,
        };
        let Some(details) = details else { continue };
        let watched = ctx.titles.shelf.watched(&recent.video.title)?;
        let Some(Next { video, resume_at }) = next(&details, &watched, now) else { continue };
        let episode = video
            .episode
            .and_then(|e| details.seasons.iter().flat_map(|s| &s.episodes).find(|info| info.episode == e).cloned());
        let duration = watched.iter().find(|w| w.video == video).and_then(|w| w.duration);
        going_on.push(Continue { title: details.card, video, episode, resume_at, duration });
    }
    Ok(Home { going_on, saved: ctx.titles.shelf.saved()? })
}

/// The releases of the film or episode `video` names, in the order a play tries them.
async fn sources(ctx: &Ctx, video: VideoRef) -> Result<TitleSources, Error> {
    let title = ctx.titles.resolve(&video, super::owner::now()).await?;
    let mut result = search(ctx, ctx.titles.query(&title, (ctx.web.preferences)(), Player::External).await?).await?;
    let jobs = ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
    let ffmpeg = (ctx.web.tools)().both().is_some();
    result.releases = ctx.titles.line(&jobs, &title.video, result.releases, Player::External, ffmpeg);
    Ok(TitleSources { video: title.video, title: title.details.card, episode: title.episode, result })
}

/// The release among `video`'s sources whose ID starts with `prefix`.
async fn pinned(ctx: &Ctx, video: &VideoRef, prefix: &str) -> Result<ReleaseId, Error> {
    let parsed: HexPrefix = prefix.parse()?;
    let sources = sources(ctx, video.clone()).await?;
    let name = title_name(&sources.title, sources.video.episode);
    let matching: Vec<&Release> = sources.result.releases.iter().filter(|r| parsed.matches(&r.id)).collect();
    match matching.as_slice() {
        [one] => Ok(one.id),
        [] => Err(Error::new(ErrorCode::NotFound, format!("no source of {name} has an ID that starts with {prefix}"))
            .with_hint(format!("run torrential sources {video} to list them"))
            .with_next(format!("torrential sources {video}"))),
        many => Err(Error::new(
            ErrorCode::AmbiguousRef,
            format!("{prefix} starts the IDs of {} sources of {name}", many.len()),
        )
        .with_hint("give more of the ID")),
    }
}

/// What a title play's steps said that its reply needs.
#[derive(Default)]
struct Seen {
    title: Option<TitleCard>,
    tried: Vec<Tried>,
    end: Option<AttemptEvent>,
}

impl Seen {
    fn note(&mut self, step: &AttemptEvent) {
        match step {
            AttemptEvent::Resolved { title, .. } => self.title = Some(title.clone()),
            AttemptEvent::Skipped { release, reason } => {
                self.tried.push(Tried { release: *release, reason: reason.clone() })
            }
            AttemptEvent::Playing { .. } | AttemptEvent::Failed { .. } => self.end = Some(step.clone()),
            AttemptEvent::Searched { .. } | AttemptEvent::Trying { .. } | AttemptEvent::Check { .. } => {}
        }
    }
}

/// Plays a film or an episode from the first of its sources that starts, sending each step to
/// `steps`. A stream answers where the video and its subtitles are served, as `play` answers, and
/// a download kept answers as `get` answers.
async fn play_title(
    ctx: &Ctx,
    request: TitleRequest,
    finish: Finish,
    steps: mpsc::UnboundedSender<AttemptEvent>,
) -> Result<Response, Error> {
    let TitleRequest { video, source } = request;
    let pin = match source {
        Some(prefix) => Some(pinned(ctx, &video, &prefix).await?),
        None => None,
    };
    let jobs = ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
    let before: BTreeSet<TorrentId> = jobs.iter().map(|job| job.id).collect();
    let mut seen = Seen::default();
    let ask = PlayTitle { video, pin, replace: None, player: Player::External, containers: Containers::Any, finish };
    attempt::run(ctx, ask, |step| {
        seen.note(&step);
        let _ = steps.send(step);
    })
    .await;
    let (video, release, id, file, resume_at) = match seen.end {
        Some(AttemptEvent::Playing { video, release, job, file, resume_at }) => (video, release, job, file, resume_at),
        Some(AttemptEvent::Failed { error, tried }) if tried.is_empty() => return Err(error),
        Some(AttemptEvent::Failed { error, tried }) => return Err(error.with_details(Details::Tried(tried))),
        _ => return Err(Error::internal("the title play ended without playing or failing")),
    };
    let card = seen.title.ok_or_else(|| Error::internal("the title play played before it found the title"))?;
    let name = title_name(&card, video.episode);
    let title = Box::new(TitlePlayed { video, name, release, tried: seen.tried, resume_at });
    let mut jobs = ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await?;
    let job = jobs.pop().ok_or_else(|| Error::new(ErrorCode::NotFound, "the download that played is gone"))?;
    match finish {
        Finish::Stream => {
            let info = metadata(ctx, MetaTarget::Job(id), Instant::now() + METADATA_WAIT).await?;
            let playable = torrential_core::stream::around(&listed(ctx, &info).await, file)
                .ok_or_else(|| Error::internal(format!("the download has no file {file}")))?;
            let files = [playable.video].into_iter().chain(playable.subtitles).collect();
            let streams = streams(ctx, id, &info.metainfo, files);
            Ok(Response::PlayTitle { reply: Box::new(PlayReply { job, streams }), title })
        }
        Finish::Keep => {
            let outcome = if before.contains(&id) { GetOutcome::Existing } else { GetOutcome::Started };
            let reply = GetReply { outcome, job, plan: None, previous_selection: None };
            Ok(Response::GetTitle { reply: Box::new(reply), title })
        }
    }
}

/// A title play over the socket: a frame for each step, then the reply. The play stops, and the
/// source under test is given up, if the client goes away first.
async fn title_frames(
    ctx: &Ctx,
    request: TitleRequest,
    finish: Finish,
    rd: &mut OwnedReadHalf,
    wr: &mut OwnedWriteHalf,
) -> std::io::Result<()> {
    let (steps_tx, mut steps) = mpsc::unbounded_channel();
    let play = play_title(ctx, request, finish, steps_tx);
    tokio::pin!(play);
    let mut probe = [0u8; 1];
    let reply = loop {
        tokio::select! {
            biased;
            Some(step) = steps.recv() => wire::write_async(wr, &TitleFrame::Step { step: Box::new(step) }).await?,
            reply = &mut play => break reply,
            read = tokio::io::AsyncReadExt::read(rd, &mut probe) => match read {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => {}
            },
        }
    };
    while let Ok(step) = steps.try_recv() {
        wire::write_async(wr, &TitleFrame::Step { step: Box::new(step) }).await?;
    }
    wire::write_async(wr, &reply.unwrap_or_else(|error| Response::Error { error })).await
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
            source: origin.as_str(),
            download: None,
        },
    };
    let m = &info.metainfo;
    Ok(FilesReply {
        id: info.id.to_hex(),
        name: Some(sanitize(&m.name)),
        total_size: m.total(),
        files: listed(ctx, &info).await,
        metadata_source: info.source.to_string(),
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

pub async fn connection(ctx: Arc<Ctx>, stream: UnixStream) {
    ctx.owner.tell(Command::Connected);
    let _ = serve(&ctx, stream).await;
    ctx.owner.tell(Command::Disconnected { keep_until: ctx.search.answers_expire() });
}

async fn serve(ctx: &Ctx, stream: UnixStream) -> std::io::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let Some(Request::Hello { client_version, wire: client_wire }) = wire::read_async::<Request>(&mut rd).await? else {
        return Ok(());
    };
    let hello = Response::Hello {
        engine_version: ctx.version.clone(),
        wire: WIRE_VERSION,
        pid: std::process::id(),
        http_port: ctx.http_port,
    };
    wire::write_async(&mut wr, &hello).await?;
    while let Some(request) = wire::read_async::<Request>(&mut rd).await? {
        if request == (Request::Shutdown { asked: false }) {
            wire::write_async(&mut wr, &switch(ctx, &client_version).await).await?;
            continue;
        }
        if client_wire != WIRE_VERSION && !matches!(request, Request::Shutdown { .. }) {
            let error = Error::new(
                ErrorCode::DaemonVersion,
                format!("the engine uses protocol version {WIRE_VERSION}, but this client uses {client_wire}"),
            )
            .with_hint("run torrential engine stop, then run the command again")
            .with_next("torrential engine stop");
            wire::write_async(&mut wr, &Response::Error { error }).await?;
            continue;
        }
        let _playing = match request {
            Request::Play(_) | Request::PlayTitle(_) => match ctx.playing.open() {
                Ok(open) => Some(open),
                Err(error) => {
                    wire::write_async(&mut wr, &Response::Error { error }).await?;
                    continue;
                }
            },
            _ => None,
        };
        match request {
            Request::Watch { refs } => return watch(ctx, refs, rd, wr).await,
            Request::Search { query, stream: true } => stream_search(ctx, query, &mut wr).await?,
            Request::PlayTitle(request) => title_frames(ctx, request, Finish::Stream, &mut rd, &mut wr).await?,
            Request::GetTitle(request) => title_frames(ctx, request, Finish::Keep, &mut rd, &mut wr).await?,
            request => {
                let response = handle(ctx, request).await;
                wire::write_async(&mut wr, &response).await?;
            }
        }
    }
    Ok(())
}

/// Answers a stop that a client sends without the person asking, to run its own version instead. An
/// engine newer than the client keeps running, since torrential 0.4 sends this stop to an engine of
/// any other version. Any other engine stops only when nothing plays and every download is saved.
async fn switch(ctx: &Ctx, client: &str) -> Response {
    if older(client, &ctx.version) {
        super::log(&format!("torrential {client} asked the engine to stop for its own version"));
        let error = Error::new(
            ErrorCode::DaemonVersion,
            format!("the engine runs the newer torrential {}, so it keeps running", ctx.version),
        )
        .with_hint("restart this torrential");
        return Response::Error { error };
    }
    let Some(closed) = ctx.playing.try_close() else {
        let error = Error::new(ErrorCode::DaemonVersion, format!("torrential switches to {client} when the film ends"));
        return Response::Error { error };
    };
    match ctx.owner.ask(|reply| Command::Restart { reply }).await {
        Ok(()) => {
            closed.keep();
            Response::Ok
        }
        Err(error) => Response::Error { error },
    }
}

fn older(client: &str, engine: &str) -> bool {
    Version::parse(engine).is_some_and(|engine| engine.newer_than(client))
}

async fn watch(ctx: &Ctx, refs: Vec<String>, mut rd: OwnedReadHalf, mut wr: OwnedWriteHalf) -> std::io::Result<()> {
    let everything = refs.is_empty();
    let (jobs, mut updates) = match ctx.owner.ask(|reply| Command::Watch { refs, reply }).await {
        Ok(watch) => watch,
        Err(error) => return wire::write_async(&mut wr, &Response::Error { error }).await,
    };
    let watched: Vec<_> = jobs.iter().map(|j| j.id).collect();
    wire::write_async(&mut wr, &Event::Snapshot { jobs }).await?;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    let mut probe = [0u8; 1];
    loop {
        let event = tokio::select! {
            update = updates.recv() => match update {
                Ok(Update::Job(job)) if everything || watched.contains(&job.id) => Event::Job { job },
                Ok(Update::Removed(id)) if everything || watched.contains(&id) => Event::Removed { id: id.to_hex() },
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let refs = if everything { Vec::new() } else { watched.iter().map(|id| id.to_hex()).collect() };
                    match ctx.owner.ask(|reply| Command::Status { refs, reply }).await {
                        Ok(jobs) => Event::Snapshot { jobs },
                        Err(_) => return Ok(()),
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = heartbeat.tick() => Event::Heartbeat { at: super::owner::now() },
            read = tokio::io::AsyncReadExt::read(&mut rd, &mut probe) => match read {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => continue,
            },
        };
        heartbeat.reset();
        wire::write_async(&mut wr, &event).await?;
    }
}
