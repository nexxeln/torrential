//! The engine's HTTP server. It serves downloads to video players, and the web app with its API.
//! It listens on 127.0.0.1, and on the home network too while a phone or TV is paired. A request
//! for bytes that have not arrived asks the engine for their pieces first and waits for them, so a
//! player can start and seek while the rest of the file downloads.

use std::collections::VecDeque;
use std::io::{self, SeekFrom};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{oneshot, watch};
use torrential_core::TorrentId;

use super::devices::{Asker, CAST, COOKIE, Devices, Reach};
use super::owner::{Command, StreamFile};
use super::server::Ctx;
use super::web;

/// Fixed so a file's URL stays the same across engine runs. Players remember where playback
/// stopped by URL.
pub const PORT: u16 = 47919;
const CHUNK: u64 = 1 << 20;
const POLL: Duration = Duration::from_millis(100);
const STALL: Duration = Duration::from_secs(5 * 60);
/// How often the engine tries the home network again while another program holds its port.
const RETRY: Duration = Duration::from_secs(2);
/// How long a connection may take to send a whole request, counted from the end of the last
/// answer. One that sends none in that time closes, so idle sockets cannot pile up.
const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// Connections from one address of the home network that no request has shown a device's cookie
/// or a cast grant on yet. A browser opens six at most to one site.
const STRANGERS_PER_ADDRESS: usize = 16;
/// Such connections from every address together, well inside the 256 file descriptors launchd
/// gives a login service.
const STRANGERS: usize = 64;
/// How long the engine waits after it fails to accept a connection, such as when it has no file
/// descriptor left, before it tries again.
const BACKOFF: Duration = Duration::from_secs(1);
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

/// Binds `port` where `reach` says, or any free port when another program holds it.
pub fn bind(port: u16, reach: Reach) -> io::Result<TcpListener> {
    let ip = address(reach);
    TcpListener::bind((ip, port)).or_else(|_| TcpListener::bind((ip, 0)))
}

fn address(reach: Reach) -> Ipv4Addr {
    match reach {
        Reach::Loopback => Ipv4Addr::LOCALHOST,
        Reach::Network => Ipv4Addr::UNSPECIFIED,
    }
}

/// `http://127.0.0.1:{port}/{torrent ID}/{file index}/{file name}`. The server routes on the ID
/// and index; the name gives the player a title and an extension.
pub fn url(port: u16, id: TorrentId, index: u32, path: &str) -> String {
    format!("http://127.0.0.1:{port}{}", route_of(id, index, path))
}

/// The path part of `url`, which the web app requests from its own origin.
pub fn route_of(id: TorrentId, index: u32, path: &str) -> String {
    format!("/{}/{index}/{}", id.to_hex(), url_name(path))
}

/// The last part of `path`, percent-encoded for a URL.
pub fn url_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Where a connection comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Peer {
    /// This Mac, over loopback.
    Loopback,
    /// Another machine, or this Mac by its address on the home network. `local` is the address it
    /// connected to.
    Network { local: IpAddr, remote: IpAddr },
}

impl Peer {
    fn of(local: SocketAddr, remote: SocketAddr) -> Self {
        let remote = remote.ip().to_canonical();
        if remote.is_loopback() { Self::Loopback } else { Self::Network { local: local.ip().to_canonical(), remote } }
    }
}

/// Serves connections, and keeps the listening socket where `reach` says the engine should listen,
/// trying the home network again every few seconds while another program holds the port. `reach`
/// was read when `listener` was bound, so a change since then counts. Connections already open
/// stay open.
pub async fn accept(ctx: Arc<Ctx>, mut listener: tokio::net::TcpListener, mut reach: watch::Receiver<Reach>) {
    let strangers = Arc::new(Strangers::default());
    let mut retry = tokio::time::Instant::now();
    loop {
        let want = *reach.borrow_and_update();
        if reach_of(&listener) != want && tokio::time::Instant::now() >= retry {
            listener = listen(listener, want, ctx.http_port, &ctx.devices).await;
            retry = tokio::time::Instant::now() + RETRY;
        }
        let astray = reach_of(&listener) != want;
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, remote)) => spawn(&ctx, &strangers, stream, remote),
                // A client that left before the engine took its connection.
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset) => {}
                // Out of file descriptors or memory, which connections give back as they end.
                Err(e) => {
                    super::log(&format!("cannot take a connection: {e}"));
                    tokio::time::sleep(BACKOFF).await;
                }
            },
            changed = reach.changed() => {
                if changed.is_err() {
                    return;
                }
                retry = tokio::time::Instant::now();
            }
            () = tokio::time::sleep_until(retry), if astray => {}
        }
    }
}

