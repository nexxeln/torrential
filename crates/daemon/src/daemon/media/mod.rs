//! What is inside a video and what ffmpeg makes of it: its tracks, the video and subtitles
//! converted for a browser, and thumbnails for the seek bar. The web routes and casts reach it
//! through `ctx.media`.

pub mod ffmpeg;
pub mod thumbnail;
pub mod tracks;

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use torrential_core::{Error, ErrorCode, TorrentId};

use super::ctx::Ctx;
use super::http;
use super::owner::{Command, StreamFile};
use crate::ffmpeg::fetch::{Ffmpeg, Found};
use ffmpeg::{Encoder, Picture};
pub use tracks::Media;

/// Files whose tracks are kept. Each is about a kilobyte.
const PROBES_KEPT: usize = 256;
/// ffmpeg prints its version at once. One that has not in this long converts unpaced this time
/// and is asked again the next.
const VERSION_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);
/// A subtitle file is text. Anything larger is something else with a subtitle's name.
const LARGEST_SUBTITLE_FILE: u64 = 16 * 1024 * 1024;

/// A whole file the engine made and keeps: a converted subtitle, or a thumbnail.
pub type Made = Arc<Vec<u8>>;

/// A program to run, its arguments, and what to write to its stdin.
pub type Conversion = (PathBuf, Vec<String>, Option<Vec<u8>>);

/// ffmpeg, and what the engine remembers of the videos it has read.
pub struct State {
    /// ffmpeg and ffprobe, installed or fetched.
    pub ffmpeg: Ffmpeg,
    probes: Mutex<Kept<(TorrentId, u32), Arc<Media>>>,
    /// Thumbnails, by file and the start of the seconds each stands for.
    thumbnails: Mutex<Kept<(TorrentId, u32, u32), Made>>,
    thumbnailing: tokio::sync::Semaphore,
    /// Whether each ffmpeg run here can pace its reading.
    paces: Mutex<HashMap<PathBuf, bool>>,
}

impl State {
    pub fn new(ffmpeg: Ffmpeg) -> Self {
        Self {
            ffmpeg,
            probes: Mutex::new(Kept::new(PROBES_KEPT)),
            thumbnails: Mutex::new(Kept::new(thumbnail::KEPT)),
            thumbnailing: tokio::sync::Semaphore::new(thumbnail::AT_ONCE),
            paces: Mutex::default(),
        }
    }

    /// Whether the ffmpeg at `program` paces its reading. Each program is asked until it answers.
    async fn paces(&self, program: &Path) -> bool {
        if let Some(known) = self.paces.lock().expect("no holder panics").get(program) {
            return *known;
        }
        let version =
            tokio::process::Command::new(program).arg("-version").stdin(Stdio::null()).kill_on_drop(true).output();
        let paces = match tokio::time::timeout(VERSION_WITHIN, version).await {
            Ok(Ok(output)) => ffmpeg::paces(&String::from_utf8_lossy(&output.stdout)),
            Ok(Err(_)) => false,
            Err(_) => return false,
        };
        self.paces.lock().expect("no holder panics").insert(program.to_path_buf(), paces);
        paces
    }
}

