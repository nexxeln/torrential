use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, oneshot};
use torrential_core::protocol::{Event, FilesReply, GetRequest, Request, Response, WIRE_VERSION};
use torrential_core::text::sanitize;
use torrential_core::{
    Error, ErrorCode, HexPrefix, Release, ReleaseId, SearchQuery, SearchResult, Target, TorrentFile,
};

use super::engine_port::{EngineHandle, Metainfo, ParseTorrent};
use super::files;
use super::magnet::{self, Magnet};
use super::owner::{Command, FileBytes, FilesInfo, GetCommand, MetaTarget, Origin, Reply, Resolved, Source, Update};
use super::search_port::{Progress, SearchPort};
use crate::machine::Detector;
use crate::wire;

const MAX_TORRENT_BYTES: u64 = 16 * 1024 * 1024;
pub const METADATA_WAIT: Duration = Duration::from_secs(60);
const HEARTBEAT: Duration = Duration::from_secs(15);

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
        Request::Status { refs } => {
            owner.ask(|reply| Command::Status { refs, reply }).await.map(|jobs| Response::Jobs { jobs })
        }
        Request::Pause { refs } => {
            owner.ask(|reply| Command::Pause { refs, reply }).await.map(|changes| Response::Changed { changes })
        }
        Request::Resume { refs, all } => {
            owner.ask(|reply| Command::Resume { refs, all, reply }).await.map(|changes| Response::Changed { changes })
        }
        Request::Remove { refs } => {
            owner.ask(|reply| Command::Remove { refs, reply }).await.map(|changes| Response::Changed { changes })
        }
        Request::Verify { reference, repair } => owner
            .ask(|reply| Command::Verify { reference, repair, reply })
            .await
            .map(|changes| Response::Changed { changes }),
        Request::Shutdown => {
            let (reply, done) = oneshot::channel();
            owner.tell(Command::Shutdown { reply });
            let _ = done.await;
            Ok(Response::Ok)
        }
    };
    result.unwrap_or_else(|error| Response::Error { error })
}

async fn search(ctx: &Ctx, query: SearchQuery) -> Result<SearchResult, Error> {
    let preferences = query.profile.preferences;
    let mut result = ctx.search.search(query, &|| ctx.machine.profile(preferences)).await?;
    record(ctx, &mut result);
    Ok(result)
}

fn record(ctx: &Ctx, result: &mut SearchResult) {
    for release in &mut result.releases {
        release.name = sanitize(&release.name);
    }
    let saved = result.releases.iter().map(|r| (r.clone(), ctx.search.saved_link(&r.id))).collect();
    ctx.owner.tell(Command::RecordResults(saved));
}

async fn stream_search(ctx: &Ctx, query: SearchQuery, wr: &mut OwnedWriteHalf) -> std::io::Result<()> {
    let (progress_tx, mut progress) = mpsc::unbounded_channel();
    let preferences = query.profile.preferences;
    let rank_for = || ctx.machine.profile(preferences);
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

async fn resolve(ctx: &Ctx, target: Target, command: &'static str) -> Result<Source, Error> {
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

async fn release_source(ctx: &Ctx, release: Release, cached: Option<Arc<Metainfo>>) -> Result<Source, Error> {
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

async fn metadata(ctx: &Ctx, target: MetaTarget, deadline: Instant) -> Result<FilesInfo, Error> {
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

async fn files(ctx: &Ctx, target: Target, timeout: Duration) -> Result<FilesReply, Error> {
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
    let verified: Vec<u64> = match info.download.as_ref().map(|d| &d.verified) {
        Some(FileBytes::Known(bytes)) => bytes.clone(),
        Some(FileBytes::Engine { saved }) => ctx.engine.file_progress(info.id).await.unwrap_or_else(|_| saved.clone()),
        None => Vec::new(),
    };
    let risks = files::risks(m);
    let files = m
        .files
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
        .collect();
    Ok(FilesReply {
        id: info.id.to_hex(),
        name: Some(sanitize(&m.name)),
        total_size: m.total(),
        files,
        metadata_source: info.source.to_string(),
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
    })
}

pub async fn connection(ctx: Arc<Ctx>, stream: UnixStream) {
    ctx.owner.tell(Command::Connected);
    let _ = serve(&ctx, stream).await;
    ctx.owner.tell(Command::Disconnected { keep_until: ctx.search.answers_expire() });
}

async fn serve(ctx: &Ctx, stream: UnixStream) -> std::io::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let Some(Request::Hello { wire: client_wire, .. }) = wire::read_async::<Request>(&mut rd).await? else {
        return Ok(());
    };
    let hello = Response::Hello { engine_version: ctx.version.clone(), wire: WIRE_VERSION, pid: std::process::id() };
    wire::write_async(&mut wr, &hello).await?;
    while let Some(request) = wire::read_async::<Request>(&mut rd).await? {
        if client_wire != WIRE_VERSION && request != Request::Shutdown {
            let error = Error::new(
                ErrorCode::DaemonVersion,
                format!("the engine speaks wire {WIRE_VERSION}, the client {client_wire}"),
            )
            .with_hint("torrential engine stop, then run the command again")
            .with_next("torrential engine stop");
            wire::write_async(&mut wr, &Response::Error { error }).await?;
            continue;
        }
        match request {
            Request::Watch { refs } => return watch(ctx, refs, rd, wr).await,
            Request::Search { query, stream: true } => stream_search(ctx, query, &mut wr).await?,
            request => {
                let response = handle(ctx, request).await;
                wire::write_async(&mut wr, &response).await?;
            }
        }
    }
    Ok(())
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
