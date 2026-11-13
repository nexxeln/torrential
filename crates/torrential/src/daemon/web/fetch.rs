//! The engine's own ffmpeg: a pinned build it fetches the first time a file needs converting and
//! none is installed.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use torrential_core::{Error, ErrorCode};

use super::ffmpeg::{self, INSTALL, Programs, Tools};
use crate::env::Env;

/// jellyfin-ffmpeg's portable builds, which link only to what every system has.
const RELEASE: &str = "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v8.1.3-2";
pub const VERSION: &str = "8.1.3-2";

struct Pin {
    os: &'static str,
    arch: &'static str,
    file: &'static str,
    sha256: &'static str,
    size: u64,
}

/// One for each platform torrential releases for, by `std::env::consts` names. The hashes are the
/// ones GitHub lists for the release's files.
const PINS: [Pin; 3] = [
    Pin {
        os: "macos",
        arch: "aarch64",
        file: "jellyfin-ffmpeg_8.1.3-2_portable_macarm64-gpl.tar.xz",
        sha256: "d898cc819e7c4e9219d6410ddbdaada0838f14bec6ff89e283639915394cb43f",
        size: 32_904_768,
    },
    Pin {
        os: "linux",
        arch: "x86_64",
        file: "jellyfin-ffmpeg_8.1.3-2_portable_linux64-gpl.tar.xz",
        sha256: "1df84d04c4322902dba81c290508cdf3d89524d210b9fe03154bea6c4e3bfbb6",
        size: 60_657_368,
    },
    Pin {
        os: "linux",
        arch: "aarch64",
        file: "jellyfin-ffmpeg_8.1.3-2_portable_linuxarm64-gpl.tar.xz",
        sha256: "6522585818059cc886363fb1dee86ac9b8579fec3b914337fb92974ef9eb9884",
        size: 54_093_080,
    },
];

const PROGRAMS: [&str; 2] = ["ffmpeg", "ffprobe"];
const ARCHIVE: &str = "archive.tar.xz";
/// Where a fetch puts its files until they are checked.
const PARTIAL: &str = ".partial";
/// A fresh program can take a while to start the first time, while macOS scans it.
const RUN_WITHIN: Duration = Duration::from_secs(30);
const REPORT_EVERY: Duration = Duration::from_millis(200);

/// A .tar.xz holding ffmpeg and ffprobe, and what it must hash to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Build {
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    /// Bytes. A download of any other size is not this build.
    pub size: u64,
}

impl Build {
    pub fn pinned(os: &str, arch: &str) -> Option<Build> {
        PINS.iter().find(|pin| pin.os == os && pin.arch == arch).map(|pin| Build {
            url: format!("{RELEASE}/{}", pin.file),
            sha256: pin.sha256.to_string(),
            size: pin.size,
        })
    }
}

/// What a conversion does when no ffmpeg is installed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fetch {
    Build(Build),
    /// config.yaml turns fetching off.
    Off,
    /// torrential has no build for this machine.
    Unavailable,
}

impl Fetch {
    /// The pinned build for this machine, unless `allowed` is false. `TORRENTIAL_TEST_FFMPEG_URL`
    /// and `TORRENTIAL_TEST_FFMPEG_SHA256` replace its URL and its hash. A test engine fetches only
    /// from a URL its test gives.
    pub fn from_env(env: &Env, allowed: bool) -> Fetch {
        let Some(mut build) = Build::pinned(std::env::consts::OS, std::env::consts::ARCH) else {
            return Fetch::Unavailable;
        };
        match env.var("TORRENTIAL_TEST_FFMPEG_URL") {
            Some(url) => build.url = url.to_string(),
            None if env.var("TORRENTIAL_TEST_NETWORK").is_some() || env.var("TORRENTIAL_TEST_ENGINE").is_some() => {
                return Fetch::Unavailable;
            }
            None => {}
        }
        if let Some(sha256) = env.var("TORRENTIAL_TEST_FFMPEG_SHA256") {
            build.sha256 = sha256.to_ascii_lowercase();
        }
        if let Some(size) = env.var("TORRENTIAL_TEST_FFMPEG_SIZE").and_then(|size| size.parse().ok()) {
            build.size = size;
        }
        if allowed { Fetch::Build(build) } else { Fetch::Off }
    }

