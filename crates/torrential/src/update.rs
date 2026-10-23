use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use torrential_core::{Error, ErrorCode};

use crate::daemon::owner::now;
use crate::env::Env;
use crate::output::Mode;
use crate::output::theme::{Token, paint};
use crate::paths::Paths;
use crate::skill::{Writes, sha256, write_atomic};

const RELEASES: &str = "https://github.com/nexxeln/torrential/releases";
const ISSUES: &str = "https://github.com/nexxeln/torrential/issues";
const DAY: i64 = 24 * 60 * 60;
const HOUR: i64 = 60 * 60;
const CHECK_WITHIN: Duration = Duration::from_secs(10);
const MAX_ASSET: usize = 256 << 20;
const CHECKED: &str = "update.json";
const UPDATED: &str = "updated.json";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Say when a newer version is out
    #[default]
    Notify,
    /// Install newer versions in the background
    Auto,
    /// Never look for newer versions
    Off,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    release: bool,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let numbers: Vec<u64> = core.split('.').map(str::parse).collect::<Result<_, _>>().ok()?;
        let [major, minor, patch] = numbers[..] else { return None };
        Some(Version { major, minor, patch, release: pre.is_none() })
    }

    pub fn newer_than(self, current: &str) -> bool {
        Version::parse(current).is_some_and(|current| self > current)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// GitHub answers `/releases/latest` with a redirect to `/releases/tag/<tag>`. Reading the
/// redirect avoids the REST API, which allows 60 requests an hour.
pub fn tag_version(location: &str) -> Option<Version> {
    let (_, tag) = location.trim_end_matches('/').rsplit_once("/tag/")?;
    Version::parse(tag)
}

pub fn target(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Checked {
    pub latest: Option<String>,
    pub checked_at: Option<i64>,
    pub failed_at: Option<i64>,
    pub install_failed_at: Option<i64>,
}

impl Checked {
    fn read(paths: &Paths) -> Checked {
        read_json(&paths.state.join(CHECKED)).unwrap_or_default()
    }

    fn write(&self, paths: &Paths) {
        write_json(&paths.state.join(CHECKED), self);
    }

    pub fn next_check(&self) -> i64 {
        let answered = self.checked_at.map_or(i64::MIN, |at| at + DAY);
        let failed = self.failed_at.map_or(i64::MIN, |at| at + HOUR);
        answered.max(failed)
    }

    pub fn newer(&self, current: &str) -> Option<Version> {
        Version::parse(self.latest.as_deref()?).filter(|latest| latest.newer_than(current))
    }

    pub fn install_due(&self, current: &str, now: i64) -> bool {
        self.newer(current).is_some() && due(self.install_failed_at, now)
    }
}

pub fn due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|at| !(at..at + DAY).contains(&now))
}

#[derive(Serialize, Deserialize)]
struct Updated {
    from: String,
    to: String,
}

pub struct Releases {
    base: String,
}

impl Releases {
    /// `TORRENTIAL_TEST_RELEASES` replaces the GitHub releases URL, so a test can serve releases
    /// from its own server. A test engine, which has `TORRENTIAL_TEST_NETWORK` or
    /// `TORRENTIAL_TEST_ENGINE` set, never looks for releases without it.
    pub fn from_env(env: &Env) -> Option<Releases> {
        let base = match env.var("TORRENTIAL_TEST_RELEASES") {
            Some(url) => url,
            None if env.var("TORRENTIAL_TEST_NETWORK").is_some() || env.var("TORRENTIAL_TEST_ENGINE").is_some() => {
                return None;
            }
            None => RELEASES,
        };
        Some(Releases { base: base.trim_end_matches('/').to_string() })
    }

