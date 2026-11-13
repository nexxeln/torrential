//! What the web player asks for while a video plays: the file's tracks, the video converted to
//! what the browser decodes, its subtitles, and the progress of every download.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use torrential_core::protocol::Event;
use torrential_core::{Error, ErrorCode, JobStatus, TorrentId};

use super::super::http::{self, Connection, Request};
use super::super::owner::{self, Command, StreamFile, Update};
use super::super::server::{Ctx, HEARTBEAT};
use super::fetch::Found;
use super::ffmpeg::{self, Encoder, Picture};
use super::media::Media;
use super::{fail, file_of, number};

/// Bytes read from each end of a video for its runtime when ffprobe is not installed, as `play` reads.
const HEADER: u64 = 2 * 1024 * 1024;
/// A subtitle file is text. Anything larger is something else with a subtitle's name.
const LARGEST_SUBTITLE_FILE: u64 = 16 * 1024 * 1024;
const PIPE: usize = 64 * 1024;
/// How much of what ffmpeg prints about a failure the engine keeps.
const COMPLAINT: usize = 4 * 1024;
/// How many runs `pieces` cuts a file into. The player's seek bar draws one mark for each.
const RUNS: u32 = 200;

/// A program to run, its arguments, and what to write to its stdin.
type Conversion = (PathBuf, Vec<String>, Option<Vec<u8>>);

