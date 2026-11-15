//! Subtitles from online sources for a video the page is playing.

use std::io::SeekFrom;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use torrential_core::{Error, ErrorCode, TorrentId, subtitle, title};
use torrential_search::subtitles::{FileHash, Found, Video};

use super::super::ctx::Ctx;
use super::super::http::Request;
use super::super::media;
use super::super::owner::{Command, StreamFile};
use super::{file_of, required};

/// `GET /api/subtitles/online`: what the engine found for the file, by the names in its torrent
/// and, once both of its ends have downloaded, by its hash.
pub async fn list(ctx: &Ctx, id: TorrentId, index: u32) -> Result<Value, Error> {
    let searched = found(ctx, id, index).await?;
    let found = &searched.found;
    let title = found.title.as_ref().map(|title| Named {
        name: &title.name,
        year: title.year,
        season: title.episode.map(|e| e.season),
        episode: title.episode.map(|e| e.episode),
    });
    let subtitles = found
        .subtitles
        .iter()
        .map(|s| Listed { id: &s.id, language: &s.language, name: s.name.as_deref(), exact: s.exact, source: s.source })
        .collect();
    super::value(Online { title, subtitles, hashed: searched.hashed })
}

/// What `GET /api/subtitles/online` answers.
#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OnlineSubtitles"))]
pub(super) struct Online<'a> {
    /// What the names in the torrent say the video is.
    title: Option<Named<'a>>,
    subtitles: Vec<Listed<'a>>,
    /// The search had the file's hash. Until it does, asking again once both ends of the file have
    /// downloaded can find subtitles timed for this exact file.
    hashed: bool,
}

/// What the sources had for a file, and whether the search knew its hash.
#[derive(Default)]
pub(super) struct Searched {
    found: Found,
    hashed: bool,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OnlineTitle"))]
pub(super) struct Named<'a> {
    name: &'a str,
    year: Option<u16>,
    season: Option<u32>,
    episode: Option<u32>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OnlineSubtitle"))]
pub(super) struct Listed<'a> {
    id: &'a str,
    language: &'a str,
    name: Option<&'a str>,
    exact: bool,
    source: &'a str,
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
    let searched = found(ctx, id, index).await?;
    let subtitle = searched.found.subtitles.iter().find(|s| s.id == wanted).ok_or_else(|| {
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

/// What the sources have for the file. An answer is kept for as long as the engine runs, and
/// replaced once by a search with the file's hash when both of its ends have downloaded. No
/// answer, as when the sources are down, is not kept, so the next request asks again.
async fn found(ctx: &Ctx, id: TorrentId, index: u32) -> Result<Arc<Searched>, Error> {
    let kept = ctx.web.found.lock().expect("no holder panics").get(&(id, index));
    if let Some(kept) = kept.as_ref().filter(|kept| kept.hashed) {
        return Ok(kept.clone());
    }
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let hash = hash(ctx, id, index, &file).await;
    if let Some(kept) = kept.as_ref().filter(|_| hash.is_none()) {
        return Ok(kept.clone());
    }
    let video = Video { titles: title::read(&file.inside, &file.torrent), file_name: media::name(&file), hash };
    let Some(found) = ctx.web.subtitles.find(&video).await else { return Ok(kept.unwrap_or_default()) };
    let searched = Arc::new(Searched { found, hashed: hash.is_some() });
    ctx.web.found.lock().expect("no holder panics").put((id, index), searched.clone());
    Ok(searched)
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
            let written = ctx.engine.ask(move |e| e.written(id, index, offset, reach, 1)).await.ok()?;
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
