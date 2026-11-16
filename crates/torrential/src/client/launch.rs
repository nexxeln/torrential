//! Codex kills a tool call's process group when the call returns, so the engine starts
//! from a launcher in a new session.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use torrential_core::error::Details;
use torrential_core::{Error, ErrorCode};

use super::connect::{self, Conn};
use torrential_daemon::daemon::OwnerLock;
use torrential_daemon::env::{Agent, Env};
use torrential_daemon::paths::Paths;

const START_WITHIN: Duration = Duration::from_secs(10);
const MAX_LOG: u64 = 10 * 1024 * 1024;

pub enum Blocked {
    Network {
        signal: String,
    },
    /// Connections to the engine's port on 127.0.0.1.
    Loopback {
        port: u16,
    },
}

pub fn sandbox_signal(env: &Env, send_udp: impl FnOnce() -> std::io::Result<()>) -> Option<String> {
    if env.var("CODEX_SANDBOX_NETWORK_DISABLED") == Some("1") {
        return Some("CODEX_SANDBOX_NETWORK_DISABLED=1".to_string());
    }
    match send_udp() {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => {
            Some(format!("sending a UDP datagram failed: {e}"))
        }
        _ => None,
    }
}

/// TEST-NET-1 routes nowhere, so only a sandbox or a host firewall denies the send itself.
pub fn send_udp() -> std::io::Result<()> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0))?;
    socket.send_to(&[0], ("192.0.2.1", 9)).map(drop)
}

pub fn sandbox_error(blocked: Blocked, env: &Env) -> Error {
    let agent = env.sandbox_harness();
    let (message, signal) = match blocked {
        Blocked::Network { signal } => {
            ("this sandbox blocks network access, so torrential cannot reach peers".to_string(), signal)
        }
        Blocked::Loopback { port } => (
            format!("this sandbox blocks the connection to the engine at 127.0.0.1:{port}"),
            "connect: operation not permitted".to_string(),
        ),
    };
    let hint = match agent {
        Some(Agent::ClaudeCode) => {
            r#"add "torrential *" to sandbox.excludedCommands in ~/.claude/settings.json, or approve running torrential outside the sandbox"#
        }
        Some(Agent::Codex) => {
            r#"in interactive Codex, rerun with sandbox_permissions "require_escalated" and prefix_rule ["torrential"]; unattended Codex (codex exec) has approvals off, so torrential cannot work there"#
        }
        _ => "run torrential outside the sandbox, or allow it network access",
    };
    let harness = agent.map(|a| match a {
        Agent::ClaudeCode => "claude-code",
        Agent::Codex => "codex",
        Agent::Gemini => "gemini",
        Agent::Cursor => "cursor",
        Agent::Other => "agent",
    });
    Error::new(ErrorCode::SandboxNetworkBlocked, message)
        .with_details(Details::Sandbox { signal, harness: harness.map(String::from) })
        .with_hint(hint)
}

