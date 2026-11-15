//! `POST /api/play`: starts or joins a download and fetches one video first, in playing order.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use torrential_core::stream::Playable;
use torrential_core::text::sanitize;
use torrential_core::{Error, ErrorCode, Job, TorrentFile, TorrentId};

use super::super::ctx::Ctx;
use super::super::downloads::{self, Served, served};
use super::super::owner::{Command, FilesInfo, MetaTarget};
use torrential_core::protocol::METADATA_WAIT;

#[derive(Deserialize)]
pub struct Play {
    /// A download or search result ID, an infohash, a magnet link, or the URL of a .torrent file.
    #[serde(rename = "ref")]
    reference: String,
    /// The index of the file to watch. Without it, the engine picks the main video.
    file: Option<u32>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "PlayInfo"))]
pub struct Playing {
    id: String,
    name: String,
    job: Job,
    files: Vec<TorrentFile>,
    total_size: u64,
    video: Served,
    subtitles: Vec<Served>,
}

pub async fn play(ctx: &Ctx, request: Play) -> Result<Playing, Error> {
    let deadline = Instant::now() + METADATA_WAIT;
    let (source, _, playable) =
        downloads::prepare(ctx, super::target(&request.reference)?, request.file, deadline).await?;
    let (job, info) = downloads::begin(ctx, source, &playable, deadline).await?;
    Ok(playing(ctx, job, &info, &playable).await)
}

/// How the web player plays `file` of a download that is already playing, as a title play
/// answers once a source starts.
pub async fn playing_file(ctx: &Ctx, id: TorrentId, file: u32) -> Result<Playing, Error> {
    let info = downloads::metadata(ctx, MetaTarget::Job(id), Instant::now() + METADATA_WAIT).await?;
    let playable = downloads::choose(&info.metainfo, Some(file))?;
    let mut jobs = ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await?;
    let job = jobs.pop().ok_or_else(|| Error::new(ErrorCode::NotFound, "the download that played is gone"))?;
    Ok(playing(ctx, job, &info, &playable).await)
}

/// Where a download's video and its subtitles are served, and its files. `POST /api/play` and a
/// title play's `playing` line both answer with it.
async fn playing(ctx: &Ctx, job: Job, info: &FilesInfo, playable: &Playable) -> Playing {
    let metainfo = &info.metainfo;
    let streamed = |index: u32| served(job.id, metainfo, index);
    Playing {
        id: job.id.to_hex(),
        name: sanitize(&metainfo.name),
        files: downloads::listed(ctx, info).await,
        total_size: metainfo.total(),
        video: streamed(playable.video),
        subtitles: playable.subtitles.iter().map(|&index| streamed(index)).collect(),
        job,
    }
}
