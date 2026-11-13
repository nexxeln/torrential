use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use torrential_core::protocol::{Request, Response, WIRE_VERSION};
use torrential_core::{Error, ErrorCode};

use super::launch::{self, Blocked};
use crate::env::Env;
use crate::paths::Paths;
use crate::update::Version;
use crate::wire;

pub const DEFAULT_REPLY_WITHIN: Duration = Duration::from_secs(30);

pub struct Conn {
    stream: UnixStream,
    pub engine_version: String,
    pub wire: u32,
    pub pid: u32,
    /// Where the engine serves the web app, and downloads to players.
    pub http_port: u16,
    log: String,
}

impl Conn {
    pub fn call(&mut self, request: &Request) -> Result<Response, Error> {
        self.call_within(request, DEFAULT_REPLY_WITHIN)
    }

    pub fn call_within(&mut self, request: &Request, within: Duration) -> Result<Response, Error> {
        self.send(request)?;
        self.read_within(within)
    }

    pub fn read_within<T: DeserializeOwned>(&mut self, within: Duration) -> Result<T, Error> {
        self.stream.set_read_timeout(Some(within)).map_err(|e| self.lost(e))?;
        let frame = wire::read(&mut self.stream).map_err(|e| self.lost(e))?;
        frame.ok_or_else(|| self.lost(std::io::Error::from(ErrorKind::UnexpectedEof)))
    }

    pub fn send(&mut self, request: &Request) -> Result<(), Error> {
        wire::write(&mut self.stream, request).map_err(|e| self.lost(e))
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }

    fn lost(&self, e: std::io::Error) -> Error {
        let message = match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => "the engine did not respond in time".to_string(),
            _ => format!("lost the connection to the engine: {e}"),
        };
        Error::new(ErrorCode::DaemonUnavailable, message).with_hint(format!("see {}", self.log))
    }
}

pub fn existing(paths: &Paths, env: &Env) -> Result<Option<Conn>, Error> {
    let socket = paths.socket();
    let stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => {
            return Err(launch::sandbox_error(Blocked::Ipc { socket }, env));
        }
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => return Ok(None),
        Err(e) => {
            return Err(Error::new(
                ErrorCode::DaemonUnavailable,
                format!("cannot reach the engine at {}: {e}", socket.display()),
            ));
        }
    };
    let mut conn = Conn {
        stream,
        engine_version: String::new(),
        wire: 0,
        pid: 0,
        http_port: 0,
        log: paths.log().display().to_string(),
    };
    match conn
        .call_within(&Request::Hello { client_version: env.version(), wire: WIRE_VERSION }, Duration::from_secs(5))?
    {
        Response::Hello { engine_version, wire, pid, http_port } => {
            conn.engine_version = engine_version;
            conn.wire = wire;
            conn.pid = pid;
            conn.http_port = http_port;
            Ok(Some(conn))
        }
        Response::Error { error } => Err(error),
        other => Err(unexpected(&other)),
    }
}

pub fn ensure(paths: &Paths, env: &Env) -> Result<Conn, Error> {
    let usable = |conn: &Conn| conn.wire == WIRE_VERSION && (conn.engine_version == env.version() || newer(conn, env));
    if let Some(conn) = existing(paths, env)? {
        if usable(&conn) {
            return Ok(conn);
        }
        // A client never stops a newer engine. It would be one from before an update, stopping the
        // engine that update restarted.
        if newer(&conn, env) {
            return Err(Error::new(
                ErrorCode::DaemonVersion,
                format!("the engine runs the newer torrential {}, so restart this torrential", conn.engine_version),
            ));
        }
        stop(conn, paths, env, Stop::Replace)?;
        // The engine may have been restarting into the update that made this command's version.
        if let Some(conn) = existing(paths, env)?.filter(usable) {
            return Ok(conn);
        }
    }
    launch::start(paths, env)
}

fn newer(conn: &Conn, env: &Env) -> bool {
    matches!(
        (Version::parse(&conn.engine_version), Version::parse(&env.version())),
        (Some(engine), Some(client)) if engine > client
    )
}

/// Why a client stops the engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// The person asked, so the engine stops even when an update has just restarted it.
    Asked,
    /// The engine is older than this client, which starts its own. An engine that restarted into an
    /// update meanwhile is what the client wanted, so it stays.
    Replace,
}

/// Asks the engine to stop and waits until it exits. An engine restarting into an update keeps its
/// process ID, so another version answering from that ID is the update.
pub fn stop(mut conn: Conn, paths: &Paths, env: &Env, why: Stop) -> Result<u32, Error> {
    let pid = conn.pid;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let version = conn.engine_version.clone();
        let _ = conn.call_within(&Request::Shutdown { asked: why == Stop::Asked }, Duration::from_secs(20));
        drop(conn);
        conn = loop {
            if !alive(pid) {
                return Ok(pid);
            }
            if Instant::now() > deadline {
                return Err(Error::new(ErrorCode::DaemonUnavailable, format!("the engine (pid {pid}) did not exit"))
                    .with_hint(format!("kill {pid}, then run the command again")));
            }
            if let Ok(Some(next)) = existing(paths, env)
                && next.pid == pid
                && next.engine_version != version
            {
                break next;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if why == Stop::Replace {
            return Ok(pid);
        }
    }
}

pub fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

pub fn unexpected(response: &Response) -> Error {
    Error::internal(format!(
        "the engine sent an unexpected reply: {}",
        serde_json::to_string(response).unwrap_or_default()
    ))
}
