pub mod admission;
pub mod book;
pub mod engine_port;
pub mod files;
mod fixture;
pub mod http;
pub mod libtorrent;
pub mod magnet;
pub mod owner;
pub mod playing;
pub mod resolve;
pub mod search_port;
pub mod server;
pub mod store;
pub mod subtitle_port;
pub mod titles;
pub mod web;

#[cfg(test)]
mod tests;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::sync::mpsc;
use torrential_core::{Error, ErrorCode, Preferences};

use crate::config::Config;
use crate::env::Env;
use crate::machine::Detector;
use crate::paths::{self, Paths};
use crate::update;
use admission::Disk;
use engine_port::{EnginePort, ParseTorrent};
use libtorrent::{Libtorrent, Network};
use owner::{Ended, Owner, OwnerConfig};
use search_port::SearchPort;
use server::{Ctx, OwnerHandle};
use store::Store;

const SAVE_RESUME_EVERY: Duration = Duration::from_secs(5 * 60);
/// Longer than libtorrent's 15-minute DHT announce, so a new source gets a round of discovery.
const STALL_AFTER: Duration = Duration::from_secs(20 * 60);
pub const MAX_ACTIVE: usize = 3;
const WRITES_STOP_WITHIN: Duration = Duration::from_secs(10);

pub struct Ports {
    pub engine: Box<dyn EnginePort>,
    pub parse: ParseTorrent,
    pub search: Arc<dyn SearchPort>,
    pub disk: Box<dyn Disk>,
    pub machine: Arc<Detector>,
    /// The preferences in config.yaml now.
    pub preferences: Arc<dyn Fn() -> Preferences + Send + Sync>,
    /// Where ffmpeg and ffprobe are now.
    pub tools: Arc<dyn Fn() -> web::ffmpeg::Tools + Send + Sync>,
    pub subtitles: Arc<dyn subtitle_port::SubtitlePort>,
    pub catalog: Arc<dyn titles::catalog_port::CatalogPort>,
}

impl Ports {
    fn from_env(env: &Env, paths: &Paths, settings: &Config, runtime: &tokio::runtime::Handle) -> Result<Self, Error> {
        let (config, path) = (paths.clone(), env.var("PATH").map(String::from));
        let preferences: Arc<dyn Fn() -> Preferences + Send + Sync> =
            Arc::new(move || crate::config::preferences(&config).unwrap_or_default());
        let tools: Arc<dyn Fn() -> web::ffmpeg::Tools + Send + Sync> =
            Arc::new(move || web::ffmpeg::Tools::find(path.as_deref()));
        // The fake engine's subtitles are made up too, unless a test asks for the real sources.
        let subtitles: Arc<dyn subtitle_port::SubtitlePort> =
            if fake(env) && env.var("TORRENTIAL_TEST_SUBTITLES") != Some("live") {
                Arc::new(subtitle_port::Fake::default())
            } else {
                Arc::new(subtitle_port::Live::new(&settings.subtitles)?)
            };
        let catalog: Arc<dyn titles::catalog_port::CatalogPort> =
            if fake(env) && env.var("TORRENTIAL_TEST_CATALOG") != Some("live") {
                Arc::new(titles::catalog_port::Fake::default())
            } else {
                let config = torrential_search::catalog::CatalogConfig {
                    cache_dir: Some(paths.cache.join("catalog")),
                    ..settings.catalog.clone()
                };
                Arc::new(titles::catalog_port::Live::new(&config)?)
            };
        if fake(env) {
            let rate =
                env.var("TORRENTIAL_TEST_FAKE_RATE").and_then(torrential_core::units::parse_size).unwrap_or(10 << 20);
            let swarm = fixture::swarm(rate);
            // Torrents no peer sends a piece of, so a title play moves past them.
            for id in env.var("TORRENTIAL_TEST_DEAD").unwrap_or_default().split(',').filter_map(|id| id.parse().ok()) {
                swarm.withhold(id, u32::MAX);
            }
            return Ok(Self {
                engine: Box::new(engine_port::Fake::new(swarm)),
                parse: engine_port::parse_fake,
                search: Arc::new(fixture::search(owner::now())),
                disk: Box::new(admission::RoomyDisk),
                machine: Detector::start(env, settings.downloads_dir(paths), Box::new(admission::RoomyDisk)),
                preferences,
                tools,
                subtitles,
                catalog,
            });
        }
        let network = match env.var("TORRENTIAL_TEST_NETWORK") {
            Some("loopback") => Network::Loopback { peers: test_peers(env)? },
            _ => Network::Public,
        };
        let roomy = env.var("TORRENTIAL_TEST_DISK") == Some("roomy");
        let disk = || -> Box<dyn Disk + Sync> {
            if roomy { Box::new(admission::RoomyDisk) } else { Box::new(admission::RealDisk) }
        };
        let search =
            torrential_search::SearchConfig { cache_dir: Some(paths.cache.join("search")), ..settings.search.clone() };
        let search: Arc<dyn SearchPort> = Arc::new(search_port::Live::new(&search)?);
        Ok(Self {
            engine: Box::new(Libtorrent::start(&paths.state, network, search.clone(), runtime.clone())?),
            search,
            parse: libtorrent::parse,
            disk: disk(),
            machine: Detector::start(env, settings.downloads_dir(paths), disk()),
            preferences,
            tools,
            subtitles,
            catalog,
        })
    }
}

