//! Small pictures of a video for the player's seek bar.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use torrential_core::{Error, ErrorCode, TorrentId};

use super::super::ctx::Ctx;
use super::super::owner::{Command, StreamFile};
use super::{ffmpeg, name, probed};

/// How many ffmpeg processes make thumbnails at once. A seek bar asks for many in a row.
pub const AT_ONCE: usize = 2;
pub const KEPT: usize = 500;
const QUEUE_FOR: Duration = Duration::from_secs(3);
const DECODE_WITHIN: Duration = Duration::from_secs(10);
const MIB: u64 = 1024 * 1024;
/// A film's bitrate varies, so the bytes of a second can be this far from where an even bitrate
/// would put them: 8 MiB, or 2% of a large file.
pub const MARGIN: u64 = 8 * MIB;
pub const MARGIN_SHARE: u64 = 50;
/// Seconds of video one thumbnail stands for. The seek bar asks for one at the start of each.
pub const EVERY: u32 = 10;

/// A JPEG of the frame at `t` seconds of file `index` of a download. Only from bytes already on
/// disk: a picture is never worth fetching pieces ahead of the ones being watched.
pub async fn jpeg(ctx: &Ctx, id: TorrentId, index: u32, t: f64) -> Result<Arc<Vec<u8>>, Error> {
    let program = ctx.media.ffmpeg.now()?.ffmpeg;
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let name = name(&file);
    let none = |why: &str| Error::new(ErrorCode::NotFound, format!("no picture of {name} at {t} s: {why}"));
    // The player reads the file's tracks before it draws a seek bar. Reading them here could fetch pieces.
    let media = probed(ctx, id, index).ok_or_else(|| none("its tracks have not been read yet"))?;
    let duration = media.duration.filter(|d| *d > 0.0).ok_or_else(|| none("its length is not known"))?;
    if media.video.is_none() || t >= duration {
        return Err(none("the video has no frame there"));
    }
    let key = (id, index, t as u32 / EVERY * EVERY);
    let kept = ctx.media.thumbnails.lock().expect("no holder panics").get(&key);
    if let Some(jpeg) = kept {
        return Ok(jpeg);
    }
    if !file.complete && !on_disk(ctx, id, index, &file, t / duration).await {
        // A download that finishes leaves the engine a moment before it is marked done.
        let now = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
        if !now.complete {
            return Err(none("that part has not downloaded yet"));
        }
    }
    let busy = || {
        Error::new(ErrorCode::DaemonUnavailable, "the engine is busy making other thumbnails")
            .with_hint("ask again in a moment")
    };
    let turn = tokio::time::timeout(QUEUE_FOR, ctx.media.thumbnailing.acquire()).await;
    let _turn = turn.map_err(|_| busy())?.map_err(|_| busy())?;
    let mut command = tokio::process::Command::new(&program);
    command.args(ffmpeg::thumbnail(&file.path, t)).stdin(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(DECODE_WITHIN, command.output()).await;
    let output = output.map_err(|_| none("ffmpeg took too long"))?;
    let output = output.map_err(|e| Error::internal(format!("cannot run {}: {e}", program.display())))?;
    // ffmpeg can draw a frame from damaged bytes and only complain. Such a picture is not kept.
    if !output.status.success() || output.stdout.is_empty() || !output.stderr.is_empty() {
        return Err(none("ffmpeg could not read a frame there"));
    }
    let jpeg = Arc::new(output.stdout);
    ctx.media.thumbnails.lock().expect("no holder panics").put(key, jpeg.clone());
    Ok(jpeg)
}

/// Whether the stretch of the file around the frame `share` of the way into it has all arrived.
/// The container's index, at one end of the file, is not checked: the player's own reading has
/// fetched what it needs of it, and a frame read from bytes that are missing fails in ffmpeg.
async fn on_disk(ctx: &Ctx, id: TorrentId, index: u32, file: &StreamFile, share: f64) -> bool {
    if file.size == 0 {
        return false;
    }
    let margin = MARGIN.max(file.size / MARGIN_SHARE);
    let at = ((file.size as f64 * share) as u64).min(file.size - 1);
    let (from, to) = (at.saturating_sub(margin), (at + margin).min(file.size));
    match ctx.engine.ask(move |e| e.written(id, index, from, to - from, 1)).await {
        Ok(written) => written.iter().all(|run| run.is_complete()),
        Err(_) => false,
    }
}
