//! The engine's own ffmpeg: a pinned build it fetches the first time a file needs converting and
//! none is installed.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use torrential_core::{Error, ErrorCode};

use super::{INSTALL, Programs, Tools, blocked_error, complaint};
use crate::env::Env;
use crate::log::log;

/// jellyfin-ffmpeg's portable macOS build, which links only to `/usr/lib` and the system frameworks
/// and leaves out fdk-aac.
const JELLYFIN: &str = "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v8.1.3-2";
/// BtbN's GPL Linux builds, which link only to the system's C library and have no nonfree parts.
/// The pin is a month's last build, which BtbN keeps for two years. It deletes other daily builds
/// after two weeks.
const BTBN: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-31-12-59";
/// Gyan's GPL Windows release builds, the files `winget install Gyan.FFmpeg.Essentials` installs.
/// Smart App Control runs an unsigned program only when Microsoft has seen many people run it, so
/// the build that many people install has the best chance.
const GYAN: &str = "https://github.com/GyanD/codexffmpeg/releases/download/9.0.1";

struct Pin {
    os: &'static str,
    arch: &'static str,
    /// The ffmpeg version, which names the folder the build unpacks into.
    version: &'static str,
    release: &'static str,
    file: &'static str,
    /// The folder in the archive that holds ffmpeg and ffprobe, empty for its top.
    bin: &'static str,
    sha256: &'static str,
    size: u64,
    /// The glibc loader the programs start with, from the system's root folder. A Linux without
    /// glibc, such as Alpine, has none and cannot run them.
    loader: Option<&'static str>,
    packing: Packing,
}

/// How a build's archive is packed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Packing {
    TarXz,
    Zip,
}

/// One for each platform torrential releases for, by `std::env::consts` names. The sizes and hashes
/// are the ones each release publishes for its files.
const PINS: [Pin; 4] = [
    Pin {
        os: "macos",
        arch: "aarch64",
        version: "8.1.3-2",
        release: JELLYFIN,
        file: "jellyfin-ffmpeg_8.1.3-2_portable_macarm64-gpl.tar.xz",
        bin: "",
        sha256: "d898cc819e7c4e9219d6410ddbdaada0838f14bec6ff89e283639915394cb43f",
        size: 32_904_768,
        loader: None,
        packing: Packing::TarXz,
    },
    Pin {
        os: "linux",
        arch: "x86_64",
        version: "n8.1.3-27-g8dca1586f4",
        release: BTBN,
        file: "ffmpeg-n8.1.3-27-g8dca1586f4-linux64-gpl-8.1.tar.xz",
        bin: "ffmpeg-n8.1.3-27-g8dca1586f4-linux64-gpl-8.1/bin",
        sha256: "41e9c565594c390622cf968d82fa4135d9e9a11d06c1515644288cbc844e3380",
        size: 152_246_308,
        loader: Some("lib64/ld-linux-x86-64.so.2"),
        packing: Packing::TarXz,
    },
    Pin {
        os: "linux",
        arch: "aarch64",
        version: "n8.1.3-27-g8dca1586f4",
        release: BTBN,
        file: "ffmpeg-n8.1.3-27-g8dca1586f4-linuxarm64-gpl-8.1.tar.xz",
        bin: "ffmpeg-n8.1.3-27-g8dca1586f4-linuxarm64-gpl-8.1/bin",
        sha256: "a08c0ee6e1c4e02139344b303002dcc7c37cf125d639f1fbd15dabee40fcb525",
        size: 127_720_576,
        loader: Some("lib/ld-linux-aarch64.so.1"),
        packing: Packing::TarXz,
    },
    Pin {
        os: "windows",
        arch: "x86_64",
        version: "9.0.1",
        release: GYAN,
        file: "ffmpeg-9.0.1-essentials_build.zip",
        bin: "ffmpeg-9.0.1-essentials_build/bin",
        sha256: "fec81ae03971d9dd4be3ebe02e263bd2ec1d789483f931bdba5f5715e65da2e9",
        size: 111_253_802,
        loader: None,
        packing: Packing::Zip,
    },
];

const PROGRAMS: [&str; 2] = ["ffmpeg", "ffprobe"];
const ARCHIVE: &str = "archive";
const MANIFEST: &str = "manifest.json";
/// What a fetch leaves in the fetched folder when Windows blocks the build it fetched.
const BLOCKED: &str = "blocked.json";
/// Where a fetch puts its files until they are checked.
const PARTIAL: &str = ".partial";
/// A fresh program can take a while to start the first time, while macOS scans it.
const RUN_WITHIN: Duration = Duration::from_secs(30);
const REPORT_EVERY: Duration = Duration::from_millis(200);

/// An archive holding ffmpeg and ffprobe, and what it must hash to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Build {
    /// The ffmpeg version, which names the folder the build unpacks into.
    pub version: String,
    pub url: String,
    /// The folder in the archive that holds ffmpeg and ffprobe.
    pub bin: PathBuf,
    /// Lowercase hex.
    pub sha256: String,
    /// Bytes. A download of any other size is not this build.
    pub size: u64,
    pub packing: Packing,
}

