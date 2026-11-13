use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::Command;

const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

struct Home {
    dir: tempfile::TempDir,
    vars: Vec<(&'static str, String)>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

const MAC: &str =
    r#"{"os": "macos", "display": {"width": 3024, "height": 1964, "hdr": true}, "arch": "arm64", "language": "en"}"#;

impl Home {
    fn new() -> Self {
        Self::with(&[])
    }

    fn with(vars: &[(&'static str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut all = vec![
            ("HOME", dir.path().display().to_string()),
            ("TORRENTIAL_HOME", dir.path().join("th").display().to_string()),
            ("TORRENTIAL_TEST_IDLE_TIMEOUT", "30s".to_string()),
            ("TORRENTIAL_TEST_NETWORK", "loopback".to_string()),
            ("TORRENTIAL_TEST_MACHINE", MAC.to_string()),
        ];
        all.extend(vars.iter().map(|(k, v)| (*k, v.to_string())));
        Self { dir, vars: all }
    }

    fn set(&mut self, key: &'static str, value: &str) {
        self.vars.retain(|(k, _)| *k != key);
        self.vars.push((key, value.to_string()));
    }

    fn unset(&mut self, key: &'static str) {
        self.vars.retain(|(k, _)| *k != key);
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::from_std(self.process(args));
        cmd.timeout(Duration::from_secs(60));
        cmd
    }

    fn process(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_torrential"));
        for key in [
            "CODEX_SANDBOX_NETWORK_DISABLED",
            "CODEX_THREAD_ID",
            "CODEX_CI",
            "TORRENTIAL_TEST_VERSION",
            "TORRENTIAL_TEST_ENGINE",
        ] {
            cmd.env_remove(key);
        }
        cmd.args(args).envs(self.vars.iter().map(|(k, v)| (*k, v.as_str())));
        cmd
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = self.command(args).output().unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
        }
    }

    fn engine(&self) -> Option<(i32, String)> {
        let run = self.run(&["daemon", "status", "--json"]);
        let doc: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
        Some((doc["pid"].as_i64()? as i32, doc["version"].as_str()?.to_string()))
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("th/state")
    }

    fn downloads(&self) -> String {
        self.dir.path().join("dl").display().to_string()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn eventually(what: &str, within: Duration, done: impl Fn() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn status_starts_an_engine_that_outlives_the_call() {
    let home = Home::new();
    let run = home.run(&["status"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "ID\tSTATE\tPROGRESS\tSIZE\tDOWN\tPEERS\tETA\tNAME\n# 0 downloads\nnext: torrential search <words>\n"
    );
    let (pid, version) = home.engine().expect("the engine is still running");
    assert!(alive(pid));
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    let lock = std::fs::read_to_string(home.state().join("engine.lock")).unwrap();
    assert_eq!(lock.trim(), pid.to_string());
    let log = std::fs::read_to_string(home.state().join("engine.log")).unwrap();
    assert!(log.contains(&format!("started, pid {pid}")), "{log}");
}

#[test]
fn concurrent_clients_share_one_engine() {
    let home = Home::new();
    let children: Vec<_> = (0..4).map(|_| home.command(&["status", "--json"]).output()).collect();
    for child in children {
        let out = child.unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let (pid, _) = home.engine().unwrap();
    assert_eq!(home.run(&["status"]).code, 0);
    assert_eq!(home.engine().unwrap().0, pid, "a second client attached to the first engine");
}

#[test]
fn a_version_mismatch_restarts_the_engine() {
    let mut home = Home::with(&[("TORRENTIAL_TEST_VERSION", "0.0.1-old")]);
    assert_eq!(home.run(&["status"]).code, 0);
    let (old_pid, old_version) = home.engine().unwrap();
    assert_eq!(old_version, "0.0.1-old");
    home.unset("TORRENTIAL_TEST_VERSION");
    let run = home.run(&["status"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let (new_pid, new_version) = home.engine().unwrap();
    assert_eq!(new_version, env!("CARGO_PKG_VERSION"));
    assert_ne!(new_pid, old_pid);
    assert!(!alive(old_pid), "the old engine exited after saving its state");
}

fn frame(json: &str) -> Vec<u8> {
    let mut bytes = (json.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(json.as_bytes());
    bytes
}

fn read_frame(stream: &mut std::os::unix::net::UnixStream) -> Option<serde_json::Value> {
    use std::io::Read;
    let mut len = [0; 4];
    stream.read_exact(&mut len).ok()?;
    let mut body = vec![0; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

#[test]
fn a_client_that_finds_a_newer_engine_uses_it_and_leaves_it_running() {
    let mut home = Home::with(&[("TORRENTIAL_TEST_VERSION", "9.0.0")]);
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();
    home.unset("TORRENTIAL_TEST_VERSION");

    let run = home.run(&["status"]);

    assert_eq!((run.code, run.stderr.as_str()), (0, ""));
    assert_eq!(home.engine(), Some((pid, "9.0.0".into())));
}

#[test]
fn a_stop_that_torrential_0_4_sends_for_its_own_version_leaves_a_newer_engine_running() {
    use std::io::Write;
    let home = Home::with(&[("TORRENTIAL_TEST_VERSION", "9.0.0")]);
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();

    // Byte for byte what torrential 0.4.0 sends when it meets an engine of another version.
    let mut old = std::os::unix::net::UnixStream::connect(home.dir.path().join("th/run/engine.sock")).unwrap();
    old.write_all(&frame(r#"{"op":"hello","client_version":"0.4.0","wire":5}"#)).unwrap();
    let hello = read_frame(&mut old).unwrap();
    old.write_all(&frame(r#"{"op":"shutdown"}"#)).unwrap();
    let answer = read_frame(&mut old);
    std::thread::sleep(Duration::from_secs(2));

    assert_eq!((hello["engine_version"].as_str(), hello["pid"].as_i64()), (Some("9.0.0"), Some(pid.into())));
    assert_eq!(answer.unwrap()["error"]["code"], "daemon_version");
    assert!(alive(pid), "the engine kept running");
    let stop = home.run(&["engine", "stop", "--json"]);
    assert_eq!(stop.code, 0, "{}", stop.stderr);
    eventually("an asked stop ends it", Duration::from_secs(10), || !alive(pid));
}

#[test]
fn a_client_that_cannot_talk_to_a_newer_engine_says_to_restart_it_and_leaves_it_running() {
    use std::io::Write;
    let home = Home::new();
    let socket = home.dir.path().join("th/run/engine.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let asked = std::thread::spawn(move || {
        let mut ops = Vec::new();
        for stream in listener.incoming().take(1) {
            let mut stream = stream.unwrap();
            while let Some(request) = read_frame(&mut stream) {
                ops.push(request["op"].as_str().unwrap().to_string());
                let hello = format!(
                    r#"{{"type":"hello","engine_version":"9.0.0","wire":99,"pid":{},"http_port":0}}"#,
                    std::process::id()
                );
                stream.write_all(&frame(&hello)).unwrap();
            }
        }
        ops
    });

    let run = home.run(&["status"]);

    assert_eq!(
        (run.code, run.stderr.as_str()),
        (6, "error[daemon_version]: the engine runs the newer torrential 9.0.0, so restart this torrential\n")
    );
    assert_eq!(asked.join().unwrap(), ["hello"], "it asked nothing else of the engine");
}

#[test]
fn engine_stop_also_stops_the_engine_an_update_restarted_into_meanwhile() {
    use std::io::Write;
    let home = Home::new();
    let socket = home.dir.path().join("th/run/engine.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // A stand-in engine: its first stop request finds it restarting into 0.1.1 under the same
    // process ID, and only a stop that reaches 0.1.1 ends it.
    let mut process = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let pid = process.id();
    let engine = std::thread::spawn(move || {
        let (mut version, mut stops) = ("0.1.0", Vec::new());
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            while let Some(request) = read_frame(&mut stream) {
                let reply = match request["op"].as_str().unwrap() {
                    "end" => return stops,
                    "hello" => format!(r#"{{"type":"hello","engine_version":"{version}","wire":5,"pid":{pid}}}"#),
                    _ => {
                        stops.push(version);
                        if version == "0.1.1" {
                            process.kill().unwrap();
                            process.wait().unwrap();
                        }
                        version = "0.1.1";
                        r#"{"type":"ok"}"#.to_string()
                    }
                };
                stream.write_all(&frame(&reply)).unwrap();
            }
        }
        stops
    });

    let stop = home.run(&["engine", "stop", "--json"]);
    let stopped = !alive(pid as i32);
    std::os::unix::net::UnixStream::connect(&socket).unwrap().write_all(&frame(r#"{"op":"end"}"#)).unwrap();

    assert_eq!((stop.code, stop.stderr.as_str()), (0, ""));
    assert_eq!(engine.join().unwrap(), ["0.1.0", "0.1.1"]);
    assert!(stopped, "the engine had exited when engine stop returned");
}

#[test]
fn a_sandboxed_client_refuses_to_start_an_engine() {
    let mut home = Home::with(&[("CODEX_SANDBOX_NETWORK_DISABLED", "1"), ("CODEX_THREAD_ID", "t-1")]);
    let run = home.run(&["status"]);
    assert_eq!(run.code, 6);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "error[sandbox_network_blocked]: this sandbox blocks network access, so torrential cannot reach peers\n  \
         signal: CODEX_SANDBOX_NETWORK_DISABLED=1 (codex)\n\
         hint: in interactive Codex, rerun with sandbox_permissions \"require_escalated\" and prefix_rule [\"torrential\"]; \
         unattended Codex (codex exec) has approvals off, so torrential cannot work there\n"
    );
    let json = home.run(&["status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!((json.code, doc["error"]["code"].as_str()), (6, Some("sandbox_network_blocked")));
    home.unset("CODEX_SANDBOX_NETWORK_DISABLED");
    home.unset("CODEX_THREAD_ID");
    assert_eq!(home.engine(), None, "no engine was started");
}

#[test]
fn the_engine_holds_no_descriptor_its_client_inherited() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let home = Home::new();
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut client = home.process(&["status"]);
    client.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let fd = writer.as_raw_fd();
    // SAFETY: dup2 is async-signal-safe. Its copy keeps no close-on-exec flag, like a
    // descriptor an agent harness hands its tools.
    unsafe {
        client.pre_exec(move || if libc::dup2(fd, 9) == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let status = client.status().unwrap();
    drop(writer);
    let (eof_tx, eof) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = eof_tx.send(reader.read_to_end(&mut Vec::new()).map(drop));
    });

    assert!(status.success());
    assert!(home.engine().is_some(), "the engine is running");
    assert!(
        matches!(eof.recv_timeout(Duration::from_secs(5)), Ok(Ok(()))),
        "the engine still holds the client's descriptor 9"
    );
}

#[test]
fn the_engine_exits_after_its_idle_timeout() {
    let mut home = Home::new();
    home.set("TORRENTIAL_TEST_IDLE_TIMEOUT", "1s");
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();
    eventually("the idle engine exits", Duration::from_secs(10), || !alive(pid));
    assert!(!home.dir.path().join("th/run/engine.sock").exists(), "the socket was removed");
    assert!(std::fs::read_to_string(home.state().join("engine.log")).unwrap().contains("engine stopped"));
}

/// A default-home engine with stand-ins for launchctl and systemctl, which record how they were
/// called, and the paths of its login service.
struct Service {
    home: Home,
    unit: PathBuf,
    calls: PathBuf,
}

impl Service {
    fn new() -> Self {
        let mut home =
            Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_HTTP_PORT", "0"), ("CLAUDECODE", "1")]);
        let dir = home.dir.path().to_path_buf();
        home.unset("TORRENTIAL_HOME");
        for (var, folder) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "run"),
        ] {
            home.set(var, &dir.join(folder).display().to_string());
        }
        let (bin, calls) = (dir.join("bin"), dir.join("calls"));
        std::fs::create_dir(&bin).unwrap();
        for program in ["launchctl", "systemctl"] {
            let script = format!("#!/bin/sh\necho \"{program} $*\" >> '{}'\n", calls.display());
            std::fs::write(bin.join(program), script).unwrap();
            std::fs::set_permissions(bin.join(program), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        home.set("TORRENTIAL_TEST_SERVICE_BIN", &bin.display().to_string());
        let unit = if cfg!(target_os = "macos") {
            dir.join("Library/LaunchAgents/dev.torrential.engine.plist")
        } else {
            dir.join(".config/systemd/user/torrential.service")
        };
        Self { home, unit, calls }
    }

    fn link(&self) -> PathBuf {
        self.home.dir.path().join(".config/systemd/user/default.target.wants/torrential.service")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

#[test]
fn the_first_engine_adds_a_login_service_and_uninstall_keeps_it_out() {
    let Service { home, unit, calls } = Service::new();
    let dir = home.dir.path().to_path_buf();
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_torrential")).unwrap();

    let first = home.run(&["status"]);
    let second = home.run(&["status"]);
    let added = std::fs::read_to_string(&unit).unwrap();
    let uninstall = home.json(&["engine", "uninstall", "--json"]);
    assert_eq!(home.run(&["engine", "stop"]).code, 0);
    let after_uninstall = home.run(&["status"]);
    let left_out = !unit.exists();
    let install = home.json(&["engine", "install", "--json"]);

    assert_eq!(
        (first.code, first.stderr.as_str()),
        (0, "# the engine now starts at login; torrential engine uninstall turns that off\n")
    );
    assert_eq!(second.stderr, "");
    assert!(added.contains(&exe.display().to_string()), "{added}");
    if cfg!(target_os = "linux") {
        let link = dir.join(".config/systemd/user/default.target.wants/torrential.service");
        assert_eq!(std::fs::read_link(link).unwrap(), unit);
    }
    assert_eq!(uninstall["result"], "uninstalled");
    assert_eq!((after_uninstall.code, after_uninstall.stderr.as_str()), (0, ""));
    assert!(left_out, "an engine started after uninstall leaves the service out");
    assert_eq!(install["result"], "installed");
    assert!(unit.exists());
    let uid = unsafe { libc::geteuid() };
    let expected = if cfg!(target_os = "macos") {
        format!(
            "launchctl bootout gui/{uid}/dev.torrential.engine\nlaunchctl bootout gui/{uid}/dev.torrential.engine\n\
             launchctl bootstrap gui/{uid} {}\n",
            unit.display()
        )
    } else {
        "systemctl --user disable --now torrential.service\nsystemctl --user daemon-reload\n\
         systemctl --user daemon-reload\nsystemctl --user enable --now torrential.service\n"
            .to_string()
    };
    assert_eq!(std::fs::read_to_string(&calls).unwrap(), expected, "adding the service at start ran neither");
}

#[test]
fn engine_install_starts_the_service_the_engine_added_at_its_first_start() {
    let service = Service::new();
    assert_eq!(service.home.run(&["status"]).code, 0);
    let added = std::fs::read_to_string(&service.unit).unwrap();

    let install = service.home.json(&["engine", "install", "--json"]);

    assert_eq!(install["result"], "already_installed");
    assert_eq!(std::fs::read_to_string(&service.unit).unwrap(), added);
    let uid = unsafe { libc::geteuid() };
    let expected = if cfg!(target_os = "macos") {
        format!(
            "launchctl bootout gui/{uid}/dev.torrential.engine\nlaunchctl bootstrap gui/{uid} {}\n",
            service.unit.display()
        )
    } else {
        "systemctl --user daemon-reload\nsystemctl --user enable --now torrential.service\n".to_string()
    };
    assert_eq!(service.calls(), expected, "the unit was there, and install still started it");
}

#[test]
fn a_login_service_left_half_written_is_finished_at_the_next_start() {
    let service = Service::new();
    assert_eq!(service.home.run(&["status"]).code, 0);
    let whole = std::fs::read_to_string(&service.unit).unwrap();
    assert_eq!(service.home.run(&["engine", "stop"]).code, 0);
    std::fs::write(&service.unit, &whole[..whole.len() / 2]).unwrap();
    if cfg!(target_os = "linux") {
        std::fs::remove_file(service.link()).unwrap();
    }

    let next = service.home.run(&["status"]);

    assert_eq!(
        (next.code, next.stderr.as_str()),
        (0, "# the engine now starts at login; torrential engine uninstall turns that off\n")
    );
    assert_eq!(std::fs::read_to_string(&service.unit).unwrap(), whole);
    if cfg!(target_os = "linux") {
        assert_eq!(std::fs::read_link(service.link()).unwrap(), service.unit);
    }
    let unit_dir = service.unit.parent().unwrap();
    let mut left: Vec<_> = std::fs::read_dir(unit_dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    left.retain(|name| name != "default.target.wants");
    assert_eq!(left, [service.unit.file_name().unwrap()], "no partial file is left behind");
}

#[test]
fn a_login_service_someone_else_wrote_is_left_alone() {
    let service = Service::new();
    std::fs::create_dir_all(service.unit.parent().unwrap()).unwrap();
    std::fs::write(&service.unit, "a unit the person wrote\n").unwrap();

    let first = service.home.run(&["status"]);

    assert_eq!((first.code, first.stderr.as_str()), (0, ""));
    assert_eq!(std::fs::read_to_string(&service.unit).unwrap(), "a unit the person wrote\n");
}

fn magnet(size: u64) -> String {
    format!("magnet:?xt=urn:btih:{ARCH}&dn=archlinux-2026.10.01-x86_64.iso&xl={size}")
}

#[test]
fn wait_times_out_with_exit_8_while_the_download_runs() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "1MiB")]);
    let get = home.run(&["get", &magnet(10 << 30), "--dir", &home.downloads()]);
    assert_eq!(get.code, 0, "{}", get.stderr);
    assert!(get.stdout.starts_with("started 9b5212"), "{}", get.stdout);
    assert!(get.stdout.ends_with("next:  torrential wait 9b5212\n"), "{}", get.stdout);
    let wait = home.run(&["wait", "9b5212", "--timeout", "1s"]);
    assert_eq!(wait.code, 8, "{}", wait.stderr);
    let lines: Vec<&str> = wait.stdout.lines().collect();
    assert_eq!(lines[0], "ELAPSED\tID\tSTATE\tPROGRESS\tSIZE\tDOWN\tPEERS\tETA\tNAME");
    assert!(lines[1].starts_with("0s\t9b5212\t"), "{}", wait.stdout);
    assert_eq!(lines[lines.len() - 2], "# still running after 1s", "{}", wait.stdout);
    assert_eq!(lines[lines.len() - 1], "next: torrential wait 9b5212");
    let json = home.run(&["wait", "9b5212", "--timeout", "1s", "--json"]);
    assert_eq!(json.code, 8);
    let last: serde_json::Value = serde_json::from_str(json.stdout.lines().last().unwrap()).unwrap();
    assert_eq!((last["type"].as_str(), last["outcome"].as_str()), (Some("result"), Some("timeout")));
}

#[test]
fn wait_ends_soon_after_the_engine_is_stopped() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "1MiB")]);
    assert_eq!(home.run(&["get", &magnet(10 << 30), "--dir", &home.downloads()]).code, 0);
    let wait = home
        .process(&["wait", "9b5212", "--timeout", "60s"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    assert_eq!(home.run(&["engine", "stop"]).code, 0);
    let wait = wait.wait_with_output().unwrap();

    assert!(started.elapsed() < Duration::from_secs(10), "wait took {:?}", started.elapsed());
    assert_eq!(
        (wait.status.code(), String::from_utf8(wait.stderr).unwrap().as_str()),
        (Some(6), "error[daemon_unavailable]: the engine stopped while waiting\n")
    );
}

#[test]
fn search_get_and_wait_rechecks_a_completed_download() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "100GiB")]);
    let search = home.run(&["search", "arch", "linux"]);
    assert_eq!(search.code, 0, "{}", search.stderr);
    assert_eq!(search.stdout.lines().nth(1).unwrap().split('\t').next(), Some("9b5212"));
    let get = home.run(&["get", "9b5212", "--dir", &home.downloads()]);
    assert_eq!(get.code, 0, "{}", get.stderr);
    let wait = home.run(&["wait", "9b5212"]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let path = Path::new(&home.downloads()).join("archlinux-2026.10.01-x86_64.iso");
    let last = wait.stdout.lines().last().unwrap();
    assert!(last.starts_with("# done after ") && last.ends_with(&format!(" · path {}", path.display())), "{last}");
    let again = home.run(&["get", "9b5212", "--dir", &home.downloads(), "--json"]);
    assert_eq!(again.code, 0, "{}{}", again.stdout, again.stderr);
    let doc: serde_json::Value = serde_json::from_str(&again.stdout).unwrap();
    assert_eq!(doc["reply"]["job"]["id"], ARCH);
    // Rechecking can start immediately or queue until the old engine handle is removed.
    assert!(matches!(doc["reply"]["outcome"].as_str(), Some("started" | "queued")), "{doc:#}");
    let checked = home.run(&["wait", "9b5212", "--json"]);
    assert_eq!(checked.code, 0, "{}{}", checked.stdout, checked.stderr);
    let last: serde_json::Value = serde_json::from_str(checked.stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["outcome"], "done");
}

const NIGHT: &str = "4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c";

/// Puts `PORT` in place of the player port, which is another one when a second engine holds it.
fn portless(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("127.0.0.1:") {
        let (before, after) = rest.split_at(at + "127.0.0.1:".len());
        out.push_str(before);
        out.push_str("PORT");
        rest = after.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    out + rest
}

#[test]
fn play_json_names_the_video_and_its_subtitles_and_opens_nothing() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "1MiB")]);
    assert_eq!(home.run(&["search", "night", "of", "the", "living", "dead"]).code, 0);
    let doc = home.json(&["play", "4c1e9a", "--json"]);
    let streams: Vec<(u64, String, u64, String)> = doc["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let url = portless(s["url"].as_str().unwrap());
            (s["index"].as_u64().unwrap(), s["path"].as_str().unwrap().into(), s["size"].as_u64().unwrap(), url)
        })
        .collect();
    assert_eq!(
        streams,
        [
            (
                0,
                "night_of_the_living_dead.mp4".into(),
                747 << 20,
                format!("http://127.0.0.1:PORT/{NIGHT}/0/night_of_the_living_dead.mp4")
            ),
            (
                3,
                "night_of_the_living_dead.srt".into(),
                49 << 10,
                format!("http://127.0.0.1:PORT/{NIGHT}/3/night_of_the_living_dead.srt")
            ),
        ]
    );
    assert_eq!(
        (doc["type"].as_str(), doc["name"].as_str(), doc["job"]["id"].as_str(), doc["next"].as_str()),
        (Some("play"), Some("night_of_the_living_dead"), Some(NIGHT), Some("torrential status 4c1e9a"))
    );
    assert_eq!(
        doc["job"]["selection"],
        serde_json::json!({"kind": "patterns", "value": ["night_of_the_living_dead.mp4", "night_of_the_living_dead.srt"]})
    );
}

#[test]
fn play_searches_for_words_and_for_an_id_that_names_nothing() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "1MiB")]);
    for words in [&["night", "of", "the", "living", "dead"][..], &["dead"]] {
        let doc = home.json(&[&["play"], words, &["--json"]].concat());
        assert_eq!(
            (doc["name"].as_str(), doc["job"]["id"].as_str()),
            (Some("Night of the Living Dead (1968)"), Some(NIGHT)),
            "{words:?}"
        );
    }
    let nothing = home.run(&["play", "zzzz", "nothing"]);
    assert_eq!(
        (nothing.code, nothing.stderr.as_str()),
        (
            3,
            "error[not_found]: no video result matches \"zzzz nothing\"\nhint: run torrential search zzzz nothing to \
             see all results\nnext: torrential search zzzz nothing\n"
        )
    );
}

#[test]
fn play_refuses_a_torrent_without_a_video() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake")]);
    assert_eq!(home.run(&["search", "night", "of", "the", "living", "dead"]).code, 0);
    let run = home.run(&["play", "c48e01"]);
    assert_eq!(
        (run.code, run.stderr.as_str()),
        (
            7,
            "error[no_selection]: Night of the Living Dead score has no video file\nhint: run torrential files c48e01 \
             to list its files\nnext: torrential files c48e01\n"
        )
    );
}

#[test]
fn play_opens_the_configured_player_with_the_subtitles_and_the_system_language() {
    let mut home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "1MiB")]);
    let bin = home.dir.path().join("bin");
    let argv = home.dir.path().join("argv");
    std::fs::create_dir_all(&bin).unwrap();
    let mpv = bin.join("mpv");
    std::fs::write(&mpv, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}.part\nmv {0}.part {0}\n", argv.display()))
        .unwrap();
    std::fs::set_permissions(&mpv, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(home.dir.path().join("th/config")).unwrap();
    std::fs::write(home.dir.path().join("th/config/config.yaml"), "player: mpv\n").unwrap();
    home.set("PATH", &format!("{}:/usr/bin:/bin", bin.display()));
    assert_eq!(home.run(&["search", "night", "of", "the", "living", "dead"]).code, 0);
    let run = home.run(&["play", "4c1e9a"]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (
            0,
            "4c1e9a night_of_the_living_dead\nvideo:     night_of_the_living_dead.mp4 (747.0 MiB)\nsubtitles: \
             night_of_the_living_dead.srt\n# playing in mpv · the download continues\nnext:      torrential \
             status 4c1e9a\n"
        ),
        "{}",
        run.stderr
    );
    eventually("the player starts", Duration::from_secs(10), || argv.exists());
    assert_eq!(
        portless(&std::fs::read_to_string(&argv).unwrap()),
        format!(
            "--sub-file=http://127.0.0.1:PORT/{NIGHT}/3/night_of_the_living_dead.srt\n--slang=en\n\
             --force-media-title=night_of_the_living_dead\nhttp://127.0.0.1:PORT/{NIGHT}/0/night_of_the_living_dead.mp4\n"
        )
    );
}

#[test]
fn a_killed_engine_comes_back_with_the_download_running() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "1MiB")]);
    assert_eq!(home.run(&["get", &magnet(10 << 30), "--dir", &home.downloads()]).code, 0);
    let (pid, _) = home.engine().unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("the killed engine is gone", Duration::from_secs(5), || !alive(pid));
    let resume = home.run(&["resume", "9b5212"]);
    assert_eq!(resume.code, 0, "{}", resume.stderr);
    assert_eq!(
        resume.stdout,
        "ID\tRESULT\tNAME\n9b5212\talready_running\tarchlinux-2026.10.01-x86_64.iso\nnext: torrential wait 9b5212\n"
    );
}

