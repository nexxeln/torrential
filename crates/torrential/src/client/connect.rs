use std::io::{BufReader, ErrorKind};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use torrential_core::protocol::{Frame, Reply, Request, Response, WIRE_VERSION, read_frame};
use torrential_core::{Error, ErrorCode};

use super::launch::{self, Blocked};
use torrential_daemon::daemon::OwnerLock;
use torrential_daemon::env::Env;
use torrential_daemon::paths::Paths;
use torrential_daemon::process;
use torrential_daemon::update::Version;
use torrential_daemon::wire::{self, Access, Answer, Client};

/// How long an engine that refuses connections, as one does while it restarts, has to answer again.
pub const BACK_WITHIN: Duration = Duration::from_secs(30);

pub struct Conn {
    access: Access,
    version: String,
    /// The frames of the reply to the last request sent.
    reply: Option<BufReader<TcpStream>>,
    pub engine_version: String,
    pub wire: u32,
    pub pid: u32,
    /// Where the engine serves the web app, and downloads to players.
    pub http_port: u16,
    log: String,
}

impl Conn {
    /// Sends `request` and reads its reply, which carries an `R`, within the request's limit.
    pub fn ask<R: Reply>(&mut self, request: &Request) -> Result<R, Error> {
        self.send(request)?;
        reply(self.read(Some(request.within()))?)
    }

    /// Sends `request`, hands each update the engine sends to `on_update`, and returns the reply
    /// that ends the stream. Each frame comes within the request's limit.
    pub fn stream<U: DeserializeOwned, R: Reply>(
        &mut self,
        request: &Request,
        mut on_update: impl FnMut(U),
    ) -> Result<R, Error> {
        self.send(request)?;
        loop {
            match self.read(Some(request.within()))? {
                Frame::Update(update) => on_update(update),
                Frame::Reply(response) => return reply(*response),
            }
        }
    }

    /// Reads the next frame of the reply, waiting at most `within`, or with no limit. `None` when
    /// the engine closed the connection between frames.
    pub fn next<T: DeserializeOwned>(&mut self, within: Option<Duration>) -> Result<Option<T>, Error> {
        let Some(reply) = &mut self.reply else { return Err(Error::internal("no request was sent to the engine")) };
        reply.get_ref().set_read_timeout(within).map_err(|e| lost(e, &self.log))?;
        read_frame(reply).map_err(|e| lost(e, &self.log))
    }

    fn read<T: DeserializeOwned>(&mut self, within: Option<Duration>) -> Result<T, Error> {
        self.next(within)?.ok_or_else(|| lost(ErrorKind::UnexpectedEof.into(), &self.log))
    }

    pub fn send(&mut self, request: &Request) -> Result<(), Error> {
        self.reply = None;
        let client = Client { version: &self.version, wire: WIRE_VERSION };
        match wire::post(&self.access, client, request, request.within()) {
            Ok(Answer::Frames(reply)) => {
                self.reply = Some(reply);
                Ok(())
            }
            Ok(Answer::Status(status)) => Err(refused(status, &self.access, &self.log)),
            Err(e) => Err(lost(e, &self.log)),
        }
    }

    /// Sends `request` and hands its reply, and the engine's log, to a reader that follows it.
    pub fn follow(mut self, request: &Request) -> Result<(BufReader<TcpStream>, String), Error> {
        self.send(request)?;
        let reply = self.reply.take().ok_or_else(|| Error::internal("the engine sent no reply"))?;
        Ok((reply, self.log))
    }
}

/// The error for a connection to the engine that ended or did not answer in time. `log` is the
/// engine's log, which says why.
pub fn lost(e: std::io::Error, log: &str) -> Error {
    let message = match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => "the engine did not respond in time".to_string(),
        _ => format!("lost the connection to the engine: {e}"),
    };
    Error::new(ErrorCode::DaemonUnavailable, message).with_hint(format!("see {log}"))
}

/// The error for an answer to a request with no reply in it, such as 401 for a token the engine
/// does not take.
fn refused(status: u16, access: &Access, log: &str) -> Error {
    Error::new(
        ErrorCode::DaemonUnavailable,
        format!("the engine on 127.0.0.1:{} refused the request with HTTP {status}", access.port),
    )
    .with_hint(format!("see {log}"))
}