impl Build {
    /// The build for `os` and `arch`, when the system whose root folder is `system` can run it.
    pub fn pinned(os: &str, arch: &str, system: &Path) -> Option<Build> {
        let pin = PINS.iter().find(|pin| pin.os == os && pin.arch == arch)?;
        if pin.loader.is_some_and(|loader| !system.join(loader).exists()) {
            return None;
        }
        Some(Build {
            version: pin.version.to_string(),
            url: format!("{}/{}", pin.release, pin.file),
            bin: PathBuf::from(pin.bin),
            sha256: pin.sha256.to_string(),
            size: pin.size,
            packing: pin.packing,
        })
    }
}

/// What a conversion does when no ffmpeg is installed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fetch {
    Build(Build),
    /// config.yaml turns fetching off. A build fetched before still runs.
    Off(Build),
    /// torrential has no build that runs on this machine.
    Unavailable,
}

impl Fetch {
    /// The pinned build for this machine, unless `allowed` is false. Nothing in the environment
    /// changes which build that is. A test engine fetches nothing; tests that fetch hand their
    /// own build to `Ffmpeg::new`.
    pub fn from_env(env: &Env, allowed: bool) -> Fetch {
        if env.test().is_some() {
            return Fetch::Unavailable;
        }
        match Build::pinned(std::env::consts::OS, std::env::consts::ARCH, Path::new("/")) {
            Some(build) if allowed => Fetch::Build(build),
            Some(build) => Fetch::Off(build),
            None => Fetch::Unavailable,
        }
    }

    /// The build a fetched folder must hold for the engine to run it.
    fn pin(&self) -> Option<&Build> {
        match self {
            Fetch::Build(build) | Fetch::Off(build) => Some(build),
            Fetch::Unavailable => None,
        }
    }

    /// What a conversion gets when there is no ffmpeg here.
    fn missing(&self) -> Error {
        let (message, hint) = match self {
            Fetch::Build(_) => (
                "torrential has not fetched ffmpeg yet, and this needs it",
                format!("play the file in the web app once, which fetches it, or {INSTALL}"),
            ),
            Fetch::Off(_) => (
                "ffmpeg is not installed, and config.yaml turns off fetching it",
                format!("{INSTALL}, then try again; torrential doctor shows what it found"),
            ),
            Fetch::Unavailable => (
                "ffmpeg is not installed, and this needs it",
                format!("{INSTALL}, then try again; torrential doctor shows what it found"),
            ),
        };
        Error::new(ErrorCode::NotFound, message).with_hint(hint)
    }
}

/// The ffmpeg the engine runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Found {
    /// One installed on this machine, which wins over a fetched one.
    Installed(Programs),
    Fetched(Programs),
    /// None yet, and what a conversion does about it.
    Missing(Fetch),
}

impl Found {
    pub fn readiness(&self) -> Readiness {
        match self {
            Found::Installed(_) | Found::Fetched(_) => Readiness::Ready,
            Found::Missing(Fetch::Build(_)) => Readiness::Fetch,
            Found::Missing(Fetch::Off(_) | Fetch::Unavailable) => Readiness::Missing,
        }
    }
}

/// Whether the engine converts a file now, once it has fetched ffmpeg, or not at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum Readiness {
    Ready,
    Fetch,
    Missing,
}

/// Where fetched builds go: one folder for each version, in the engine's state folder.
pub fn root(state: &Path) -> PathBuf {
    state.join("ffmpeg")
}

#[derive(Clone, Debug)]
pub enum Stage {
    /// Bytes of the archive so far. Unpacking follows the last of them.
    Downloading {
        done: u64,
        size: u64,
    },
    Done(Result<(), Error>),
}

/// Whether ffmpeg is here, or on its way.
pub enum Fetching {
    Here,
    Running(watch::Receiver<Stage>),
}

/// The engine's ffmpeg. Every request that finds none follows the same fetch.
pub struct Ffmpeg {
    /// ffmpeg and ffprobe as installed now. A person can install them while the engine runs.
    installed: Arc<dyn Fn() -> Tools + Send + Sync>,
    root: PathBuf,
    fetch: Fetch,
    /// What this engine run knows of the fetched build. A fetch sets it as it ends.
    fetched: Arc<Mutex<Fetched>>,
}

/// The fetched build, as far as this engine run knows.
enum Fetched {
    /// Not looked at yet. The first look hashes the programs, once for each engine run.
    Unchecked,
    /// The folder holds the pinned build's programs as they were unpacked.
    Ready(Programs),
    /// None, or not as it was fetched, so it needs fetching again.
    Absent,
    Running(watch::Receiver<Stage>),
    /// The fetched build did not run here, so this engine run does not fetch it again. One Windows
    /// blocked is not fetched again by later runs either.
    Refused(Error),
}

impl Ffmpeg {
    /// `root` is the folder fetched builds go in.
    pub fn new(installed: Arc<dyn Fn() -> Tools + Send + Sync>, root: PathBuf, fetch: Fetch) -> Ffmpeg {
        Ffmpeg { installed, root, fetch, fetched: Arc::new(Mutex::new(Fetched::Unchecked)) }
    }