#[test]
fn a_new_fake_installation_starts_with_downloads_that_survive_a_restart() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake")]);
    let states = || {
        let run = home.run(&["status", "--json"]);
        let doc: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
        let jobs = doc["jobs"].as_array().unwrap();
        jobs.iter().map(|j| format!("{} {}", &j["id"].as_str().unwrap()[..6], j["state"].as_str().unwrap())).collect()
    };
    let first: Vec<String> = states();
    assert_eq!(first[..4], ["c7b9e3 done", "5e02c8 done", "91fd3b paused", "a90d44 error"]);
    assert!(["6f2c81 checking", "6f2c81 downloading"].contains(&first[4].as_str()), "{first:?}");
    assert_eq!(home.run(&["daemon", "stop"]).code, 0);
    let again: Vec<String> = states();
    assert_eq!(again[..4], ["c7b9e3 done", "5e02c8 done", "91fd3b paused", "a90d44 error"]);
    assert!(["6f2c81 checking", "6f2c81 downloading"].contains(&again[4].as_str()), "{again:?}");
}

#[test]
fn damaged_state_stops_the_engine_and_reaches_the_client_untouched() {
    let home = Home::new();
    std::fs::create_dir_all(home.state()).unwrap();
    let junk = b"not a database, but somebody's data";
    std::fs::write(home.state().join("state.db"), junk).unwrap();
    let run = home.run(&["status"]);
    assert_eq!(run.code, 6, "{}", run.stderr);
    let db = home.state().join("state.db");
    assert!(
        run.stderr.starts_with(&format!("error[state_corrupt]: the state database {} is damaged", db.display())),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("the state is never reset automatically"), "{}", run.stderr);
    assert_eq!(std::fs::read(&db).unwrap(), junk);
    assert_eq!(home.engine(), None);
}

