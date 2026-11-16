use std::path::{Path, PathBuf};
use std::process::Command;

use torrential_core::{Error, ErrorCode};

use crate::env::{Env, TestHooks, hooks};
use crate::fsutil::write_atomic;
use crate::paths::Paths;
use crate::update::Audience;

pub const LABEL: &str = "dev.torrential.engine";

/// The name of the Windows logon task.
pub const TASK: &str = "torrential engine";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Manager {
    Launchd,
    Systemd,
    /// A per-user task that Task Scheduler runs at logon.
    TaskScheduler,
}

impl Manager {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Launchd
        } else if cfg!(windows) {
            Self::TaskScheduler
        } else {
            Self::Systemd
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd",
            Self::TaskScheduler => "task scheduler",
        }
    }

    /// Where the unit is. Task Scheduler keeps its tasks itself, so torrential keeps the task it
    /// registered in its state folder, to know what it registered.
    pub fn unit_path(self, home: &Path, paths: &Paths) -> PathBuf {
        match self {
            Self::Launchd => home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")),
            Self::Systemd => home.join(".config/systemd/user/torrential.service"),
            Self::TaskScheduler => paths.state.join("logon-task.xml"),
        }
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn unit(manager: Manager, exe: &Path, log: &Path, torrential_home: Option<&str>, user: &str) -> String {
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
        Manager::TaskScheduler => {
            // A task passes no environment and keeps no output, so cmd sets TORRENTIAL_HOME and
            // appends what the engine prints to its log. conhost gives it a console with no window.
            let set = torrential_home.map_or_else(String::new, |home| format!("set \"TORRENTIAL_HOME={home}\"&& "));
            let exe = without_verbatim(&exe);
            let command =
                format!("cmd.exe /d /s /c \"{set}\"{exe}\" daemon run --service >> \"{}\" 2>&1\"", log.display());
            format!(
                r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the torrential engine when you log in.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>conhost.exe</Command>
      <Arguments>--headless {command}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
                user = xml(user),
                command = xml(&command),
            )
        }
    }
}

/// `path` without the `\\?\` a canonical Windows path starts with, which cmd cannot run.
fn without_verbatim(path: &str) -> &str {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => path,
    }
}

