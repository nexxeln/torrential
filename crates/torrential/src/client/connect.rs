use std::io::{BufReader, ErrorKind};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use torrential_core::protocol::{Frame, Reply, Request, Response, WIRE_VERSION, read_frame};
use torrential_core::{Error, ErrorCode};

use super::launch::{self, Blocked};
use super::remote::{Remote, Remotes};
use torrential_daemon::daemon::OwnerLock;
use torrential_daemon::env::Env;
use torrential_daemon::paths::Paths;
use torrential_daemon::process;
use torrential_daemon::service;
use torrential_daemon::update::{Audience, Version};
use torrential_daemon::wire::{self, Access, Address, Answer, Client, Stream};

/// How long an engine that refuses connections, as one does while it restarts, has to answer again.
pub const BACK_WITHIN: Duration = Duration::from_secs(30);

/// The engine a connection reaches: this machine's, whose log says what went wrong, or one on
/// another machine that `torrential connect` chose, which this torrential never starts or stops.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Engine {
    Local { port: u16, log: String },
    Remote(Address),
}

impl Engine {
    /// Where the engine serves the web app and this torrential's requests.
    pub fn address(&self) -> Address {
        match self {
            Self::Local { port, .. } => Address::local(*port),
            Self::Remote(address) => address.clone(),
        }
    }

    /// The error for a connection to the engine that failed, ended, or did not answer in time.
    pub fn lost(&self, e: std::io::Error) -> Error {
        match self {
            Self::Local { log, .. } => lost(e, log),
            Self::Remote(address) => unreachable(address, &e),
        }
    }

    /// The error for an answer to a request with no reply in it, such as 401 for a token the engine
    /// does not take.
    fn refused(&self, status: u16) -> Error {
        match self {
            Self::Local { port, log } => Error::new(
                ErrorCode::DaemonUnavailable,
                format!("the engine on 127.0.0.1:{port} refused the request with HTTP {status}"),
            )
            .with_hint(format!("see {log}")),
            Self::Remote(address) if status == 401 => Error::new(
                ErrorCode::NotPaired,
                format!("this machine is no longer paired with the engine at {address}"),
            )
            .with_hint(format!(
                "run torrential devices add on that machine, then torrential connect {address} --code <code>"
            )),
            Self::Remote(address) => Error::new(
                ErrorCode::DaemonUnavailable,
                format!("the engine at {address} refused the request with HTTP {status}"),
            )
            .with_hint(LOCAL_HINT),
        }
    }
}

const LOCAL_HINT: &str = "run torrential connect --local to use this machine's engine instead";

/// Why the engine at `address` on another machine cannot be reached.
fn unreachable(address: &Address, e: &std::io::Error) -> Error {
    let message = match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => format!("the engine at {address} did not respond in time"),
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
            format!("lost the connection to the engine at {address}")
        }
        _ => format!("cannot reach the engine at {address}: {e}"),
    };
    Error::new(ErrorCode::DaemonUnavailable, message)
        .with_hint(format!("check that it runs and that this machine can reach it, or {LOCAL_HINT}"))
}

pub struct Conn {
    pub engine: Engine,
    token: String,
    version: String,
    /// The frames of the reply to the last request sent.
    reply: Option<BufReader<Stream>>,
    pub engine_version: String,
    pub wire: u32,
    pub pid: u32,
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
        reply.get_ref().set_read_timeout(within).map_err(|e| self.engine.lost(e))?;
        read_frame(reply).map_err(|e| self.engine.lost(e))
    }

    fn read<T: DeserializeOwned>(&mut self, within: Option<Duration>) -> Result<T, Error> {
        self.next(within)?.ok_or_else(|| self.engine.lost(ErrorKind::UnexpectedEof.into()))
    }

    pub fn send(&mut self, request: &Request) -> Result<(), Error> {
        self.reply = None;
        let client = Client { version: &self.version, wire: WIRE_VERSION };
        match wire::post(&self.engine.address(), &self.token, client, request, request.within()) {
            Ok(Answer::Frames(reply)) => {
                self.reply = Some(reply);
                Ok(())
            }
            Ok(Answer::Status(status)) => Err(self.engine.refused(status)),
            Err(e) => Err(self.engine.lost(e)),
        }
    }

    /// Sends `request` and hands its reply, and the engine it comes from, to a reader that
    /// follows it.
    pub fn follow(mut self, request: &Request) -> Result<(BufReader<Stream>, Engine), Error> {
        self.send(request)?;
        let reply = self.reply.take().ok_or_else(|| Error::internal("the engine sent no reply"))?;
        Ok((reply, self.engine))
    }
}