    pub fn found(&self) -> Found {
        if let Some(programs) = (self.installed)().both() {
            return Found::Installed(programs);
        }
        let mut fetched = self.fetched.lock().expect("no holder panics");
        if let Fetched::Unchecked = *fetched {
            *fetched = match self.fetch.pin() {
                Some(pin) => match check(&self.root, pin) {
                    Some(programs) => Fetched::Ready(programs),
                    None => Blocked::read(&self.root, pin).map_or(Fetched::Absent, Fetched::Refused),
                },
                None => Fetched::Absent,
            };
        }
        match &*fetched {
            Fetched::Ready(programs) => Found::Fetched(programs.clone()),
            _ => Found::Missing(self.fetch.clone()),
        }
    }

    /// Why the fetched build does not run here, once `found` has looked and it did not.
    pub fn refused(&self) -> Option<Error> {
        match &*self.fetched.lock().expect("no holder panics") {
            Fetched::Refused(error) => Some(error.clone()),
            _ => None,
        }
    }

    /// Whether a file the browser cannot read plays: ffmpeg and ffprobe are here, or a fetch
    /// brings them first.
    pub fn converts(&self) -> bool {
        self.found().readiness() != Readiness::Missing
    }

    /// The programs that are here now. Never fetches them.
    pub fn now(&self) -> Result<Programs, Error> {
        match self.found() {
            Found::Installed(programs) | Found::Fetched(programs) => Ok(programs),
            Found::Missing(fetch) => Err(fetch.missing()),
        }
    }

    /// Starts fetching ffmpeg when it is not here and no fetch is under way, and answers the fetch
    /// to follow. A fetch runs to its end even when no one follows it any more.
    pub fn fetch(&self) -> Result<Fetching, Error> {
        let build = match self.found() {
            Found::Installed(_) | Found::Fetched(_) => return Ok(Fetching::Here),
            Found::Missing(Fetch::Build(build)) => build,
            Found::Missing(fetch) => return Err(fetch.missing()),
        };
        let mut fetched = self.fetched.lock().expect("no holder panics");
        match &*fetched {
            // A fetch ended since this request looked.
            Fetched::Ready(_) => return Ok(Fetching::Here),
            // A fetch that panicked has dropped its sender.
            Fetched::Running(stage) if stage.has_changed().is_ok() => return Ok(Fetching::Running(stage.clone())),
            Fetched::Refused(error) => return Err(error.clone()),
            _ => {}
        }
        let (report, stage) = watch::channel(Stage::Downloading { done: 0, size: build.size });
        *fetched = Fetched::Running(stage.clone());
        let (root, state) = (self.root.clone(), self.fetched.clone());
        tokio::spawn(async move {
            let (now, done) = match fetch(&build, &root, &report).await {
                Ok(programs) => (Fetched::Ready(programs), Ok(())),
                Err(Failure::Again(error)) => (Fetched::Absent, Err(error)),
                Err(Failure::Refused(error) | Failure::Blocked(error)) => (Fetched::Refused(error.clone()), Err(error)),
            };
            *state.lock().expect("no holder panics") = now;
            report.send_replace(Stage::Done(done));
        });
        Ok(Fetching::Running(stage))
    }
}

/// What a fetch left in its folder: the build it unpacked, and the SHA-256 of each program as it
/// came out of the archive.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct Manifest {
    build: String,
    ffmpeg: String,
    ffprobe: String,
}

impl Manifest {
    /// What the manifest in `folder` says when its programs are those of `build`, as they are now.
    /// Fails when a program is a link or is not executable.
    fn of(folder: &Path, build: &Build) -> io::Result<Manifest> {
        let hash = |program: &str| {
            let path = folder.join(program_file(program));
            let meta = path.symlink_metadata()?;
            if !meta.is_file() || !runnable(&meta) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{program} is not a program")));
            }
            let mut hash = Sha256::new();
            io::copy(&mut File::open(&path)?, &mut hash)?;
            Ok(format!("{:x}", hash.finalize()))
        };
        Ok(Manifest { build: build.sha256.clone(), ffmpeg: hash("ffmpeg")?, ffprobe: hash("ffprobe")? })
    }

    fn write(&self, folder: &Path) -> io::Result<()> {
        let mut file = new_file(&folder.join(MANIFEST), false)?;
        io::Write::write_all(&mut file, &serde_json::to_vec(self)?)
    }
}

/// The programs `root`'s folder for `pin` holds, when only this user can change the folders and the
/// programs are those its manifest records for `pin`.
fn check(root: &Path, pin: &Build) -> Option<Programs> {
    let folder = root.join(&pin.version);
    for dir in [root.parent().unwrap_or(root), root, &folder] {
        private(dir).ok()?;
    }
    let recorded: Manifest = serde_json::from_slice(&std::fs::read(folder.join(MANIFEST)).ok()?).ok()?;
    (recorded == Manifest::of(&folder, pin).ok()?).then(|| programs(&folder))
}

