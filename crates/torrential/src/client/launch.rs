//! Codex kills a tool call's process group when the call returns, so the engine starts
//! from a launcher in a new session.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use torrential_core::error::Details;
use torrential_core::{Error, ErrorCode};

use super::connect::{self, Conn};
use crate::env::{Agent, Env};
use crate::paths::Paths;

const START_WITHIN: Duration = Duration::from_secs(10);
const MAX_LOG: u64 = 10 * 1024 * 1024;

pub enum Blocked {
    Network { signal: String },
    Ipc { socket: PathBuf },
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
        Blocked::Ipc { socket } => (
            format!("this sandbox blocks the engine socket at {}", socket.display()),
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
        _ => "run torrential outside the sandbox, or allow it network and Unix socket access",
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
    std::fs::create_dir_all(&paths.state)
        .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot create {}: {e}", paths.state.display())))?;
    let _ = std::fs::remove_file(paths.startup_error());
    let exe = std::env::current_exe().map_err(|e| unavailable(format!("cannot find the torrential binary: {e}")))?;
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
    let started = poll(START_WITHIN, || {
        if let Some(conn) = connect::existing(paths, env)? {
            return Ok(Some(conn));
        }
        if !connect::alive(pid)
            && let Ok(bytes) = std::fs::read(paths.startup_error())
        {
            return Err(
                serde_json::from_slice(&bytes).unwrap_or_else(|_| unavailable("the engine failed to start".into()))
            );
        }
        Ok(None)
    })?;
    started.ok_or_else(|| unavailable(format!("the engine did not start within {}s", START_WITHIN.as_secs())))
}

fn poll<T>(within: Duration, mut ready: impl FnMut() -> Result<Option<T>, Error>) -> Result<Option<T>, Error> {
    let started = Instant::now();
    loop {
        if let Some(found) = ready()? {
            return Ok(Some(found));
        }
        let waited = started.elapsed();
        if waited > within {
            return Ok(None);
        }
        std::thread::sleep(pause(waited));
    }
}

fn pause(waited: Duration) -> Duration {
    (waited / 10).clamp(Duration::from_millis(2), Duration::from_millis(20))
}

pub fn launch_stage(env: &Env) -> u8 {
    close_inherited_descriptors();
    let result = (|| -> Result<u32, String> {
        let paths = Paths::from_process(env).map_err(|e| e.message)?;
        let log_path = paths.log();
        if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > MAX_LOG) {
            let _ = std::fs::rename(&log_path, paths.state.join("engine.log.1"));
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).map_err(|e| e.to_string())?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
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
            ErrorCode::DaemonUnavailable.exit_code()
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
    fn the_wait_for_the_engine_asks_often_at_first_and_less_later() {
        let ms = Duration::from_millis;
        let pauses = [ms(0), ms(25), ms(100), ms(10_000)].map(pause);
        assert_eq!(pauses, [ms(2), Duration::from_micros(2_500), ms(10), ms(20)]);
    }

    #[test]
    fn the_wait_for_the_engine_ends_when_it_answers_fails_or_runs_out() {
        let mut asked = 0;
        let answered = poll(Duration::from_secs(10), || {
            asked += 1;
            Ok((asked == 3).then_some("conn"))
        });
        let failed = poll(Duration::from_secs(10), || Err::<Option<()>, _>(Error::internal("startup failed")));
        let started = Instant::now();
        let mut idle_asks = 0;
        let ran_out = poll(Duration::from_millis(300), || {
            idle_asks += 1;
            Ok(None::<()>)
        });

        assert_eq!((answered.unwrap(), asked), (Some("conn"), 3));
        assert_eq!(failed.unwrap_err().message, "startup failed");
        assert_eq!(ran_out.unwrap(), None);
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(idle_asks < 60, "asked {idle_asks} times in 300 ms");
    }

    #[test]
    fn hints_name_the_detected_harness() {
        let blocked = || Blocked::Network { signal: "x".into() };
        let claude = sandbox_error(blocked(), &env(&[("CLAUDECODE", "1")]));
        assert!(claude.hint.unwrap().contains(r#""torrential *" to sandbox.excludedCommands"#));
        let codex = sandbox_error(blocked(), &env(&[("CODEX_THREAD_ID", "t")]));
        let hint = codex.hint.unwrap();
        assert!(hint.contains(r#"prefix_rule ["torrential"]"#) && hint.contains("cannot work there"), "{hint}");
        let ipc = sandbox_error(Blocked::Ipc { socket: "/tmp/t/engine.sock".into() }, &env(&[]));
        assert_eq!(ipc.message, "this sandbox blocks the engine socket at /tmp/t/engine.sock");
        assert_eq!(ipc.code, ErrorCode::SandboxNetworkBlocked);
    }
}
