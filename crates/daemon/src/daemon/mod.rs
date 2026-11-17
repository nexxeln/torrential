pub mod admission;
pub(crate) mod book;
pub(crate) mod cast;
pub(crate) mod ctx;
pub(crate) mod db;
mod detector;
pub mod devices;
pub(crate) mod downloads;
pub(crate) mod engine_port;
pub mod fake;
pub mod files;
pub(crate) mod http;
pub(crate) mod libtorrent;
pub(crate) mod magnet;
pub(crate) mod media;
pub(crate) mod metainfo;
pub(crate) mod names;
pub(crate) mod owner;
pub(crate) mod playing;
pub mod resolve;
pub(crate) mod search_port;
pub(crate) mod server;
pub(crate) mod shelf;
pub(crate) mod store;
pub(crate) mod subtitle_port;
pub(crate) mod switch;
pub(crate) mod titles;
pub(crate) mod web;

#[cfg(test)]
mod tests;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use torrential_core::protocol::WRITES_STOP_WITHIN;
use torrential_core::{Error, ErrorCode, Preferences};

use crate::config::Config;
use crate::disk::Disk;
use crate::env::{Env, TestHooks, hooks};
use crate::fsutil;
use crate::log::{log, persist};
use crate::paths::{self, Paths};
use crate::service;
use crate::wire::Access;
use ctx::{Ctx, OwnerHandle};
use db::Db;
use detector::Detector;
use engine_port::Engine;
use libtorrent::{Libtorrent, Network};
use metainfo::ParseTorrent;
use owner::{Ended, Owner, OwnerConfig};
use search_port::SearchPort;
use store::Store;

const SAVE_RESUME_EVERY: Duration = Duration::from_secs(5 * 60);
/// Longer than libtorrent's 15-minute DHT announce, so a new source gets a round of discovery.
const STALL_AFTER: Duration = Duration::from_secs(20 * 60);
pub const MAX_ACTIVE: usize = 3;

/// A future the daemon's ports return, boxed so a trait object can name it.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub struct Ports {
    pub engine: Box<dyn Engine>,
    /// Looks up web seeds for the torrents the engine downloads.
    pub web_seeds: torrential_search::WebSeeds,
    pub parse: ParseTorrent,
    pub search: Arc<dyn SearchPort>,
    pub disk: Box<dyn Disk>,
    pub machine: Arc<Detector>,
    /// The preferences in config.yaml now.
    pub preferences: Arc<dyn Fn() -> Preferences + Send + Sync>,
    /// ffmpeg and ffprobe, installed or fetched.
    pub ffmpeg: crate::ffmpeg::fetch::Ffmpeg,
    pub subtitles: Arc<dyn subtitle_port::SubtitlePort>,
    pub catalog: Arc<dyn titles::catalog_port::CatalogPort>,
    /// Where searches for TVs go.
    pub tvs: cast::Find,
}

