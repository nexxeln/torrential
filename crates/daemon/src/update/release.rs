//! Fetching a release, checking it, trying it as an engine, and installing it.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Child;

use torrential_core::protocol::{Request, Response, WIRE_VERSION};
use torrential_core::{Error, ErrorCode};

use crate::env::{Env, hooks};
use crate::fsutil::{Writes, sha256};
use crate::paths::Paths;
use crate::wire::{self, Access, Client};

use super::{Checked, Version, tag_version, target};

const RELEASES: &str = "https://github.com/nexxeln/torrential/releases";
const ISSUES: &str = "https://github.com/nexxeln/torrential/issues";
const CHECK_WITHIN: Duration = Duration::from_secs(10);
const MAX_ASSET: usize = 256 << 20;
const CONFIRM_WITHIN: Duration = Duration::from_secs(10);
/// How long a release has, in its trial run as an engine, to answer and then to stop.
const TRIAL_WITHIN: Duration = Duration::from_secs(20);

pub struct Releases {
    base: String,
}

impl Releases {
    /// The GitHub releases. A test reads releases only from the server it names in `RELEASES`.
    pub fn from_env(env: &Env) -> Option<Releases> {
        let base = match env.test() {
            Some(test) => test.var(hooks::RELEASES)?,
            None => RELEASES,
        };
        Some(Releases { base: base.trim_end_matches('/').to_string() })
    }

    pub fn required(env: &Env) -> Result<Releases, Error> {
        Releases::from_env(env).ok_or_else(|| {
            Error::new(ErrorCode::NetworkUnavailable, "this test environment does not look for releases")
                .with_hint(format!("set {} to a release server", hooks::RELEASES))
        })
    }

    async fn latest(&self) -> Result<Version, Error> {
        let url = format!("{}/latest", self.base);
        let client =
            client(reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(CHECK_WITHIN))?;
        let response = client.get(&url).send().await.map_err(|e| unreachable(&url, &e))?;
        let location = response.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok());
        match location.and_then(tag_version) {
            Some(latest) if response.status().is_redirection() => Ok(latest),
            _ => Err(Error::new(
                ErrorCode::FetchFailed,
                format!("{url} returned {} without a release version", response.status()),
            )
            .with_hint("run torrential update again later")),
        }
    }

    async fn download(&self, version: Version, asset: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{}/download/v{version}/{asset}", self.base);
        let client = client(
            reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).read_timeout(Duration::from_secs(30)),
        )?;
        let mut response = client.get(&url).send().await.map_err(|e| unreachable(&url, &e))?;
        if !response.status().is_success() {
            return Err(Error::new(ErrorCode::FetchFailed, format!("{url} returned {}", response.status()))
                .with_hint("run torrential update again later"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| unreachable(&url, &e))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_ASSET {
                return Err(Error::new(ErrorCode::FetchFailed, format!("{url} is larger than 256 MiB")));
            }
        }
        Ok(body)
    }
}

fn client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client, Error> {
    builder
        .user_agent(concat!("torrential/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::internal(format!("cannot build an HTTP client: {e}")))
}

fn unreachable(url: &str, e: &reqwest::Error) -> Error {
    let mut cause: &dyn std::error::Error = e;
    while let Some(source) = cause.source() {
        cause = source;
    }
    Error::new(ErrorCode::NetworkUnavailable, format!("cannot reach {url}: {cause}"))
        .with_hint("check the network connection, then run torrential update again")
}

pub fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::internal(format!("cannot start the async runtime: {e}")))
}

pub async fn check(releases: &Releases, paths: &Paths, now: i64) -> Result<Version, Error> {
    let mut checked = Checked::read(paths);
    let latest = releases.latest().await;
    match &latest {
        Ok(version) => {
            checked.latest = Some(version.to_string());
            checked.checked_at = Some(now);
        }
        Err(_) => checked.failed_at = Some(now),
    }
    checked.write(paths);
    latest
}

pub enum Outcome {
    Latest(Version),
    Updated(Version),
}

