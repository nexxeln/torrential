use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::Deserialize;
use torrential_core::{Arch, Display, Error, ErrorCode, Language, Machine, Os, Preferences, Profile, Viewer};

use crate::daemon::admission::{Disk, reserve};
use crate::env::Env;

const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Detector {
    machine: OnceLock<Machine>,
    downloads: PathBuf,
    disk: Box<dyn Disk + Sync>,
}

impl Detector {
    pub fn start(env: &Env, downloads: PathBuf, disk: Box<dyn Disk + Sync>) -> Arc<Detector> {
        let detector = Arc::new(Detector { machine: OnceLock::new(), downloads, disk });
        let (env, filling) = (env.clone(), detector.clone());
        std::thread::spawn(move || match detect(&env) {
            Ok(machine) => {
                let profile = Profile { detected: Some(machine.clone()), ..Profile::default() };
                crate::daemon::log(&format!("ranking for {}", profile.describe()));
                let _ = filling.machine.set(machine);
            }
            Err(error) => crate::daemon::log(&format!("cannot detect this machine: {}", error.message)),
        });
        detector
    }

    #[cfg(test)]
    pub fn detected(machine: Machine, downloads: PathBuf, disk: Box<dyn Disk + Sync>) -> Arc<Detector> {
        Arc::new(Detector { machine: OnceLock::from(machine), downloads, disk })
    }

    pub fn profile(&self, preferences: Preferences, viewer: Option<Viewer>) -> Profile {
        let room = self.disk.space(&self.downloads).ok().map(|space| space.free.saturating_sub(reserve(space.total)));
        Profile { detected: self.machine.get().cloned(), preferences, room, viewer }
    }
}

pub fn detect(env: &Env) -> Result<Machine, Error> {
    if let Some(json) = env.var("TORRENTIAL_TEST_MACHINE") {
        return serde_json::from_str(json)
            .map_err(|e| Error::new(ErrorCode::Usage, format!("TORRENTIAL_TEST_MACHINE is not a machine: {e}")));
    }
    let os = Os::of_host();
    let display = match os {
        Os::Macos => run("/usr/sbin/system_profiler", &["SPDisplaysDataType", "-json"], TIMEOUT)
            .and_then(|json| profiled_display(&json)),
        Os::Linux => drm_display(Path::new("/sys/class/drm")),
        Os::Other => None,
    };
    Ok(Machine { os, display, arch: Arch::of_host(), language: language(os, env) })
}

/// The language `detect` reads, without waiting for its display probe.
pub fn system_language(env: &Env) -> Option<Language> {
    match env.var("TORRENTIAL_TEST_MACHINE") {
        Some(_) => detect(env).ok()?.language,
        None => language(Os::of_host(), env),
    }
}

/// macOS keeps the language the user picked in `AppleLanguages`, and a shell often has no
/// `LANG` at all. Elsewhere the locale variables decide, in the order POSIX reads them.
fn language(os: Os, env: &Env) -> Option<Language> {
    let apple = (os == Os::Macos)
        .then(|| run("/usr/bin/defaults", &["read", "-g", "AppleLanguages"], TIMEOUT))
        .flatten()
        .and_then(|list| first_apple_language(&list));
    apple.or_else(|| {
        ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|var| env.var(var)).and_then(Language::from_locale)
    })
}

/// The first entry of `defaults read -g AppleLanguages`, which prints `(\n    "en-IN"\n)`.
fn first_apple_language(list: &str) -> Option<Language> {
    list.lines()
        .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
        .find(|entry| !entry.is_empty() && *entry != "(" && *entry != ")")
        .and_then(Language::from_locale)
}

#[derive(Deserialize)]
struct Profiler {
    #[serde(rename = "SPDisplaysDataType", default)]
    gpus: Vec<Gpu>,
}

#[derive(Deserialize)]
struct Gpu {
    #[serde(rename = "spdisplays_ndrvs", default)]
    displays: Vec<Profiled>,
}

#[derive(Deserialize)]
struct Profiled {
    /// The panel, as `spdisplays_3024x1964Retina`.
    #[serde(rename = "spdisplays_pixelresolution")]
    panel: Option<String>,
    /// The frame the desktop draws, as `3600 x 2338`, which a scaled mode makes larger than the panel.
    #[serde(rename = "_spdisplays_pixels")]
    pixels: Option<String>,
    /// As `spdisplays_built-in-liquid-retina-xdr`.
    #[serde(rename = "spdisplays_display_type")]
    kind: Option<String>,
}

fn profiled_display(json: &str) -> Option<Display> {
    let profiler: Profiler = serde_json::from_str(json).ok()?;
    profiler
        .gpus
        .iter()
        .flat_map(|gpu| &gpu.displays)
        .filter_map(|d| {
            let (width, height) =
                d.panel.as_deref().and_then(dimensions).or(d.pixels.as_deref().and_then(dimensions))?;
            let hdr = d.kind.as_deref().is_some_and(|kind| kind.to_ascii_lowercase().contains("xdr"));
            Some(Display { width, height, hdr })
        })
        .max_by_key(|d| u64::from(d.width) * u64::from(d.height))
}