impl Ports {
    fn from_env(env: &Env, paths: &Paths, settings: &Config) -> Result<Self, Error> {
        let (config, path) = (paths.clone(), env.var("PATH").map(String::from));
        let preferences: Arc<dyn Fn() -> Preferences + Send + Sync> = Arc::new(move || {
            let read = crate::config::preferences(&config);
            read.inspect_err(|e| log(&format!("using the default preferences, for {}", e.message))).unwrap_or_default()
        });
        let installed = Arc::new(move || crate::ffmpeg::Tools::find(path.as_deref()));
        let fetch = crate::ffmpeg::fetch::Fetch::from_env(env, settings.ffmpeg.fetch);
        let ffmpeg = crate::ffmpeg::fetch::Ffmpeg::new(installed, crate::ffmpeg::fetch::root(&paths.state), fetch);
        let test = env.test();
        // The fake engine's subtitles and catalog are made up too, unless a test asks for the real ones.
        let fake = |real: &str| test.is_some_and(|test| test.fake_engine() && !test.live(real));
        let subtitles: Arc<dyn subtitle_port::SubtitlePort> = if fake(hooks::SUBTITLES) {
            Arc::new(subtitle_port::Fake::default())
        } else {
            Arc::new(torrential_search::subtitles::Online::new(&settings.subtitles)?)
        };
        let catalog: Arc<dyn titles::catalog_port::CatalogPort> = if fake(hooks::CATALOG) {
            Arc::new(titles::catalog_port::Fake::default())
        } else {
            let config = torrential_search::catalog::CatalogConfig {
                cache_dir: Some(paths.cache.join("catalog")),
                ..settings.catalog.clone()
            };
            Arc::new(titles::catalog_port::Live::new(&config)?)
        };
        let tvs = match test.map(|test| test.ssdp()).transpose()?.flatten() {
            Some(address) => cast::Find::At(address),
            None => cast::Find::Multicast,
        };
        if let Some(test) = test.filter(TestHooks::fake_engine) {
            let swarm = fake::fixture::swarm(test.fake_rate());
            // Torrents no peer sends a piece of, so a title play moves past them.
            for id in test.dead() {
                swarm.withhold(id, u32::MAX);
            }
            return Ok(Self {
                engine: Box::new(fake::Engine::new(swarm)),
                // The fake engine's torrents list no web seeds, so nothing is looked up.
                web_seeds: torrential_search::WebSeeds::new(&torrential_search::SearchConfig::default())?,
                parse: fake::parse,
                search: Arc::new(fake::fixture::search(torrential_core::time::now())),
                disk: Box::new(crate::disk::RoomyDisk),
                machine: Detector::start(env, settings.downloads_dir(paths), Arc::new(crate::disk::RoomyDisk)),
                preferences,
                ffmpeg,
                subtitles,
                catalog,
                tvs,
            });
        }
        let network = match test.map(|test| test.loopback_peers()).transpose()?.flatten() {
            Some(peers) => Network::Loopback { peers },
            None => Network::Public,
        };
        let roomy = test.is_some_and(|test| test.roomy_disk());
        let disk = || -> Box<dyn Disk> {
            if roomy { Box::new(crate::disk::RoomyDisk) } else { Box::new(crate::disk::RealDisk) }
        };
        let search =
            torrential_search::SearchConfig { cache_dir: Some(paths.cache.join("search")), ..settings.search.clone() };
        let search = torrential_search::Search::new(&search, torrential_search::ResultCache::new())?;
        // The engine's web-seed lookups share the search's, so servers asked for with a `.torrent` are not asked again.
        let web_seeds = search.web_seeds();
        let search: Arc<dyn SearchPort> = Arc::new(search);
        Ok(Self {
            engine: Box::new(Libtorrent::start(&paths.state, network, settings.torrent_interface.clone())?),
            web_seeds,
            search,
            parse: libtorrent::parse,
            disk: disk(),
            machine: Detector::start(env, settings.downloads_dir(paths), Arc::from(disk())),
            preferences,
            ffmpeg,
            subtitles,
            catalog,
            tvs,
        })
    }
}

/// How long a play no one streams keeps downloading. A test engine may ask for less.
fn plays_idle(test: Option<TestHooks>) -> Duration {
    test.and_then(|test| test.duration(hooks::PLAYS_IDLE)).unwrap_or(owner::plays::IDLE_AFTER)
}

/// Where the files of a download in a folder the person chose go when they delete them. A test
/// engine never uses the person's Trash: `TRASH` names its own, or it keeps one beside its state.
fn trash(env: &Env, paths: &Paths) -> owner::trash::Trash {
    if let Some(test) = env.test() {
        let dir = test.path(hooks::TRASH).map_or_else(|| paths.state.join("trash"), Path::to_path_buf);
        return owner::trash::Trash::At(dir);
    }
    let home = || env.home().unwrap_or(Path::new("/")).join(".local/share");
    owner::trash::Trash::system(&env.var("XDG_DATA_HOME").map_or_else(home, PathBuf::from))
}

pub struct Running {
    pub ctx: Arc<Ctx>,
    pub owner: tokio::task::JoinHandle<Ended>,
    pub engine: std::thread::JoinHandle<()>,
    pub http: tokio::task::JoinHandle<()>,
}

/// Where the engine listens: the first of `ports` that is free, else any. It writes the port it
/// takes and the token of this user's torrential to `access` before it answers anything there.
pub struct Listen<'a> {
    pub ports: &'a [u16],
    pub access: &'a Path,
    /// On the network from the start, with nothing paired, as `server: true` in config.yaml asks.
    pub server: bool,
}