    pub fn required(env: &Env) -> Result<Releases, Error> {
        Releases::from_env(env).ok_or_else(|| {
            Error::new(ErrorCode::NetworkUnavailable, "this test environment does not look for releases")
                .with_hint("set TORRENTIAL_TEST_RELEASES to a release server")
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
                format!("{url} answered {} without naming a release", response.status()),
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
            return Err(Error::new(ErrorCode::FetchFailed, format!("{url} answered {}", response.status()))
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
    let Some(target) = target(std::env::consts::OS, std::env::consts::ARCH) else {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("there is no prebuilt torrential for {} {}", std::env::consts::OS, std::env::consts::ARCH),
        )
        .with_hint("build it from source as https://github.com/nexxeln/torrential#install shows"));
    };
    let exe = executable()?;
    if exe.starts_with("/nix/store") {
        return Err(Error::new(
            ErrorCode::StorageError,
            format!("torrential runs from {}, which Nix manages", exe.display()),
        )
        .with_hint("update it with Nix, for example nix profile upgrade torrential"));
    }
    let writes = Writes::new(env, "torrential update".into());
    let partial = Partial::create(&exe, &writes)?;
    let asset = format!("torrential-{target}.tar.gz");
    let tarball = releases.download(latest, &asset).await?;
    let sums = releases.download(latest, &format!("{asset}.sha256")).await?;
    verify(&asset, &tarball, &sums)?;
    partial.replace(&exe, &unpack(&asset, &tarball)?, &writes)?;
    let _ = Command::new(&exe)
        .args(["skill", "refresh"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(Outcome::Updated(latest))
}

pub fn executable() -> Result<PathBuf, Error> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| Error::new(ErrorCode::StorageError, format!("cannot find the torrential binary: {e}")))
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
    Err(Error::new(ErrorCode::FetchFailed, format!("{asset} holds no torrential binary")))
}

struct Partial {
    path: PathBuf,
    file: File,
}

impl Partial {
    fn create(exe: &Path, writes: &Writes) -> Result<Partial, Error> {
        let dir = exe.parent().unwrap_or(Path::new("/"));
        let path = dir.join(format!(".torrential.{}.update", std::process::id()));
        let file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o755).open(&path).map_err(|e| {
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
        Ok(Partial { path, file })
    }

    fn replace(mut self, exe: &Path, binary: &[u8], writes: &Writes) -> Result<(), Error> {
        self.file
            .write_all(binary)
            .and_then(|()| self.file.set_permissions(std::fs::Permissions::from_mode(0o755)))
            .and_then(|()| self.file.sync_all())
            .and_then(|()| std::fs::rename(&self.path, exe))
            .map_err(|e| writes.error(exe, e))
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn watch(env: Env, paths: Paths) {
    let Some(releases) = Releases::from_env(&env) else { return };
    let current = env.version();
    loop {
        let now = now();
        let wait = match crate::config::updates(&paths) {
            Ok(Policy::Off) | Err(_) => HOUR,
            Ok(policy) => {
                if Checked::read(&paths).next_check() <= now
                    && let Err(e) = check(&releases, &paths, now).await
                {
                    crate::daemon::log(&format!("update check failed: {}", e.message));
                }
                let checked = Checked::read(&paths);
                if policy == Policy::Auto && checked.install_due(&current, now) {
                    match install(&releases, &paths, &env, now).await {
                        Ok(Outcome::Updated(to)) => {
                            crate::daemon::log(&format!("installed torrential {to}"));
                            write_json(&paths.state.join(UPDATED), &Updated { from: current, to: to.to_string() });
                            return;
                        }
                        Ok(Outcome::Latest(_)) => {}
                        Err(e) => {
                            crate::daemon::log(&format!("update failed: {}", e.message));
                            let mut checked = Checked::read(&paths);
                            checked.install_failed_at = Some(now);
                            checked.write(&paths);
                        }
                    }
                }
                (Checked::read(&paths).next_check() - now).clamp(60, HOUR)
            }
        };
        tokio::time::sleep(Duration::from_secs(wait as u64)).await;
    }
}

pub fn available(paths: &Paths, env: &Env) -> Option<Version> {
    if crate::config::updates(paths).ok()? == Policy::Off {
        return None;
    }
    Checked::read(paths).newer(&env.version())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Audience {
    Person,
    Agent,
}

impl Audience {
    pub fn of(mode: Mode, env: &Env, stderr_is_terminal: bool) -> Option<Audience> {
        match mode {
            Mode::Json => None,
            _ if env.agent().is_some() => Some(Audience::Agent),
            _ if stderr_is_terminal => Some(Audience::Person),
            _ => None,
        }
    }

    fn out(self, current: &str, latest: Version) -> String {
        let news = format!("torrential {latest} is out (you have {current}). Run torrential update to install it.");
        match self {
            Self::Person => news,
            Self::Agent => format!("# update: {news}"),
        }
    }

    fn updated(self, from: &str, to: &str) -> String {
        match self {
            Self::Person => format!("Updated torrential to {to}."),
            Self::Agent => format!("# updated torrential from {from} to {to}"),
        }
    }

    fn notice_file(self, paths: &Paths) -> PathBuf {
        let name = match self {
            Self::Person => "update-notice-person",
            Self::Agent => "update-notice-agent",
        };
        paths.state.join(name)
    }
}

pub fn notice(paths: &Paths, env: &Env, audience: Audience, now: i64) -> Option<String> {
    let updated = paths.state.join(UPDATED);
    if let Some(Updated { from, to }) = read_json(&updated)
        && std::fs::remove_file(&updated).is_ok()
    {
        return Some(audience.updated(&from, &to));
    }
    let latest = available(paths, env)?;
    let file = audience.notice_file(paths);
    let last = std::fs::read_to_string(&file).ok().and_then(|text| text.trim().parse().ok());
    if !due(last, now) {
        return None;
    }
    let _ = write_atomic(&file, now.to_string().as_bytes());
    Some(audience.out(&env.version(), latest))
}

pub fn print_notice(mode: Mode, env: &Env) {
    let Some(audience) = Audience::of(mode, env, std::io::stderr().is_terminal()) else { return };
    let Ok(paths) = Paths::from_process(env) else { return };
    match (notice(&paths, env, audience, now()), audience) {
        (Some(line), Audience::Person) => anstream::eprintln!("{}", paint(Token::Muted, &line)),
        (Some(line), Audience::Agent) => eprintln!("{line}"),
        (None, _) => {}
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json(path: &Path, value: &impl Serialize) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = write_atomic(path, &serde_json::to_vec(value).expect("update records serialize"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_parse_with_or_without_a_v_and_order_numerically() {
        assert_eq!(version("v0.1.10").to_string(), "0.1.10");
        assert!(version("0.1.10") > version("0.1.9"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert!(version("0.2.0-rc1") < version("0.2.0"));
        assert!(version("0.2.0-rc1") > version("0.1.9"));
        for bad in ["", "0.1", "0.1.0.0", "x.1.0", "latest"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_latest_redirect_names_the_tag() {
        let found = |location: &str| tag_version(location).map(|v| v.to_string());
        assert_eq!(found("https://github.com/nexxeln/torrential/releases/tag/v0.1.1").as_deref(), Some("0.1.1"));
        assert_eq!(found("/nexxeln/torrential/releases/tag/v2.0.3/").as_deref(), Some("2.0.3"));
        assert_eq!(found("https://github.com/nexxeln/torrential/releases"), None);
        assert_eq!(found("https://github.com/nexxeln/torrential/releases/tag/nightly"), None);
    }

    #[test]
    fn apple_silicon_and_linux_on_arm64_and_x86_64_have_a_prebuilt_binary() {
        assert_eq!(target("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target("linux", "x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(target("linux", "aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(target("macos", "x86_64"), None);
        assert_eq!(target("windows", "x86_64"), None);
        assert_eq!(target("freebsd", "x86_64"), None);
    }

    #[test]
    fn a_test_engine_looks_for_releases_only_where_the_test_points_it() {
        let base = |pairs: &[(&str, &str)]| Releases::from_env(&Env::from_pairs(pairs.iter().copied())).map(|r| r.base);
        let local = ("TORRENTIAL_TEST_RELEASES", "http://127.0.0.1:8080/");
        assert_eq!(base(&[]).as_deref(), Some("https://github.com/nexxeln/torrential/releases"));
        assert_eq!(base(&[("TORRENTIAL_TEST_NETWORK", "loopback")]), None);
        assert_eq!(base(&[("TORRENTIAL_TEST_ENGINE", "fake")]), None);
        assert_eq!(base(&[("TORRENTIAL_TEST_NETWORK", "loopback"), local]).as_deref(), Some("http://127.0.0.1:8080"));
    }

    #[test]
    fn the_engine_checks_a_day_after_an_answer_and_an_hour_after_a_failure() {
        let t = 1_790_000_000;
        let next = |checked_at, failed_at| Checked { checked_at, failed_at, ..Checked::default() }.next_check();
        assert!(next(None, None) <= t);
        assert_eq!(next(Some(t), None), t + DAY);
        assert_eq!(next(None, Some(t)), t + HOUR);
        assert_eq!(next(Some(t), Some(t + 25 * HOUR)), t + 26 * HOUR);
        assert_eq!(next(Some(t + HOUR), Some(t)), t + HOUR + DAY);
    }

    #[test]
    fn a_notice_waits_a_day_after_the_last() {
        let t = 1_790_000_000;
        assert!(due(None, t));
        assert!(!due(Some(t), t));
        assert!(!due(Some(t), t + DAY - 1));
        assert!(due(Some(t), t + DAY));
        assert!(due(Some(t + HOUR), t), "a clock that went back does not silence notices for good");
    }

    #[test]
    fn auto_mode_installs_a_newer_version_and_waits_a_day_after_a_failure() {
        let t = 1_790_000_000;
        let checked = |latest: &str, install_failed_at| Checked {
            latest: Some(latest.into()),
            install_failed_at,
            ..Checked::default()
        };
        assert!(checked("0.1.1", None).install_due("0.1.0", t));
        assert!(!checked("0.1.0", None).install_due("0.1.0", t));
        assert!(!checked("0.1.1", Some(t - HOUR)).install_due("0.1.0", t));
        assert!(checked("0.1.1", Some(t - DAY)).install_due("0.1.0", t));
    }

    #[test]
    fn people_and_agents_get_their_own_lines() {
        let env = |pairs: &[(&str, &str)]| Env::from_pairs(pairs.iter().copied());
        let latest = version("0.1.1");
        assert_eq!(Audience::of(Mode::Json, &env(&[("CLAUDECODE", "1")]), true), None);
        assert_eq!(Audience::of(Mode::Plain, &env(&[("CLAUDECODE", "1")]), false), Some(Audience::Agent));
        assert_eq!(Audience::of(Mode::Human, &env(&[]), true), Some(Audience::Person));
        assert_eq!(Audience::of(Mode::Plain, &env(&[]), false), None);
        assert_eq!(
            Audience::Person.out("0.1.0", latest),
            "torrential 0.1.1 is out (you have 0.1.0). Run torrential update to install it."
        );
        assert_eq!(
            Audience::Agent.out("0.1.0", latest),
            "# update: torrential 0.1.1 is out (you have 0.1.0). Run torrential update to install it."
        );
        assert_eq!(Audience::Person.updated("0.1.0", "0.1.1"), "Updated torrential to 0.1.1.");
        assert_eq!(Audience::Agent.updated("0.1.0", "0.1.1"), "# updated torrential from 0.1.0 to 0.1.1");
    }

    #[test]
    fn each_audience_hears_once_a_day_and_off_silences_both() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let env = Env::from_pairs([("HOME", home.as_str()), ("TORRENTIAL_HOME", home.as_str())]);
        let paths = Paths::resolve(&env, 501, None).unwrap();
        let t = 1_790_000_000;
        Checked { latest: Some("9.0.0".into()), checked_at: Some(t), ..Checked::default() }.write(&paths);
        let heard = |audience, now| notice(&paths, &env, audience, now).is_some();

        assert_eq!(
            [heard(Audience::Person, t), heard(Audience::Person, t + HOUR), heard(Audience::Agent, t + HOUR)],
            [true, false, true]
        );
        assert!(heard(Audience::Person, t + DAY));
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(paths.config.join("config.yaml"), "updates: off\n").unwrap();
        assert!(!heard(Audience::Person, t + 3 * DAY));
    }
}