fn dimensions(text: &str) -> Option<(u32, u32)> {
    let mut numbers = text.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok());
    Some((numbers.next()?, numbers.next()?))
}

fn drm_display(root: &Path) -> Option<Display> {
    let read = |dir: &Path, file: &str| std::fs::read_to_string(dir.join(file)).ok();
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| read(dir, "status").is_some_and(|status| status.trim() == "connected"))
        .flat_map(|dir| read(&dir, "modes").unwrap_or_default().lines().filter_map(dimensions).collect::<Vec<_>>())
        .max_by_key(|(width, height)| u64::from(*width) * u64::from(*height))
        .map(|(width, height)| Display { width, height, hdr: false })
}

fn run(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sent, output) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = sent.send(stdout.read_to_string(&mut text).map(|_| text));
    });
    let deadline = Instant::now() + timeout;
    let text = output.recv_timeout(timeout).ok().and_then(Result::ok);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return text.filter(|_| status.success()),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/machine").join(name)
    }

    #[test]
    fn reads_the_panel_of_this_macbook_pro_and_its_hdr() {
        let json = std::fs::read_to_string(fixture("system_profiler-macbook-pro-m4.json")).unwrap();
        assert_eq!(profiled_display(&json), Some(Display { width: 3024, height: 1964, hdr: true }));
    }

    #[test]
    fn takes_the_largest_display_and_falls_back_to_the_frame_size() {
        let json = r#"{"SPDisplaysDataType": [{"spdisplays_ndrvs": [
            {"spdisplays_pixelresolution": "spdisplays_2880x1800Retina", "spdisplays_display_type": "spdisplays_built-in_retinaLCD"},
            {"_spdisplays_pixels": "3840 x 2160", "spdisplays_pixelresolution": "spdisplays_2160p"}
        ]}]}"#;
        let headless = r#"{"SPDisplaysDataType": [{"_name": "Apple M2", "sppci_model": "Apple M2"}]}"#;
        assert_eq!(profiled_display(json), Some(Display { width: 3840, height: 2160, hdr: false }));
        assert_eq!(profiled_display(headless), None);
        assert_eq!(profiled_display("not json"), None);
    }

    #[test]
    fn reads_the_largest_mode_of_connected_drm_connectors() {
        assert_eq!(drm_display(&fixture("drm")), Some(Display { width: 3840, height: 2160, hdr: false }));
        let none = tempfile::tempdir().unwrap();
        std::fs::create_dir(none.path().join("card0-HDMI-A-1")).unwrap();
        std::fs::write(none.path().join("card0-HDMI-A-1/status"), "disconnected\n").unwrap();
        std::fs::write(none.path().join("card0-HDMI-A-1/modes"), "1920x1080\n").unwrap();
        assert_eq!(drm_display(none.path()), None);
        assert_eq!(drm_display(&none.path().join("missing")), None);
    }

    #[test]
    fn reads_the_first_apple_language_then_the_locale_variables() {
        let first = |list: &str| first_apple_language(list).map(|l| l.code().to_string());
        assert_eq!(first("(\n    \"en-IN\"\n)\n").as_deref(), Some("en"));
        assert_eq!(first("(\n    fr,\n    \"en-GB\"\n)\n").as_deref(), Some("fr"));
        assert_eq!(first("").as_deref(), None);
        let from = |pairs: &[(&str, &str)]| {
            language(Os::Linux, &Env::from_pairs(pairs.iter().copied())).map(|l| l.code().to_string())
        };
        assert_eq!(from(&[("LANG", "de_DE.UTF-8")]).as_deref(), Some("de"));
        assert_eq!(from(&[("LANG", "de_DE.UTF-8"), ("LC_ALL", "C")]).as_deref(), None);
        assert_eq!(from(&[]).as_deref(), None);
    }

    #[test]
    fn a_test_machine_replaces_detection() {
        let env = Env::from_pairs([(
            "TORRENTIAL_TEST_MACHINE",
            r#"{"os": "linux", "display": null, "arch": "x86_64", "language": "en"}"#,
        )]);
        let machine = detect(&env).unwrap();
        assert_eq!((machine.os, machine.display, machine.arch), (Os::Linux, None, Some(Arch::X86_64)));
        let bad = detect(&Env::from_pairs([("TORRENTIAL_TEST_MACHINE", "{}")])).unwrap_err();
        assert_eq!(bad.message, "TORRENTIAL_TEST_MACHINE is not a machine: missing field `os` at line 1 column 2");
    }

    #[test]
    fn a_command_that_fails_or_outlives_the_timeout_gives_nothing() {
        let second = Duration::from_secs(1);
        assert_eq!(run("/bin/echo", &["1920x1080"], second).as_deref(), Some("1920x1080\n"));
        assert_eq!(run("/bin/sh", &["-c", "echo partial; exit 3"], second), None);
        assert_eq!(run("/nonexistent/program", &[], second), None);
        let started = Instant::now();
        assert_eq!(run("/bin/sh", &["-c", "echo early; sleep 30"], Duration::from_millis(200)), None);
        assert!(started.elapsed() < second, "stopped at the timeout, not after {:?}", started.elapsed());
    }
}
