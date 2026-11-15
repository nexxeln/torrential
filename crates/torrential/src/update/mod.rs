//! What torrential knows about its own releases: their versions, when it last looked for one,
//! and which one an update installed.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;
use crate::log::{log, persist};
use crate::paths::Paths;

pub mod release;

pub(crate) const DAY: i64 = 24 * 60 * 60;
pub(crate) const HOUR: i64 = 60 * 60;
pub(crate) const CHECKED: &str = "update.json";
pub(crate) const UPDATED: &str = "updated.json";

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

    /// Whether `version` names a release newer than `than`. An engine never stops for a client
    /// older than itself, and a client never uses or stops an engine newer than itself.
    pub fn newer(version: &str, than: &str) -> bool {
        Version::parse(version).is_some_and(|version| version.newer_than(than))
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
    /// A release whose binary failed its checks here, which the engine never fetches again.
    pub refused: Option<String>,
}

impl Checked {
    pub(crate) fn read(paths: &Paths) -> Checked {
        read_json(&paths.state.join(CHECKED)).unwrap_or_default()
    }

    pub(crate) fn write(&self, paths: &Paths) {
        write_json(&paths.state.join(CHECKED), self);
    }

    pub fn next_check(&self) -> i64 {
        let answered = self.checked_at.map_or(i64::MIN, |at| at + DAY);
        let failed = self.failed_at.map_or(i64::MIN, |at| at + HOUR);
        answered.max(failed)
    }

    /// The latest release, when it is newer than `current` and has not been refused here.
    pub fn newer(&self, current: &str) -> Option<Version> {
        let refused = self.refused.as_deref().and_then(Version::parse);
        Version::parse(self.latest.as_deref()?).filter(|latest| latest.newer_than(current) && refused != Some(*latest))
    }

    pub fn install_due(&self, current: &str, now: i64) -> bool {
        self.newer(current).is_some() && due(self.install_failed_at, now)
    }
}

pub fn due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|at| !(at..at + DAY).contains(&now))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Updated {
    pub(crate) from: String,
    pub(crate) to: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Audience {
    Person,
    Agent,
}

impl Audience {
    pub(crate) fn out(self, current: &str, latest: Version) -> String {
        let news =
            format!("torrential {latest} is available (you have {current}). Run torrential update to install it.");
        match self {
            Self::Person => news,
            Self::Agent => format!("# update: {news}"),
        }
    }

    pub(crate) fn updated(self, from: &str, to: &str) -> String {
        match self {
            Self::Person => format!("Updated torrential to {to}."),
            Self::Agent => format!("# updated torrential from {from} to {to}"),
        }
    }

    pub(crate) fn notice_file(self, paths: &Paths) -> PathBuf {
        let name = match self {
            Self::Person => "update-notice-person",
            Self::Agent => "update-notice-agent",
        };
        paths.state.join(name)
    }
}

/// The record at `path`, or `None` when there is none. One that cannot be read is logged and
/// read as none, so the next write replaces it.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log(&format!("cannot read {}: {e}", path.display()));
            return None;
        }
    };
    serde_json::from_slice(&bytes).inspect_err(|e| log(&format!("cannot read {}: {e}", path.display()))).ok()
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) {
    let bytes = serde_json::to_vec(value).expect("update records serialize");
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| write_atomic(path, &bytes));
    persist(&format!("write {}", path.display()), written);
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
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
        let latest = version("0.1.1");
        assert_eq!(
            Audience::Person.out("0.1.0", latest),
            "torrential 0.1.1 is available (you have 0.1.0). Run torrential update to install it."
        );
        assert_eq!(
            Audience::Agent.out("0.1.0", latest),
            "# update: torrential 0.1.1 is available (you have 0.1.0). Run torrential update to install it."
        );
        assert_eq!(Audience::Person.updated("0.1.0", "0.1.1"), "Updated torrential to 0.1.1.");
        assert_eq!(Audience::Agent.updated("0.1.0", "0.1.1"), "# updated torrential from 0.1.0 to 0.1.1");
    }
}