pub async fn install(releases: &Releases, paths: &Paths, env: &Env, now: i64) -> Result<Outcome, Error> {
    let latest = check(releases, paths, now).await?;
    if !latest.newer_than(&env.version()) {
        return Ok(Outcome::Latest(latest));
    }
    let (exe, target) = installed(env)?;
    let installed = match put_release(releases, env, latest, &exe, target, || true).await? {
        Put::Installed | Put::NotWanted => latest,
        Put::Current(installed) => installed,
        Put::Refused(error) => return Err(error),
    };
    let _ = Command::new(&exe)
        .args(["skill", "refresh"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(Outcome::Updated(installed))
}

/// What `put_release` found or did.
pub(crate) enum Put {
    Installed,
    /// This release or a newer one is installed already, by another torrential that runs from the
    /// same executable, such as the engine of another home.
    Current(Version),
    /// `wanted` said no before the download or before the binary went in.
    NotWanted,
    /// The release's binary does not run here, says it is another release, or fails as an engine.
    Refused(Error),
}

/// Downloads release `version` for `target` and puts its binary in place of `exe`, once the new
/// binary says it is the release and runs as an engine. It asks `wanted` right before the download
/// and right before the binary goes in, and holds the lock on the folder of `exe` throughout, so
/// torrentials that share the executable install one release at a time.
pub(crate) async fn put_release(
    releases: &Releases,
    env: &Env,
    version: Version,
    exe: &Path,
    target: &str,
    wanted: impl Fn() -> bool,
) -> Result<Put, Error> {
    let _lock = FolderLock::take(exe.parent().unwrap_or(Path::new("/"))).await?;
    if let Some(installed) = version_of(exe).await.filter(|installed| *installed >= version) {
        return Ok(Put::Current(installed));
    }
    let writes = Writes::new(env, "torrential update".into());
    let partial = Partial::create(exe, &writes)?;
    if !wanted() {
        return Ok(Put::NotWanted);
    }
    partial.write(&fetch(releases, version, target).await?, exe, &writes)?;
    let tried = async {
        confirm(&partial.path, version).await?;
        trial(&partial.path, version, env).await
    };
    if let Err(error) = tried.await {
        return match error.code {
            ErrorCode::VerificationFailed => Ok(Put::Refused(error)),
            _ => Err(error),
        };
    }
    if !wanted() {
        return Ok(Put::NotWanted);
    }
    partial.place(exe, &writes)?;
    Ok(Put::Installed)
}

/// An exclusive lock on a folder, held until it drops.
struct FolderLock {
    _folder: std::fs::File,
}

impl FolderLock {
    async fn take(dir: &Path) -> Result<FolderLock, Error> {
        use std::os::fd::AsRawFd;
        let failed =
            |e: std::io::Error| Error::new(ErrorCode::StorageError, format!("cannot lock {}: {e}", dir.display()));
        let folder = std::fs::File::open(dir).map_err(failed)?;
        while unsafe { libc::flock(folder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(failed(e));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(FolderLock { _folder: folder })
    }
}

/// Where this torrential is installed and which release asset fits this machine, or why it cannot
/// update itself.
pub(crate) fn installed(env: &Env) -> Result<(PathBuf, &'static str), Error> {
    let Some(target) = target(std::env::consts::OS, std::env::consts::ARCH) else {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("there is no prebuilt torrential for {} {}", std::env::consts::OS, std::env::consts::ARCH),
        )
        .with_hint("build it from source: https://github.com/nexxeln/torrential#install"));
    };
    let exe = env.installed()?;
    if exe.starts_with("/nix/store") {
        return Err(Error::new(
            ErrorCode::StorageError,
            format!("torrential is installed at {}, which is managed by Nix", exe.display()),
        )
        .with_hint("update it with Nix, for example nix profile upgrade torrential"));
    }
    Ok((exe, target))
}

/// Downloads the binary of release `version` for `target` and checks it against its published checksum.
async fn fetch(releases: &Releases, version: Version, target: &str) -> Result<Vec<u8>, Error> {
    let asset = format!("torrential-{target}.tar.gz");
    let tarball = releases.download(version, &asset).await?;
    let sums = releases.download(version, &format!("{asset}.sha256")).await?;
    verify(&asset, &tarball, &sums)?;
    unpack(&asset, &tarball)
}

/// Runs `binary --version` and accepts it only when it says it is torrential `version`. A binary
/// for another system, a damaged one, or one from another release fails here, before it replaces
/// anything.
async fn confirm(binary: &Path, version: Version) -> Result<(), Error> {
    match says(binary).await {
        Ok(said) if said == format!("torrential {version}") => Ok(()),
        Ok(said) => Err(refused(version, format!("says it is {said}"))),
        Err(why) => Err(refused(version, why)),
    }
}

/// The torrential version `binary --version` says it is.
async fn version_of(binary: &Path) -> Option<Version> {
    Version::parse(says(binary).await.ok()?.strip_prefix("torrential ")?)
}

/// What `binary --version` prints, or why it prints nothing.
async fn says(binary: &Path) -> Result<String, String> {
    let mut child = tokio::process::Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("does not run here ({e})"))?;
    let status = exited(&mut child, CONFIRM_WITHIN).await.ok_or("does not answer --version")?;
    let mut said = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut said).await;
    }
    match said.trim() {
        said if status.success() && !said.is_empty() => Ok(said.to_string()),
        _ => Err(format!("does not run here ({status})")),
    }
}

