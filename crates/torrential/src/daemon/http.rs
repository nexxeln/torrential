//! The engine's HTTP server on 127.0.0.1. It serves downloads to video players, and the web app
//! with its API. A request for bytes that have not arrived asks the engine for their pieces first
//! and waits for them, so a player can start and seek while the rest of the file downloads.

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
use super::web;

/// Fixed so a file's URL stays the same across engine runs. Players remember where playback
/// stopped by URL.
pub const PORT: u16 = 47919;
const CHUNK: u64 = 1 << 20;
const POLL: Duration = Duration::from_millis(100);
const STALL: Duration = Duration::from_secs(5 * 60);
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
/// Smaller chunks of a streamed answer go out in one write with their framing.
const ONE_WRITE: usize = 8 * 1024;

const JSON: &str = "application/json";

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

/// `http://127.0.0.1:{port}/{torrent ID}/{file index}/{file name}`. The server routes on the ID
/// and index; the name gives the player a title and an extension.
pub fn url(port: u16, id: TorrentId, index: u32, path: &str) -> String {
    format!("http://127.0.0.1:{port}{}", route_of(id, index, path))
}

/// The path part of `url`, which the web app requests from its own origin.
pub fn route_of(id: TorrentId, index: u32, path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let name: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("/{}/{index}/{name}", id.to_hex())
}

pub async fn accept(ctx: Arc<Ctx>, listener: tokio::net::TcpListener) {
    while let Ok((stream, _)) = listener.accept().await {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            ctx.owner.tell(Command::Connected);
            let _ = stream.set_nodelay(true);
            let (rd, wr) = stream.into_split();
            let _ = Connection { ctx: &ctx, rd, wr, pending: Vec::new(), chunked: false }.serve().await;
            ctx.owner.tell(Command::Disconnected { keep_until: None });
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Head,
    Post,
}

pub struct Request {
    pub method: Method,
    /// Without the query.
    pub path: String,
    query: String,
    range: Option<String>,
    /// The body of a POST, which the guard has checked is JSON.
    pub body: Vec<u8>,
    keep_alive: bool,
    /// HTTP/1.1, which can take a chunked answer.
    chunks: bool,
    length: usize,
}

impl Request {
    /// A query parameter, percent-decoded. An empty value counts as absent.
    pub fn param(&self, name: &str) -> Option<String> {
        let (_, value) = url::form_urlencoded::parse(self.query.as_bytes()).find(|(key, _)| key == name)?;
        (!value.is_empty()).then(|| value.into_owned())
    }

    fn head_only(&self) -> bool {
        self.method == Method::Head
    }
}

enum Head {
    Request(Request),
    Refused(&'static str),
}

pub struct Connection<'a> {
    pub ctx: &'a Ctx,
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    /// Bytes read past the end of the request being answered.
    pending: Vec<u8>,
    /// The streamed answer in progress is chunked, not ended by closing the connection.
    chunked: bool,
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
                Some(Head::Request(mut request)) => {
                    if request.length > MAX_BODY {
                        self.empty("413 Content Too Large", "", false).await?;
                        return Ok(());
                    }
                    request.body = self.read_body(request.length).await?;
                    self.answer(request).await?
                }
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
                    let head = head(&parsed, self.ctx.http_port);
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

    async fn read_body(&mut self, length: usize) -> io::Result<Vec<u8>> {
        while self.pending.len() < length {
            if self.rd.read_buf(&mut self.pending).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(self.pending.drain(..length).collect())
    }

    async fn answer(&mut self, request: Request) -> io::Result<bool> {
        if let Some(route) = request.path.strip_prefix("/api/") {
            return web::answer(self, &request, route).await;
        }
        if request.method == Method::Post {
            return self.empty("405 Method Not Allowed", "", request.keep_alive).await;
        }
        match route(&request.path) {
            Some((id, index)) => self.file(request, id, index).await,
            None => match web::assets::find(&request.path) {
                Some(asset) => {
                    let headers = format!("Cache-Control: {}\r\n", asset.cache);
                    self.send("200 OK", asset.content_type, &headers, asset.bytes, &request).await
                }
                None => self.empty("404 Not Found", "", request.keep_alive).await,
            },
        }
    }

    /// Answers with a whole body.
    pub async fn send(
        &mut self,
        status: &str,
        content_type: &str,
        headers: &str,
        body: &[u8],
        request: &Request,
    ) -> io::Result<bool> {
        let close = if request.keep_alive { "" } else { "Connection: close\r\n" };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{headers}{close}\r\n",
            body.len()
        );
        self.wr.write_all(head.as_bytes()).await?;
        if !request.head_only() {
            self.wr.write_all(body).await?;
        }
        Ok(request.keep_alive)
    }

    pub async fn json(&mut self, status: &str, body: &impl serde::Serialize, request: &Request) -> io::Result<bool> {
        let body = serde_json::to_vec(body).map_err(io::Error::other)?;
        self.send(status, JSON, "Cache-Control: no-store\r\n", &body, request).await
    }

    /// Starts an answer whose length is unknown. `chunk` sends each part and `end` finishes it. The
    /// connection closes after it.
    pub async fn begin(&mut self, content_type: &str, request: &Request) -> io::Result<()> {
        self.chunked = request.chunks;
        let encoding = if self.chunked { "Transfer-Encoding: chunked\r\n" } else { "" };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\n{encoding}Connection: close\r\n\r\n"
        );
        self.wr.write_all(head.as_bytes()).await
    }

    pub async fn chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !self.chunked {
            return self.wr.write_all(bytes).await;
        }
        let size = format!("{:x}\r\n", bytes.len());
        if bytes.len() <= ONE_WRITE {
            return self.wr.write_all(&[size.as_bytes(), bytes, b"\r\n"].concat()).await;
        }
        self.wr.write_all(size.as_bytes()).await?;
        self.wr.write_all(bytes).await?;
        self.wr.write_all(b"\r\n").await
    }

    /// Marks a streamed answer as complete. An answer cut short ends without this, so the client
    /// can tell.
    pub async fn end(&mut self) -> io::Result<bool> {
        if self.chunked {
            self.wr.write_all(b"0\r\n\r\n").await?;
        }
        Ok(false)
    }

    /// Returns once the client has closed the connection.
    pub async fn gone(&mut self) -> io::Error {
        loop {
            match self.rd.read_buf(&mut self.pending).await {
                Ok(0) => return io::ErrorKind::ConnectionAborted.into(),
                Ok(_) if self.pending.len() > MAX_HEAD => return io::ErrorKind::ConnectionAborted.into(),
                Ok(_) => {}
                Err(e) => return e,
            }
        }
    }

    async fn file(&mut self, request: Request, id: TorrentId, index: u32) -> io::Result<bool> {
        let keep = request.keep_alive;
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
        if !request.head_only() {
            self.body(&file, id, index, start, end).await?;
        }
        Ok(keep)
    }

    pub async fn empty(&mut self, status: &str, extra: &str, keep: bool) -> io::Result<bool> {
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
        let ctx = self.ctx;
        // A player that seeks closes this connection and opens another.
        tokio::select! {
            arrived = arrived(ctx, id, index, offset, length) => arrived,
            gone = self.gone() => Err(gone),
        }
    }
}