    /// What a conversion gets when there is no ffmpeg here.
    fn missing(&self) -> Error {
        let (message, hint) = match self {
            Fetch::Build(_) => (
                "torrential has not fetched ffmpeg yet, and this needs it",
                format!("play the file in the web app once, which fetches it, or {INSTALL}"),
            ),
            Fetch::Off => (
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
    /// `root` is the folder fetched builds go in.
    pub fn look(installed: &Tools, root: &Path, fetch: &Fetch) -> Found {
        if let Some(programs) = installed.both() {
            return Found::Installed(programs);
        }
        match Tools::find_in(&[&root.join(VERSION)]).both() {
            Some(programs) => Found::Fetched(programs),
            None => Found::Missing(fetch.clone()),
        }
    }

    pub fn readiness(&self) -> Readiness {
        match self {
            Found::Installed(_) | Found::Fetched(_) => Readiness::Ready,
            Found::Missing(Fetch::Build(_)) => Readiness::Fetch,
            Found::Missing(Fetch::Off | Fetch::Unavailable) => Readiness::Missing,
        }
    }
}

/// Whether the engine converts a file now, once it has fetched ffmpeg, or not at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    Ready,
    Fetch,
    Missing,
}

/// Where fetched builds go: one folder for each version, in the engine's state folder.
pub fn root(state: &Path) -> PathBuf {
    state.join("ffmpeg")
}

/// How a fetch is going.
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
    /// The fetch under way, or the last one.
    running: Mutex<Option<watch::Receiver<Stage>>>,
}

impl Ffmpeg {
    pub fn new(installed: Arc<dyn Fn() -> Tools + Send + Sync>, root: PathBuf, fetch: Fetch) -> Ffmpeg {
        Ffmpeg { installed, root, fetch, running: Mutex::default() }
    }

    pub fn found(&self) -> Found {
        Found::look(&(self.installed)(), &self.root, &self.fetch)
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
        let mut running = self.running.lock().expect("no holder panics");
        let live =
            |stage: &&watch::Receiver<Stage>| !matches!(*stage.borrow(), Stage::Done(_)) && stage.has_changed().is_ok();
        if let Some(stage) = running.as_ref().filter(live) {
            return Ok(Fetching::Running(stage.clone()));
        }
        let (report, stage) = watch::channel(Stage::Downloading { done: 0, size: build.size });
        let root = self.root.clone();
        tokio::spawn(async move {
            let done = fetch(&build, &root, &report).await;
            report.send_replace(Stage::Done(done));
        });
        *running = Some(stage.clone());
        Ok(Fetching::Running(stage))
    }
}

/// Downloads `build`, checks its hash, unpacks ffmpeg and ffprobe, and checks that they run, into
/// `root`'s folder for this version. Anything else in `root` is removed. A failure leaves nothing.
async fn fetch(build: &Build, root: &Path, report: &watch::Sender<Stage>) -> Result<(), Error> {
    let folder = root.join(VERSION);
    // A request that looked before the last fetch finished starts another, which has nothing to do.
    if Tools::find_in(&[&folder]).both().is_some() {
        return Ok(());
    }
    let started = Instant::now();
    super::super::log(&format!("fetching ffmpeg {VERSION} from {}", build.url));
    let fetched = async {
        let partial = Partial::create(root)?;
        let archive = partial.path.join(ARCHIVE);
        download(build, &archive, report).await?;
        let into = partial.path.clone();
        tokio::task::spawn_blocking(move || unpack(&archive, &into))
            .await
            .map_err(|e| Error::internal(format!("the ffmpeg unpacking stopped: {e}")))?
            .map_err(|e| {
                Error::new(ErrorCode::StorageError, format!("cannot unpack ffmpeg into {}: {e}", root.display()))
                    .with_hint("free some space or fix the folder's permissions, then try again")
            })?;
        for program in PROGRAMS {
            runs(&partial.path.join(program)).await?;
        }
        partial.finish(&folder)
    }
    .await;
    match &fetched {
        Ok(()) => super::super::log(&format!("fetched ffmpeg {VERSION} in {} s", started.elapsed().as_secs())),
        Err(error) => super::super::log(&format!("cannot fetch ffmpeg: {}", error.message)),
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
    let mut file = tokio::fs::File::create(to).await.map_err(|e| unwritable(to, &e))?;
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

/// Writes the archive's ffmpeg and ffprobe into `into`, and nothing else from it.
fn unpack(archive: &Path, into: &Path) -> io::Result<()> {
    let xz = lzma_rust2::XzReader::new(BufReader::new(File::open(archive)?), true);
    let mut tar = tar::Archive::new(xz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let path = path.strip_prefix(".").unwrap_or(&path);
        let program = PROGRAMS.into_iter().find(|program| path == Path::new(program));
        if let Some(program) = program.filter(|_| entry.header().entry_type().is_file()) {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o755).open(into.join(program))?;
            io::copy(&mut entry, &mut file)?;
        }
    }
    for program in PROGRAMS {
        let path = into.join(program);
        if !path.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("the archive has no {program}")));
        }
        unquarantine(&path)?;
    }
    Ok(())
}

/// Checks that a fetched program starts here, so a build macOS refuses, or one this Linux cannot
/// load, fails now and not in the player.
async fn runs(program: &Path) -> Result<(), Error> {
    let run = tokio::process::Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let why = match tokio::time::timeout(RUN_WITHIN, run).await {
        Ok(Ok(output)) if output.status.success() => return Ok(()),
        Ok(Ok(output)) => ffmpeg::complaint(&String::from_utf8_lossy(&output.stderr)),
        Ok(Err(e)) => e.to_string(),
        Err(_) => format!("it did not answer within {} s", RUN_WITHIN.as_secs()),
    };
    let name = program.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    Err(Error::new(
        ErrorCode::FetchFailed,
        format!("the {name} torrential fetched does not run on this machine: {why}"),
    )
    .with_hint(format!("{INSTALL}, then try again")))
}