/// What `response` carries, the engine's error, or an error for a reply of another kind.
pub fn reply<R: Reply>(response: Response) -> Result<R, Error> {
    match response {
        Response::Error { error } => Err(error),
        other => R::from_response(other).map_err(|other| unexpected(&other)),
    }
}

/// The engine that runs now, whatever its version, for reports and for stops the person asks for.
/// It is the one at the port in the token file, which the engine writes before it answers there.
pub fn running(paths: &Paths, env: &Env) -> Result<Option<Conn>, Error> {
    let file = paths.engine_token();
    let read = || {
        Access::read(&file)
            .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot read {}: {e}", file.display())))
    };
    let (version, log) = (env.version(), paths.log().display().to_string());
    let Some(mut access) = read()? else { return Ok(None) };
    let frames = loop {
        let client = Client { version: &version, wire: WIRE_VERSION };
        match wire::post(&access, client, &Request::Hello, Request::Hello.within()) {
            Ok(Answer::Frames(frames)) => break frames,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                return Err(launch::sandbox_error(Blocked::Loopback { port: access.port }, env));
            }
            // A port that takes the request and never answers, as a stuck engine's does.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Err(lost(e, &log)),
            // Not the engine that wrote the file: none listens at its port, or another program does.
            Ok(Answer::Status(_)) | Err(_) => {}
        }
        match read()? {
            // An engine that started since took the port, and wrote its own token first.
            Some(newer) if newer != access => access = newer,
            _ => return Ok(None),
        }
    };
    let mut conn = Conn {
        http_port: access.port,
        access,
        version,
        reply: Some(frames),
        engine_version: String::new(),
        wire: 0,
        pid: 0,
        log,
    };
    (conn.engine_version, conn.wire, conn.pid) = conn.read(Some(Request::Hello.within())).and_then(reply)?;
    Ok(Some(conn))
}

/// The engine, for a command to use. A client never uses or stops a newer engine: it runs a release
/// installed after this torrential started, which the person restarts this torrential to run too.
/// Every connection a command uses comes from here.
pub fn existing(paths: &Paths, env: &Env) -> Result<Option<Conn>, Error> {
    match running(paths, env)? {
        Some(conn) if Version::newer(&conn.engine_version, &env.version()) => Err(Error::new(
            ErrorCode::DaemonVersion,
            format!("the engine runs the newer torrential {}, so restart this torrential", conn.engine_version),
        )),
        found => Ok(found),
    }
}

pub fn ensure(paths: &Paths, env: &Env) -> Result<Conn, Error> {
    let Some(conn) = existing(paths, env)? else { return start(paths, env) };
    if conn.engine_version == env.version() && conn.wire == WIRE_VERSION {
        return Ok(conn);
    }
    let speaks = conn.wire == WIRE_VERSION;
    match stop(conn, Stop::Switch) {
        Ok(_) => replace(paths, env),
        // The engine keeps its version while a film plays. A client that speaks its protocol uses it.
        Err(staying) if speaks => existing(paths, env)?.ok_or(staying),
        Err(staying) => Err(staying),
    }
}

/// Starts this torrential's engine, once a torrential 0.5 engine that the update to this release
/// left running has made way.
// Remove in 0.7, with `bridge05`.
fn start(paths: &Paths, env: &Env) -> Result<Conn, Error> {
    #[cfg(unix)]
    if let Some(pid) = torrential_daemon::daemon::bridge05::make_way(paths, &env.version())? {
        exits(pid, Request::Shutdown { asked: false }.within())?;
        return replace(paths, env);
    }
    launch::start(paths, env)
}

/// This torrential's engine, in place of an older one that just exited. When the login service
/// started the older engine, it starts this release once that engine exits, so this waits for that
/// engine instead of starting one, which would leave the service's engine without the state folder.
fn replace(paths: &Paths, env: &Env) -> Result<Conn, Error> {
    if torrential_daemon::service::switching(paths, torrential_core::time::now())
        && let Some(conn) = poll(BACK_WITHIN, || existing(paths, env))?
    {
        return Ok(conn);
    }
    launch::start(paths, env)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    Asked,
    /// The engine is older than this client, which starts its own version once the engine stops.
    /// The engine stays while a film plays.
    Switch,
}

