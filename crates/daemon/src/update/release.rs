//! Fetching a release, checking it, trying it as an engine, and installing it.

use std::fs::{OpenOptions, TryLockError};
use std::io::{Read, Write};
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

use super::{Archive, Checked, Version, tag_version, target};

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
    _held: std::fs::File,
}

impl FolderLock {
    async fn take(dir: &Path) -> Result<FolderLock, Error> {
        let failed =
            |e: std::io::Error| Error::new(ErrorCode::StorageError, format!("cannot lock {}: {e}", dir.display()));
        let held = lockable(dir).map_err(failed)?;
        loop {
            match held.try_lock() {
                Ok(()) => return Ok(FolderLock { _held: held }),
                Err(TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(TryLockError::Error(e)) => return Err(failed(e)),
            }
        }
    }
}

/// What stands for the folder `dir` in a lock: the folder itself.
#[cfg(unix)]
fn lockable(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(dir)
}

/// What stands for the folder `dir` in a lock: a file in it, as Windows locks only files.
#[cfg(windows)]
fn lockable(dir: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().write(true).create(true).truncate(false).open(dir.join("torrential.update.lock"))
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
    let archive = Archive::of(target);
    let asset = archive.asset(target);
    let packed = releases.download(version, &asset).await?;
    let sums = releases.download(version, &format!("{asset}.sha256")).await?;
    verify(&asset, &packed, &sums)?;
    unpack(archive, &asset, &packed)
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
    private_folder(&home.0.join("config")).map_err(unprepared)?;
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

/// The binary in the release asset `asset`, packed as `archive`.
fn unpack(archive: Archive, asset: &str, packed: &[u8]) -> Result<Vec<u8>, Error> {
    let bad = |e: std::io::Error| Error::new(ErrorCode::FetchFailed, format!("cannot unpack {asset}: {e}"));
    let name = Path::new(archive.binary());
    let mut binary = Vec::new();
    match archive {
        Archive::TarGz => {
            let mut tarball = tar::Archive::new(flate2::read::GzDecoder::new(packed));
            for entry in tarball.entries().map_err(bad)? {
                let mut entry = entry.map_err(bad)?;
                let path = entry.path().map_err(bad)?.into_owned();
                if entry.header().entry_type().is_file() && path.strip_prefix(".").unwrap_or(&path) == name {
                    entry.read_to_end(&mut binary).map_err(bad)?;
                    return Ok(binary);
                }
            }
        }
        Archive::Zip => {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(packed)).map_err(|e| bad(e.into()))?;
            for index in 0..zip.len() {
                let mut entry = zip.by_index(index).map_err(|e| bad(e.into()))?;
                if entry.is_file() && entry.enclosed_name().as_deref() == Some(name) {
                    entry.read_to_end(&mut binary).map_err(bad)?;
                    return Ok(binary);
                }
            }
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
        let path = dir.join(format!(".torrential.{}.update{}", std::process::id(), std::env::consts::EXE_SUFFIX));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o755);
        options.open(&path).map_err(|e| {
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
                #[cfg(unix)]
                file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
                file.sync_all()
            })
            .map_err(|e| writes.error(at, e))
    }

    /// Moves the binary to `at` and syncs the folder, so the move outlasts a power cut.
    #[cfg(unix)]
    fn place(self, at: &Path, writes: &Writes) -> Result<(), Error> {
        std::fs::rename(&self.path, at).map_err(|e| writes.error(at, e))?;
        let dir = at.parent().unwrap_or(Path::new("/"));
        std::fs::File::open(dir).and_then(|folder| folder.sync_all()).map_err(|e| writes.error(dir, e))
    }

    /// Moves the binary to `at`. Windows replaces no program while it runs but lets it be renamed,
    /// so the binary at `at` moves aside first, and `remove_set_aside` deletes it at a later start.
    #[cfg(windows)]
    fn place(self, at: &Path, writes: &Writes) -> Result<(), Error> {
        set_aside(at).map_err(|e| writes.error(at, e))?;
        std::fs::rename(&self.path, at).map_err(|e| writes.error(at, e))
    }
}

/// Moves the binary `exe` aside to `torrential.old`, or to `torrential.<pid>.old` while an earlier
/// one still runs from `torrential.old`.
#[cfg(windows)]
fn set_aside(exe: &Path) -> std::io::Result<()> {
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)
        .or_else(|_| std::fs::rename(exe, exe.with_extension(format!("{}.old", std::process::id()))))
}

/// Deletes the binaries updates moved aside next to `exe`, except those still running.
#[cfg(windows)]
pub fn remove_set_aside(exe: &Path) {
    let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem().and_then(|stem| stem.to_str())) else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(middle) = name.to_str().and_then(|name| name.strip_prefix(stem)?.strip_suffix("old")) else {
            continue;
        };
        if middle == "."
            || middle.strip_prefix('.').and_then(|n| n.strip_suffix('.')).is_some_and(|n| n.parse::<u32>().is_ok())
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Makes the folder `path` and those above it, for the person alone.
fn private_folder(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
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

    // The stand-in releases are shell scripts.
    #[cfg(unix)]
    #[test]
    fn a_binary_counts_only_when_it_says_it_is_the_release() {
        use std::os::unix::fs::PermissionsExt;
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

    #[test]
    fn the_binary_comes_out_of_a_tarball_or_a_zip() {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, text) in [("LICENSE", "MIT"), ("torrential", "unix binary")] {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, text.as_bytes()).unwrap();
        }
        let tarball = tar.into_inner().unwrap().finish().unwrap();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, text) in [("LICENSE", "MIT"), ("torrential.exe", "windows binary")] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        let zipped = zip.finish().unwrap().into_inner();
        let unpacked = |archive: Archive, packed: &[u8]| unpack(archive, "asset", packed).map_err(|e| e.message);
        assert_eq!(unpacked(Archive::TarGz, &tarball), Ok(b"unix binary".to_vec()));
        assert_eq!(unpacked(Archive::Zip, &zipped), Ok(b"windows binary".to_vec()));
        let wrong = unpacked(Archive::Zip, &tarball).unwrap_err();
        assert!(wrong.starts_with("cannot unpack asset: "), "{wrong}");
    }

    #[cfg(windows)]
    #[test]
    fn a_running_binary_moves_aside_for_the_new_one_and_goes_once_it_stops() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("torrential.exe");
        // Any program will do, as long as it runs from the file being replaced.
        std::fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
        let mut running =
            std::process::Command::new(&exe).args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
        let writes = Writes { sandboxed: false, rerun: "torrential update".into() };
        let partial = Partial::create(&exe, &writes).unwrap();
        partial.write(b"new binary", &exe, &writes).unwrap();
        partial.place(&exe, &writes).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        let old = dir.path().join("torrential.old");
        assert!(old.exists(), "the running binary waits beside it");
        remove_set_aside(&exe);
        assert!(old.exists(), "it stays while it runs");
        running.kill().unwrap();
        running.wait().unwrap();
        remove_set_aside(&exe);
        assert!(!old.exists());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
    }
}