/// The last `most` values put in, by key. Putting one more drops the oldest.
pub struct Kept<K, V> {
    most: usize,
    values: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Clone + Eq + Hash, V: Clone> Kept<K, V> {
    pub fn new(most: usize) -> Self {
        Self { most, values: HashMap::new(), order: VecDeque::new() }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        self.values.get(key).cloned()
    }

    pub fn put(&mut self, key: K, value: V) {
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

/// What is inside file `index` of a download. ffprobe reads the file from the engine's own HTTP
/// server, which fetches the pieces it asks for first.
pub async fn probe(ctx: &Ctx, id: TorrentId, index: u32) -> Result<Arc<Media>, Error> {
    let cached = ctx.media.probes.lock().expect("no holder panics").get(&(id, index));
    if let Some(media) = cached {
        return Ok(media);
    }
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let name = name(&file);
    let ffprobe = match ctx.media.ffmpeg.found() {
        Found::Installed(programs) | Found::Fetched(programs) => programs.ffprobe,
        missing => {
            let end = file.size.min(crate::playback::media::RUNTIME_BYTES);
            let head = http::read(ctx, id, index, &file, 0, end).await.unwrap_or_default();
            let tail = http::read(ctx, id, index, &file, file.size - end, file.size).await.unwrap_or_default();
            let duration = crate::playback::media::duration(&head, &tail).map(|d| d.as_secs_f64());
            return Ok(Arc::new(Media::without_ffprobe(&name, duration, missing.readiness())));
        }
    };
    let url = http::url(ctx.http_port, id, index, &name);
    let output = tokio::process::Command::new(&ffprobe)
        .args(ffmpeg::probe(&url))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| Error::internal(format!("cannot run {}: {e}", ffprobe.display())))?;
    if !output.status.success() {
        let complaint = crate::ffmpeg::complaint(&String::from_utf8_lossy(&output.stderr));
        return Err(Error::internal(format!("ffprobe cannot read {name}: {complaint}"))
            .with_hint("the file may not be a video, or its download may be damaged"));
    }
    let media = Arc::new(Media::from_ffprobe(&output.stdout, &name)?);
    ctx.media.probes.lock().expect("no holder panics").put((id, index), media.clone());
    Ok(media)
}

/// What `probe` has already read about the file, without reading it now.
pub fn probed(ctx: &Ctx, id: TorrentId, index: u32) -> Option<Arc<Media>> {
    ctx.media.probes.lock().expect("no holder panics").get(&(id, index))
}

/// The command that converts file `index` of a download to a fragmented MP4 from `t` seconds on,
/// with audio track `audio` or the default one, and the picture copied or made H.264.
pub async fn stream(
    ctx: &Ctx,
    id: TorrentId,
    index: u32,
    t: f64,
    audio: Option<u32>,
    picture: Picture,
) -> Result<Conversion, Error> {
    let program = ctx.media.ffmpeg.now()?.ffmpeg;
    let media = probe(ctx, id, index).await?;
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let name = name(&file);
    if media.video.is_none() {
        return Err(Error::new(ErrorCode::NoSelection, format!("{name} has no video track")));
    }
    if audio.is_some() && media.audio_track(audio).is_none() {
        return Err(Error::new(
            ErrorCode::Usage,
            format!("{name} has no audio track at stream {}", audio.unwrap_or(0)),
        ));
    }
    let url = http::url(ctx.http_port, id, index, &name);
    let paced = ctx.media.paces(&program).await;
    Ok((program, ffmpeg::stream(&url, &media, t, audio, picture, Encoder::of_host(), paced), None))
}

/// The command that writes WebVTT of the subtitle track at `stream` inside file `index` of a
/// download from `t` seconds on, or with no stream, of that file itself, a subtitle file.
pub async fn subtitles(ctx: &Ctx, id: TorrentId, index: u32, stream: Option<u32>, t: f64) -> Result<Conversion, Error> {
    let program = ctx.media.ffmpeg.now()?.ffmpeg;
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let name = name(&file);
    let Some(stream) = stream else {
        if file.size > LARGEST_SUBTITLE_FILE {
            return Err(Error::new(ErrorCode::Usage, format!("{name} is too large to be a subtitle file")));
        }
        let bytes = http::read(ctx, id, index, &file, 0, file.size)
            .await
            .map_err(|e| Error::new(ErrorCode::StorageError, format!("cannot read {name}: {e}")))?;
        let text = crate::playback::subtitle::decode(&bytes).into_bytes();
        return Ok((program, ffmpeg::subtitle_file(ffmpeg::subtitle_format(&name)), Some(text)));
    };
    let media = probe(ctx, id, index).await?;
    match media.subtitles.iter().find(|track| track.stream == stream) {
        None => {
            return Err(Error::new(ErrorCode::NotFound, format!("{name} has no subtitle track at stream {stream}")));
        }
        Some(track) if !track.text => {
            return Err(Error::new(ErrorCode::Usage, format!("stream {stream} of {name} holds pictures, not text"))
                .with_hint("picture subtitles cannot be shown in the web player yet"));
        }
        Some(_) => {}
    }
    let url = http::url(ctx.http_port, id, index, &name);
    let paced = ctx.media.paces(&program).await;
    Ok((program, ffmpeg::embedded_subtitles(&url, stream, t, paced), None))
}

/// The file's name, which gives ffmpeg its extension.
pub fn name(file: &StreamFile) -> String {
    Path::new(&file.path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}