pub fn start(
    runtime: &tokio::runtime::Handle,
    db: &Db,
    ports: Ports,
    config: OwnerConfig,
    timing: titles::Timing,
    version: String,
    listen: Listen,
) -> Result<Running, Error> {
    let store = Store::open(db)?;
    ports.search.restore(store.results(torrential_core::time::now())?);
    let devices = Arc::new(devices::Devices::open(db, listen.server)?);
    // Taken before the bind, so a change while the engine starts reaches the accept loop.
    let mut reach = devices.reach();
    let players = http::bind(listen.ports, *reach.borrow_and_update())
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            let _entered = runtime.enter();
            tokio::net::TcpListener::from_std(listener)
        })
        .map_err(|e| Error::internal(format!("cannot listen for players and the web app: {e}")))?;
    let http_port =
        players.local_addr().map_err(|e| Error::internal(format!("cannot read the player port: {e}")))?.port();
    Access { port: http_port, token: devices.local_token() }
        .write(listen.access)
        .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot write {}: {e}", listen.access.display())))?;
    let shelf = Arc::new(shelf::Shelf::open(db)?);
    let (events_tx, events) = mpsc::unbounded_channel();
    let (engine, thread) = engine_port::spawn(ports.engine, events_tx);
    let playing = playing::Playing::new();
    let owner = Owner::new(
        store,
        shelf.clone(),
        engine.clone(),
        ports.web_seeds,
        ports.parse,
        Arc::from(ports.disk),
        playing.clone(),
        config,
    )?;
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let task = runtime.spawn(owner.run(commands, events));
    let ctx = Arc::new_cyclic(|ctx| Ctx {
        owner: OwnerHandle(commands_tx),
        search: ports.search,
        machine: ports.machine,
        engine,
        parse: ports.parse,
        version,
        preferences: ports.preferences,
        http_port,
        web: web::State::new(ports.subtitles),
        media: media::State::new(ports.ffmpeg),
        titles: titles::Titles::new(ports.catalog, shelf, timing),
        playing,
        news: switch::Board::default(),
        devices,
        casts: cast::Casts::new(ctx.clone(), ports.tvs),
    });
    let http = runtime.spawn(http::accept(ctx.clone(), players, reach));
    Ok(Running { ctx, owner: task, engine: thread, http })
}

/// Runs the engine until `torrential engine stop`, a signal, or, when `service` says launchd,
/// systemd or Task Scheduler started it, a switch to an installed update.
pub fn run(env: &Env, service: bool, http_port: Option<u16>) -> u8 {
    let paths = match Paths::from_process(env) {
        Ok(paths) => paths,
        Err(e) => return fail_start(None, &e),
    };
    match serve(env, &paths, service, http_port) {
        Ok(Ended::Restart) if service => {
            service::start_next(env, &paths);
            service::RESTART
        }
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
        paths.record_startup_error(error);
    }
    crate::exit::code(error.code)
}

fn serve(env: &Env, paths: &Paths, service: bool, http_port: Option<u16>) -> Result<Ended, Startup> {
    let settings = Config::load(paths, env)?;
    let io = |what: &str, e: std::io::Error| Error::new(ErrorCode::StateCorrupt, format!("cannot {what}: {e}"));
    paths.make_state().map_err(|e| io(&format!("create {}", paths.state.display()), e))?;
    paths::check_state_filesystem(&paths.state)?;
    let _lock = match OwnerLock::acquire(&paths.lock())? {
        Some(lock) => lock,
        None => return Err(Startup::AlreadyRunning(format!("another engine owns {}", paths.state.display()))),
    };
    persist(&format!("remove {}", paths.startup_error().display()), fsutil::remove(&paths.startup_error()));
    #[cfg(windows)]
    if let Ok(exe) = env.installed() {
        crate::update::release::remove_set_aside(&exe);
    }
    prepare_runtime_dir(&paths.runtime)?;
    match service::add_at_start(env, paths) {
        Ok(Some(unit)) => log(&format!("added a login service at {}, so the engine starts at login", unit.display())),
        Ok(None) => {}
        Err(e) => log(&format!("cannot add a login service: {}", e.message)),
    }
    let test = env.test();
    let seed = test.is_some_and(|test| test.fake_engine()) && !paths.database().exists();
    let db = db::connect(&paths.database())?;
    let default_dir = settings.downloads_dir(paths);
    if seed {
        fake::fixture::seed(&mut Store::open(&db)?, &default_dir, torrential_core::time::now())?;
    }
    let asked = |hook| test.and_then(|test| test.duration(hook));
    let timing = match asked(hooks::TITLE_WAIT) {
        Some(wait) => titles::Timing { file_list: wait, first_bytes: wait, ..titles::Timing::default() },
        None => titles::Timing::default(),
    };
    let pause = |hook| test.map_or(Duration::ZERO, |test| test.pause(hook));
    let config = OwnerConfig {
        default_dir,
        probe_dir: paths.cache.join("probes"),
        max_active: MAX_ACTIVE,
        save_resume_every: asked(hooks::SAVE_RESUME_EVERY).unwrap_or(SAVE_RESUME_EVERY),
        stall_after: asked(hooks::STALL_AFTER).unwrap_or(STALL_AFTER),
        writes_stop_within: WRITES_STOP_WITHIN,
        plays: owner::plays::Plays { root: paths.plays(), max: settings.plays_max.0, idle_after: plays_idle(test) },
        trash: trash(env, paths),
        deletion_pause: pause(hooks::DELETION_PAUSE),
        trash_pause: owner::deleter::TrashPause::sleep(pause(hooks::TRASH_PAUSE)),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::internal(format!("cannot start the async runtime: {e}")))?;
    let version = env.version();
    let ports = Ports::from_env(env, paths, &settings)?;
    let (engine, ended) = runtime.block_on(async {
        let wanted = http_port.or(test.and_then(|test| test.http_port())).unwrap_or(http::PORT);
        // Open pages reconnect to the address they loaded from, so the engine serves where it last did.
        let last = std::fs::read_to_string(paths.http_port()).ok().and_then(|text| text.trim().parse().ok());
        let http_ports: Vec<u16> = last.into_iter().chain([wanted]).collect();
        let access = paths.engine_token();
        let listen = Listen { ports: &http_ports, access: &access, server: settings.server };
        let running = start(&tokio::runtime::Handle::current(), &db, ports, config, timing, version.clone(), listen)?;
        running.ctx.news.post(switch::opening(env, paths));
        let (pid, players) = (std::process::id(), running.ctx.http_port);
        if last != Some(players) {
            let written = fsutil::write_atomic(&paths.http_port(), players.to_string().as_bytes());
            persist(&format!("write {}", paths.http_port().display()), written);
        }
        log(&format!("engine {version} started, pid {pid}, web app and players http://127.0.0.1:{players}"));
        if settings.server {
            log(&format!("engine {version} runs as a server, on the network on port {players}"));
        }
        persist(&format!("remove {}", paths.switching().display()), fsutil::remove(&paths.switching()));
        let signals = tokio::spawn(stop_on_signal(running.ctx.owner.clone()));
        let updates = tokio::spawn(switch::follow_updates(env.clone(), paths.clone(), running.ctx.clone(), service));
        let ended = running.owner.await.unwrap_or(Ended::Stopped);
        // Before the port stops answering, so a stop that finds it refusing waits for the next engine.
        if ended == Ended::Restart && service {
            service::leave_switching(paths, torrential_core::time::now());
        }
        running.http.abort();
        signals.abort();
        updates.abort();
        // An engine that stops for a newer version leaves its token file, so a client following it
        // finds a port that refuses for a moment instead of none, and waits for the next engine.
        if ended == Ended::Stopped {
            persist(&format!("remove {}", access.display()), fsutil::remove(&access));
        }
        Ok::<_, Error>((running.engine, ended))
    })?;
    drop(runtime);
    let _ = engine.join();
    log("engine stopped");
    Ok(ended)
}

