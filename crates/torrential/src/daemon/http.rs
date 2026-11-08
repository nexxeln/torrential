//! Serves downloads to video players over HTTP on 127.0.0.1. A request for bytes that have not
//! arrived asks the engine for their pieces first and waits for them, so a player can start and
//! seek while the rest of the file downloads.

use std::io::{self, SeekFrom};
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use torrential_core::TorrentId;

use super::owner::{Command, StreamFile};
use super::server::Ctx;

/// Fixed so a file's URL stays the same across engine runs. Players remember where playback
/// stopped by URL.
pub const PORT: u16 = 47919;
const CHUNK: u64 = 1 << 20;
const POLL: Duration = Duration::from_millis(100);
const STALL: Duration = Duration::from_secs(5 * 60);
const MAX_HEAD: usize = 16 * 1024;

const TYPES: [(&str, &str); 6] = [
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("vtt", "text/vtt"),
    ("srt", "application/x-subrip"),
];

/// Binds `port` on 127.0.0.1, or any free port when another program holds it.
pub fn bind(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).or_else(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
}

pub async fn accept(ctx: Arc<Ctx>, listener: tokio::net::TcpListener) {
    while let Ok((stream, _)) = listener.accept().await {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            ctx.owner.tell(Command::Connected);
            let (rd, wr) = stream.into_split();
            let _ = Connection { ctx: &ctx, rd, wr, pending: Vec::new() }.serve().await;
            ctx.owner.tell(Command::Disconnected { keep_until: None });
        });
    }
}

struct Request {
    head_only: bool,
    path: String,
    range: Option<String>,
    keep_alive: bool,
}

enum Head {
    Request(Request),
    Refused(&'static str),
}

struct Connection<'a> {
    ctx: &'a Ctx,
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    /// Bytes read past the end of the request being answered.
    pending: Vec<u8>,
}

impl Connection<'_> {
    async fn serve(mut self) -> io::Result<()> {
        loop {
            let keep_alive = match self.read_head().await? {
                None => return Ok(()),
                Some(Head::Refused(status)) => {
                    self.empty(status, "", false).await?;
                    false
                }
                Some(Head::Request(request)) => self.answer(request).await?,
            };
            if !keep_alive {
                return Ok(());
            }
        }
    }

    async fn read_head(&mut self) -> io::Result<Option<Head>> {
        loop {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut parsed = httparse::Request::new(&mut headers);
            match parsed.parse(&self.pending) {
                Ok(httparse::Status::Complete(length)) => {
                    let head = head(&parsed);
                    self.pending.drain(..length);
                    return Ok(Some(head));
                }
                Ok(httparse::Status::Partial) if self.pending.len() < MAX_HEAD => {}
                _ => return Ok(Some(Head::Refused("400 Bad Request"))),
            }
            if self.rd.read_buf(&mut self.pending).await? == 0 {
                return Ok(None);
            }
        }
    }

    async fn answer(&mut self, request: Request) -> io::Result<bool> {
        let keep = request.keep_alive;
        let Some((id, index)) = route(&request.path) else { return self.empty("404 Not Found", "", keep).await };
        let Ok(file) = self.ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await else {
            return self.empty("404 Not Found", "", keep).await;
        };
        let Ok(range) = range(request.range.as_deref(), file.size) else {
            let unsatisfiable = format!("Content-Range: bytes */{}\r\n", file.size);
            return self.empty("416 Range Not Satisfiable", &unsatisfiable, keep).await;
        };
        let (status, (start, end), extra) = match range {
            Some((start, end)) => (
                "206 Partial Content",
                (start, end),
                format!("Content-Range: bytes {start}-{}/{}\r\n", end - 1, file.size),
            ),
            None => ("200 OK", (0, file.size), String::new()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{extra}{}\r\n",
            content_type(&file.path),
            end - start,
            if keep { "" } else { "Connection: close\r\n" },
        );
        self.wr.write_all(head.as_bytes()).await?;
        if !request.head_only {
            self.body(&file, id, index, start, end).await?;
        }
        Ok(keep)
    }

    async fn empty(&mut self, status: &str, extra: &str, keep: bool) -> io::Result<bool> {
        let close = if keep { "" } else { "Connection: close\r\n" };
        let head = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nAccept-Ranges: bytes\r\n{extra}{close}\r\n");
        self.wr.write_all(head.as_bytes()).await?;
        Ok(keep)
    }

    async fn body(&mut self, file: &StreamFile, id: TorrentId, index: u32, start: u64, end: u64) -> io::Result<()> {
        let mut complete = file.complete;
        let mut disk = None;
        let mut buf = vec![0; CHUNK as usize];
        let mut offset = start;
        while offset < end {
            let length = CHUNK.min(end - offset);
            if !complete {
                complete = self.arrived(id, index, offset, length).await?;
            }
            let disk = match &mut disk {
                Some(disk) => disk,
                None => disk.insert(tokio::fs::File::open(&file.path).await?),
            };
            let chunk = &mut buf[..length as usize];
            disk.seek(SeekFrom::Start(offset)).await?;
            disk.read_exact(chunk).await?;
            self.wr.write_all(chunk).await?;
            offset += length;
        }
        Ok(())
    }

    /// Waits until the engine has written `length` bytes at `offset`. True once the whole download
    /// is done, which takes it out of the engine.
    async fn arrived(&mut self, id: TorrentId, index: u32, offset: u64, length: u64) -> io::Result<bool> {
        let started = Instant::now();
        loop {
            if matches!(self.ctx.engine.want_range(id, index, offset, length).await, Ok(true)) {
                return Ok(false);
            }
            match self.ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await {
                Ok(file) if file.complete => return Ok(true),
                Ok(_) => {}
                Err(e) => return Err(io::Error::other(e.message)),
            }
            if started.elapsed() >= STALL {
                return Err(io::ErrorKind::TimedOut.into());
            }
            // A player that seeks closes this connection and opens another.
            tokio::select! {
                read = self.rd.read_buf(&mut self.pending) => {
                    if read? == 0 || self.pending.len() > MAX_HEAD {
                        return Err(io::ErrorKind::ConnectionAborted.into());
                    }
                }
                _ = tokio::time::sleep(POLL) => {}
            }
        }
    }
}