#[test]
fn usage_errors_exit_2_with_a_hint() {
    let home = Home::new();
    let run = home.run(&["wait"]);
    assert_eq!(run.code, 2);
    assert!(
        run.stderr.starts_with("error[usage]: the following required arguments were not provided:"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.ends_with("hint: torrential wait --help\nnext: torrential wait --help\n"), "{}", run.stderr);
    let json = home.run(&["frobnicate", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!((json.code, doc["error"]["hint"].as_str()), (2, Some("torrential --help")));
    assert_eq!(doc["next"], "torrential --help");
    let bare = home.run(&[]);
    assert_eq!(bare.code, 0);
    assert!(bare.stdout.starts_with("torrential searches torrent indexes"));
}

#[test]
fn daemon_stop_is_idempotent() {
    let home = Home::new();
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();
    let stop = home.run(&["daemon", "stop"]);
    assert_eq!(stop.stdout, format!("result: stopped\npid:    {pid}\n"));
    assert!(!alive(pid));
    assert_eq!(home.run(&["daemon", "stop"]).stdout, "result: already_stopped\n");
}

const SKILL: &str = include_str!("../../../skills/torrential/SKILL.md");

impl Home {
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let run = self.run(args);
        assert_eq!(run.code, 0, "torrential {args:?}: {}{}", run.stdout, run.stderr);
        serde_json::from_str(&run.stdout).unwrap()
    }

    fn skill_results(&self, args: &[&str]) -> Vec<(String, String)> {
        let doc = self.json(args);
        let row = |c: &serde_json::Value| (c["target"].as_str().unwrap().into(), c["result"].as_str().unwrap().into());
        doc["changes"].as_array().unwrap().iter().map(row).collect()
    }
}

fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

#[test]
fn skill_install_links_claude_to_the_shared_copy_and_twice_changes_nothing() {
    let home = Home::new();
    let installed = home.skill_results(&["skill", "install", "--json"]);
    assert_eq!(installed, pairs(&[("agents", "installed"), ("claude", "installed")]));
    let root = home.dir.path().display().to_string();
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        home.run(&["skill", "status"]).stdout,
        format!(
            "TARGET\tSTATE\tVERSION\tPATH\n\
             agents\tup_to_date\t{version}\t{root}/.agents/skills/torrential\n\
             claude\tup_to_date\t{version}\t{root}/.claude/skills/torrential\n\
             # this binary includes the skill from torrential {version}\n"
        )
    );
    let copy = home.dir.path().join(".agents/skills/torrential");
    let link = home.dir.path().join(".claude/skills/torrential");
    assert_eq!(link.canonicalize().unwrap(), copy.canonicalize().unwrap());
    assert_eq!(std::fs::read_to_string(link.join("SKILL.md")).unwrap(), SKILL);
    let again = home.skill_results(&["skill", "install", "--json"]);
    assert_eq!(again, pairs(&[("agents", "up_to_date"), ("claude", "up_to_date")]));
    let removed = home.skill_results(&["skill", "uninstall", "--json"]);
    assert_eq!(removed, pairs(&[("agents", "removed"), ("claude", "removed")]));
    assert!(!link.exists() && !copy.exists(), "uninstall left {link:?} or {copy:?}");
}

#[test]
fn skill_show_prints_the_embedded_skill_and_help_points_agents_at_it() {
    let home = Home::new();
    assert_eq!(home.run(&["skill", "show"]).stdout, SKILL);
    let help = home.run(&["--help"]);
    assert_eq!(help.stdout.lines().last(), Some("Agents: run torrential skill install to add the torrential skill."));
}

#[test]
fn doctor_reports_whether_the_skill_is_installed() {
    let home = Home::new();
    let skill_row = |doc: serde_json::Value| {
        let checks = doc["checks"].as_array().unwrap().clone();
        let row = checks.into_iter().find(|c| c["check"] == "skill").expect("a skill row");
        (row["status"].as_str().unwrap().to_string(), row["detail"].as_str().unwrap().to_string())
    };
    let before = home.json(&["doctor", "--json"]);
    assert_eq!(before["next"], "torrential skill install");
    assert_eq!(skill_row(before), ("skipped".into(), "agents missing, claude missing".into()));
    home.json(&["skill", "install", "--json"]);
    let after = skill_row(home.json(&["doctor", "--json"]));
    assert_eq!(after, ("ok".into(), "agents up_to_date, claude up_to_date".into()));
}

#[test]
fn setup_shows_the_detected_machine_and_saves_and_clears_overrides() {
    let home = Home::new();
    let config = home.dir.path().join("th/config/config.yaml");
    let file = config.display().to_string();
    let flags = "torrential setup --screen phone|laptop|tv|tv-4k|auto --max-size 8GB|none --playback \
                 hevc-hdr|hevc|h264|auto --updates notify|auto|off";
    let mac = "detected: 3024x1964 display with HDR, arm64, English";

    let shown = home.run(&["setup"]);
    let saved = home.run(&["setup", "--screen", "tv-4k", "--max-size", "8GB"]);
    let saved_file = std::fs::read_to_string(&config).unwrap();
    let json = home.json(&["setup", "--playback", "h264", "--json"]);
    let cleared = home.run(&["setup", "--screen", "auto", "--max-size", "none", "--playback", "auto"]);

    assert_eq!(
        shown.stdout,
        format!(
            "{mac}\nupdates:  auto\nfile:     {file}\n\
             # ranking for this Mac: 1080p screen, HDR, HEVC, arm64, English · overrides: none\n\
             next:     {flags}\n"
        )
    );
    assert_eq!(
        saved.stdout,
        format!(
            "saved\n{mac}\nupdates:  auto\nfile:     {file}\n\
             # ranking for this Mac: 2160p screen, up to 8 GB, arm64, English · overrides: screen tv-4k, max_size \
             8GB\n# get rejects downloads over 8 GB unless --max-size is higher\nnext:     torrential search <words>\n"
        )
    );
    assert_eq!(saved_file, "preferences:\n  screen: tv-4k\n  max_size: 8GB\n");
    assert_eq!(
        (&json["result"], &json["profile"]["screen"], &json["profile"]["playback"], &json["profile"]["preferences"]),
        (
            &serde_json::json!("saved"),
            &serde_json::json!({"value": "2160p", "source": "override"}),
            &serde_json::json!({"value": "h264", "source": "override"}),
            &serde_json::json!({"screen": "tv-4k", "max_size": 8_000_000_000u64, "playback": "h264"}),
        )
    );
    assert_eq!(cleared.code, 0, "{}", cleared.stderr);
    assert!(cleared.stdout.contains("· overrides: none\n"), "{}", cleared.stdout);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "");

    let off = home.json(&["setup", "--updates", "off", "--json"]);
    assert_eq!((&off["result"], &off["updates"]), (&serde_json::json!("saved"), &serde_json::json!("off")));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "updates: off\n");
    assert!(home.run(&["setup"]).stdout.contains("\nupdates:  off\n"));
}