/// The folder a fetch fills before its files are checked. Dropping it deletes it, and `root` with
/// it when nothing else is there.
struct Partial {
    path: PathBuf,
}

impl Partial {
    /// Empties `root` of what earlier fetches left, other versions included, and makes the folder.
    fn create(root: &Path) -> Result<Partial, Error> {
        let made = (|| {
            if root.exists() {
                for entry in std::fs::read_dir(root)? {
                    let path = entry?.path();
                    if path.is_dir() { std::fs::remove_dir_all(&path)? } else { std::fs::remove_file(&path)? }
                }
            }
            let path = root.join(PARTIAL);
            std::fs::create_dir_all(&path)?;
            Ok(Partial { path })
        })();
        made.map_err(|e: io::Error| unwritable(root, &e))
    }

    /// Moves the programs to `folder`, where the engine finds them.
    fn finish(self, folder: &Path) -> Result<(), Error> {
        std::fs::remove_file(self.path.join(ARCHIVE))
            .and_then(|()| std::fs::rename(&self.path, folder))
            .map_err(|e| unwritable(folder, &e))
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
    fn apple_silicon_and_linux_on_arm64_and_x86_64_have_a_pinned_build() {
        let pinned = |os, arch| Build::pinned(os, arch).map(|build| build.url.rsplit('/').next().unwrap().to_string());
        assert_eq!(pinned("macos", "aarch64").as_deref(), Some("jellyfin-ffmpeg_8.1.3-2_portable_macarm64-gpl.tar.xz"));
        assert_eq!(pinned("linux", "x86_64").as_deref(), Some("jellyfin-ffmpeg_8.1.3-2_portable_linux64-gpl.tar.xz"));
        assert_eq!(
            pinned("linux", "aarch64").as_deref(),
            Some("jellyfin-ffmpeg_8.1.3-2_portable_linuxarm64-gpl.tar.xz")
        );
        assert_eq!(pinned("macos", "x86_64"), None);
        assert_eq!(pinned("windows", "x86_64"), None);
    }

    #[test]
    fn a_test_engine_fetches_only_from_where_its_test_points_it() {
        let fetch = |vars: &[(&str, &str)], allowed| Fetch::from_env(&Env::from_pairs(vars.iter().copied()), allowed);
        let pinned = Build::pinned(std::env::consts::OS, std::env::consts::ARCH);
        let Some(pinned) = pinned else {
            assert_eq!(fetch(&[], true), Fetch::Unavailable);
            return;
        };
        assert_eq!(fetch(&[], true), Fetch::Build(pinned.clone()));
        assert_eq!(fetch(&[], false), Fetch::Off);
        assert_eq!(fetch(&[("TORRENTIAL_TEST_ENGINE", "fake")], true), Fetch::Unavailable);
        let served = [
            ("TORRENTIAL_TEST_NETWORK", "loopback"),
            ("TORRENTIAL_TEST_FFMPEG_URL", "http://127.0.0.1:9/ffmpeg.tar.xz"),
            ("TORRENTIAL_TEST_FFMPEG_SHA256", "ABC123"),
            ("TORRENTIAL_TEST_FFMPEG_SIZE", "4096"),
        ];
        let build = Build { url: "http://127.0.0.1:9/ffmpeg.tar.xz".into(), sha256: "abc123".into(), size: 4096 };
        assert_eq!(fetch(&served, true), Fetch::Build(build));
    }

    #[test]
    fn an_installed_ffmpeg_wins_over_the_fetched_one() {
        let dir = tempfile::tempdir().unwrap();
        let (installed, root) = (dir.path().join("bin"), dir.path().join("ffmpeg"));
        for folder in [&installed, &root.join(VERSION)] {
            std::fs::create_dir_all(folder).unwrap();
            for program in PROGRAMS {
                std::fs::write(folder.join(program), "").unwrap();
            }
        }
        let programs = |folder: &Path| Programs { ffmpeg: folder.join("ffmpeg"), ffprobe: folder.join("ffprobe") };
        let system = Tools::find_in(&[&installed]);
        assert_eq!(Found::look(&system, &root, &Fetch::Off), Found::Installed(programs(&installed)));
        let fetched = Found::look(&Tools::default(), &root, &Fetch::Off);
        assert_eq!(fetched, Found::Fetched(programs(&root.join(VERSION))));
        assert_eq!(fetched.readiness(), Readiness::Ready);
        let only_ffmpeg = Tools { ffmpeg: system.ffmpeg, ffprobe: None };
        std::fs::remove_dir_all(root.join(VERSION)).unwrap();
        let missing = |fetch| Found::look(&only_ffmpeg, &root, &fetch).readiness();
        let build = Build { url: String::new(), sha256: String::new(), size: 0 };
        assert_eq!(
            [missing(Fetch::Build(build)), missing(Fetch::Off), missing(Fetch::Unavailable)],
            [Readiness::Fetch, Readiness::Missing, Readiness::Missing]
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
