use std::path::{Path, PathBuf};
use std::process::Command;

use torrential_core::{Error, ErrorCode};

use crate::env::Env;
use crate::paths::uid;

pub const LABEL: &str = "dev.torrential.engine";

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

pub struct Report {
    pub result: &'static str,
    pub manager: Manager,
    pub unit: PathBuf,
}

fn home(env: &Env) -> Result<PathBuf, Error> {
    env.var("HOME").map(PathBuf::from).ok_or_else(|| Error::new(ErrorCode::Usage, "HOME is not set"))
}

fn run(program: &str, args: &[&str]) -> Result<(), Error> {
    let output = Command::new(program)
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

pub fn install(env: &Env, log: &Path) -> Result<Report, Error> {
    let manager = Manager::current();
    let path = manager.unit_path(&home(env)?);
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| Error::internal(format!("cannot find the binary: {e}")))?;
    let text = unit(manager, &exe, log, env.var("TORRENTIAL_HOME"));
    if std::fs::read_to_string(&path).is_ok_and(|current| current == text) {
        return Ok(Report { result: "already_installed", manager, unit: path });
    }
    let write_failed = |e: std::io::Error| {
        Error::new(ErrorCode::SandboxWriteBlocked, format!("cannot write {}: {e}", path.display()))
            .with_hint("run torrential engine install from a terminal outside any sandbox")
    };
    std::fs::create_dir_all(path.parent().expect("unit paths have a parent")).map_err(write_failed)?;
    std::fs::write(&path, &text).map_err(write_failed)?;
    let unit = path.display().to_string();
    match manager {
        Manager::Launchd => {
            let domain = format!("gui/{}", uid());
            let _ = run("launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
            run("launchctl", &["bootstrap", &domain, &unit])?;
        }
        Manager::Systemd => {
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", "--now", "torrential.service"])?;
        }
    }
    Ok(Report { result: "installed", manager, unit: path })
}

pub fn uninstall(env: &Env) -> Result<Report, Error> {
    let manager = Manager::current();
    let path = manager.unit_path(&home(env)?);
    if !path.exists() {
        return Ok(Report { result: "not_installed", manager, unit: path });
    }
    match manager {
        Manager::Launchd => {
            let _ = run("launchctl", &["bootout", &format!("gui/{}/{LABEL}", uid())]);
        }
        Manager::Systemd => {
            let _ = run("systemctl", &["--user", "disable", "--now", "torrential.service"]);
        }
    }
    std::fs::remove_file(&path).map_err(|e| Error::internal(format!("cannot remove {}: {e}", path.display())))?;
    if manager == Manager::Systemd {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(Report { result: "uninstalled", manager, unit: path })
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
    fn systemd_unit_carries_a_relocated_home() {
        let text = unit(
            Manager::Systemd,
            Path::new("/usr/bin/torrential"),
            Path::new("/x/engine.log"),
            Some("/srv/torrential"),
        );
        insta::assert_snapshot!(text);
    }
}