pub fn start(paths: &Paths, env: &Env) -> Result<Conn, Error> {
    if let Some(signal) = sandbox_signal(env, send_udp) {
        return Err(sandbox_error(Blocked::Network { signal }, env));
    }
    let unavailable =
        |why: String| Error::new(ErrorCode::DaemonUnavailable, why).with_hint(format!("see {}", paths.log().display()));
    paths
        .make_state()
        .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot create {}: {e}", paths.state.display())))?;
    let _ = std::fs::remove_file(paths.startup_error());
    let exe = env.executable().map_err(|e| unavailable(e.message))?;
    let mut launcher = Command::new(exe);
    launcher.args(["daemon", "launch"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe, the only kind of call allowed between fork and exec.
    unsafe {
        launcher.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = launcher.spawn().map_err(|e| unavailable(format!("cannot start the engine: {e}")))?;
    let mut out = String::new();
    let _ = child.stdout.take().map(|mut s| s.read_to_string(&mut out));
    let status = child.wait().map_err(|e| unavailable(format!("cannot start the engine: {e}")))?;
    let pid: u32 = match out.trim().parse() {
        Ok(pid) if status.success() => pid,
        _ => return Err(unavailable(format!("the engine launcher failed: {}", out.trim()))),
    };
    let started = connect::poll(START_WITHIN, || {
        if let Some(conn) = connect::existing(paths, env)? {
            return Ok(Some(conn));
        }
        if !connect::alive(pid)
            && let Some(recorded) = paths.recorded_startup_error()
        {
            return Err(recorded.unwrap_or_else(|_| unavailable("the engine failed to start".into())));
        }
        Ok(None)
    })?;
    if let Some(started) = started {
        return Ok(started);
    }
    // The engine this started gave way to one that holds the state folder and does not answer.
    if let Some(holder) = OwnerLock::holder(&paths.lock()).filter(|holder| *holder != pid) {
        return Err(Error::new(
            ErrorCode::DaemonUnavailable,
            format!("another engine (pid {holder}) holds {} and does not answer", paths.state.display()),
        )
        .with_hint("stop it, then run the command again")
        .with_next("torrential engine stop"));
    }
    Err(unavailable(format!("the engine did not start within {}s", START_WITHIN.as_secs())))
}

pub fn launch_stage(env: &Env) -> u8 {
    close_inherited_descriptors();
    let result = (|| -> Result<u32, String> {
        let paths = Paths::from_process(env).map_err(|e| e.message)?;
        let log_path = paths.log();
        if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > MAX_LOG) {
            let _ = std::fs::rename(&log_path, paths.previous_log());
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).map_err(|e| e.to_string())?;
        let exe = env.executable().map_err(|e| e.message)?;
        let engine = Command::new(exe)
            .args(["daemon", "run"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok(engine.id())
    })();
    match result {
        Ok(pid) => {
            println!("{pid}");
            0
        }
        Err(e) => {
            println!("{e}");
            torrential_daemon::exit::code(ErrorCode::DaemonUnavailable)
        }
    }
}

fn close_inherited_descriptors() {
    let listed: Option<Vec<libc::c_int>> = std::fs::read_dir("/dev/fd")
        .ok()
        .map(|dir| dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok()).collect());
    let open = listed.unwrap_or_else(|| {
        let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        let max = if max <= 0 { 1024 } else { max.min(65_536) } as libc::c_int;
        (3..max).collect()
    });
    for fd in open.into_iter().filter(|fd| *fd > 2) {
        unsafe { libc::close(fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> Env {
        Env::from_pairs(vars.iter().copied())
    }

    #[test]
    fn codex_network_flag_counts_without_probing() {
        let signal = sandbox_signal(&env(&[("CODEX_SANDBOX_NETWORK_DISABLED", "1")]), || panic!("must not probe"));
        assert_eq!(signal.as_deref(), Some("CODEX_SANDBOX_NETWORK_DISABLED=1"));
    }

    #[test]
    fn only_a_denied_send_counts_as_a_sandbox() {
        let denied = || Err(std::io::Error::from_raw_os_error(libc::EPERM));
        let unreachable = || Err(std::io::Error::from_raw_os_error(libc::ENETUNREACH));
        assert!(sandbox_signal(&env(&[]), denied).unwrap().starts_with("sending a UDP datagram failed"));
        assert_eq!(sandbox_signal(&env(&[]), unreachable), None);
        assert_eq!(sandbox_signal(&env(&[]), || Ok(())), None);
    }

    #[test]
    fn the_engine_starts_from_the_binary_this_process_started_from() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("torrential");
        std::fs::write(&installed, "#!/bin/sh\necho \"asked to $*\"\nexit 1\n").unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = dir.path().join("th");
        let env = env(&[("TORRENTIAL_HOME", home.to_str().unwrap())]).with_executable(&installed);
        let Err(error) = start(&Paths::from_process(&env).unwrap(), &env) else {
            panic!("the stand-in started an engine")
        };
        assert_eq!(error.message, "the engine launcher failed: asked to daemon launch");
    }

    #[test]
    fn hints_name_the_detected_harness() {
        let blocked = || Blocked::Network { signal: "x".into() };
        let claude = sandbox_error(blocked(), &env(&[("CLAUDECODE", "1")]));
        assert!(claude.hint.unwrap().contains(r#""torrential *" to sandbox.excludedCommands"#));
        let codex = sandbox_error(blocked(), &env(&[("CODEX_THREAD_ID", "t")]));
        let hint = codex.hint.unwrap();
        assert!(hint.contains(r#"prefix_rule ["torrential"]"#) && hint.contains("cannot work there"), "{hint}");
        let loopback = sandbox_error(Blocked::Loopback { port: 47919 }, &env(&[]));
        assert_eq!(loopback.message, "this sandbox blocks the connection to the engine at 127.0.0.1:47919");
        assert_eq!(loopback.code, ErrorCode::SandboxNetworkBlocked);
    }
}
