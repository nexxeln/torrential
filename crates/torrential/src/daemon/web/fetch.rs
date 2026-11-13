//! The engine's own ffmpeg: a pinned build it fetches the first time a file needs converting and
//! none is installed.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, BufReader};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use torrential_core::{Error, ErrorCode};

use super::ffmpeg::{self, INSTALL, Programs, Tools};
use crate::env::Env;

/// jellyfin-ffmpeg's portable builds, which link only to the system's own libraries: glibc on
/// Linux, and `/usr/lib` and the system frameworks on macOS.
const RELEASE: &str = "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v8.1.3-2";
pub const VERSION: &str = "8.1.3-2";

struct Pin {
    os: &'static str,
    arch: &'static str,
    file: &'static str,
    sha256: &'static str,
    size: u64,
    /// The glibc loader the programs start with, from the system's root folder. A Linux without
    /// glibc, such as Alpine, has none and cannot run them.
    loader: Option<&'static str>,
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
        loader: None,
    },
    Pin {
        os: "linux",
        arch: "x86_64",
        file: "jellyfin-ffmpeg_8.1.3-2_portable_linux64-gpl.tar.xz",
        sha256: "1df84d04c4322902dba81c290508cdf3d89524d210b9fe03154bea6c4e3bfbb6",
        size: 60_657_368,
        loader: Some("lib64/ld-linux-x86-64.so.2"),
    },
    Pin {
        os: "linux",
        arch: "aarch64",
        file: "jellyfin-ffmpeg_8.1.3-2_portable_linuxarm64-gpl.tar.xz",
        sha256: "6522585818059cc886363fb1dee86ac9b8579fec3b914337fb92974ef9eb9884",
        size: 54_093_080,
        loader: Some("lib/ld-linux-aarch64.so.1"),
    },
];

const PROGRAMS: [&str; 2] = ["ffmpeg", "ffprobe"];
const ARCHIVE: &str = "archive.tar.xz";
const MANIFEST: &str = "manifest.json";
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
    /// The build for `os` and `arch`, when the system whose root folder is `system` can run it.
    pub fn pinned(os: &str, arch: &str, system: &Path) -> Option<Build> {
        let pin = PINS.iter().find(|pin| pin.os == os && pin.arch == arch)?;
        if pin.loader.is_some_and(|loader| !system.join(loader).exists()) {
            return None;
        }
        Some(Build { url: format!("{RELEASE}/{}", pin.file), sha256: pin.sha256.to_string(), size: pin.size })
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
        if env.var("TORRENTIAL_TEST_NETWORK").is_some() || env.var("TORRENTIAL_TEST_ENGINE").is_some() {
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
    /// The fetched build did not run here, so this engine run does not fetch it again.
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
            let checked = self.fetch.pin().and_then(|pin| check(&self.root, pin));
            *fetched = checked.map_or(Fetched::Absent, Fetched::Ready);
        }
        match &*fetched {
            Fetched::Ready(programs) => Found::Fetched(programs.clone()),
            _ => Found::Missing(self.fetch.clone()),
        }
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
                Err(Failure::Refused(error)) => (Fetched::Refused(error.clone()), Err(error)),
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
            let path = folder.join(program);
            let meta = path.symlink_metadata()?;
            if !meta.is_file() || meta.mode() & 0o100 == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{program} is not a program")));
            }
            let mut hash = Sha256::new();
            io::copy(&mut File::open(&path)?, &mut hash)?;
            Ok(format!("{:x}", hash.finalize()))
        };
        Ok(Manifest { build: build.sha256.clone(), ffmpeg: hash("ffmpeg")?, ffprobe: hash("ffprobe")? })
    }

    fn write(&self, folder: &Path) -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(folder.join(MANIFEST))?;
        io::Write::write_all(&mut file, &serde_json::to_vec(self)?)
    }
}

/// The programs `root`'s folder for this version holds, when only this user can change the
/// folders and the programs are those its manifest records for `pin`.
fn check(root: &Path, pin: &Build) -> Option<Programs> {
    let folder = root.join(VERSION);
    for dir in [root.parent().unwrap_or(root), root, &folder] {
        private(dir).ok()?;
    }
    let recorded: Manifest = serde_json::from_slice(&std::fs::read(folder.join(MANIFEST)).ok()?).ok()?;
    (recorded == Manifest::of(&folder, pin).ok()?).then(|| programs(&folder))
}

fn programs(folder: &Path) -> Programs {
    Programs { ffmpeg: folder.join("ffmpeg"), ffprobe: folder.join("ffprobe") }
}

/// Why a fetch brought no ffmpeg.
enum Failure {
    /// Another try can work: the network, the disk, or a download that is not the build.
    Again(Error),
    /// The build does not run on this machine, so another download would not help.
    Refused(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure::Again(error)
    }
}