/// Serves one connection in a task of its own.
fn spawn(ctx: &Arc<Ctx>, strangers: &Arc<Strangers>, stream: TcpStream, remote: SocketAddr) {
    let Ok(local) = stream.local_addr() else { return };
    let peer = Peer::of(local, remote);
    let stranger = match peer {
        Peer::Loopback => None,
        Peer::Network { remote, .. } => Some(strangers.admit(remote)),
    };
    let ctx = ctx.clone();
    tokio::spawn(async move {
        ctx.owner.tell(Command::Connected);
        let _ = stream.set_nodelay(true);
        let (rd, wr) = stream.into_split();
        let connection =
            |stranger| Connection { ctx: &ctx, peer, rd, wr, pending: Vec::new(), chunked: false, stranger };
        match stranger {
            None => drop(connection(None).serve().await),
            Some((stranger, closed)) => tokio::select! {
                _ = connection(Some(stranger)).serve() => {}
                // Once the engine stops listening on the network, nothing from it stays open.
                () = ctx.devices.closed() => {}
                Ok(()) = closed => {}
            },
        }
        ctx.owner.tell(Command::Disconnected { keep_until: None });
    });
}

/// The connections from the network that no request has shown a device's cookie or a cast grant
/// on yet, oldest first. A new one always gets in, so strangers never turn away a paired phone or
/// a TV. To make room, the oldest from its address closes once that address holds
/// `STRANGERS_PER_ADDRESS`, or else the oldest of all once there are `STRANGERS`. Idle sockets
/// cannot use up the engine's file descriptors, and one address cannot crowd out the others.
#[derive(Default)]
struct Strangers {
    open: Mutex<Open>,
}

#[derive(Default)]
struct Open {
    connections: VecDeque<Waiting>,
    next: u64,
}

struct Waiting {
    id: u64,
    ip: IpAddr,
    close: oneshot::Sender<()>,
}

/// Resolves when a newer stranger needs the room. It fails instead once the connection stops
/// counting, so a device's connection never closes for it.
type Closed = oneshot::Receiver<()>;

impl Strangers {
    fn open(&self) -> MutexGuard<'_, Open> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn admit(self: &Arc<Self>, ip: IpAddr) -> (Stranger, Closed) {
        let mut open = self.open();
        let connections = &mut open.connections;
        let oldest = if connections.iter().filter(|waiting| waiting.ip == ip).count() >= STRANGERS_PER_ADDRESS {
            connections.iter().position(|waiting| waiting.ip == ip)
        } else {
            (connections.len() >= STRANGERS).then_some(0)
        };
        if let Some(waiting) = oldest.and_then(|at| connections.remove(at)) {
            let _ = waiting.close.send(());
        }
        let (close, closed) = oneshot::channel();
        let id = open.next;
        open.next += 1;
        open.connections.push_back(Waiting { id, ip, close });
        (Stranger { strangers: self.clone(), id }, closed)
    }
}

/// A connection that counts in `Strangers` until it is dropped.
struct Stranger {
    strangers: Arc<Strangers>,
    id: u64,
}

impl Stranger {
    /// Stops counting a connection whose request showed a device's cookie or a cast grant, so no
    /// newer stranger can close it. False when one already has.
    fn trust(self) -> bool {
        let mut open = self.strangers.open();
        let at = open.connections.iter().position(|waiting| waiting.id == self.id);
        at.and_then(|at| open.connections.remove(at)).is_some()
    }
}

impl Drop for Stranger {
    fn drop(&mut self) {
        let mut open = self.strangers.open();
        open.connections.retain(|waiting| waiting.id != self.id);
    }
}

/// Where `listener` takes connections from, read from the socket itself.
fn reach_of(listener: &tokio::net::TcpListener) -> Reach {
    match listener.local_addr() {
        Ok(address) if address.ip().is_unspecified() => Reach::Network,
        _ => Reach::Loopback,
    }
}