/// Waits until the engine has written `length` bytes of a file at `offset`. True once the whole
/// download is done, which takes it out of the engine.
async fn arrived(ctx: &Ctx, id: TorrentId, index: u32, offset: u64, length: u64) -> io::Result<bool> {
    let started = Instant::now();
    loop {
        if matches!(ctx.engine.want_range(id, index, offset, length).await, Ok(true)) {
            return Ok(false);
        }
        match ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await {
            Ok(file) if file.complete => return Ok(true),
            Ok(_) => {}
            Err(e) => return Err(io::Error::other(e.message)),
        }
        if started.elapsed() >= STALL {
            return Err(io::ErrorKind::TimedOut.into());
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Bytes `start..end` of a download's file, waiting for the pieces that hold them.
pub async fn read(
    ctx: &Ctx,
    id: TorrentId,
    index: u32,
    file: &StreamFile,
    start: u64,
    end: u64,
) -> io::Result<Vec<u8>> {
    let end = end.min(file.size);
    let mut complete = file.complete;
    let mut bytes = vec![0; end.saturating_sub(start) as usize];
    let mut offset = start;
    while offset < end && !complete {
        let length = CHUNK.min(end - offset);
        complete = arrived(ctx, id, index, offset, length).await?;
        offset += length;
    }
    let mut disk = tokio::fs::File::open(&file.path).await?;
    disk.seek(SeekFrom::Start(start)).await?;
    disk.read_exact(&mut bytes).await?;
    Ok(bytes)
}

fn head(parsed: &httparse::Request, port: u16) -> Head {
    let header = |name: &str| {
        let value = parsed.headers.iter().find(|h| h.name.eq_ignore_ascii_case(name))?.value;
        Some(String::from_utf8_lossy(value).trim().to_ascii_lowercase())
    };
    let (path, query) = match parsed.path.unwrap_or_default().split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (parsed.path.unwrap_or_default().to_string(), String::new()),
    };
    let from = Sender { host: header("host"), site: header("sec-fetch-site"), origin: header("origin") };
    if !from.allowed(port) {
        return Head::Refused("403 Forbidden");
    }
    let method = match parsed.method {
        Some("GET") => Method::Get,
        Some("HEAD") => Method::Head,
        Some("POST") => Method::Post,
        _ => return Head::Refused("405 Method Not Allowed"),
    };
    let json = header("content-type").is_some_and(|t| t.split(';').next().unwrap_or_default().trim() == JSON);
    if method == Method::Post && !json {
        return Head::Refused("415 Unsupported Media Type");
    }
    let length = match header("content-length").map(|length| length.parse()) {
        None => 0,
        Some(Ok(length)) => length,
        Some(Err(_)) => return Head::Refused("400 Bad Request"),
    };
    if header("transfer-encoding").is_some() {
        return Head::Refused("411 Length Required");
    }
    let connection = header("connection");
    let keep_alive = match parsed.version {
        Some(1) => connection.as_deref() != Some("close"),
        _ => connection.as_deref() == Some("keep-alive"),
    };
    let chunks = parsed.version == Some(1);
    Head::Request(Request { method, path, query, range: header("range"), body: Vec::new(), keep_alive, chunks, length })
}

/// What a request says about where it comes from, lowercased.
struct Sender {
    host: Option<String>,
    site: Option<String>,
    origin: Option<String>,
}

impl Sender {
    /// False for a request another website made through the person's browser. Players, curl, and
    /// agents send none of these headers but `Host`.
    fn allowed(&self, port: u16) -> bool {
        let local =
            |host: &str| matches!(host.rsplit_once(':').map_or(host, |(name, _)| name), "127.0.0.1" | "localhost");
        // A web page that rebinds its own hostname to 127.0.0.1 still names that hostname.
        if self.host.as_deref().is_some_and(|host| !local(host)) {
            return false;
        }
        if matches!(self.site.as_deref(), Some("cross-site" | "same-site")) {
            return false;
        }
        let own = |origin: &str| {
            origin.strip_prefix("http://").and_then(|host| host.rsplit_once(':')).is_some_and(|(name, origin_port)| {
                matches!(name, "127.0.0.1" | "localhost") && origin_port == port.to_string()
            })
        };
        self.origin.as_deref().is_none_or(own)
    }
}

/// The torrent ID and file index of a path `url` built.
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
    fn requests_other_websites_make_through_the_browser_are_refused() {
        let allowed = |host: &str, site: Option<&str>, origin: Option<&str>| {
            let from =
                Sender { host: Some(host.into()), site: site.map(String::from), origin: origin.map(String::from) };
            from.allowed(47919)
        };
        // A player, curl, or an agent.
        assert!(allowed("127.0.0.1:47919", None, None));
        assert!(allowed("localhost", None, None));
        assert!(Sender { host: None, site: None, origin: None }.allowed(47919), "HTTP/1.0 has no Host");
        // The web app itself, and its address typed into the browser.
        assert!(allowed("127.0.0.1:47919", Some("same-origin"), Some("http://127.0.0.1:47919")));
        assert!(allowed("localhost:47919", Some("same-origin"), Some("http://localhost:47919")));
        assert!(allowed("127.0.0.1:47919", Some("none"), None));
        // A page that rebinds its hostname to 127.0.0.1.
        assert!(!allowed("rebound.example:47919", None, None));
        // Another website, including one on another port of this machine.
        assert!(!allowed("127.0.0.1:47919", Some("cross-site"), Some("https://example.org")));
        assert!(!allowed("127.0.0.1:47919", Some("cross-site"), None));
        assert!(!allowed("127.0.0.1:47919", Some("same-site"), None));
        // A browser too old to send Sec-Fetch-Site still names the origin of a POST.
        assert!(!allowed("127.0.0.1:47919", None, Some("https://example.org")));
        assert!(!allowed("127.0.0.1:47919", None, Some("http://127.0.0.1:3000")));
        assert!(!allowed("127.0.0.1:47919", None, Some("null")));
    }

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