/// The build Windows would not run, and what the person read about it.
#[derive(Serialize, Deserialize)]
struct Blocked {
    /// The archive's SHA-256, so a new pin is fetched again.
    build: String,
    error: Error,
}

impl Blocked {
    /// Why Windows blocked `pin`, when a fetch of it in `root` said so.
    fn read(root: &Path, pin: &Build) -> Option<Error> {
        let blocked: Blocked = serde_json::from_slice(&std::fs::read(root.join(BLOCKED)).ok()?).ok()?;
        (blocked.build == pin.sha256).then_some(blocked.error)
    }

    fn write(&self, root: &Path) -> io::Result<()> {
        let mut file = new_file(&root.join(BLOCKED), false)?;
        io::Write::write_all(&mut file, &serde_json::to_vec(self)?)
    }
}

fn programs(folder: &Path) -> Programs {
    Programs { ffmpeg: folder.join(program_file("ffmpeg")), ffprobe: folder.join(program_file("ffprobe")) }
}

/// The file a program is in: `ffmpeg`, or `ffmpeg.exe` on Windows.
fn program_file(program: &str) -> String {
    format!("{program}{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(unix)]
fn runnable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o100 != 0
}

#[cfg(windows)]
fn runnable(_: &std::fs::Metadata) -> bool {
    true
}

/// A new file at `path` for this user alone, which runs when it is a `program`.
fn new_file(path: &Path, program: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, if program { 0o700 } else { 0o600 });
    #[cfg(not(unix))]
    let _ = program;
    options.open(path)
}

enum Failure {
    /// Another try can work: the network, the disk, or a download that is not the build.
    Again(Error),
    /// The build does not run on this machine, so another download would not help.
    Refused(Error),
    /// Windows will not run the build, which stays so after the engine restarts.
    Blocked(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure::Again(error)
    }
}

/// Downloads `build`, checks its hash, unpacks ffmpeg and ffprobe with a manifest of them, and
/// checks that they run, into `root`'s folder for the build. Anything else in `root` is removed. A
/// failure leaves nothing.
async fn fetch(build: &Build, root: &Path, report: &watch::Sender<Stage>) -> Result<Programs, Failure> {
    let folder = root.join(&build.version);
    let started = Instant::now();
    let fetched: Result<Programs, Failure> = async {
        let partial = Partial::create(root)?;
        log(&format!("fetching ffmpeg {} from {}", build.version, build.url));
        let archive = partial.path.join(ARCHIVE);
        download(build, &archive, report).await?;
        let (into, unpacked) = (partial.path.clone(), build.clone());
        tokio::task::spawn_blocking(move || {
            unpack(&archive, unpacked.packing, &unpacked.bin, &into)?;
            Manifest::of(&into, &unpacked)?.write(&into)
        })
        .await
        .map_err(|e| Error::internal(format!("the ffmpeg unpacking stopped: {e}")))?
        .map_err(|e| {
            Error::new(ErrorCode::StorageError, format!("cannot unpack ffmpeg into {}: {e}", root.display()))
                .with_hint("free some space or fix the folder's permissions, then try again")
        })?;
        for program in PROGRAMS {
            if let Err(failure) = runs(&partial.path.join(program_file(program))).await {
                if let Failure::Blocked(error) = &failure {
                    let blocked = Blocked { build: build.sha256.clone(), error: error.clone() };
                    if let Err(e) = blocked.write(root) {
                        log(&format!("cannot note that Windows blocked ffmpeg: {e}"));
                    }
                }
                return Err(failure);
            }
        }
        Ok(partial.finish(&folder)?)
    }
    .await;
    match &fetched {
        Ok(_) => {
            log(&format!("fetched ffmpeg {} in {} s", build.version, started.elapsed().as_secs()));
        }
        Err(Failure::Again(error) | Failure::Refused(error) | Failure::Blocked(error)) => {
            log(&format!("cannot fetch ffmpeg: {}", error.message));
        }
    }
    fetched
}