fn test_peers(env: &Env) -> Result<Vec<std::net::SocketAddr>, Error> {
    let Some(list) = env.var("TORRENTIAL_TEST_PEERS") else { return Ok(Vec::new()) };
    list.split(',')
        .map(|peer| {
            peer.trim().parse().map_err(|_| {
                Error::new(ErrorCode::Usage, format!("TORRENTIAL_TEST_PEERS has {peer:?}, which is not an address"))
            })
        })
        .collect()
}

fn fake(env: &Env) -> bool {
    env.var("TORRENTIAL_TEST_ENGINE") == Some("fake")
}

pub struct Running {
    pub ctx: Arc<Ctx>,
    pub owner: tokio::task::JoinHandle<Ended>,
    pub engine: std::thread::JoinHandle<()>,
    pub http: tokio::task::JoinHandle<()>,
}

pub fn start(
    runtime: &tokio::runtime::Handle,
    store: Store,
    ports: Ports,
    config: OwnerConfig,
    timing: titles::Timing,
    version: String,
    players: std::net::TcpListener,
) -> Result<Running, Error> {
    ports.search.restore(store.results(owner::now())?);
    let players = players
        .try_clone()
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            let _entered = runtime.enter();
            tokio::net::TcpListener::from_std(listener)
        })
        .map_err(|e| Error::internal(format!("cannot listen for players on 127.0.0.1: {e}")))?;
    let http_port =
        players.local_addr().map_err(|e| Error::internal(format!("cannot read the player port: {e}")))?.port();
    let shelf = titles::shelf::Shelf::open(store.path())?;
    let (events_tx, events) = mpsc::unbounded_channel();
    let (engine, thread) = engine_port::spawn(ports.engine, events_tx);
    let owner = Owner::new(store, engine.clone(), ports.parse, ports.disk, config)?;
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let task = runtime.spawn(owner.run(commands, events));
    let ctx = Arc::new(Ctx {
        owner: OwnerHandle(commands_tx),
        search: ports.search,
        machine: ports.machine,
        engine,
        parse: ports.parse,
        version,
        http_port,
        web: web::State::new(ports.preferences, ports.tools, ports.subtitles),
        titles: titles::Titles::new(ports.catalog, shelf, timing),
        playing: playing::Playing::new(),
        news: update::Board::default(),
    });
    let http = runtime.spawn(http::accept(ctx.clone(), players));
    Ok(Running { ctx, owner: task, engine: thread, http })
}

pub fn log(message: &str) {
    eprintln!("{} {message}", crate::output::format::timestamp(owner::now()));
}

/// Runs the engine until `torrential engine stop`, a signal, or an update, which it restarts into.
pub fn run(env: &Env) -> u8 {
    let paths = match Paths::from_process(env) {
        Ok(paths) => paths,
        Err(e) => return fail_start(None, &e),
    };
    // Linux names a replaced binary "(deleted)", so the path is read before an update replaces it.
    let exe = update::executable();
    let held = match Held::take(env, &paths) {
        Ok(Some(held)) => held,
        Ok(None) => {
            // The other engine serves this home, so launchd and systemd have nothing to start again.
            log(&format!("another engine owns {}", paths.state.display()));
            return 0;
        }
        Err(e) => return fail_start(Some(&paths), &e),
    };
    match serve(env, &paths, &held) {
        Ok(Ended::Stopped) => {
            let _ = std::fs::remove_file(paths.socket());
            0
        }
        Ok(Ended::Restart) => match exe {
            Ok(exe) => {
                log("restarting into the installed torrential");
                exec(&held, &exe)
            }
            Err(e) => fail_start(Some(&paths), &e),
        },
        Err(e) => {
            let _ = std::fs::remove_file(paths.socket());
            fail_start(Some(&paths), &e)
        }
    }
}

fn fail_start(paths: Option<&Paths>, error: &Error) -> u8 {
    log(&format!("engine failed to start: {error}"));
    if let Some(paths) = paths {
        let _ = std::fs::write(paths.startup_error(), serde_json::to_vec(error).expect("errors serialize"));
    }
    error.code.exit_code()
}