fn fake_torrent(name: &str, size: u64) -> Vec<u8> {
    let torrent = serde_json::json!({"info_hash": {"v1": "7".repeat(40)}, "name": name, "files": [[name, size]]});
    serde_json::to_vec(&torrent).unwrap()
}

#[test]
fn get_takes_its_size_cap_from_setup_unless_max_size_is_passed() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake")]);
    let file = home.dir.path().join("night.torrent");
    std::fs::write(&file, fake_torrent("night-of-the-living-dead-2160p.mkv", 3 << 30)).unwrap();
    let file = file.display().to_string();
    assert_eq!(home.run(&["setup", "--max-size", "2GB"]).code, 0);

    let capped = home.run(&["get", &file, "--dir", &home.downloads()]);
    let allowed = home.run(&["get", &file, "--dir", &home.downloads(), "--max-size", "3GiB"]);

    assert_eq!(capped.code, 7, "{}{}", capped.stdout, capped.stderr);
    assert_eq!(
        capped.stderr,
        "error[too_large]: the selection is 3.0 GiB, over the 2 GB cap from torrential setup\n\
         hint: add --max-size 3GiB to allow it, or pick fewer files with --files\n"
    );
    assert_eq!(allowed.code, 0, "{}", allowed.stderr);
    assert!(allowed.stdout.starts_with("started "), "{}", allowed.stdout);
}