/// A socket on `port` where `reach` says, in place of `old`. Linux refuses every address while
/// 127.0.0.1 holds the port, so the old socket closes first. When the home network cannot be had,
/// 127.0.0.1 can, and `devices` records why.
async fn listen(old: tokio::net::TcpListener, reach: Reach, port: u16, devices: &Devices) -> tokio::net::TcpListener {
    drop(old);
    let open = |ip: Ipv4Addr| {
        let listener = TcpListener::bind((ip, port))?;
        listener.set_nonblocking(true)?;
        tokio::net::TcpListener::from_std(listener)
    };
    loop {
        match open(address(reach)) {
            Ok(listener) => {
                devices.refused(None);
                super::log(&match reach {
                    Reach::Network => format!("listening on the home network on port {port}"),
                    Reach::Loopback => format!("listening on 127.0.0.1 only on port {port}"),
                });
                return listener;
            }
            Err(e) => {
                // The engine tries the home network again every few seconds, so each new reason
                // is logged once.
                let new = reach == Reach::Loopback || devices.refused(Some(e.kind()));
                if new {
                    super::log(&format!("cannot listen on {}:{port}: {e}", address(reach)));
                }
            }
        }
        if reach == Reach::Network
            && let Ok(listener) = open(Ipv4Addr::LOCALHOST)
        {
            return listener;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
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
    from: Sender,
    /// The device token in the request's cookie.
    pub cookie: Option<String>,
    /// The User-Agent, lowercased.
    pub agent: Option<String>,
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
    Request(Box<Request>),
    Refused(&'static str),
}

pub struct Connection<'a> {
    pub ctx: &'a Ctx,
    pub peer: Peer,
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    /// Bytes read past the end of the request being answered.
    pending: Vec<u8>,
    /// The streamed answer in progress is chunked, not ended by closing the connection.
    chunked: bool,
    /// Counts against the limit on strangers until a request head on this connection shows a
    /// paired device's cookie or a cast grant.
    stranger: Option<Stranger>,
}

impl Connection<'_> {
    async fn serve(mut self) -> io::Result<()> {
        loop {
            let by = tokio::time::Instant::now() + REQUEST_WAIT;
            let keep_alive = match self.read_head(by).await? {
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
                    let (ctx, peer) = (self.ctx, self.peer);
                    let ask_now =
                        |request: &Request| ask(peer, request, ctx.http_port, &ctx.devices, super::owner::now());
                    // Before the body, so strangers that come while a slow body arrives cannot
                    // close a paired device's connection.
                    if let Ok(Asker::Device(_) | Asker::Cast(_)) = ask_now(&request)
                        && let Some(stranger) = self.stranger.take()
                        && !stranger.trust()
                    {
                        return Ok(());
                    }
                    // Read before any refusal, since closing on an unread body resets the
                    // connection before the browser reads the answer.
                    request.body = self.read_body(request.length, by).await?;
                    // Asked again, since a device may have been removed, or a cast dropped, while
                    // the body arrived.
                    let asker = match ask_now(&request) {
                        Ok(asker) => asker,
                        Err(refusal) => {
                            self.refuse(refusal).await?;
                            return Ok(());
                        }
                    };
                    // A removed device or a dropped cast loses what it was still receiving. Checked
                    // first, so one lost since it was asked starts nothing.
                    tokio::select! {
                        biased;
                        () = ctx.devices.revoked(&asker) => return Ok(()),
                        keep = self.answer(*request, &asker) => keep?,
                    }
                }
            };
            if !keep_alive {
                return Ok(());
            }
        }
    }

    /// The next request's head, or `None` once the client closes the connection or sends no whole
    /// head `by` then.
    async fn read_head(&mut self, by: tokio::time::Instant) -> io::Result<Option<Head>> {
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
            let Ok(read) = tokio::time::timeout_at(by, self.rd.read_buf(&mut self.pending)).await else {
                return Ok(None);
            };
            if read? == 0 {
                return Ok(None);
            }
        }
    }

    async fn read_body(&mut self, length: usize, by: tokio::time::Instant) -> io::Result<Vec<u8>> {
        while self.pending.len() < length {
            let read = tokio::time::timeout_at(by, self.rd.read_buf(&mut self.pending)).await?;
            if read? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(self.pending.drain(..length).collect())
    }

    async fn answer(&mut self, request: Request, asker: &Asker) -> io::Result<bool> {
        match asker {
            Asker::Stranger { removed } => return web::devices::stranger(self, &request, *removed).await,
            Asker::Cast(_) if request.method == Method::Post => {
                return self.empty("405 Method Not Allowed", "", request.keep_alive).await;
            }
            Asker::Cast(granted) => return self.file(request, granted.id, granted.index).await,
            Asker::Local | Asker::Device(_) => {}
        }
        if let Some(route) = request.path.strip_prefix("/api/") {
            return web::answer(self, &request, route, asker).await;
        }
        if request.method == Method::Post {
            return self.empty("405 Method Not Allowed", "", request.keep_alive).await;
        }
        if request.path == web::devices::PAIR {
            return self.empty("303 See Other", "Location: /\r\n", request.keep_alive).await;
        }
        match route(&request.path) {
            Some((id, index)) => self.file(request, id, index).await,
            None => match web::assets::find(&request.path) {
                Some(asset) => {
                    let mut headers = format!("Cache-Control: {}\r\n", asset.cache);
                    // Browsers cap a cookie at 400 days, so each visit starts the 400 days again.
                    if let (Asker::Device(_), Some(token)) = (asker, &request.cookie)
                        && asset.content_type.starts_with("text/html")
                    {
                        headers.push_str(&web::devices::set_cookie(token));
                    }
                    self.send("200 OK", asset.content_type, &headers, asset.bytes, &request).await
                }
                None => self.empty("404 Not Found", "", request.keep_alive).await,
            },
        }
    }

    async fn refuse(&mut self, refusal: Refusal) -> io::Result<()> {
        let (status, text) = match refusal {
            Refusal::Forbidden => return self.empty("403 Forbidden", "", false).await.map(drop),
            Refusal::Elsewhere(address) => (
                "403 Forbidden",
                format!(
                    "torrential answers only at its own address, so open http://{address} on this device instead.\n"
                ),
            ),
            Refusal::CastEnded => ("404 Not Found", "This cast has ended, so press Play on TV again.\n".to_string()),
            Refusal::Moved(location) => {
                return self
                    .empty("307 Temporary Redirect", &format!("Location: {location}\r\n"), false)
                    .await
                    .map(drop);
            }
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            text.len()
        );
        self.wr.write_all(head.as_bytes()).await?;
        self.wr.write_all(text.as_bytes()).await
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

fn head(parsed: &httparse::Request) -> Head {
    let header = |name: &str| {
        let value = parsed.headers.iter().find(|h| h.name.eq_ignore_ascii_case(name))?.value;
        Some(String::from_utf8_lossy(value).trim().to_ascii_lowercase())
    };
    let (path, query) = match parsed.path.unwrap_or_default().split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (parsed.path.unwrap_or_default().to_string(), String::new()),
    };
    let from = Sender { host: header("host"), site: header("sec-fetch-site"), origin: header("origin") };
    let cookie = header("cookie").and_then(|cookies| {
        let token = cookies.split(';').find_map(|cookie| cookie.trim().strip_prefix(COOKIE)?.strip_prefix('='));
        token.map(String::from)
    });
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
    Head::Request(Box::new(Request {
        method,
        path,
        query,
        range: header("range"),
        body: Vec::new(),
        from,
        cookie,
        agent: header("user-agent"),
        keep_alive,
        chunks,
        length,
    }))
}

/// Why a request is refused before anything reads it.
enum Refusal {
    /// Over loopback, a request another website made, or one that names another host.
    Forbidden,
    /// From the home network, a request another website made, or one that names another host. It
    /// holds the address to open instead.
    Elsewhere(String),
    /// A cast URL whose grant was dropped, or never made.
    CastEnded,
    /// Over loopback, a request that names this Mac's .local name. It holds the same path on
    /// 127.0.0.1, where browsers say which website made a request.
    Moved(String),
}

/// Who sent `request`. Over loopback, a request that names this Mac's .local name moves to
/// 127.0.0.1 first. A cast URL is let in by its grant alone, anything else over loopback is this
/// Mac, and from the network a paired device's cookie lets it in.
fn ask(peer: Peer, request: &Request, port: u16, devices: &Devices, now: i64) -> Result<Asker, Refusal> {
    let name = devices.local_name();
    let named = |host: &str| name.as_deref() == Some(split_host(host).0);
    match peer {
        Peer::Loopback if request.from.host.as_deref().is_some_and(named) => {
            if !request.path.starts_with('/') {
                return Err(Refusal::Forbidden);
            }
            let query = if request.query.is_empty() { String::new() } else { format!("?{}", request.query) };
            return Err(Refusal::Moved(format!("http://127.0.0.1:{port}{}{query}", request.path)));
        }
        Peer::Loopback if !request.from.loopback_allowed(port) => return Err(Refusal::Forbidden),
        Peer::Network { local, .. } => {
            if !request.from.home_allowed(port, local, name.as_deref()) {
                let host = name.unwrap_or_else(|| local.to_string());
                return Err(Refusal::Elsewhere(format!("{host}:{port}")));
            }
        }
        Peer::Loopback => {}
    }
    if request.path.starts_with(CAST) {
        return devices.granted(&request.path).map(Asker::Cast).ok_or(Refusal::CastEnded);
    }
    if peer == Peer::Loopback {
        return Ok(Asker::Local);
    }
    Ok(match request.cookie.as_deref() {
        Some(token) => devices.holder(token, now).map_or(Asker::Stranger { removed: true }, |d| Asker::Device(d.id)),
        None => Asker::Stranger { removed: false },
    })
}

/// What a request says about where it comes from, lowercased.
struct Sender {
    host: Option<String>,
    site: Option<String>,
    origin: Option<String>,
}

impl Sender {
    /// For a request over loopback, false when another website made it through the person's
    /// browser. Players, curl, and agents send none of these headers but `Host`. Only loopback
    /// names count, since browsers send Sec-Fetch-Site over plain HTTP to those alone. A browser
    /// that sends neither header passes too, so no GET starts work.
    fn loopback_allowed(&self, port: u16) -> bool {
        let loopback = |host: &str| matches!(host, "127.0.0.1" | "localhost" | "::1");
        // A web page that rebinds its own hostname to 127.0.0.1 still names that hostname.
        if self.host.as_deref().is_some_and(|host| !loopback(split_host(host).0)) {
            return false;
        }
        if self.cross_site() {
            return false;
        }
        let own = |origin: &str| {
            origin.strip_prefix("http://").map(split_host).is_some_and(|(host, at)| loopback(host) && at == Some(port))
        };
        self.origin.as_deref().is_none_or(own)
    }

    /// For a request from the home network, true when it names this Mac by its .local name or by
    /// the address it connected to, on `port`, and no other website made it. A web page that
    /// rebinds its own hostname to this Mac's address still names that hostname.
    fn home_allowed(&self, port: u16, local: IpAddr, name: Option<&str>) -> bool {
        let Some(host) = self.host.as_deref() else { return false };
        let (host_name, host_port) = split_host(host);
        let ours = host_name.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical() == local) || name == Some(host_name);
        let same_origin = self.origin.as_deref().is_none_or(|origin| origin.strip_prefix("http://") == Some(host));
        ours && host_port == Some(port) && !self.cross_site() && same_origin
    }

    fn cross_site(&self) -> bool {
        matches!(self.site.as_deref(), Some("cross-site" | "same-site"))
    }
}