/// Replaces this process with the engine in `exe`, handing it the lock and the listeners. Returns
/// only when the exec fails.
fn exec(held: &Held, exe: &Path) -> u8 {
    let mut engine = std::process::Command::new(exe);
    engine.args(["daemon", "run"]);
    held.hand_over(&mut engine);
    let e = engine.exec();
    log(&format!("cannot start {}: {e}", exe.display()));
    ErrorCode::DaemonUnavailable.exit_code()
}

fn serve(env: &Env, paths: &Paths, held: &Held) -> Result<Ended, Error> {
    let settings = Config::load(paths, env)?;
    match crate::client::service::add_at_start(env, paths) {
        Ok(Some(unit)) => log(&format!("added a login service at {}, so the engine starts at login", unit.display())),
        Ok(None) => {}
        Err(e) => log(&format!("cannot add a login service: {}", e.message)),
    }
    let seed = fake(env) && !paths.database().exists();
    let mut store = Store::open(&paths.database())?;
    let default_dir = settings.downloads_dir(paths);
    if seed {
        fixture::seed(&mut store, &default_dir, owner::now())?;
    }
    let save_resume_every = env
        .var("TORRENTIAL_TEST_SAVE_RESUME_EVERY")
        .and_then(torrential_core::units::parse_duration)
        .unwrap_or(SAVE_RESUME_EVERY);
    let stall_after =
        env.var("TORRENTIAL_TEST_STALL_AFTER").and_then(torrential_core::units::parse_duration).unwrap_or(STALL_AFTER);
    let timing = match env.var("TORRENTIAL_TEST_TITLE_WAIT").and_then(torrential_core::units::parse_duration) {
        Some(wait) => titles::Timing { file_list: wait, first_bytes: wait, ..titles::Timing::default() },
        None => titles::Timing::default(),
    };
    let config = OwnerConfig {
        default_dir,
        probe_dir: paths.cache.join("probes"),
        max_active: MAX_ACTIVE,
        idle_timeout: env.var("TORRENTIAL_TEST_IDLE_TIMEOUT").and_then(torrential_core::units::parse_duration),
        save_resume_every,
        stall_after,
        writes_stop_within: WRITES_STOP_WITHIN,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::internal(format!("cannot start the async runtime: {e}")))?;
    let version = env.version();
    let ports = Ports::from_env(env, paths, &settings, runtime.handle())?;
    let (engine, ended) = runtime.block_on(async {
        let socket = paths.socket();
        let listener = held
            .socket
            .try_clone()
            .and_then(|listener| {
                listener.set_nonblocking(true)?;
                UnixListener::from_std(listener)
            })
            .map_err(|e| {
                Error::new(ErrorCode::DaemonUnavailable, format!("cannot listen on {}: {e}", socket.display()))
            })?;
        let http = held.http.try_clone().map_err(|e| Error::internal(format!("cannot listen for players: {e}")))?;
        let running = start(&tokio::runtime::Handle::current(), store, ports, config, timing, version.clone(), http)?;
        let (pid, players) = (std::process::id(), running.ctx.http_port);
        log(&format!(
            "engine {version} started, pid {pid}, socket {}, web app and players http://127.0.0.1:{players}",
            socket.display()
        ));
        let ctx = running.ctx.clone();
        let accept = tokio::spawn(async move {
            let uid = paths::uid();
            while let Ok((stream, _)) = listener.accept().await {
                if stream.peer_cred().is_ok_and(|cred| cred.uid() == uid) {
                    tokio::spawn(server::connection(ctx.clone(), stream));
                }
            }
        });
        let signals = tokio::spawn(stop_on_signal(running.ctx.owner.clone()));
        let updates = tokio::spawn(update::watch(env.clone(), paths.clone(), running.ctx.clone()));
        let ended = running.owner.await.unwrap_or(Ended::Stopped);
        accept.abort();
        running.http.abort();
        signals.abort();
        updates.abort();
        Ok::<_, Error>((running.engine, ended))
    })?;
    drop(runtime);
    let _ = engine.join();
    log("engine stopped");
    Ok(ended)
}

fn shut_down(owner: &OwnerHandle) {
    let (reply, _) = tokio::sync::oneshot::channel();
    owner.tell(owner::Command::Shutdown { reply });
}

async fn stop_on_signal(owner: OwnerHandle) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    log("stopping on a signal");
    shut_down(&owner);
}

fn prepare_runtime_dir(dir: &Path) -> Result<(), Error> {
    let refuse =
        |why: String| Error::new(ErrorCode::DaemonUnavailable, why).with_hint("remove it, or set XDG_RUNTIME_DIR");
    std::fs::create_dir_all(dir).map_err(|e| refuse(format!("cannot create {}: {e}", dir.display())))?;
    let meta = dir.symlink_metadata().map_err(|e| refuse(format!("cannot inspect {}: {e}", dir.display())))?;
    if !meta.is_dir() || meta.uid() != paths::uid() {
        return Err(refuse(format!("{} is not a directory owned by you", dir.display())));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| refuse(format!("cannot protect {}: {e}", dir.display())))
}