async fn stop_on_signal(owner: OwnerHandle) {
    crate::process::asked_to_stop().await;
    log("stopping on a signal");
    let (reply, _) = tokio::sync::oneshot::channel();
    owner.tell(owner::Command::Shutdown { reply });
}

fn prepare_runtime_dir(dir: &Path) -> Result<(), Error> {
    let refuse =
        |why: String| Error::new(ErrorCode::DaemonUnavailable, why).with_hint("remove it, or set XDG_RUNTIME_DIR");
    std::fs::create_dir_all(dir).map_err(|e| refuse(format!("cannot create {}: {e}", dir.display())))?;
    let meta = dir.symlink_metadata().map_err(|e| refuse(format!("cannot inspect {}: {e}", dir.display())))?;
    if !meta.is_dir() {
        return Err(refuse(format!("{} is not a directory", dir.display())));
    }
    // On Windows the folder is under the person's local app data folder, which is theirs alone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != paths::uid() {
            return Err(refuse(format!("{} is not a directory owned by you", dir.display())));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| refuse(format!("cannot protect {}: {e}", dir.display())))?;
    }
    Ok(())
}

/// The lock that makes one engine the owner of a state folder. The engine's process ID is in the
/// lock file on Unix. Windows lets no other process read a locked file, so there it is beside it.
pub struct OwnerLock {
    _file: File,
}

impl OwnerLock {
    pub fn acquire(path: &Path) -> Result<Option<Self>, Error> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(path)
            .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot open {}: {e}", path.display())))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(Error::new(ErrorCode::StateCorrupt, format!("cannot lock {}: {e}", path.display())));
            }
        }
        let pid = std::process::id();
        if cfg!(windows) {
            let _ = std::fs::write(pid_file(path), format!("{pid}\n"));
        } else {
            let _ = file.set_len(0).and_then(|()| writeln!(file, "{pid}"));
        }
        Ok(Some(Self { _file: file }))
    }

    /// The process that holds the lock at `path`, which wrote its ID, when one does.
    pub fn holder(path: &Path) -> Option<u32> {
        let file = File::open(path).ok()?;
        match file.try_lock_shared() {
            Err(std::fs::TryLockError::WouldBlock) => {}
            _ => return None,
        }
        let written = if cfg!(windows) { pid_file(path) } else { path.to_path_buf() };
        std::fs::read_to_string(written).ok()?.trim().parse().ok()
    }
}

/// Where the engine that holds the lock at `lock` writes its process ID on Windows.
fn pid_file(lock: &Path) -> PathBuf {
    lock.with_extension("pid")
}