async fn download(build: &Build, to: &Path, report: &watch::Sender<Stage>) -> Result<(), Error> {
    let unreachable = |e: &reqwest::Error| {
        let mut cause: &dyn std::error::Error = e;
        while let Some(source) = cause.source() {
            cause = source;
        }
        Error::new(ErrorCode::FetchFailed, format!("cannot fetch ffmpeg: {cause}"))
            .with_hint(format!("check the network connection and try again, or {INSTALL}"))
    };
    let client = reqwest::Client::builder()
        .user_agent(concat!("torrential/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::internal(format!("cannot build an HTTP client: {e}")))?;
    let mut response = client.get(&build.url).send().await.map_err(|e| unreachable(&e))?;
    if !response.status().is_success() {
        let status = response.status();
        return Err(Error::new(ErrorCode::FetchFailed, format!("cannot fetch ffmpeg: the server answered {status}"))
            .with_hint(format!("try again later, or {INSTALL}")));
    }
    let mut file = tokio::fs::File::from_std(new_file(to, false).map_err(|e| unwritable(to, &e))?);
    let (mut hash, mut done, mut reported) = (Sha256::new(), 0, Instant::now());
    while let Some(chunk) = response.chunk().await.map_err(|e| unreachable(&e))? {
        done += chunk.len() as u64;
        if done > build.size {
            return Err(tampered());
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| unwritable(to, &e))?;
        if reported.elapsed() >= REPORT_EVERY || done == build.size {
            report.send_replace(Stage::Downloading { done, size: build.size });
            reported = Instant::now();
        }
    }
    file.flush().await.map_err(|e| unwritable(to, &e))?;
    if done != build.size || format!("{:x}", hash.finalize()) != build.sha256 {
        return Err(tampered());
    }
    Ok(())
}

fn tampered() -> Error {
    Error::new(ErrorCode::VerificationFailed, "the ffmpeg download is not the build torrential pins, so it was deleted")
        .with_hint(format!("try again later, or {INSTALL}"))
}

fn unwritable(path: &Path, e: &io::Error) -> Error {
    Error::new(ErrorCode::StorageError, format!("cannot write ffmpeg to {}: {e}", path.display()))
        .with_hint("free some space or fix the folder's permissions, then try again")
}

/// Writes the ffmpeg and ffprobe in the archive's `bin` folder into `into`, and nothing else from it.
fn unpack(archive: &Path, packing: Packing, bin: &Path, into: &Path) -> io::Result<()> {
    let wanted = |path: &Path| PROGRAMS.into_iter().map(program_file).find(|file| path == bin.join(file));
    match packing {
        Packing::TarXz => {
            let xz = lzma_rust2::XzReader::new(BufReader::new(File::open(archive)?), true);
            let mut tar = tar::Archive::new(xz);
            for entry in tar.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                let file = wanted(path.strip_prefix(".").unwrap_or(&path));
                if let Some(file) = file.filter(|_| entry.header().entry_type().is_file()) {
                    io::copy(&mut entry, &mut new_file(&into.join(file), true)?)?;
                }
            }
        }
        Packing::Zip => {
            let mut zip = zip::ZipArchive::new(BufReader::new(File::open(archive)?))?;
            for index in 0..zip.len() {
                let mut entry = zip.by_index(index)?;
                let file = entry.enclosed_name().and_then(|path| wanted(&path));
                if let Some(file) = file.filter(|_| entry.is_file()) {
                    io::copy(&mut entry, &mut new_file(&into.join(file), true)?)?;
                }
            }
        }
    }
    for program in PROGRAMS {
        let path = into.join(program_file(program));
        if !path.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("the archive has no {program}")));
        }
        unquarantine(&path)?;
    }
    Ok(())
}

/// Checks that a fetched program starts here, so a build macOS or Windows refuses, or one this Linux
/// cannot load, fails now and not in the player.
async fn runs(program: &Path) -> Result<(), Failure> {
    let run = tokio::process::Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let name = program.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let why = match tokio::time::timeout(RUN_WITHIN, run).await {
        Ok(Ok(output)) if output.status.success() => return Ok(()),
        Ok(Ok(output)) => complaint(&String::from_utf8_lossy(&output.stderr)),
        Ok(Err(e)) if super::blocked(&e) => {
            let what = format!("the {name} torrential fetched");
            return Err(Failure::Blocked(blocked_error(ErrorCode::FetchFailed, &what)));
        }
        Ok(Err(e)) => e.to_string(),
        Err(_) => format!("it did not answer within {} s", RUN_WITHIN.as_secs()),
    };
    Err(Failure::Refused(
        Error::new(
            ErrorCode::FetchFailed,
            format!("the {name} torrential fetched does not run on this machine: {why}"),
        )
        .with_hint(format!("{INSTALL}, then try again")),
    ))
}

/// The folder a fetch fills before its files are checked. Dropping it deletes it, and `root` with
/// it when nothing else is there.
struct Partial {
    path: PathBuf,
}

impl Partial {
    /// Empties `root` of what earlier fetches left, other versions included, and makes the folder.
    fn create(root: &Path) -> Result<Partial, Error> {
        private(root.parent().unwrap_or(root))?;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut folder = DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut folder, 0o700);
        match folder.create(root) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(unwritable(root, &e)),
            _ => private(root)?,
        }
        let made = (|| {
            for entry in std::fs::read_dir(root)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() { std::fs::remove_dir_all(&path)? } else { std::fs::remove_file(&path)? }
            }
            let path = root.join(PARTIAL);
            folder.create(&path)?;
            Ok(Partial { path })
        })();
        made.map_err(|e: io::Error| unwritable(root, &e))
    }

    /// Moves the programs to `folder`, where the engine finds them.
    fn finish(self, folder: &Path) -> Result<Programs, Error> {
        std::fs::remove_file(self.path.join(ARCHIVE))
            .and_then(|()| std::fs::rename(&self.path, folder))
            .map_err(|e| unwritable(folder, &e))?;
        Ok(programs(folder))
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        if let Some(root) = self.path.parent() {
            let _ = std::fs::remove_dir(root);
        }
    }
}

