//! `POST /api/play`: starts or joins a download and fetches one video first, in playing order.

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use torrential_core::stream::{self, Playable};
use torrential_core::text::sanitize;
use torrential_core::{Error, ErrorCode, Job, Target, TorrentFile, TorrentId};

use super::super::engine_port::Metainfo;
use super::super::http;
use super::super::owner::{Command, FilesInfo, Finish, MetaTarget, Origin, PlayCommand, Source};
use super::super::server::{self, Ctx, METADATA_WAIT};

#[derive(Deserialize)]
pub struct Play {
    /// A download or search result ID, an infohash, a magnet link, or the URL of a .torrent file.
    #[serde(rename = "ref")]
    reference: String,
    /// The index of the file to watch. Without it, the engine picks the main video.
    file: Option<u32>,
}

#[derive(Serialize)]
pub struct Playing {
    id: String,
    name: String,
    job: Job,
    files: Vec<TorrentFile>,
    total_size: u64,
    video: Streamed,
    subtitles: Vec<Streamed>,
}

/// A file the engine serves while it downloads.
#[derive(Serialize)]
struct Streamed {
    index: u32,
    path: String,
    size: u64,
    /// The path of `http::url`, for a page on the engine's own origin.
    url: String,
    /// Where the file starts in the torrent, in bytes.
    offset: u64,
}

pub async fn play(ctx: &Ctx, request: Play) -> Result<Playing, Error> {
    let deadline = Instant::now() + METADATA_WAIT;
    let (source, _, playable) = prepare(ctx, super::target(&request.reference)?, request.file, deadline).await?;
    let (job, info) = begin(ctx, source, &playable, deadline).await?;
    Ok(playing(ctx, job, &info, &playable).await)
}

/// The torrent `target` names, its files, and the video to watch in it with its subtitles: file
/// `file`, or the main video.
pub async fn prepare(
    ctx: &Ctx,
    target: Target,
    file: Option<u32>,
    deadline: Instant,
) -> Result<(Source, Arc<Metainfo>, Playable), Error> {
    let (source, metainfo) = match server::resolve(ctx, target, "play").await? {
        Source::Download(id) => {
            let info = server::metadata(ctx, MetaTarget::Job(id), deadline).await?;
            (Source::Download(id), info.metainfo)
        }
        Source::Magnet(magnet) => {
            let info = server::metadata(ctx, MetaTarget::Magnet(magnet), deadline).await?;
            let source = match info.download {
                Some(_) => Source::Download(info.id),
                None => Source::Torrent { metainfo: (*info.metainfo).clone(), origin: Origin::Peers },
            };
            (source, info.metainfo)
        }
        Source::Torrent { metainfo, origin } => {
            let shared = Arc::new(metainfo.clone());
            (Source::Torrent { metainfo, origin }, shared)
        }
    };
    let playable = choose(&metainfo, file)?;
    Ok((source, metainfo, playable))
}

/// Starts or joins the download of `playable`'s video and subtitles, which come first and in
/// playing order.
pub async fn begin(
    ctx: &Ctx,
    source: Source,
    playable: &Playable,
    deadline: Instant,
) -> Result<(Job, FilesInfo), Error> {
    let files = [playable.video].into_iter().chain(playable.subtitles.iter().copied()).collect();
    let setup_max_size = (ctx.web.preferences)().max_size;
    let play = PlayCommand { source, dir: None, files, setup_max_size, title: None, finish: Finish::Stream };
    let (job, _) = ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await?;
    let info = server::metadata(ctx, MetaTarget::Job(job.id), deadline).await?;
    Ok((job, info))
}

/// How the web player plays `file` of a download that is already playing, as a title play
/// answers once a source starts.
pub async fn playing_file(ctx: &Ctx, id: TorrentId, file: u32) -> Result<Playing, Error> {
    let info = server::metadata(ctx, MetaTarget::Job(id), Instant::now() + METADATA_WAIT).await?;
    let playable = choose(&info.metainfo, Some(file))?;
    let mut jobs = ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await?;
    let job = jobs.pop().ok_or_else(|| Error::new(ErrorCode::NotFound, "the download that played is gone"))?;
    Ok(playing(ctx, job, &info, &playable).await)
}

/// Where a download's video and its subtitles are served, and its files. `POST /api/play` and a
/// title play's `playing` line both answer with it.
async fn playing(ctx: &Ctx, job: Job, info: &FilesInfo, playable: &Playable) -> Playing {
    let metainfo = &info.metainfo;
    let streamed = |index: u32| {
        let file = &metainfo.files[index as usize];
        let path = sanitize(&file.path);
        Streamed {
            index,
            url: http::route_of(job.id, index, &path),
            path,
            size: file.size,
            offset: metainfo.files[..index as usize].iter().map(|f| f.size).sum(),
        }
    };
    Playing {
        id: job.id.to_hex(),
        name: sanitize(&metainfo.name),
        files: server::listed(ctx, info).await,
        total_size: metainfo.total(),
        video: streamed(playable.video),
        subtitles: playable.subtitles.iter().map(|&index| streamed(index)).collect(),
        job,
    }
}

/// The file to watch and its subtitles: the one the page chose, or the torrent's main video.
pub fn choose(metainfo: &Metainfo, file: Option<u32>) -> Result<Playable, Error> {
    let listing: Vec<TorrentFile> = (0..)
        .zip(&metainfo.files)
        .map(|(index, f)| TorrentFile {
            index,
            path: sanitize(&f.path),
            size: f.size,
            selected: false,
            verified: 0,
            risk: None,
        })
        .collect();
    let name = sanitize(&metainfo.name);
    match file {
        Some(index) => stream::around(&listing, index).ok_or_else(|| {
            let count = listing.len();
            let files = if count == 1 { "1 file".to_string() } else { format!("{count} files") };
            Error::new(ErrorCode::Usage, format!("{name} has {files}, so it has no file {index}"))
        }),
        None => stream::pick(&listing).ok_or_else(|| {
            let id = metainfo.info_hash.id().to_hex();
            Error::new(ErrorCode::NoSelection, format!("{name} has no video file"))
                .with_hint(format!("run torrential files {} to list its files", &id[..8]))
        }),
    }
}