/// The person the login service runs as, as Task Scheduler names them: `DOMAIN\user`.
fn user(env: &Env) -> String {
    match (env.var("USERDOMAIN"), env.var("USERNAME")) {
        (Some(domain), Some(name)) => format!("{domain}\\{name}"),
        (None, Some(name)) => name.to_string(),
        _ => String::new(),
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

fn home(env: &Env) -> Result<&Path, Error> {
    env.home().ok_or_else(|| Error::new(ErrorCode::Usage, "HOME is not set"))
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

/// The unit as its file holds it. Task Scheduler reads a task's XML as UTF-16.
fn encoded(manager: Manager, text: &str) -> Vec<u8> {
    match manager {
        Manager::TaskScheduler => {
            [0xFEFFu16].into_iter().chain(text.encode_utf16()).flat_map(u16::to_le_bytes).collect()
        }
        Manager::Launchd | Manager::Systemd => text.as_bytes().to_vec(),
    }
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
    let bytes = encoded(manager, text);
    if std::fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::create_dir_all(dir).map_err(write_failed)?;
        write_atomic(path, &bytes).map_err(write_failed)?;
        wrote = true;
    }
    #[cfg(unix)]
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
            let domain = gui_domain();
            let _ = run(env, "launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
            run(env, "launchctl", &["bootstrap", &domain, &path.display().to_string()])
        }
        Manager::Systemd => {
            run(env, "systemctl", &["--user", "daemon-reload"])?;
            run(env, "systemctl", &["--user", "enable", "--now", "torrential.service"])
        }
        Manager::TaskScheduler => {
            run(env, "schtasks", &["/Create", "/TN", TASK, "/XML", &path.display().to_string(), "/F"])?;
            run(env, "schtasks", &["/Run", "/TN", TASK])
        }
    }
}

/// launchd's domain for this person's agents.
fn gui_domain() -> String {
    #[cfg(unix)]
    let uid = crate::paths::uid();
    // launchd runs only on macOS.
    #[cfg(not(unix))]
    let uid = 0;
    format!("gui/{uid}")
}

pub fn install(env: &Env, paths: &Paths) -> Result<Report, Error> {
    let manager = Manager::current();
    let path = manager.unit_path(home(env)?, paths);
    let _ = std::fs::remove_file(paths.login_service_declined());
    let text = unit(manager, &env.installed()?, &paths.log(), env.var("TORRENTIAL_HOME"), &user(env));
    let wrote = put(manager, &path, &text)?;
    activate(env, manager, &path)?;
    let result = if wrote { Change::Installed } else { Change::AlreadyInstalled };
    Ok(Report { result, manager, unit: path })
}

pub fn uninstall(env: &Env, paths: &Paths) -> Result<Report, Error> {
    let manager = Manager::current();
    let path = manager.unit_path(home(env)?, paths);
    std::fs::create_dir_all(&paths.state)
        .and_then(|()| std::fs::write(paths.login_service_declined(), b""))
        .map_err(|e| Error::new(ErrorCode::StorageError, format!("cannot write {}: {e}", paths.state.display())))?;
    if !path.exists() {
        return Ok(Report { result: Change::NotInstalled, manager, unit: path });
    }
    match manager {
        Manager::Launchd => {
            let _ = run(env, "launchctl", &["bootout", &format!("{}/{LABEL}", gui_domain())]);
        }
        Manager::Systemd => {
            let _ = run(env, "systemctl", &["--user", "disable", "--now", "torrential.service"]);
        }
        // Deleting a task leaves its engine running, so the command stops the engine itself.
        Manager::TaskScheduler => {
            let _ = run(env, "schtasks", &["/Delete", "/TN", TASK, "/F"]);
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
        || paths.login_service_declined().exists()
    {
        return Ok(None);
    }
    let manager = Manager::current();
    let path = manager.unit_path(home(env)?, paths);
    let text = unit(manager, &env.installed()?, &paths.log(), None, &user(env));
    let current = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Error::new(ErrorCode::StorageError, format!("cannot read {}: {e}", path.display()))),
    };
    // Anything other than this unit, or the start of it that a failed write left, is someone else's.
    if !encoded(manager, &text).starts_with(&current) || !put(manager, &path, &text)? {
        return Ok(None);
    }
    if manager == Manager::TaskScheduler {
        // Task Scheduler runs only a task registered with it.
        run(env, "schtasks", &["/Create", "/TN", TASK, "/XML", &path.display().to_string(), "/F"])?;
    }
    let _ = std::fs::write(paths.login_service_added(), b"");
    Ok(Some(path))
}

/// Says once, after the engine added the login service, that it did.
pub fn notice(paths: &Paths, audience: Audience) -> Option<String> {
    std::fs::remove_file(paths.login_service_added()).ok()?;
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

/// Starts the engine of the release an update installed, once this one has let go of its state
/// folder and port. launchd and systemd do it themselves when the engine exits with `RESTART`.
/// Task Scheduler does not, so on Windows the engine starts it, on the console it runs on.
pub fn start_next(env: &Env, paths: &Paths) {
    if Manager::current() != Manager::TaskScheduler {
        return;
    }
    let started = env.installed().and_then(|exe| {
        let failed = |e: std::io::Error| Error::internal(format!("cannot start {}: {e}", exe.display()));
        let log = std::fs::OpenOptions::new().create(true).append(true).open(paths.log()).map_err(failed)?;
        Command::new(&exe)
            .args(["daemon", "run", "--service"])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(failed)?)
            .stderr(log)
            .spawn()
            .map(drop)
            .map_err(failed)
    });
    if let Err(e) = started {
        crate::log::log(&format!("cannot start the next engine: {}", e.message));
    }
}

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
    fn the_line_about_an_added_service_comes_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let env = Env::from_pairs([("HOME", home.as_str()), ("TORRENTIAL_HOME", home.as_str())]);
        let paths = Paths::resolve(&env, 501, None).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        let said = |audience| notice(&paths, audience);

        assert_eq!(said(Audience::Person), None);
        std::fs::write(paths.login_service_added(), b"").unwrap();
        assert_eq!(
            [said(Audience::Person), said(Audience::Person)],
            [
                Some(
                    "The engine now starts when you log in. To turn that off, run torrential engine uninstall.".into()
                ),
                None
            ]
        );
        std::fs::write(paths.login_service_added(), b"").unwrap();
        assert_eq!(
            said(Audience::Agent).as_deref(),
            Some("# the engine now starts at login; torrential engine uninstall turns that off")
        );
    }

    #[test]
    fn the_logon_task_runs_the_engine_for_this_person_with_no_window_and_logs_what_it_prints() {
        let exe = Path::new(r"C:\Users\Ada & Bo\AppData\Local\Programs\torrential\torrential.exe");
        let log = Path::new(r"C:\Users\Ada & Bo\AppData\Local\torrential\state\engine.log");
        let task = unit(Manager::TaskScheduler, exe, log, None, r"STUDIO\ada");
        let line = |tag: &str| task.lines().find(|line| line.contains(tag)).unwrap().trim().to_string();
        assert_eq!(line("<UserId>"), r"<UserId>STUDIO\ada</UserId>");
        assert_eq!(line("<Command>"), "<Command>conhost.exe</Command>");
        assert_eq!(
            line("<Arguments>"),
            r#"<Arguments>--headless cmd.exe /d /s /c ""C:\Users\Ada &amp; Bo\AppData\Local\Programs\torrential\torrential.exe" daemon run --service &gt;&gt; "C:\Users\Ada &amp; Bo\AppData\Local\torrential\state\engine.log" 2&gt;&amp;1"</Arguments>"#
        );
        let canonical = Path::new(r"\\?\C:\Users\Ada & Bo\AppData\Local\Programs\torrential\torrential.exe");
        assert_eq!(
            unit(Manager::TaskScheduler, canonical, log, None, r"STUDIO\ada"),
            task,
            "cmd runs no verbatim path"
        );
        let home = unit(Manager::TaskScheduler, exe, log, Some(r"D:\th"), r"STUDIO\ada");
        assert!(home.contains(r#"/c "set "TORRENTIAL_HOME=D:\th"&amp;&amp; "C:\"#), "{home}");
        assert_eq!(encoded(Manager::TaskScheduler, "<a/>")[..4], [0xFF, 0xFE, b'<', 0]);
    }
}
