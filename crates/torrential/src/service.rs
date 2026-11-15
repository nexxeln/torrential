use std::path::{Path, PathBuf};
use std::process::Command;

use torrential_core::{Error, ErrorCode};

use crate::env::{Env, TestHooks, hooks};
use crate::fsutil::write_atomic;
use crate::paths::{Paths, uid};
use crate::update::{Audience, executable};

pub const LABEL: &str = "dev.torrential.engine";
/// Its presence records that the person removed the login service, so the engine leaves it out.
const DECLINED: &str = "no-login-service";
/// The engine leaves this for the next command to say it added the login service.
const ADDED: &str = "login-service-added";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") { Self::Launchd } else { Self::Systemd }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd",
        }
    }

    pub fn unit_path(self, home: &Path) -> PathBuf {
        match self {
            Self::Launchd => home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")),
            Self::Systemd => home.join(".config/systemd/user/torrential.service"),
        }
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn unit(manager: Manager, exe: &Path, log: &Path, torrential_home: Option<&str>) -> String {
    let exe = exe.display().to_string();
    match manager {
        Manager::Launchd => {
            let env = torrential_home.map_or_else(String::new, |home| {
                format!(
                    "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>TORRENTIAL_HOME</key>\n    <string>{}</string>\n  </dict>\n",
                    xml(home)
                )
            });
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>daemon</string>
    <string>run</string>
    <string>--service</string>
  </array>
{env}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
                exe = xml(&exe),
                log = xml(&log.display().to_string()),
            )
        }
        Manager::Systemd => {
            let env =
                torrential_home.map_or_else(String::new, |home| format!("Environment=\"TORRENTIAL_HOME={home}\"\n"));
            format!(
                "[Unit]\nDescription=torrential engine\n\n[Service]\nExecStart=\"{exe}\" daemon run --service\n{env}Restart=on-failure\n\n[Install]\nWantedBy=default.target\n"
            )
        }
    }
}

torrential_core::words! {
    /// What `engine install` and `engine uninstall` did to the login service.
    pub enum Change {
        Installed = "installed",
        AlreadyInstalled = "already_installed",
        Uninstalled = "uninstalled",
        NotInstalled = "not_installed",
    }
}

pub struct Report {
    pub result: Change,
    pub manager: Manager,
    pub unit: PathBuf,
}

fn home(env: &Env) -> Result<PathBuf, Error> {
    env.var("HOME").map(PathBuf::from).ok_or_else(|| Error::new(ErrorCode::Usage, "HOME is not set"))
}

