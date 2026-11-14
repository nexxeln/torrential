pub mod admission;
pub mod book;
pub mod cast;
pub mod devices;
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
use std::path::{Path, PathBuf};
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
    /// ffmpeg and ffprobe, installed or fetched.
    pub ffmpeg: web::fetch::Ffmpeg,
    pub subtitles: Arc<dyn subtitle_port::SubtitlePort>,
    pub catalog: Arc<dyn titles::catalog_port::CatalogPort>,
    /// Where searches for TVs go.
    pub tvs: cast::Find,
}

impl Ports {
    fn from_env(env: &Env, paths: &Paths, settings: &Config, runtime: &tokio::runtime::Handle) -> Result<Self, Error> {
        let (config, path) = (paths.clone(), env.var("PATH").map(String::from));
        let preferences: Arc<dyn Fn() -> Preferences + Send + Sync> =
            Arc::new(move || crate::config::preferences(&config).unwrap_or_default());
        let installed = Arc::new(move || web::ffmpeg::Tools::find(path.as_deref()));
        let fetch = web::fetch::Fetch::from_env(env, settings.ffmpeg.fetch);
        let ffmpeg = web::fetch::Ffmpeg::new(installed, web::fetch::root(&paths.state), fetch);
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
        let tvs = match env.var("TORRENTIAL_TEST_SSDP").map(str::parse) {
            Some(Ok(address)) => cast::Find::At(address),
            Some(Err(_)) => return Err(Error::new(ErrorCode::Usage, "TORRENTIAL_TEST_SSDP is not an address")),
            None => cast::Find::Multicast,
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
                ffmpeg,
                subtitles,
                catalog,
                tvs,
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
            ffmpeg,
            subtitles,
            catalog,
            tvs,
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

/// The settings of an engine a test started, the one with the fake engine or on the loopback
/// network, and `None` for any other. Every setting that lets a test reach into what the engine
/// does is read through it.
fn test_engine(env: &Env) -> Option<&Env> {
    (fake(env) || env.var("TORRENTIAL_TEST_NETWORK") == Some("loopback")).then_some(env)
}

/// How long a play leaving the plays folder waits before it goes: no time, unless a test engine
/// asks for up to a minute, to do something in between.
fn deletion_pause(env: &Env) -> Duration {
    test_pause(env, "TORRENTIAL_TEST_DELETION_PAUSE")
}

/// How long a file waits between the check that it is still the file to move to the Trash and
/// the move: no time, unless a test engine asks for up to a minute, to change it in between.
fn trash_pause(env: &Env) -> Duration {
    test_pause(env, "TORRENTIAL_TEST_TRASH_PAUSE")
}

fn test_pause(env: &Env, name: &str) -> Duration {
    let asked = test_engine(env).and_then(|test| test.var(name));
    asked.and_then(torrential_core::units::parse_duration).map_or(Duration::ZERO, |pause| pause.min(MAX_TEST_PAUSE))
}

const MAX_TEST_PAUSE: Duration = Duration::from_secs(60);

/// How long a play no one streams keeps downloading. A test engine may ask for less.
fn plays_idle(env: &Env) -> Duration {
    let asked = test_engine(env).and_then(|test| test.var("TORRENTIAL_TEST_PLAYS_IDLE"));
    asked.and_then(torrential_core::units::parse_duration).unwrap_or(owner::plays::IDLE_AFTER)
}

/// Where the files of a download in a folder the person chose go when they delete them. A test
/// engine never uses the person's Trash: `TORRENTIAL_TEST_TRASH` names its own, or it keeps one
/// beside its state.
fn trash(env: &Env, paths: &Paths) -> owner::trash::Trash {
    if let Some(test) = test_engine(env) {
        let dir = test.var("TORRENTIAL_TEST_TRASH").map_or_else(|| paths.state.join("trash"), PathBuf::from);
        return owner::trash::Trash::At(dir);
    }
    let home = || Path::new(env.var("HOME").unwrap_or("/")).join(".local/share");
    owner::trash::Trash::system(&env.var("XDG_DATA_HOME").map_or_else(home, PathBuf::from))
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
    http_ports: &[u16],
) -> Result<Running, Error> {
    ports.search.restore(store.results(owner::now())?);
    let devices = Arc::new(devices::Devices::open(store.path())?);
    // Taken before the bind, so a change while the engine starts reaches the accept loop.
    let mut reach = devices.reach();
    let players = http::bind(http_ports, *reach.borrow_and_update())
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            let _entered = runtime.enter();
            tokio::net::TcpListener::from_std(listener)
        })
        .map_err(|e| Error::internal(format!("cannot listen for players and the web app: {e}")))?;
    let http_port =
        players.local_addr().map_err(|e| Error::internal(format!("cannot read the player port: {e}")))?.port();
    let shelf = titles::shelf::Shelf::open(store.path())?;
    let (events_tx, events) = mpsc::unbounded_channel();
    let (engine, thread) = engine_port::spawn(ports.engine, events_tx);
    let playing = playing::Playing::new();
    let owner = Owner::new(store, engine.clone(), ports.parse, ports.disk, playing.clone(), config)?;
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let task = runtime.spawn(owner.run(commands, events));
    let ctx = Arc::new_cyclic(|ctx| Ctx {
        owner: OwnerHandle(commands_tx),
        search: ports.search,
        machine: ports.machine,
        engine,
        parse: ports.parse,
        version,
        http_port,
        web: web::State::new(ports.preferences, ports.ffmpeg, ports.subtitles),
        titles: titles::Titles::new(ports.catalog, shelf, timing),
        playing,
        news: update::Board::default(),
        devices,
        casts: cast::Casts::new(ctx.clone(), ports.tvs),
    });
    let http = runtime.spawn(http::accept(ctx.clone(), players, reach));
    Ok(Running { ctx, owner: task, engine: thread, http })
}

pub fn log(message: &str) {
    eprintln!("{} {message}", crate::output::format::timestamp(owner::now()));
}

/// The exit status after which launchd (`KeepAlive` without `SuccessfulExit`) and systemd
/// (`Restart=on-failure`) start the engine again, from the binary an update installed. Any other
/// clean stop exits 0, which neither restarts.
const RESTART: u8 = 75;

/// What an engine that exits for the login service leaves, so `engine stop` waits for the next
/// engine to stop it too. The next engine removes it once it is up.
#[derive(serde::Serialize, serde::Deserialize)]
struct Switching {
    pid: u32,
    at: i64,
}

/// How long a stop waits for the login service to start the next engine.
const SWITCH_WITHIN: i64 = 60;

fn leave_switching(paths: &Paths, at: i64) {
    let switching = Switching { pid: std::process::id(), at };
    let _ = crate::skill::write_atomic(&paths.switching(), &serde_json::to_vec(&switching).expect("records serialize"));
}

/// Whether an engine exited for the login service under a minute before `now`, and no engine has
/// started since.
pub fn switching(paths: &Paths, now: i64) -> bool {
    let Ok(bytes) = std::fs::read(paths.switching()) else { return false };
    serde_json::from_slice::<Switching>(&bytes).is_ok_and(|switching| now - switching.at < SWITCH_WITHIN)
}

/// Runs the engine until `torrential engine stop`, a signal, or, when `service` says launchd or
/// systemd started it, a switch to an installed update.
pub fn run(env: &Env, service: bool) -> u8 {
    let paths = match Paths::from_process(env) {
        Ok(paths) => paths,
        Err(e) => return fail_start(None, &e),
    };
    match serve(env, &paths, service) {
        Ok(Ended::Restart) if service => RESTART,
        Ok(_) => 0,
        // The other engine serves this home, so launchd and systemd have nothing to start again.
        Err(Startup::AlreadyRunning(message)) => {
            log(&message);
            0
        }
        Err(Startup::Failed(e)) => fail_start(Some(&paths), &e),
    }
}

enum Startup {
    AlreadyRunning(String),
    Failed(Error),
}

impl From<Error> for Startup {
    fn from(e: Error) -> Self {
        Self::Failed(e)
    }
}

fn fail_start(paths: Option<&Paths>, error: &Error) -> u8 {
    log(&format!("engine failed to start: {error}"));
    if let Some(paths) = paths {
        let _ = std::fs::write(paths.startup_error(), serde_json::to_vec(error).expect("errors serialize"));
    }
    error.code.exit_code()
}

fn serve(env: &Env, paths: &Paths, service: bool) -> Result<Ended, Startup> {
    let settings = Config::load(paths, env)?;
    let io = |what: &str, e: std::io::Error| Error::new(ErrorCode::StateCorrupt, format!("cannot {what}: {e}"));
    paths.make_state().map_err(|e| io(&format!("create {}", paths.state.display()), e))?;
    paths::check_state_filesystem(&paths.state)?;
    let _lock = match OwnerLock::acquire(&paths.lock())? {
        Some(lock) => lock,
        None => return Err(Startup::AlreadyRunning(format!("another engine owns {}", paths.state.display()))),
    };
    let _ = std::fs::remove_file(paths.startup_error());
    prepare_runtime_dir(&paths.runtime)?;
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
        plays: owner::plays::Plays { root: paths.plays(), max: settings.plays_max.0, idle_after: plays_idle(env) },
        trash: trash(env, paths),
        deletion_pause: deletion_pause(env),
        trash_pause: owner::deleter::TrashPause::sleep(trash_pause(env)),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::internal(format!("cannot start the async runtime: {e}")))?;
    let version = env.version();
    let ports = Ports::from_env(env, paths, &settings, runtime.handle())?;
    let (engine, ended) = runtime.block_on(async {
        let socket = paths.socket();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).map_err(|e| {
            Error::new(ErrorCode::DaemonUnavailable, format!("cannot listen on {}: {e}", socket.display()))
        })?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io(&format!("protect {}", socket.display()), e))?;
        let wanted = env.var("TORRENTIAL_TEST_HTTP_PORT").and_then(|port| port.parse().ok()).unwrap_or(http::PORT);
        // Open pages reconnect to the address they loaded from, so the engine serves where it last did.
        let last = std::fs::read_to_string(paths.http_port()).ok().and_then(|text| text.trim().parse().ok());
        let http_ports: Vec<u16> = last.into_iter().chain([wanted]).collect();
        let running =
            start(&tokio::runtime::Handle::current(), store, ports, config, timing, version.clone(), &http_ports)?;
        // Before the socket answers, so a page that reconnects the moment this engine is up says what was updated.
        running.ctx.news.post(update::opening(env, paths));
        let (pid, players) = (std::process::id(), running.ctx.http_port);
        if last != Some(players) {
            let _ = crate::skill::write_atomic(&paths.http_port(), players.to_string().as_bytes());
        }
        log(&format!(
            "engine {version} started, pid {pid}, socket {}, web app and players http://127.0.0.1:{players}",
            socket.display()
        ));
        let _ = std::fs::remove_file(paths.switching());
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
        let updates = tokio::spawn(update::watch(env.clone(), paths.clone(), running.ctx.clone(), service));
        let ended = running.owner.await.unwrap_or(Ended::Stopped);
        // Before the socket stops answering, so a stop that finds it refusing waits for the next engine.
        if ended == Ended::Restart && service {
            leave_switching(paths, owner::now());
        }
        accept.abort();
        running.http.abort();
        signals.abort();
        updates.abort();
        // An engine that stops for a newer version leaves its socket, so a client following it
        // finds one that refuses for a moment instead of none, and waits for the next engine.
        if ended == Ended::Stopped {
            let _ = std::fs::remove_file(&socket);
        }
        Ok::<_, Error>((running.engine, ended))
    })?;
    drop(runtime);
    let _ = engine.join();
    log("engine stopped");
    Ok(ended)
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
    let (reply, _) = tokio::sync::oneshot::channel();
    owner.tell(owner::Command::Shutdown { reply });
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

pub struct OwnerLock {
    _file: File,
}

impl OwnerLock {
    pub fn acquire(path: &Path) -> Result<Option<Self>, Error> {
        use std::os::fd::AsRawFd;
        let file = OpenOptions::new()
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
        let mut file = file;
        let _ = file.set_len(0).and_then(|()| writeln!(file, "{}", std::process::id()));
        Ok(Some(Self { _file: file }))
    }
}