/// Refuses a folder that another user could change between the engine checking a program in it and
/// running it.
fn private(folder: &Path) -> Result<(), Error> {
    let meta = folder.symlink_metadata().map_err(|e| unwritable(folder, &e))?;
    let (why, hint) = if meta.file_type().is_symlink() {
        ("it is a symbolic link", "make it a real folder, not a link, then try again".to_string())
    } else if !meta.is_dir() {
        ("it is not a folder", "move it away, then try again".to_string())
    } else if let Some(why) = shared(&meta, folder) {
        why
    } else {
        return Ok(());
    };
    Err(Error::new(ErrorCode::StorageError, format!("torrential will not keep ffmpeg in {}: {why}", folder.display()))
        .with_hint(hint))
}

/// Why another user could change `folder`, and what to do about it. Windows keeps a folder under
/// the person's profile to them by its permissions.
#[cfg(unix)]
fn shared(meta: &std::fs::Metadata, folder: &Path) -> Option<(&'static str, String)> {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != crate::paths::uid() {
        Some(("it belongs to another user", "make it yours with chown, then try again".to_string()))
    } else if meta.mode() & 0o022 != 0 {
        Some(("other users can write to it", format!("run chmod go-w {}, then try again", folder.display())))
    } else {
        None
    }
}

#[cfg(windows)]
fn shared(_: &std::fs::Metadata, _: &Path) -> Option<(&'static str, String)> {
    None
}

