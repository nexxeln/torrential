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
/// How long an engine that refuses connections, as one does while it restarts, has to answer again.
pub const BACK_WITHIN: Duration = Duration::from_secs(30);

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

/// The engine that runs now, whatever its version, for reports and for stops the person asks for.
pub fn running(paths: &Paths, env: &Env) -> Result<Option<Conn>, Error> {
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

/// The engine, for a command to use. A client never uses or stops a newer engine: it runs a release
/// installed after this torrential started, which the person restarts this torrential to run too.
/// Every connection a command uses comes from here.
pub fn existing(paths: &Paths, env: &Env) -> Result<Option<Conn>, Error> {
    match running(paths, env)? {
        Some(conn) if newer(&conn, env) => Err(Error::new(
            ErrorCode::DaemonVersion,
            format!("the engine runs the newer torrential {}, so restart this torrential", conn.engine_version),
        )),
        found => Ok(found),
    }
}

pub fn ensure(paths: &Paths, env: &Env) -> Result<Conn, Error> {
    let Some(conn) = existing(paths, env)? else { return launch::start(paths, env) };
    if conn.engine_version == env.version() && conn.wire == WIRE_VERSION {
        return Ok(conn);
    }
    let speaks = conn.wire == WIRE_VERSION;
    match stop(conn, Stop::Switch) {
        Ok(_) => launch::start(paths, env),
        // The engine keeps its version while a film plays. A client that speaks its protocol uses it.
        Err(staying) if speaks => existing(paths, env)?.ok_or(staying),
        Err(staying) => Err(staying),
    }
}

fn newer(conn: &Conn, env: &Env) -> bool {
    Version::parse(&conn.engine_version).is_some_and(|engine| engine.newer_than(&env.version()))
}

/// Why a client stops the engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// The person asked.
    Asked,
    /// The engine is older than this client, which starts its own version once the engine stops.
    /// The engine stays while a film plays.
    Switch,
}

/// Asks the engine to stop and waits until it exits, or returns why the engine stays.
pub fn stop(mut conn: Conn, why: Stop) -> Result<u32, Error> {
    let pid = conn.pid;
    let request = Request::Shutdown { asked: why == Stop::Asked };
    if let Ok(Response::Error { error }) = conn.call_within(&request, Duration::from_secs(20)) {
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        if Instant::now() > deadline {
            return Err(Error::new(ErrorCode::DaemonUnavailable, format!("the engine (pid {pid}) did not exit"))
                .with_hint(format!("kill {pid}, then run the command again")));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(pid)
}

/// Stops the engine as the person asks, and returns the process ID of the last engine it stopped.
/// An engine that switches to an update keeps its socket, which refuses connections until the login
/// service starts the new engine, so this waits across that gap and stops the new one too.
pub fn stop_asked(paths: &Paths, env: &Env) -> Result<Option<u32>, Error> {
    let deadline = Instant::now() + BACK_WITHIN;
    let mut stopped = None;
    loop {
        if let Some(conn) = running(paths, env)? {
            stopped = Some(stop(conn, Stop::Asked)?);
        }
        if !paths.socket().exists() || Instant::now() > deadline {
            return Ok(stopped);
        }
        std::thread::sleep(Duration::from_millis(100));
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
