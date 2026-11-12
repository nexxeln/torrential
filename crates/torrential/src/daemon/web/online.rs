//! Subtitles from online sources for a video the page is playing.

use std::io::SeekFrom;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use torrential_core::{Error, ErrorCode, TorrentId, subtitle, title};
use torrential_search::subtitles::{FileHash, Found, Video};

use super::super::http::Request;
use super::super::owner::{Command, StreamFile};
use super::super::server::Ctx;
use super::{file_of, required, watch};

/// `GET /api/subtitles/online`: what the engine found for the file, by the names in its torrent.
pub async fn list(ctx: &Ctx, id: TorrentId, index: u32) -> Result<Value, Error> {
    let found = found(ctx, id, index).await?;
    let title = found.title.as_ref().map(|title| {
        json!({
            "name": title.name,
            "year": title.year,
            "season": title.episode.map(|e| e.season),
            "episode": title.episode.map(|e| e.episode),
        })
    });
    let subtitles: Vec<Value> = found
        .subtitles
        .iter()
        .map(|s| json!({ "id": s.id, "language": s.language, "name": s.name, "exact": s.exact, "source": s.source }))
        .collect();
    Ok(json!({ "title": title, "subtitles": subtitles }))
}

/// `GET /api/subtitles/online/fetch`: one of those subtitles as WebVTT. The page names it by the
/// ID `list` gave, and the engine downloads it from the address the source gave with it.
pub async fn vtt(ctx: &Ctx, request: &Request) -> Result<Arc<Vec<u8>>, Error> {
    let (id, index) = file_of(request)?;
    let wanted = required(request, "subtitle")?;
    let key = (id, index, wanted.clone());
    let kept = ctx.web.converted.lock().expect("no holder panics").get(&key);
    if let Some(vtt) = kept {
        return Ok(vtt);
    }
    let found = found(ctx, id, index).await?;
    let subtitle = found.subtitles.iter().find(|s| s.id == wanted).ok_or_else(|| {
        Error::new(ErrorCode::NotFound, format!("no subtitle {wanted} was found for this file"))
            .with_hint("ask for the file's online subtitles again, and pick one of those")
    })?;
    let bytes = ctx.web.subtitles.fetch(subtitle).await?;
    let vtt = subtitle::to_vtt(&subtitle::decode(&bytes)).ok_or_else(|| {
        Error::new(ErrorCode::FetchFailed, format!("{} did not send a subtitle with timed lines", subtitle.source))
            .with_hint("pick another subtitle")
    })?;
    let vtt = Arc::new(vtt.into_bytes());
    ctx.web.converted.lock().expect("no holder panics").put(key, vtt.clone());
    Ok(vtt)
}

/// What the sources have for the file. An answer is kept for as long as the engine runs. No
/// answer, as when the sources are down, is not kept, so the next request asks again.
async fn found(ctx: &Ctx, id: TorrentId, index: u32) -> Result<Arc<Found>, Error> {
    let kept = ctx.web.found.lock().expect("no holder panics").get(&(id, index)).cloned();
    if let Some(found) = kept {
        return Ok(found);
    }
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let video = Video {
        titles: title::read(&file.inside, &file.torrent),
        file_name: watch::name(&file),
        hash: hash(ctx, id, index, &file).await,
    };
    let Some(found) = ctx.web.subtitles.find(&video).await else { return Ok(Arc::new(Found::default())) };
    let found = Arc::new(found);
    ctx.web.found.lock().expect("no holder panics").insert((id, index), found.clone());
    Ok(found)
}

/// The file's hash, if both of its ends are on disk already. Subtitles are not worth fetching
/// pieces out of order for, and a search without the hash still finds them by title.
async fn hash(ctx: &Ctx, id: TorrentId, index: u32, file: &StreamFile) -> Option<FileHash> {
    let reach = FileHash::reach(file.size);
    let ends = [0, file.size - reach];
    if reach == 0 {
        return None;
    }
    if !file.complete {
        for offset in ends {
            let written = ctx.engine.written(id, index, offset, reach, 1).await.ok()?;
            if !written.iter().all(|run| run.is_complete()) {
                return None;
            }
        }
    }
    let mut disk = tokio::fs::File::open(&file.path).await.ok()?;
    let mut read = [vec![0; reach as usize], vec![0; reach as usize]];
    for (offset, bytes) in ends.into_iter().zip(&mut read) {
        disk.seek(SeekFrom::Start(offset)).await.ok()?;
        disk.read_exact(bytes).await.ok()?;
    }
    Some(FileHash::of(file.size, &read[0], &read[1]))
}