#[test]
fn errors_name_a_reference_by_its_hash_or_kind_never_its_url() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake")]);
    let hash = "ab".repeat(20);
    let magnet = format!("magnet:?xt=urn:btih:{hash}&tr=https%3A%2F%2Fprivate.example%2Fannounce%2Fs3cret");

    let runs = [
        home.run(&["status", "https://private.example/dl/7.torrent?apikey=s3cret"]),
        home.run(&["status", &magnet]),
        home.run(&["pause", "udp://private.example/announce?passkey=s3cret"]),
    ];

    let errors: Vec<(i32, &str)> = runs.iter().map(|r| (r.code, r.stderr.as_str())).collect();
    assert_eq!(
        errors,
        [
            (
                3,
                "error[invalid_ref]: this command takes a download ID, not a .torrent URL\nhint: run torrential \
                 status to list download IDs\nnext: torrential status\n"
            ),
            (
                3,
                &*format!(
                    "error[not_found]: no download matches {hash}\nhint: run torrential status to list downloads\nnext: \
                     torrential status\n"
                )
            ),
            (
                3,
                "error[invalid_ref]: a URL is not a torrent reference unless it is a magnet link or an http or https \
                 URL\nhint: pass an ID from `torrential search`, a magnet link, a .torrent file, or a .torrent URL\n"
            ),
        ]
    );
}