/// Asks the engine to stop and waits until it exits, or returns why the engine stays.
pub fn stop(mut conn: Conn, why: Stop) -> Result<u32, Error> {
    let pid = conn.pid;
    let request = Request::Shutdown { asked: why == Stop::Asked };
    let answered = conn.send(&request).and_then(|()| conn.read::<Response>(Some(request.within())));
    if let Ok(Response::Error { error }) = answered {
        return Err(error);
    }
    exits(pid, request.within())
}

/// Waits up to `within` for the engine `pid` to exit.
fn exits(pid: u32, within: Duration) -> Result<u32, Error> {
    let exited = poll(within, || Ok((!process::alive(pid)).then_some(())))?;
    exited.map(|()| pid).ok_or_else(|| {
        Error::new(ErrorCode::DaemonUnavailable, format!("the engine (pid {pid}) did not exit"))
            .with_hint(format!("kill {pid}, then run the command again"))
    })
}

/// Stops the engine as the person asks, and returns the process ID of the last engine it stopped.
/// An engine that exits for the login service to start an update says so in its state folder, so
/// this waits across the gap until the new engine starts and stops that one too. An engine that
/// holds the state folder but does not answer, such as one from a torrential before 0.6, gets a
/// SIGTERM, or Ctrl-C on its console on Windows, which it takes as a stop.
pub fn stop_asked(paths: &Paths, env: &Env) -> Result<Option<u32>, Error> {
    let deadline = Instant::now() + BACK_WITHIN;
    let mut stopped = None;
    loop {
        let failed = match running(paths, env) {
            Ok(found) => {
                let switching = torrential_daemon::service::switching(paths, torrential_core::time::now());
                if let Some(conn) = found {
                    stopped = Some(stop(conn, Stop::Asked)?);
                } else if !switching && let Some(pid) = OwnerLock::holder(&paths.lock()) {
                    process::interrupt(pid);
                    stopped = Some(exits(pid, Request::Shutdown { asked: true }.within())?);
                }
                if !switching {
                    return Ok(stopped);
                }
                None
            }
            // An engine that switches can take the connection and exit before it answers.
            Err(e) if e.code == ErrorCode::DaemonUnavailable => Some(e),
            Err(e) => return Err(e),
        };
        if Instant::now() > deadline {
            return failed.map_or(Ok(stopped), Err);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Asks `ready` until it answers, fails, or `within` passes, often at first and less often later.
pub fn poll<T>(within: Duration, mut ready: impl FnMut() -> Result<Option<T>, Error>) -> Result<Option<T>, Error> {
    let started = Instant::now();
    loop {
        if let Some(found) = ready()? {
            return Ok(Some(found));
        }
        let waited = started.elapsed();
        if waited > within {
            return Ok(None);
        }
        std::thread::sleep(pause(waited));
    }
}

fn pause(waited: Duration) -> Duration {
    (waited / 10).clamp(Duration::from_millis(2), Duration::from_millis(20))
}

pub fn unexpected(response: &Response) -> Error {
    Error::internal(format!(
        "the engine sent an unexpected reply: {}",
        serde_json::to_string(response).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_for_the_engine_ends_when_it_answers_fails_or_runs_out() {
        let mut asked = 0;
        let answered = poll(Duration::from_secs(10), || {
            asked += 1;
            Ok((asked == 3).then_some("conn"))
        });
        let failed = poll(Duration::from_secs(10), || Err::<Option<()>, _>(Error::internal("startup failed")));
        let started = Instant::now();
        let mut idle_asks = 0;
        let ran_out = poll(Duration::from_millis(300), || {
            idle_asks += 1;
            Ok(None::<()>)
        });

        assert_eq!((answered.unwrap(), asked), (Some("conn"), 3));
        assert_eq!(failed.unwrap_err().message, "startup failed");
        assert_eq!(ran_out.unwrap(), None);
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(idle_asks < 60, "asked {idle_asks} times in 300 ms");
    }
}