/// Follows every download as server-sent events. The first is a snapshot.
pub async fn events(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let ctx = conn.ctx;
    let watch = ctx.owner.ask(|reply| Command::Watch { refs: Vec::new(), reply }).await;
    let (jobs, mut updates) = match watch {
        Ok(watch) => watch,
        Err(error) => return fail(conn, request, &error).await,
    };
    conn.begin("text/event-stream", request).await?;
    send(conn, &Event::Snapshot { jobs }).await?;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    loop {
        let event = tokio::select! {
            update = updates.recv() => match update {
                Ok(Update::Job(job)) => Event::Job { job },
                Ok(Update::Removed(id)) => Event::Removed { id: id.to_hex() },
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    match ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await {
                        Ok(jobs) => Event::Snapshot { jobs },
                        Err(_) => return conn.end().await,
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return conn.end().await,
            },
            _ = heartbeat.tick() => Event::Heartbeat { at: owner::now() },
            gone = conn.gone() => return Err(gone),
        };
        heartbeat.reset();
        send(conn, &event).await?;
    }
}

async fn send(conn: &mut Connection<'_>, event: &Event) -> io::Result<()> {
    let json = serde_json::to_string(event).map_err(io::Error::other)?;
    conn.chunk(format!("data: {json}\n\n").as_bytes()).await
}

/// What is inside file `index` of a download. ffprobe reads the file from the engine's own HTTP
/// server, which fetches the pieces it asks for first.
pub async fn media(ctx: &Ctx, id: TorrentId, index: u32) -> Result<Arc<Media>, Error> {
    let cached = ctx.web.probes.lock().expect("no holder panics").get(&(id, index)).cloned();
    if let Some(media) = cached {
        return Ok(media);
    }
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let name = name(&file);
    let ffprobe = match ctx.web.ffmpeg.found() {
        Found::Installed(programs) | Found::Fetched(programs) => programs.ffprobe,
        missing => {
            let end = file.size.min(HEADER);
            let head = http::read(ctx, id, index, &file, 0, end).await.unwrap_or_default();
            let tail = http::read(ctx, id, index, &file, file.size - end, file.size).await.unwrap_or_default();
            let duration = torrential_core::media::duration(&head, &tail).map(|d| d.as_secs_f64());
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
        let complaint = ffmpeg::complaint(&String::from_utf8_lossy(&output.stderr));
        return Err(Error::internal(format!("ffprobe cannot read {name}: {complaint}"))
            .with_hint("the file may not be a video, or its download may be damaged"));
    }
    let media = Arc::new(Media::from_ffprobe(&output.stdout, &name)?);
    ctx.web.probes.lock().expect("no holder panics").insert((id, index), media.clone());
    Ok(media)
}

/// What `media` has already read about the file, without reading it now.
pub fn probed(ctx: &Ctx, id: TorrentId, index: u32) -> Option<Arc<Media>> {
    ctx.web.probes.lock().expect("no holder panics").get(&(id, index)).cloned()
}

/// `GET /api/pieces`: which parts of one file are on disk, as `RUNS` characters of `0` to `8`.
pub async fn pieces(ctx: &Ctx, id: TorrentId, index: u32) -> Result<String, Error> {
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    if file.complete || file.size == 0 {
        return Ok("8".repeat(RUNS as usize));
    }
    let runs = match ctx.engine.written(id, index, 0, file.size, RUNS).await {
        Ok(runs) => runs,
        // The engine does not hold a download that just finished, one that is stopped, or one that
        // has not started yet.
        Err(_) => {
            let jobs = ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await?;
            if jobs.first().is_some_and(|job| matches!(job.status, JobStatus::Done { .. })) {
                return Ok("8".repeat(RUNS as usize));
            }
            if jobs.first().is_none_or(|job| job.status.is_stopped()) {
                let name = name(&file);
                return Err(Error::new(
                    ErrorCode::NotFound,
                    format!("{name} is not downloading now, so what has arrived of it is not known"),
                )
                .with_hint("resume the download, or play the file again"));
            }
            return Ok("0".repeat(RUNS as usize));
        }
    };
    Ok(runs.into_iter().map(|run| run.symbol()).collect())
}

/// The video as a fragmented MP4 from `t` seconds on, converted by ffmpeg as the browser reads it.
pub async fn stream(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let ctx = conn.ctx;
    let conversion = tokio::select! {
        conversion = stream_conversion(ctx, request) => conversion,
        gone = conn.gone() => return Err(gone),
    };
    match conversion {
        Ok(conversion) => pipe(conn, request, conversion, "video/mp4").await,
        Err(error) => fail(conn, request, &error).await,
    }
}

async fn stream_conversion(ctx: &Ctx, request: &Request) -> Result<Conversion, Error> {
    let (id, index) = file_of(request)?;
    let t = number::<f64>(request, "t")?.unwrap_or(0.0);
    let audio = number::<u32>(request, "audio")?;
    let picture = match request.param("video").as_deref() {
        None | Some("copy") => Picture::Copy,
        Some("h264") => Picture::H264,
        Some(other) => return Err(Error::new(ErrorCode::Usage, format!("video={other} is not copy or h264"))),
    };
    let program = ctx.web.ffmpeg.now()?.ffmpeg;
    let media = media(ctx, id, index).await?;
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
    Ok((program, ffmpeg::stream(&url, &media, t, audio, picture, Encoder::of_host()), None))
}

/// WebVTT of a subtitle track inside a video, or of a subtitle file of the torrent.
pub async fn subtitles(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let ctx = conn.ctx;
    let conversion = tokio::select! {
        conversion = subtitle_conversion(ctx, request) => conversion,
        gone = conn.gone() => return Err(gone),
    };
    match conversion {
        Ok(conversion) => pipe(conn, request, conversion, super::VTT).await,
        Err(error) => fail(conn, request, &error).await,
    }
}

async fn subtitle_conversion(ctx: &Ctx, request: &Request) -> Result<Conversion, Error> {
    let (id, index) = file_of(request)?;
    let program = ctx.web.ffmpeg.now()?.ffmpeg;
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let name = name(&file);
    let Some(stream) = number::<u32>(request, "stream")? else {
        if file.size > LARGEST_SUBTITLE_FILE {
            return Err(Error::new(ErrorCode::Usage, format!("{name} is too large to be a subtitle file")));
        }
        let bytes = http::read(ctx, id, index, &file, 0, file.size)
            .await
            .map_err(|e| Error::new(ErrorCode::StorageError, format!("cannot read {name}: {e}")))?;
        let text = torrential_core::subtitle::decode(&bytes).into_bytes();
        return Ok((program, ffmpeg::subtitle_file(ffmpeg::subtitle_format(&name)), Some(text)));
    };
    let t = number::<f64>(request, "t")?.unwrap_or(0.0);
    let media = media(ctx, id, index).await?;
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
    Ok((program, ffmpeg::embedded_subtitles(&url, stream, t), None))
}

/// Runs a conversion and sends what it writes as it writes it. The program stops when the browser
/// stops reading: it is killed once this returns.
async fn pipe(
    conn: &mut Connection<'_>,
    request: &Request,
    (program, args, input): Conversion,
    content_type: &str,
) -> io::Result<bool> {
    let mut command = tokio::process::Command::new(&program);
    command
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return fail(conn, request, &Error::internal(format!("cannot run {}: {e}", program.display()))).await,
    };
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
    let (mut stdout, mut stderr) =
        (child.stdout.take().expect("stdout is piped"), child.stderr.take().expect("stderr is piped"));
    let complaints = tokio::spawn(async move {
        let (mut kept, mut buf) = (Vec::new(), [0; 1024]);
        while let Ok(read @ 1..) = stderr.read(&mut buf).await {
            if kept.len() < COMPLAINT {
                kept.extend_from_slice(&buf[..read]);
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    });
    let mut buf = vec![0; PIPE];
    let mut begun = false;
    loop {
        let read = tokio::select! {
            read = stdout.read(&mut buf) => read?,
            gone = conn.gone() => return Err(gone),
        };
        if read == 0 {
            break;
        }
        if !begun {
            conn.begin(content_type, request).await?;
            begun = true;
        }
        conn.chunk(&buf[..read]).await?;
    }
    let succeeded = child.wait().await?.success();
    let complaint = ffmpeg::complaint(&complaints.await.unwrap_or_default());
    match (begun, succeeded) {
        (true, true) => conn.end().await,
        (false, true) => {
            conn.begin(content_type, request).await?;
            conn.end().await
        }
        (true, false) => {
            super::super::log(&format!("ffmpeg stopped while converting for the web player: {complaint}"));
            Err(io::ErrorKind::ConnectionAborted.into())
        }
        (false, false) => {
            let error = Error::internal(format!("ffmpeg cannot convert this file: {complaint}"));
            conn.json("502 Bad Gateway", &json!({ "error": error }), request).await
        }
    }
}

/// The file's name, which gives ffmpeg its extension.
pub fn name(file: &StreamFile) -> String {
    Path::new(&file.path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}