/// A test names a folder of stand-ins for launchctl and systemctl in `SERVICE_BIN`, so it never
/// changes the real login services.
fn run(env: &Env, program: &str, args: &[&str]) -> Result<(), Error> {
    let bin = env.test().and_then(|test| test.path(hooks::SERVICE_BIN));
    let path = bin.map_or_else(|| PathBuf::from(program), |bin| bin.join(program));
    let output = Command::new(path)
        .args(args)
        .output()
        .map_err(|e| Error::new(ErrorCode::Internal, format!("cannot run {program}: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::internal(format!(
        "{program} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Writes whatever of the unit, and on Linux the link that starts it at login, is not already in
/// place, without starting it. Returns whether it wrote anything.
fn put(manager: Manager, path: &Path, text: &str) -> Result<bool, Error> {
    let write_failed = |e: std::io::Error| {
        Error::new(ErrorCode::SandboxWriteBlocked, format!("cannot write {}: {e}", path.display()))
            .with_hint("run torrential engine install from a terminal outside any sandbox")
    };
    let dir = path.parent().expect("unit paths have a parent");
    let mut wrote = false;
    if std::fs::read(path).ok().as_deref() != Some(text.as_bytes()) {
        std::fs::create_dir_all(dir).map_err(write_failed)?;
        write_atomic(path, text.as_bytes()).map_err(write_failed)?;
        wrote = true;
    }
    if manager == Manager::Systemd {
        let wants = dir.join("default.target.wants");
        let link = wants.join("torrential.service");
        if std::fs::read_link(&link).ok().as_deref() != Some(path) {
            std::fs::create_dir_all(&wants).map_err(write_failed)?;
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(path, &link).map_err(write_failed)?;
            wrote = true;
        }
    }
    Ok(wrote)
}

/// Starts the service now and at every login, whether or not its unit just changed.
fn activate(env: &Env, manager: Manager, path: &Path) -> Result<(), Error> {
    match manager {
        Manager::Launchd => {
            let domain = format!("gui/{}", uid());
            let _ = run(env, "launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
            run(env, "launchctl", &["bootstrap", &domain, &path.display().to_string()])
        }
        Manager::Systemd => {
            run(env, "systemctl", &["--user", "daemon-reload"])?;
            run(env, "systemctl", &["--user", "enable", "--now", "torrential.service"])
        }
    }
}

pub fn install(env: &Env, paths: &Paths) -> Result<Report, Error> {
    let manager = Manager::current();
    let path = manager.unit_path(&home(env)?);
    let _ = std::fs::remove_file(paths.state.join(DECLINED));
    let text = unit(manager, &executable()?, &paths.log(), env.var("TORRENTIAL_HOME"));
    let wrote = put(manager, &path, &text)?;
    activate(env, manager, &path)?;
    let result = if wrote { Change::Installed } else { Change::AlreadyInstalled };
    Ok(Report { result, manager, unit: path })
}

pub fn uninstall(env: &Env, paths: &Paths) -> Result<Report, Error> {
    let manager = Manager::current();
    let path = manager.unit_path(&home(env)?);
    std::fs::create_dir_all(&paths.state)
        .and_then(|()| std::fs::write(paths.state.join(DECLINED), b""))
        .map_err(|e| Error::new(ErrorCode::StorageError, format!("cannot write {}: {e}", paths.state.display())))?;
    if !path.exists() {
        return Ok(Report { result: Change::NotInstalled, manager, unit: path });
    }
    match manager {
        Manager::Launchd => {
            let _ = run(env, "launchctl", &["bootout", &format!("gui/{}/{LABEL}", uid())]);
        }
        Manager::Systemd => {
            let _ = run(env, "systemctl", &["--user", "disable", "--now", "torrential.service"]);
        }
    }
    std::fs::remove_file(&path).map_err(|e| Error::internal(format!("cannot remove {}: {e}", path.display())))?;
    if manager == Manager::Systemd {
        let _ = run(env, "systemctl", &["--user", "daemon-reload"]);
    }
    Ok(Report { result: Change::Uninstalled, manager, unit: path })
}

/// Adds the login service the first time the default home's engine starts, unless the person
/// removed it with `torrential engine uninstall`. It takes effect at the next login. A test engine
/// adds one only when its test supplies stand-ins for launchctl and systemctl.
pub fn add_at_start(env: &Env, paths: &Paths) -> Result<Option<PathBuf>, Error> {
    let stand_ins = |test: TestHooks| test.var(hooks::SERVICE_BIN).is_some();
    if env.var("TORRENTIAL_HOME").is_some()
        || env.test().is_some_and(|test| !stand_ins(test))
        || paths.state.join(DECLINED).exists()
    {
        return Ok(None);
    }
    let manager = Manager::current();
    let path = manager.unit_path(&home(env)?);
    let text = unit(manager, &executable()?, &paths.log(), None);
    let current = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Error::new(ErrorCode::StorageError, format!("cannot read {}: {e}", path.display()))),
    };
    // Anything other than this unit, or the start of it that a failed write left, is someone else's.
    if !text.as_bytes().starts_with(&current) || !put(manager, &path, &text)? {
        return Ok(None);
    }
    let _ = std::fs::write(paths.state.join(ADDED), b"");
    Ok(Some(path))
}

/// Says once, after the engine added the login service, that it did.
pub fn notice(paths: &Paths, audience: Audience) -> Option<String> {
    std::fs::remove_file(paths.state.join(ADDED)).ok()?;
    Some(match audience {
        Audience::Person => {
            "The engine now starts when you log in. To turn that off, run torrential engine uninstall.".to_string()
        }
        Audience::Agent => "# the engine now starts at login; torrential engine uninstall turns that off".to_string(),
    })
}

/// The exit status after which launchd (`KeepAlive` without `SuccessfulExit`) and systemd
/// (`Restart=on-failure`) start the engine again, from the binary an update installed. Any other
/// clean stop exits 0, which neither restarts.
pub const RESTART: u8 = 75;

/// What an engine that exits for the login service leaves, so `engine stop` waits for the next
/// engine to stop it too. The next engine removes it once it is up.
#[derive(serde::Serialize, serde::Deserialize)]
struct Switching {
    pid: u32,
    at: i64,
}

/// How long a stop waits for the login service to start the next engine.
const SWITCH_WITHIN: i64 = 60;

pub fn leave_switching(paths: &Paths, at: i64) {
    let switching = Switching { pid: std::process::id(), at };
    let _ = write_atomic(&paths.switching(), &serde_json::to_vec(&switching).expect("records serialize"));
}

/// Whether an engine exited for the login service under a minute before `now`, and no engine has
/// started since.
pub fn switching(paths: &Paths, now: i64) -> bool {
    let Ok(bytes) = std::fs::read(paths.switching()) else { return false };
    serde_json::from_slice::<Switching>(&bytes).is_ok_and(|switching| now - switching.at < SWITCH_WITHIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_agent() {
        let text = unit(
            Manager::Launchd,
            Path::new("/opt/bin/torrential"),
            Path::new("/Users/ada/.local/state/torrential/engine.log"),
            None,
        );
        insta::assert_snapshot!(text);
    }

    #[test]
    fn the_line_about_an_added_service_comes_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let env = Env::from_pairs([("HOME", home.as_str()), ("TORRENTIAL_HOME", home.as_str())]);
        let paths = Paths::resolve(&env, 501, None).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        let said = |audience| notice(&paths, audience);

        assert_eq!(said(Audience::Person), None);
        std::fs::write(paths.state.join(ADDED), b"").unwrap();
        assert_eq!(
            [said(Audience::Person), said(Audience::Person)],
            [
                Some(
                    "The engine now starts when you log in. To turn that off, run torrential engine uninstall.".into()
                ),
                None
            ]
        );
        std::fs::write(paths.state.join(ADDED), b"").unwrap();
        assert_eq!(
            said(Audience::Agent).as_deref(),
            Some("# the engine now starts at login; torrential engine uninstall turns that off")
        );
    }

    #[test]
    fn systemd_unit_carries_a_relocated_home() {
        let text = unit(
            Manager::Systemd,
            Path::new("/usr/bin/torrential"),
            Path::new("/x/engine.log"),
            Some("/srv/torrential"),
        );
        insta::assert_snapshot!(text);
    }

    #[test]
    fn an_engine_that_exits_for_the_login_service_counts_as_switching_for_a_minute() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name);
        let paths = Paths {
            config: at("config"),
            state: at("state"),
            cache: at("cache"),
            data: at("data"),
            runtime: at("run"),
            downloads: at("downloads"),
        };
        std::fs::create_dir_all(&paths.state).unwrap();
        let before = switching(&paths, 1_000);

        leave_switching(&paths, 1_000);

        assert_eq!(
            (before, switching(&paths, 1_000), switching(&paths, 1_059), switching(&paths, 1_060)),
            (false, true, true, false)
        );
    }
}
