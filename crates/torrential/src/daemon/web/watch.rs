//! What the web player asks for while a video plays: the video converted to what the browser
//! decodes, its subtitles, and the progress of every download.

use std::io;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use torrential_core::protocol::Event;
use torrential_core::{Error, ErrorCode};

use super::super::ctx::Ctx;
use super::super::downloads::JobEvents;
use super::super::http::{Connection, Request};
use super::super::media::ffmpeg::Picture;
use super::super::media::{self, Conversion};
use super::{fail, file_of, number};
use crate::log::log;

const PIPE: usize = 64 * 1024;
/// How much of what ffmpeg prints about a failure the engine keeps.
const COMPLAINT: usize = 4 * 1024;

/// Follows every download as server-sent events. The first is a snapshot.
pub async fn events(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let (mut events, snapshot) = match JobEvents::start(conn.ctx, Vec::new()).await {
        Ok(started) => started,
        Err(error) => return fail(conn, request, &error).await,
    };
    conn.begin("text/event-stream", request).await?;
    send(conn, &snapshot).await?;
    loop {
        let event = tokio::select! {
            event = events.next() => match event {
                Some(event) => event,
                None => return conn.end().await,
            },
            gone = conn.gone() => return Err(gone),
        };
        send(conn, &event).await?;
    }
}

async fn send(conn: &mut Connection<'_>, event: &Event) -> io::Result<()> {
    let json = serde_json::to_string(event).map_err(io::Error::other)?;
    conn.chunk(format!("data: {json}\n\n").as_bytes()).await
}

/// The video as a fragmented MP4 from `t` seconds on, converted by ffmpeg as the browser reads it.
pub async fn stream(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let ctx = conn.ctx;
    let _playing = match ctx.playing.open() {
        Ok(open) => open,
        Err(updating) => return fail(conn, request, &updating).await,
    };
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
    media::stream(ctx, id, index, t, audio, picture).await
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
    let stream = number::<u32>(request, "stream")?;
    let t = number::<f64>(request, "t")?.unwrap_or(0.0);
    media::subtitles(ctx, id, index, stream, t).await
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
    let complaint = crate::ffmpeg::complaint(&complaints.await.unwrap_or_default());
    match (begun, succeeded) {
        (true, true) => conn.end().await,
        (false, true) => {
            conn.begin(content_type, request).await?;
            conn.end().await
        }
        (true, false) => {
            log(&format!("ffmpeg stopped while converting for the web player: {complaint}"));
            Err(io::ErrorKind::ConnectionAborted.into())
        }
        (false, false) => {
            let error = Error::internal(format!("ffmpeg cannot convert this file: {complaint}"));
            conn.json("502 Bad Gateway", &super::ErrorBody { error: &error }, request).await
        }
    }
}