/// The name and port of a `Host` header, which brackets an IPv6 address.
fn split_host(host: &str) -> (&str, Option<u16>) {
    let (name, port) = match host.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
        Some((name, rest)) => (name, rest.strip_prefix(':')),
        None => match host.rsplit_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        },
    };
    (name, port.and_then(|port| port.parse().ok()))
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
            from.loopback_allowed(47919)
        };
        // A player, curl, or an agent.
        assert!(allowed("127.0.0.1:47919", None, None));
        assert!(allowed("localhost", None, None));
        assert!(Sender { host: None, site: None, origin: None }.loopback_allowed(47919), "HTTP/1.0 has no Host");
        assert!(allowed("[::1]:47919", None, None));
        // Plain HTTP to a .local name carries no Sec-Fetch-Site, so another website's request to
        // this Mac's own name looks like the Mac's.
        assert!(!allowed("studio-mac.local:47919", Some("same-origin"), Some("http://studio-mac.local:47919")));
        assert!(!allowed("studio-mac.local:47919", None, None));
        assert!(!allowed("127.0.0.1:47919", None, Some("http://studio-mac.local:47919")));
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
    fn a_request_from_the_home_network_must_name_this_mac_and_come_from_its_page() {
        let mac: IpAddr = "192.168.1.23".parse().unwrap();
        let allowed = |host: Option<&str>, site: Option<&str>, origin: Option<&str>| {
            let from =
                Sender { host: host.map(String::from), site: site.map(String::from), origin: origin.map(String::from) };
            from.home_allowed(47919, mac, Some("studio-mac.local"))
        };
        // A phone that opened either address, and a TV that plays a cast.
        assert!(allowed(Some("studio-mac.local:47919"), Some("none"), None));
        assert!(allowed(Some("studio-mac.local:47919"), Some("same-origin"), Some("http://studio-mac.local:47919")));
        assert!(allowed(Some("192.168.1.23:47919"), None, None));
        // A page that rebinds its hostname to this Mac's address.
        assert!(!allowed(Some("rebound.example:47919"), None, None));
        assert!(!allowed(Some("127.0.0.1:47919"), None, None));
        // Another address, another port, or none.
        assert!(!allowed(Some("192.168.1.24:47919"), None, None));
        assert!(!allowed(Some("192.168.1.23:8080"), None, None));
        assert!(!allowed(Some("192.168.1.23"), None, None));
        assert!(!allowed(None, None, None));
        // Another website, or another origin of this Mac.
        assert!(!allowed(Some("studio-mac.local:47919"), Some("cross-site"), None));
        assert!(!allowed(Some("studio-mac.local:47919"), Some("same-site"), None));
        assert!(!allowed(Some("studio-mac.local:47919"), None, Some("http://192.168.1.23:47919")));
        assert!(!allowed(Some("192.168.1.23:47919"), None, Some("http://192.168.1.23:3000")));
        assert!(!allowed(Some("192.168.1.23:47919"), None, Some("null")));
        let v6 = Sender { host: Some("[fd00::23]:47919".into()), site: None, origin: None };
        assert!(v6.home_allowed(47919, "fd00::23".parse().unwrap(), None));
    }

    #[test]
    fn a_new_stranger_closes_the_oldest_of_its_address_or_else_the_oldest_of_all() {
        let strangers = Arc::new(Strangers::default());
        let address = |n: u8| IpAddr::V4(Ipv4Addr::new(192, 168, 1, n));
        let mut open: Vec<_> = (0..64).map(|n| strangers.admit(address(40 + n / 16))).collect();
        let closed = |open: &mut Vec<(Stranger, Closed)>| -> Vec<usize> {
            let closed = open.iter_mut().map(|(_, closed)| closed.try_recv().is_ok());
            closed.enumerate().filter_map(|(n, closed)| closed.then_some(n)).collect()
        };
        assert!(closed(&mut open).is_empty());
        let phone = strangers.admit(address(50));
        assert_eq!(closed(&mut open), [0], "a 65th from another address");
        let more = strangers.admit(address(43));
        assert_eq!(closed(&mut open), [48], "a 17th from one address");
        drop(open.remove(5));
        let tv = strangers.admit(address(51));
        assert!(closed(&mut open).is_empty(), "a connection that shows a cookie makes room");
        drop((phone, more, tv, open));
        assert_eq!(strangers.open().connections.len(), 0, "nothing stays behind");
    }

    #[test]
    fn a_trusted_connection_never_closes_for_strangers_and_one_already_closed_stays_closed() {
        use tokio::sync::oneshot::error::TryRecvError;
        let strangers = Arc::new(Strangers::default());
        let (phone_ip, tv_ip) =
            (IpAddr::V4(Ipv4Addr::new(192, 168, 1, 40)), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 41)));
        let (phone, mut phone_closed) = strangers.admit(phone_ip);
        assert!(phone.trust(), "a cookie shown before any stranger needs the room");
        let crowd: Vec<_> = (0..80).map(|_| strangers.admit(phone_ip)).collect();
        assert_eq!(phone_closed.try_recv(), Err(TryRecvError::Closed), "80 strangers from its address later");
        let (tv, mut tv_closed) = strangers.admit(tv_ip);
        let more: Vec<_> = (0..16).map(|_| strangers.admit(tv_ip)).collect();
        assert!(!tv.trust(), "a grant shown after a 17th stranger from its address took the room");
        assert_eq!(tv_closed.try_recv(), Ok(()));
        drop((crowd, more));
        assert_eq!(strangers.open().connections.len(), 0, "nothing stays behind");
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