/// The lock on the engine's state and its two listeners. A restart into an update hands them to the
/// binary it execs, so no other engine starts meanwhile, and commands and pages that arrive then
/// wait on the same socket and port for the new engine instead of finding none.
struct Held {
    lock: File,
    socket: std::os::unix::net::UnixListener,
    http: std::net::TcpListener,
}

/// Names the descriptors of a `Held` that survive the exec, as `lock,socket,http`.
const HANDOVER: &str = "TORRENTIAL_HANDOVER";

impl Held {
    /// Takes over what the engine that exec'd this one held, or else takes the lock and binds both
    /// listeners. Nothing when another engine holds the lock.
    fn take(env: &Env, paths: &Paths) -> Result<Option<Held>, Error> {
        let io = |what: &str, e: std::io::Error| Error::new(ErrorCode::StateCorrupt, format!("cannot {what}: {e}"));
        std::fs::create_dir_all(&paths.state).map_err(|e| io(&format!("create {}", paths.state.display()), e))?;
        paths::check_state_filesystem(&paths.state)?;
        if let Some(held) = env.var(HANDOVER).and_then(|fds| Held::inherited(fds, paths)) {
            return Ok(Some(held));
        }
        let Some(lock) = lock(&paths.lock())? else { return Ok(None) };
        let _ = std::fs::remove_file(paths.startup_error());
        prepare_runtime_dir(&paths.runtime)?;
        let socket = paths.socket();
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).map_err(|e| {
            Error::new(ErrorCode::DaemonUnavailable, format!("cannot listen on {}: {e}", socket.display()))
        })?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io(&format!("protect {}", socket.display()), e))?;
        let port = env.var("TORRENTIAL_TEST_HTTP_PORT").and_then(|port| port.parse().ok()).unwrap_or(http::PORT);
        let http =
            http::bind(port).map_err(|e| Error::internal(format!("cannot listen for players on 127.0.0.1: {e}")))?;
        Ok(Some(Held { lock, socket: listener, http }))
    }

    /// The descriptors in `fds`, when the first is this state's lock file and the others are sockets.
    // `st_dev` is an i32 on macOS and a u64 on Linux.
    #[allow(clippy::unnecessary_cast)]
    fn inherited(fds: &str, paths: &Paths) -> Option<Held> {
        use std::os::fd::FromRawFd;
        let fds: Vec<i32> = fds.split(',').map(|fd| fd.parse().ok()).collect::<Option<_>>()?;
        let [lock, socket, http] = fds[..] else { return None };
        let lock_file = std::fs::metadata(paths.lock()).ok()?;
        let is = |fd: i32, kind: libc::mode_t| {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            (unsafe { libc::fstat(fd, &mut stat) } == 0 && stat.st_mode & libc::S_IFMT == kind).then_some(stat)
        };
        let ours = is(lock, libc::S_IFREG)
            .is_some_and(|stat| stat.st_dev as u64 == lock_file.dev() && stat.st_ino as u64 == lock_file.ino());
        if !ours || is(socket, libc::S_IFSOCK).is_none() || is(http, libc::S_IFSOCK).is_none() {
            return None;
        }
        for fd in [lock, socket, http] {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        // SAFETY: the engine that exec'd this one opened these and handed them over, and nothing
        // else in this process owns them.
        unsafe {
            Some(Held {
                lock: File::from_raw_fd(lock),
                socket: std::os::unix::net::UnixListener::from_raw_fd(socket),
                http: std::net::TcpListener::from_raw_fd(http),
            })
        }
    }

    /// Keeps the descriptors open across the exec of `engine`, and names them for it.
    fn hand_over(&self, engine: &mut std::process::Command) {
        use std::os::fd::AsRawFd;
        let fds = [self.lock.as_raw_fd(), self.socket.as_raw_fd(), self.http.as_raw_fd()];
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        }
        engine.env(HANDOVER, fds.map(|fd| fd.to_string()).join(","));
    }
}

/// Locks the engine's state for this process, or nothing when another engine holds it.
fn lock(path: &Path) -> Result<Option<File>, Error> {
    use std::os::fd::AsRawFd;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot open {}: {e}", path.display())))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(Error::new(ErrorCode::StateCorrupt, format!("cannot lock {}: {e}", path.display())));
    }
    let _ = file.set_len(0).and_then(|()| writeln!(file, "{}", std::process::id()));
    Ok(Some(file))
}