/// Downloads `build`, checks its hash, unpacks ffmpeg and ffprobe with a manifest of them, and
/// checks that they run, into `root`'s folder for this version. Anything else in `root` is removed.
/// A failure leaves nothing.
async fn fetch(build: &Build, root: &Path, report: &watch::Sender<Stage>) -> Result<Programs, Failure> {
    let folder = root.join(VERSION);
    let started = Instant::now();
    super::super::log(&format!("fetching ffmpeg {VERSION} from {}", build.url));
    let fetched: Result<Programs, Failure> = async {
        let partial = Partial::create(root)?;
        let archive = partial.path.join(ARCHIVE);
        download(build, &archive, report).await?;
        let (into, unpacked) = (partial.path.clone(), build.clone());
        tokio::task::spawn_blocking(move || {
            unpack(&archive, &into)?;
            Manifest::of(&into, &unpacked)?.write(&into)
        })
        .await
        .map_err(|e| Error::internal(format!("the ffmpeg unpacking stopped: {e}")))?
        .map_err(|e| {
            Error::new(ErrorCode::StorageError, format!("cannot unpack ffmpeg into {}: {e}", root.display()))
                .with_hint("free some space or fix the folder's permissions, then try again")
        })?;
        for program in PROGRAMS {
            runs(&partial.path.join(program)).await.map_err(Failure::Refused)?;
        }
        Ok(partial.finish(&folder)?)
    }
    .await;
    match &fetched {
        Ok(_) => super::super::log(&format!("fetched ffmpeg {VERSION} in {} s", started.elapsed().as_secs())),
        Err(Failure::Again(error) | Failure::Refused(error)) => {
            super::super::log(&format!("cannot fetch ffmpeg: {}", error.message));
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
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)
        .await
        .map_err(|e| unwritable(to, &e))?;
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
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o700).open(into.join(program))?;
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
        private(root.parent().unwrap_or(root))?;
        let mut folder = DirBuilder::new();
        folder.mode(0o700);
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
    } else if meta.uid() != crate::paths::uid() {
        ("it belongs to another user", "make it yours with chown, then try again".to_string())
    } else if meta.mode() & 0o022 != 0 {
        ("other users can write to it", format!("run chmod go-w {}, then try again", folder.display()))
    } else {
        return Ok(());
    };
    Err(Error::new(ErrorCode::StorageError, format!("torrential will not keep ffmpeg in {}: {why}", folder.display()))
        .with_hint(hint))
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
        let system = tempfile::tempdir().unwrap();
        for loader in ["lib64/ld-linux-x86-64.so.2", "lib/ld-linux-aarch64.so.1"] {
            std::fs::create_dir_all(system.path().join(loader).parent().unwrap()).unwrap();
            std::fs::write(system.path().join(loader), "").unwrap();
        }
        let pinned = |os, arch| {
            Build::pinned(os, arch, system.path()).map(|build| build.url.rsplit('/').next().unwrap().to_string())
        };
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
    fn a_linux_without_the_glibc_loader_its_build_starts_with_has_no_build() {
        let system = tempfile::tempdir().unwrap();
        let pinned = |os, arch| {
            Build::pinned(os, arch, system.path()).map(|build| build.url.rsplit('/').next().unwrap().to_string())
        };
        assert_eq!([pinned("linux", "x86_64"), pinned("linux", "aarch64")], [None, None]);
        assert_eq!(pinned("macos", "aarch64").as_deref(), Some("jellyfin-ffmpeg_8.1.3-2_portable_macarm64-gpl.tar.xz"));
        for loader in ["lib64/ld-linux-x86-64.so.2", "lib/ld-linux-aarch64.so.1"] {
            std::fs::create_dir_all(system.path().join(loader).parent().unwrap()).unwrap();
            std::fs::write(system.path().join(loader), "").unwrap();
        }
        assert_eq!(pinned("linux", "x86_64").as_deref(), Some("jellyfin-ffmpeg_8.1.3-2_portable_linux64-gpl.tar.xz"));
        assert_eq!(
            pinned("linux", "aarch64").as_deref(),
            Some("jellyfin-ffmpeg_8.1.3-2_portable_linuxarm64-gpl.tar.xz")
        );
    }

    #[test]
    fn an_engine_fetches_the_pinned_build_whatever_its_environment_says() {
        let fetch = |vars: &[(&str, &str)], allowed| Fetch::from_env(&Env::from_pairs(vars.iter().copied()), allowed);
        let pinned = Build::pinned(std::env::consts::OS, std::env::consts::ARCH, Path::new("/"));
        let Some(pinned) = pinned else {
            assert_eq!(fetch(&[], true), Fetch::Unavailable);
            return;
        };
        let elsewhere = [
            ("TORRENTIAL_TEST_FFMPEG_URL", "http://127.0.0.1:9/ffmpeg.tar.xz"),
            ("TORRENTIAL_TEST_FFMPEG_SHA256", "ABC123"),
            ("TORRENTIAL_TEST_FFMPEG_SIZE", "4096"),
        ];
        assert_eq!(fetch(&elsewhere, true), Fetch::Build(pinned.clone()));
        assert_eq!(fetch(&elsewhere, false), Fetch::Off(pinned));
        assert_eq!(fetch(&[("TORRENTIAL_TEST_ENGINE", "fake"), elsewhere[0]], true), Fetch::Unavailable);
        assert_eq!(fetch(&[("TORRENTIAL_TEST_NETWORK", "loopback"), elsewhere[0]], true), Fetch::Unavailable);
    }

    #[test]
    fn an_installed_ffmpeg_wins_over_the_fetched_one() {
        let dir = tempfile::tempdir().unwrap();
        let (installed, root) = (dir.path().join("bin"), dir.path().join("ffmpeg"));
        let fetched = root.join(VERSION);
        let build = Build { url: String::new(), sha256: "ab".repeat(32), size: 0 };
        for folder in [&installed, &fetched] {
            DirBuilder::new().recursive(true).mode(0o700).create(folder).unwrap();
            for program in PROGRAMS {
                OpenOptions::new().write(true).create_new(true).mode(0o700).open(folder.join(program)).unwrap();
            }
        }
        Manifest::of(&fetched, &build).unwrap().write(&fetched).unwrap();
        let found =
            |installed: Tools, fetch| Ffmpeg::new(Arc::new(move || installed.clone()), root.clone(), fetch).found();
        let system = Tools { ffmpeg: Some(installed.join("ffmpeg")), ffprobe: Some(installed.join("ffprobe")) };
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