/// Clears the quarantine mark macOS can put on a downloaded program, which stops it running. A
/// file without the mark is left as it is.
#[cfg(target_os = "macos")]
fn unquarantine(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let name = c"com.apple.quarantine";
    // SAFETY: both are NUL-terminated and outlive the calls, and a null buffer of size 0 asks only
    // whether the attribute is there.
    let size =
        unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, libc::XATTR_NOFOLLOW) };
    if size < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOATTR) { Ok(()) } else { Err(error) };
    }
    // SAFETY: as above.
    if unsafe { libc::removexattr(path.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn unquarantine(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linux_without_the_glibc_loader_its_build_starts_with_has_no_build() {
        let system = tempfile::tempdir().unwrap();
        let pinned = |os, arch| {
            Build::pinned(os, arch, system.path()).map(|build| build.url.rsplit('/').next().unwrap().to_string())
        };
        assert_eq!([pinned("linux", "x86_64"), pinned("linux", "aarch64")], [None, None]);
        assert_eq!(pinned("macos", "aarch64").as_deref(), Some("jellyfin-ffmpeg_8.1.3-2_portable_macarm64-gpl.tar.xz"));
        assert_eq!(pinned("windows", "x86_64").as_deref(), Some("ffmpeg-9.0.1-essentials_build.zip"));
        for loader in ["lib64/ld-linux-x86-64.so.2", "lib/ld-linux-aarch64.so.1"] {
            std::fs::create_dir_all(system.path().join(loader).parent().unwrap()).unwrap();
            std::fs::write(system.path().join(loader), "").unwrap();
        }
        assert_eq!(
            [pinned("linux", "x86_64"), pinned("linux", "aarch64")],
            [
                Some("ffmpeg-n8.1.3-27-g8dca1586f4-linux64-gpl-8.1.tar.xz".to_string()),
                Some("ffmpeg-n8.1.3-27-g8dca1586f4-linuxarm64-gpl-8.1.tar.xz".to_string())
            ]
        );
    }

    #[test]
    fn an_installed_ffmpeg_wins_over_the_fetched_one() {
        let dir = tempfile::tempdir().unwrap();
        let (installed, root) = (dir.path().join("bin"), dir.path().join("ffmpeg"));
        let build = Build {
            version: "8.1".into(),
            url: String::new(),
            bin: PathBuf::new(),
            sha256: "ab".repeat(32),
            size: 0,
            packing: Packing::TarXz,
        };
        let fetched = root.join(&build.version);
        for folder in [&installed, &fetched] {
            let mut builder = DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.create(folder).unwrap();
            for program in PROGRAMS {
                new_file(&folder.join(program_file(program)), true).unwrap();
            }
        }
        Manifest::of(&fetched, &build).unwrap().write(&fetched).unwrap();
        let found =
            |installed: Tools, fetch| Ffmpeg::new(Arc::new(move || installed.clone()), root.clone(), fetch).found();
        let system = Tools {
            ffmpeg: Some(installed.join(program_file("ffmpeg"))),
            ffprobe: Some(installed.join(program_file("ffprobe"))),
        };
        assert_eq!(found(system.clone(), Fetch::Off(build.clone())), Found::Installed(programs(&installed)));
        let fetched_found = found(Tools::default(), Fetch::Off(build.clone()));
        assert_eq!(fetched_found, Found::Fetched(programs(&fetched)));
        assert_eq!(fetched_found.readiness(), Readiness::Ready);
        let only_ffmpeg = Tools { ffmpeg: system.ffmpeg, ffprobe: None };
        std::fs::remove_dir_all(&fetched).unwrap();
        let missing = |fetch| found(only_ffmpeg.clone(), fetch).readiness();
        assert_eq!(
            [missing(Fetch::Build(build.clone())), missing(Fetch::Off(build)), missing(Fetch::Unavailable)],
            [Readiness::Fetch, Readiness::Missing, Readiness::Missing]
        );
    }

    #[test]
    fn a_build_windows_blocked_is_not_fetched_again_but_a_new_pin_or_an_installed_ffmpeg_plays() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ffmpeg");
        std::fs::create_dir(&root).unwrap();
        let build = Build {
            version: "9.0.1".into(),
            url: "http://127.0.0.1:9/never".into(),
            bin: PathBuf::new(),
            sha256: "ab".repeat(32),
            size: 1,
            packing: Packing::Zip,
        };
        let error = blocked_error(ErrorCode::FetchFailed, "the ffprobe.exe torrential fetched");
        Blocked { build: build.sha256.clone(), error: error.clone() }.write(&root).unwrap();
        let ffmpeg = |installed: Tools, build: &Build| {
            Ffmpeg::new(Arc::new(move || installed.clone()), root.clone(), Fetch::Build(build.clone()))
        };

        let blocked = ffmpeg(Tools::default(), &build);
        assert_eq!(blocked.fetch().err(), Some(error.clone()), "no download starts");
        assert_eq!(blocked.refused(), Some(error));
        assert_eq!(
            blocked.refused().unwrap().message,
            "Windows blocked the ffprobe.exe torrential fetched (Smart App Control or an App Control policy)"
        );

        let newer = ffmpeg(Tools::default(), &Build { sha256: "cd".repeat(32), ..build.clone() });
        assert_eq!((newer.found().readiness(), newer.refused()), (Readiness::Fetch, None));

        let bin = dir.path().join("bin");
        let installed = Tools { ffmpeg: Some(bin.join("ffmpeg")), ffprobe: Some(bin.join("ffprobe")) };
        assert_eq!(ffmpeg(installed, &build).found(), Found::Installed(programs(&bin)));
    }

    /// Fetches this machine's pinned build and checks that it runs and has every encoder, decoder,
    /// format, filter and option the web player's conversions use. CI runs it on Windows.
    #[tokio::test]
    #[ignore = "downloads the pinned ffmpeg build, about 110 MB on Windows"]
    async fn the_pinned_build_runs_here_and_has_what_conversions_use() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ffmpeg");
        let build = Build::pinned(std::env::consts::OS, std::env::consts::ARCH, Path::new("/")).expect("a pin here");
        let (report, _) = watch::channel(Stage::Downloading { done: 0, size: build.size });
        let programs = match fetch(&build, &root, &report).await {
            Ok(programs) => programs,
            Err(Failure::Again(e) | Failure::Refused(e) | Failure::Blocked(e)) => panic!("{e:?}"),
        };
        let list = |what: &str| {
            let output = std::process::Command::new(&programs.ffmpeg).args(["-hide_banner", what]).output().unwrap();
            assert!(output.status.success(), "ffmpeg {what} failed");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let named = |list: &str, at: usize| -> Vec<String> {
            list.lines().filter_map(|line| line.split_whitespace().nth(at)).map(String::from).collect()
        };
        let wanted: [(&str, usize, &[&str]); 6] = [
            ("-encoders", 1, &["libx264", "aac", "webvtt", "mjpeg"]),
            ("-decoders", 1, &["hevc", "h264", "ac3", "eac3", "dca", "truehd", "subrip", "ass", "webvtt"]),
            ("-muxers", 1, &["mp4", "webvtt", "mjpeg"]),
            ("-demuxers", 1, &["matroska,webm", "mov,mp4,m4a,3gp,3g2,mj2", "srt", "ass", "webvtt"]),
            ("-filters", 1, &["scale", "format"]),
            ("-protocols", 0, &["http", "pipe"]),
        ];
        for (what, at, names) in wanted {
            let have = named(&list(what), at);
            for name in names {
                assert!(have.iter().any(|have| have == name), "the pinned ffmpeg has no {name} in {what}");
            }
        }
        let help = std::process::Command::new(&programs.ffmpeg).args(["-hide_banner", "-h", "full"]).output().unwrap();
        let help = String::from_utf8_lossy(&help.stdout);
        for option in ["-readrate_initial_burst", "-copyts", "-avoid_negative_ts", "-movflags", "-flush_packets"] {
            assert!(help.contains(option), "the pinned ffmpeg has no {option}");
        }
        let probe = std::process::Command::new(&programs.ffprobe).arg("-version").output().unwrap();
        assert!(probe.status.success());
    }

    #[cfg(unix)]
    /// Unpacks the programs in `bin` from a .tar.xz of `files`, each a path and its text, and `links`,
    /// each a path and where it points. Answers what unpacking said, and each file it wrote as
    /// `name: text`.
    fn unpacked(files: &[(&str, &str)], links: &[(&str, &str)], bin: &str) -> (Result<(), String>, Vec<String>) {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, text) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o755);
            tar.append_data(&mut header, path, text.as_bytes()).unwrap();
        }
        for (path, target) in links {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            tar.append_link(&mut header, path, target).unwrap();
        }
        let mut xz = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
        io::Write::write_all(&mut xz, &tar.into_inner().unwrap()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (archive, into) = (dir.path().join(ARCHIVE), dir.path().join("into"));
        std::fs::write(&archive, xz.finish().unwrap()).unwrap();
        std::fs::create_dir(&into).unwrap();
        let said = unpack(&archive, Packing::TarXz, Path::new(bin), &into).map_err(|e| e.to_string());
        let mut wrote: Vec<String> = std::fs::read_dir(&into)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                format!("{name}: {}", std::fs::read_to_string(&path).unwrap())
            })
            .collect();
        wrote.sort();
        (said, wrote)
    }

    // The builds they unpack are the Linux and macOS ones, whose programs have no .exe.
    #[cfg(unix)]
    #[test]
    fn only_the_ffmpeg_and_ffprobe_in_the_builds_folder_come_out_of_its_archive() {
        let btbn = [
            ("ffmpeg", "a decoy at the top"),
            ("ffmpeg-n8.1/bin/ffmpeg", "ffmpeg"),
            ("ffmpeg-n8.1/bin/ffplay", "ffplay"),
            ("ffmpeg-n8.1/bin/ffprobe", "ffprobe"),
            ("ffmpeg-n8.1/doc/ffprobe", "a decoy in another folder"),
            ("ffmpeg-n8.1/LICENSE.txt", "GPL"),
        ];
        let (said, wrote) = unpacked(&btbn, &[], "ffmpeg-n8.1/bin");
        assert_eq!(said, Ok(()));
        assert_eq!(wrote, ["ffmpeg: ffmpeg", "ffprobe: ffprobe"]);

        let jellyfin = [("ffmpeg", "ffmpeg"), ("ffprobe", "ffprobe"), ("LICENSE", "GPL")];
        let (said, wrote) = unpacked(&jellyfin, &[], "");
        assert_eq!(said, Ok(()));
        assert_eq!(wrote, ["ffmpeg: ffmpeg", "ffprobe: ffprobe"]);
    }

    // The builds they unpack are the Linux and macOS ones, whose programs have no .exe.
    #[cfg(unix)]
    #[test]
    fn a_link_in_the_archive_is_not_unpacked_as_a_program() {
        let files = [("ffmpeg-n8.1/bin/ffmpeg", "ffmpeg"), ("ffmpeg-n8.1/bin/real", "ffprobe")];
        let (said, wrote) = unpacked(&files, &[("ffmpeg-n8.1/bin/ffprobe", "real")], "ffmpeg-n8.1/bin");
        assert_eq!(said, Err("the archive has no ffprobe".to_string()));
        assert_eq!(wrote, ["ffmpeg: ffmpeg"]);
    }

    #[test]
    fn the_programs_come_out_of_a_zip_build_as_windows_builds_are() {
        let dir = tempfile::tempdir().unwrap();
        let (archive, into) = (dir.path().join(ARCHIVE), dir.path().join("into"));
        let mut zip = zip::ZipWriter::new(File::create(&archive).unwrap());
        let bin = |name: &str| format!("ffmpeg-n8.1/bin/{}", program_file(name));
        let files = [
            (bin("ffmpeg"), "ffmpeg"),
            (bin("ffprobe"), "ffprobe"),
            (bin("ffplay"), "ffplay"),
            ("LICENSE.txt".into(), "GPL"),
        ];
        for (name, text) in files {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            io::Write::write_all(&mut zip, text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        std::fs::create_dir(&into).unwrap();
        unpack(&archive, Packing::Zip, Path::new("ffmpeg-n8.1/bin"), &into).unwrap();
        let read = |name: &str| std::fs::read_to_string(into.join(program_file(name))).ok();
        assert_eq!(
            [read("ffmpeg"), read("ffprobe"), read("ffplay")],
            [Some("ffmpeg".into()), Some("ffprobe".into()), None]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_quarantined_program_loses_only_that_mark() {
        let dir = tempfile::tempdir().unwrap();
        let (marked, plain) = (dir.path().join("marked"), dir.path().join("plain"));
        std::fs::write(&marked, "").unwrap();
        std::fs::write(&plain, "").unwrap();
        let xattr = |args: &[&str], path: &Path| {
            let path = path.display().to_string();
            std::process::Command::new("/usr/bin/xattr").args(args).arg(path).output().unwrap().status.success()
        };
        assert!(xattr(&["-w", "com.apple.quarantine", "0083;00000000;torrential;"], &marked));
        assert!(xattr(&["-w", "org.torrential.kept", "yes"], &marked));
        unquarantine(&marked).unwrap();
        unquarantine(&plain).unwrap();
        assert!(!xattr(&["-p", "com.apple.quarantine"], &marked), "the mark is gone");
        assert!(xattr(&["-p", "org.torrential.kept"], &marked), "other attributes stay");
    }
}