/// The error for a connection to this machine's engine that ended or did not answer in time.
/// `log` is the engine's log, which says why.
pub fn lost(e: std::io::Error, log: &str) -> Error {
    let message = match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => "the engine did not respond in time".to_string(),
        _ => format!("lost the connection to the engine: {e}"),
    };
    Error::new(ErrorCode::DaemonUnavailable, message).with_hint(format!("see {log}"))
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
        match wire::post(&access.address(), &access.token, client, &Request::Hello, Request::Hello.within()) {
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
        engine: Engine::Local { port: access.port, log },
        token: access.token,
        version,
        reply: Some(frames),
        engine_version: String::new(),
        wire: 0,
        pid: 0,
    };
    (conn.engine_version, conn.wire, conn.pid) = conn.read(Some(Request::Hello.within())).and_then(reply)?;
    Ok(Some(conn))
}

/// The engine, for a command to use. A client never stops a newer engine, and uses one only when
/// it is newer by its patch number alone: a newer minor release runs code this torrential does not
/// know, which the person restarts this torrential to run too. Every connection a command uses
/// comes from here.
pub fn existing(paths: &Paths, env: &Env) -> Result<Option<Conn>, Error> {
    match running(paths, env)? {
        Some(conn) if Version::newer_minor(&conn.engine_version, &env.version()) => Err(Error::new(
            ErrorCode::DaemonVersion,
            format!("the engine runs the newer torrential {}, so restart this torrential", conn.engine_version),
        )),
        found => Ok(found),
    }
}

/// The engine on another machine that `remote` names. One that does not answer is an error that
/// says so, since this torrential never starts an engine in its place.
pub fn remote(remote: &Remote, env: &Env) -> Result<Conn, Error> {
    let mut conn = Conn {
        engine: Engine::Remote(remote.address.clone()),
        token: remote.token.clone(),
        version: env.version(),
        reply: None,
        engine_version: String::new(),
        wire: 0,
        pid: 0,
    };
    (conn.engine_version, conn.wire, conn.pid) = conn.ask(&Request::Hello)?;
    if conn.wire != WIRE_VERSION {
        return Err(Error::new(
            ErrorCode::DaemonVersion,
            format!(
                "the engine at {} runs torrential {}, which speaks protocol {}, and this torrential speaks {WIRE_VERSION}",
                remote.address, conn.engine_version, conn.wire
            ),
        )
        .with_hint("run torrential update on both machines"));
    }
    Ok(conn)
}

/// The engine a command uses, without starting one: the one on another machine that `torrential
/// connect` chose, or this machine's when it runs.
pub fn found(paths: &Paths, env: &Env) -> Result<Option<Conn>, Error> {
    match Remotes::using(paths)? {
        Some(chosen) => remote(&chosen, env).map(Some),
        None => existing(paths, env),
    }
}

/// Why the engine a command used may not run this torrential, said once after the command.
static MISMATCH: OnceLock<String> = OnceLock::new();