/// Waits up to `within` for `child` to exit.
async fn exited(child: &mut Child, within: Duration) -> Option<ExitStatus> {
    tokio::time::timeout(within, child.wait()).await.ok()?.ok()
}

/// Starts `binary` as an engine in a home and on a port of its own, and accepts it once that engine
/// answers a status request and then stops cleanly. A release that answers `--version` but fails as
/// an engine fails here, before it replaces anything.
async fn trial(binary: &Path, version: Version, env: &Env) -> Result<(), Error> {
    let home = Scratch(std::env::temp_dir().join(format!("torrential-trial.{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&home.0);
    let unprepared =
        |e: std::io::Error| Error::new(ErrorCode::StorageError, format!("cannot prepare {}: {e}", home.0.display()));
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(home.0.join("config")).map_err(unprepared)?;
    std::fs::write(home.0.join("config/config.yaml"), "updates: off\n").map_err(unprepared)?;
    let paths =
        Paths::resolve(&env.clone().with("TORRENTIAL_HOME", &home.0.display().to_string()), crate::paths::uid(), None)?;
    // Killed if the trial drops before it exits, as it does when the engine stops mid-update.
    let mut engine = tokio::process::Command::new(binary)
        .args(["daemon", "run", "--http-port", "0"])
        .env("HOME", &home.0)
        .env("TORRENTIAL_HOME", &home.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| refused(version, format!("does not run here ({e})")))?;
    serves(&mut engine, &paths, env).await.map_err(|why| refused(version, why))?;
    match exited(&mut engine, TRIAL_WITHIN).await {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(refused(version, format!("does not stop its engine cleanly ({status})"))),
        None => Err(refused(version, format!("does not stop its engine within {}s", TRIAL_WITHIN.as_secs()))),
    }
}

/// Waits until `engine`, in `paths`, answers a status request, then asks it to stop.
async fn serves(engine: &mut Child, paths: &Paths, env: &Env) -> Result<(), String> {
    let deadline = Instant::now() + TRIAL_WITHIN;
    let access = loop {
        if let Ok(Some(status)) = engine.try_wait() {
            return Err(format!("does not start an engine ({status})"));
        }
        if let Ok(Some(access)) = Access::read(&paths.engine_token()) {
            break access;
        }
        if Instant::now() > deadline {
            return Err(format!("does not start an engine within {}s", TRIAL_WITHIN.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let version = env.version();
    let answered = tokio::task::spawn_blocking(move || {
        let client = Client { version: &version, wire: WIRE_VERSION };
        let status = wire::ask(&access, client, &Request::Hello)
            .and_then(|_| wire::ask(&access, client, &Request::Status { refs: Vec::new() }));
        let _ = wire::ask(&access, client, &Request::Shutdown { asked: true });
        status
    });
    match tokio::time::timeout(2 * TRIAL_WITHIN, answered).await {
        Ok(Ok(Ok(Response::Jobs { .. }))) => Ok(()),
        // A release with another protocol refuses this torrential's requests, which shows it runs.
        Ok(Ok(Ok(Response::Error { error }))) if error.code == ErrorCode::DaemonVersion => Ok(()),
        _ => Err("starts an engine that does not answer".into()),
    }
}

/// A folder removed when this drops.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn refused(version: Version, why: String) -> Error {
    Error::new(
        ErrorCode::VerificationFailed,
        format!("the binary of torrential {version} {why}, so the installed torrential is unchanged"),
    )
    .with_hint(format!("report it at {ISSUES}"))
}

fn verify(asset: &str, tarball: &[u8], sums: &[u8]) -> Result<(), Error> {
    let published = std::str::from_utf8(sums).ok().and_then(|s| s.split_whitespace().next()).unwrap_or_default();
    if published.eq_ignore_ascii_case(&sha256(tarball)) {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::VerificationFailed,
        format!("{asset} does not match its published checksum, so the installed torrential is unchanged"),
    )
    .with_hint(format!("run torrential update again later; if it fails again, report it at {ISSUES}")))
}

fn unpack(asset: &str, tarball: &[u8]) -> Result<Vec<u8>, Error> {
    let bad = |e: std::io::Error| Error::new(ErrorCode::FetchFailed, format!("cannot unpack {asset}: {e}"));
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let path = entry.path().map_err(bad)?.into_owned();
        if entry.header().entry_type().is_file() && path.strip_prefix(".").unwrap_or(&path) == Path::new("torrential") {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary).map_err(bad)?;
            return Ok(binary);
        }
    }
    Err(Error::new(ErrorCode::FetchFailed, format!("{asset} contains no torrential binary")))
}

struct Partial {
    path: PathBuf,
}

impl Partial {
    fn create(exe: &Path, writes: &Writes) -> Result<Partial, Error> {
        let dir = exe.parent().unwrap_or(Path::new("/"));
        let path = dir.join(format!(".torrential.{}.update", std::process::id()));
        OpenOptions::new().write(true).create(true).truncate(true).mode(0o755).open(&path).map_err(|e| {
            let error = writes.error(dir, e);
            match error.code {
                ErrorCode::StorageError => error.with_hint(format!(
                    "reinstall torrential in a folder you can write, such as ~/.local/bin, or run torrential update \
                     as a user who can write {}",
                    dir.display()
                )),
                _ => error,
            }
        })?;
        Ok(Partial { path })
    }

    /// Writes `binary`, which is to replace `at`.
    fn write(&self, binary: &[u8], at: &Path, writes: &Writes) -> Result<(), Error> {
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
            .and_then(|mut file| {
                file.write_all(binary)?;
                file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
                file.sync_all()
            })
            .map_err(|e| writes.error(at, e))
    }

    /// Moves the binary to `at` and syncs the folder, so the move outlasts a power cut.
    fn place(self, at: &Path, writes: &Writes) -> Result<(), Error> {
        std::fs::rename(&self.path, at).map_err(|e| writes.error(at, e))?;
        let dir = at.parent().unwrap_or(Path::new("/"));
        std::fs::File::open(dir).and_then(|folder| folder.sync_all()).map_err(|e| writes.error(dir, e))
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn a_test_engine_looks_for_releases_only_where_the_test_points_it() {
        let base = |pairs: &[(&str, &str)]| Releases::from_env(&Env::from_pairs(pairs.iter().copied())).map(|r| r.base);
        let local = (hooks::RELEASES, "http://127.0.0.1:8080/");
        let github = Some("https://github.com/nexxeln/torrential/releases");
        assert_eq!(base(&[]).as_deref(), github);
        assert_eq!(base(&[local]).as_deref(), github, "only a test process reads it");
        assert_eq!(base(&[(hooks::NETWORK, "loopback")]), None);
        assert_eq!(base(&[(hooks::ENGINE, "fake")]), None);
        assert_eq!(base(&[(hooks::NETWORK, "loopback"), local]).as_deref(), Some("http://127.0.0.1:8080"));
    }

    #[test]
    fn a_binary_counts_only_when_it_says_it_is_the_release() {
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let said = |path: &Path| runtime().unwrap().block_on(confirm(path, version("0.1.1"))).map_err(|e| e.message);

        assert_eq!(said(&script("right", "#!/bin/sh\necho torrential 0.1.1\n")), Ok(()));
        assert_eq!(
            said(&script("older", "#!/bin/sh\necho torrential 0.1.0\n")),
            Err("the binary of torrential 0.1.1 says it is torrential 0.1.0, so the installed torrential is unchanged"
                .into())
        );
        let garbage = said(&script("garbage", "\u{7f}ELF garbage")).unwrap_err();
        assert!(garbage.starts_with("the binary of torrential 0.1.1 does not run here ("), "{garbage}");
    }
}