fn head(parsed: &httparse::Request) -> Head {
    let header = |name: &str| {
        let value = parsed.headers.iter().find(|h| h.name.eq_ignore_ascii_case(name))?.value;
        Some(String::from_utf8_lossy(value).trim().to_ascii_lowercase())
    };
    // A web page that rebinds its own hostname to 127.0.0.1 still names that hostname.
    let host = header("host");
    if host
        .as_deref()
        .is_some_and(|h| !matches!(h.rsplit_once(':').map_or(h, |(name, _)| name), "127.0.0.1" | "localhost"))
    {
        return Head::Refused("403 Forbidden");
    }
    let head_only = match parsed.method {
        Some("GET") => false,
        Some("HEAD") => true,
        _ => return Head::Refused("405 Method Not Allowed"),
    };
    let connection = header("connection");
    let keep_alive = match parsed.version {
        Some(1) => connection.as_deref() != Some("close"),
        _ => connection.as_deref() == Some("keep-alive"),
    };
    let path = parsed.path.unwrap_or_default().to_string();
    Head::Request(Request { head_only, path, range: header("range"), keep_alive })
}

/// `/{torrent ID}/{file index}/{file name}`, where the name is only for the player.
fn route(path: &str) -> Option<(TorrentId, u32)> {
    let mut parts = path.strip_prefix('/')?.split('/');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

struct Unsatisfiable;

/// The bytes a `Range` header asks for, end exclusive, or `None` for the whole file. A header this
/// server cannot read, or one with several ranges, gets the whole file, as RFC 9110 allows.
fn range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, Unsatisfiable> {
    let Some((first, last)) = header.and_then(|h| h.strip_prefix("bytes=")?.split_once('-')) else { return Ok(None) };
    if last.contains(',') {
        return Ok(None);
    }
    let (first, last) = (first.trim(), last.trim());
    let (start, end) = if first.is_empty() {
        match last.parse::<u64>() {
            Ok(0) => return Err(Unsatisfiable),
            Ok(suffix) => (size.saturating_sub(suffix), size),
            Err(_) => return Ok(None),
        }
    } else {
        let Ok(start) = first.parse::<u64>() else { return Ok(None) };
        match last.parse::<u64>() {
            _ if last.is_empty() => (start, size),
            Ok(last) if last >= start => (start, size.min(last.saturating_add(1))),
            _ => return Ok(None),
        }
    };
    if start >= size {
        return Err(Unsatisfiable);
    }
    Ok(Some((start, end)))
}

fn content_type(path: &Path) -> &'static str {
    let extension = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    TYPES.iter().find(|(e, _)| *e == extension).map_or("application/octet-stream", |(_, t)| t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_read_as_rfc_9110_describes() {
        let read = |header| match range(header, 1000) {
            Ok(Some((start, end))) => format!("{start}..{end}"),
            Ok(None) => "whole".to_string(),
            Err(Unsatisfiable) => "416".to_string(),
        };
        let cases = [
            (None, "whole"),
            (Some("bytes=0-99"), "0..100"),
            (Some("bytes=900-"), "900..1000"),
            (Some("bytes=990-5000"), "990..1000"),
            (Some("bytes=-100"), "900..1000"),
            (Some("bytes=-5000"), "0..1000"),
            (Some("bytes=1000-"), "416"),
            (Some("bytes=-0"), "416"),
            (Some("bytes=5-1"), "whole"),
            (Some("bytes=0-1,5-6"), "whole"),
            (Some("items=0-1"), "whole"),
        ];
        for (header, expected) in cases {
            assert_eq!(read(header), expected, "{header:?}");
        }
        assert!(matches!(range(Some("bytes=-1"), 0), Err(Unsatisfiable)), "an empty file has no last byte");
    }
}