/// The engine a command uses: the one on another machine that `torrential connect` chose, or this
/// machine's, which this starts, or restarts to run this release, as needed.
pub fn ensure(paths: &Paths, env: &Env) -> Result<Conn, Error> {
    if let Some(chosen) = Remotes::using(paths)? {
        return remote(&chosen, env);
    }
    let service = service::starts_another(env, paths);
    let conn = this_or_any(paths, env, service.is_some())?;
    if let Some(mismatch) = mismatch(Some(&conn.engine_version), service.as_deref(), &env.version()) {
        let _ = MISMATCH.set(mismatch);
    }
    Ok(conn)
}

/// This torrential's engine, or the engine that runs when this one cannot take over. An engine
/// newer by its patch stays, and so does one the login service would only start again from
/// another binary.
fn this_or_any(paths: &Paths, env: &Env, service_elsewhere: bool) -> Result<Conn, Error> {
    let Some(conn) = existing(paths, env)? else { return launch::start(paths, env) };
    let stays = conn.engine_version == env.version()
        || Version::newer(&conn.engine_version, &env.version())
        || service_elsewhere;
    if stays && conn.wire == WIRE_VERSION {
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

/// Why an engine of version `engine`, or no engine, may not run this torrential of version `this`,
/// in plain words with the fix: the engine runs another version, or the login service starts
/// `service`, another binary, as an install from another path leaves it.
pub fn mismatch(engine: Option<&str>, service: Option<&str>, this: &str) -> Option<String> {
    let install = "run torrential engine install to have it start this one";
    match (engine.filter(|version| *version != this), service) {
        (None, None) => None,
        (Some(version), Some(exe)) => Some(format!(
            "the engine runs torrential {version} from {exe}, which the login service starts, not this torrential \
             {this}; {install}"
        )),
        (None, Some(exe)) => Some(format!("the login service starts {exe}, not this torrential {this}; {install}")),
        (Some(version), None) if Version::newer(version, this) => Some(format!(
            "the engine runs the newer torrential {version}, not this torrential {this}; run torrential update to \
             bring this one up to date"
        )),
        (Some(version), None) => Some(format!(
            "the engine still runs torrential {version}, not this torrential {this}; it switches once no film \
             plays, or run torrential engine stop"
        )),
    }
}

/// Says, after a command, why the engine it used may not run this torrential.
pub fn notice(audience: Audience) -> Option<String> {
    let mismatch = MISMATCH.get()?;
    Some(match audience {
        Audience::Person => {
            let mut said = mismatch.clone();
            said[..1].make_ascii_uppercase();
            format!("{said}.")
        }
        Audience::Agent => format!("# {mismatch}"),
    })
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
    fn a_mismatch_says_what_runs_and_how_to_run_this_torrential() {
        let old = Some("/opt/old/torrential");
        let said = [
            mismatch(Some("0.7.0"), None, "0.7.0"),
            mismatch(None, None, "0.7.0"),
            mismatch(Some("0.6.1"), old, "0.7.0"),
            mismatch(Some("0.7.0"), old, "0.7.0"),
            mismatch(None, old, "0.7.0"),
            mismatch(Some("0.7.1"), None, "0.7.0"),
            mismatch(Some("0.6.1"), None, "0.7.0"),
        ];
        assert_eq!(
            said.each_ref().map(Option::as_deref),
            [
                None,
                None,
                Some(
                    "the engine runs torrential 0.6.1 from /opt/old/torrential, which the login service starts, \
                     not this torrential 0.7.0; run torrential engine install to have it start this one"
                ),
                Some(
                    "the login service starts /opt/old/torrential, not this torrential 0.7.0; run torrential \
                     engine install to have it start this one"
                ),
                Some(
                    "the login service starts /opt/old/torrential, not this torrential 0.7.0; run torrential \
                     engine install to have it start this one"
                ),
                Some(
                    "the engine runs the newer torrential 0.7.1, not this torrential 0.7.0; run torrential update \
                     to bring this one up to date"
                ),
                Some(
                    "the engine still runs torrential 0.6.1, not this torrential 0.7.0; it switches once no film \
                     plays, or run torrential engine stop"
                ),
            ]
        );
    }

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