#[test]
fn web_prints_the_address_of_the_web_app_and_opens_it_unless_told_not_to() {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    let mut home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_HTTP_PORT", "0")]);
    // Stands in for the browser, and records the address it was asked to open.
    let (opener, opened) = (home.dir.path().join("opener"), home.dir.path().join("opened"));
    std::fs::write(&opener, format!("#!/bin/sh\necho \"$1\" > '{}'\n", opened.display())).unwrap();
    std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
    home.set("TORRENTIAL_TEST_OPENER", &opener.display().to_string());

    let json = home.run(&["web", "--json"]);
    assert_eq!(json.code, 0, "{}", json.stderr);
    let doc: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let url = doc["url"].as_str().unwrap().to_string();
    assert_eq!((doc["type"].as_str(), doc["opened"].as_bool()), (Some("web"), Some(false)));
    let port: u16 = url.strip_prefix("http://127.0.0.1:").expect("a local address").parse().unwrap();
    let quiet = home.run(&["web", "--no-open"]);
    assert_eq!(quiet.stdout, format!("url:   {url}\nstate: ready\n# the web app works while the engine runs\n"));
    assert!(!opened.exists(), "neither --json nor --no-open opens a browser");

    let run = home.run(&["web"]);
    assert_eq!(
        run.stdout,
        format!("url:   {url}\nstate: opened in your browser\n# the web app works while the engine runs\n")
    );
    eventually("the browser is asked to open the address", Duration::from_secs(5), || {
        std::fs::read_to_string(&opened).is_ok_and(|asked| asked.trim() == url)
    });

    let mut page = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    page.write_all(b"GET /api/jobs HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = String::new();
    page.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.contains("\r\n\r\n{\"jobs\":["), "{answer}");
}

#[test]
fn an_open_page_keeps_the_engine_running_and_a_closed_one_lets_it_exit() {
    use std::io::{Read, Write};
    let mut home = Home::with(&[("TORRENTIAL_TEST_HTTP_PORT", "0")]);
    home.set("TORRENTIAL_TEST_IDLE_TIMEOUT", "1s");
    let json = home.run(&["web", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let port: u16 = doc["url"].as_str().unwrap().rsplit(':').next().unwrap().parse().unwrap();
    let (pid, _) = home.engine().unwrap();
    // The page follows downloads for as long as it is open.
    let mut page = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    page.write_all(b"GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    let mut head = [0; 15];
    page.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"HTTP/1.1 200 OK");
    std::thread::sleep(Duration::from_secs(3));
    assert!(alive(pid), "the engine exited under an open page");
    drop(page);
    eventually("the idle engine exits", Duration::from_secs(10), || !alive(pid));
}
