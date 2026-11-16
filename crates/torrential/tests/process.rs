use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use torrential_core::protocol::{Request, WIRE_VERSION};
use torrential_daemon::wire::{Access, Client};

mod common;
use common::{Home, alive, eventually, hooks, portless, read_post};

const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

impl Home {
    fn engine(&self) -> Option<(i32, String)> {
        let run = self.run(&["daemon", "status", "--json"]);
        let doc: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
        Some((doc["pid"].as_i64()? as i32, doc["version"].as_str()?.to_string()))
    }

    fn state(&self) -> PathBuf {
        self.path("th/state")
    }

    fn downloads(&self) -> String {
        self.path("dl").display().to_string()
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
    let piped = std::process::Stdio::piped;
    let children: Vec<_> =
        (0..4).map(|_| home.process(&["status", "--json"]).stdout(piped()).stderr(piped()).spawn().unwrap()).collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let (pid, _) = home.engine().unwrap();
    assert_eq!(home.run(&["status"]).code, 0);
    assert_eq!(home.engine().unwrap().0, pid, "a second client attached to the first engine");
}

#[test]
fn a_version_mismatch_restarts_the_engine() {
    let mut home = Home::new().with(hooks::VERSION, "0.0.1-old");
    assert_eq!(home.run(&["status"]).code, 0);
    let (old_pid, old_version) = home.engine().unwrap();
    assert_eq!(old_version, "0.0.1-old");
    home.unset(hooks::VERSION);
    let run = home.run(&["status"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let (new_pid, new_version) = home.engine().unwrap();
    assert_eq!(new_version, env!("CARGO_PKG_VERSION"));
    assert_ne!(new_pid, old_pid);
    assert!(!alive(old_pid), "the old engine exited after saving its state");
}

#[test]
fn a_client_that_finds_a_newer_engine_says_to_restart_this_torrential_and_leaves_it_running() {
    let mut home = Home::new().with(hooks::VERSION, "9.0.0");
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();
    home.unset(hooks::VERSION);

    let runs = [home.run(&["status"]), home.run(&["status"])];

    for run in runs {
        assert_eq!(
            (run.code, run.stderr.as_str()),
            (6, "error[daemon_version]: the engine runs the newer torrential 9.0.0, so restart this torrential\n")
        );
    }
    assert_eq!(home.engine(), Some((pid, "9.0.0".into())));
}

#[test]
fn a_stop_that_an_older_torrential_sends_for_its_own_version_leaves_a_newer_engine_running() {
    let home = Home::new().with(hooks::VERSION, "9.0.0");
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();

    let old = Client { version: "0.6.0", wire: WIRE_VERSION - 1 };
    let hello = home.ask_as(old, &Request::Hello);
    let status = home.ask_as(old, &Request::Status { refs: Vec::new() });
    let answer = home.ask_as(old, &Request::Shutdown { asked: false });

    assert_eq!((hello["engine_version"].as_str(), hello["pid"].as_i64()), (Some("9.0.0"), Some(pid.into())));
    assert_eq!(
        status["error"]["message"],
        format!("the engine uses protocol version {WIRE_VERSION}, but this client uses {}", WIRE_VERSION - 1)
    );
    assert_eq!(answer["error"]["code"], "daemon_version");
    assert_eq!(home.engine(), Some((pid, "9.0.0".into())), "the engine kept running");
    let stop = home.run(&["engine", "stop", "--json"]);
    assert_eq!(stop.code, 0, "{}", stop.stderr);
    eventually("an asked stop ends it", Duration::from_secs(10), || !alive(pid));
}

#[test]
fn a_newer_client_uses_an_engine_that_plays_a_film_and_switches_it_once_the_film_ends() {
    use std::io::{Read, Write};
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port().to_string();
    let mut home = Home::new()
        .with(hooks::ENGINE, "fake")
        .with(hooks::FAKE_RATE, "0")
        .with(hooks::HTTP_PORT, &port)
        .with(hooks::VERSION, "0.1.0");
    let jobs: serde_json::Value = serde_json::from_str(&home.run(&["status", "--json"]).stdout).unwrap();
    let jobs = jobs["jobs"].as_array().unwrap();
    let id = jobs.iter().find(|j| j["name"] == "wikipedia-en-2026-09-text").unwrap()["id"].as_str().unwrap();
    let (pid, _) = home.engine().unwrap();
    // A player asks for the end of a file that has not arrived, so its stream stays open.
    let mut player = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    let ask = format!("GET /{id}/0/enwiki.xml.bz2 HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=23514217000-\r\n\r\n");
    player.write_all(ask.as_bytes()).unwrap();
    let mut head = [0; 12];
    player.read_exact(&mut head).unwrap();

    let newer = Client { version: "0.1.1", wire: WIRE_VERSION };
    home.ask_as(newer, &Request::Hello);
    let answer = home.ask_as(newer, &Request::Shutdown { asked: false });
    home.set(hooks::VERSION, "0.1.1");
    let during = home.run(&["status", "--json"]);
    let kept = home.engine();
    drop(player);
    eventually("the next command switches the engine", Duration::from_secs(20), || {
        home.run(&["status", "--json"]).code == 0 && home.engine().is_some_and(|(_, version)| version == "0.1.1")
    });

    assert_eq!(&head, b"HTTP/1.1 206");
    assert_eq!(
        (answer["error"]["code"].as_str(), answer["error"]["message"].as_str()),
        (Some("daemon_version"), Some("torrential switches to 0.1.1 when the film ends"))
    );
    assert_eq!((during.code, during.stderr.as_str()), (0, ""), "the newer client used the engine meanwhile");
    assert_eq!(kept, Some((pid, "0.1.0".into())), "the engine kept its version while the film played");
    assert_ne!(home.engine().unwrap().0, pid);
}

#[test]
fn a_newer_client_that_cannot_talk_to_an_engine_playing_a_film_says_when_it_switches() {
    let home = Home::new();
    let asked = home.stand_in(|request| match request["op"].as_str().unwrap() {
        "hello" => (json!({"type": "hello", "engine_version": "0.1.0", "wire": 99, "pid": std::process::id()}), false),
        _ => (refused_switch(), true),
    });

    let run = home.run(&["status"]);

    assert_eq!(
        (run.code, run.stderr.as_str()),
        (6, "error[daemon_version]: torrential switches to 0.4.0 when the film ends\n")
    );
    assert_eq!(asked.join().unwrap(), ["hello", "shutdown"]);
}

const NEWER_ENGINE: &str =
    "error[daemon_version]: the engine runs the newer torrential 9.0.0, so restart this torrential\n";

fn refused_switch() -> Value {
    json!({"type": "error", "error": {"code": "daemon_version", "message": "torrential switches to 0.4.0 when the film ends"}})
}

/// A stand-in engine that answers as each torrential of `versions` in turn: hello with the version,
/// a refused switch to shutdown, which hands over to the next version, and no downloads to the rest.
/// It stops after the last version's hello.
fn stand_in(home: &Home, versions: &'static [&'static str]) -> std::thread::JoinHandle<Vec<String>> {
    let mut at = 0;
    home.stand_in(move |request| match request["op"].as_str().unwrap() {
        "hello" => {
            let hello = json!({"type": "hello", "engine_version": versions[at], "wire": WIRE_VERSION, "pid": std::process::id()});
            (hello, at + 1 == versions.len())
        }
        "shutdown" => {
            at += 1;
            (refused_switch(), false)
        }
        _ => (json!({"type": "jobs", "jobs": []}), false),
    })
}

#[test]
fn a_client_whose_engine_gives_way_to_a_newer_one_says_to_restart_this_torrential() {
    use std::os::fd::AsRawFd;
    let home = Home::new();
    std::fs::create_dir_all(home.state()).unwrap();
    // A newer engine holds the home's lock as it starts, so the engine this client launches gives way.
    let lock = std::fs::File::create(home.state().join("engine.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let client = home
        .process(&["status"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    eventually("the launched engine gave way", Duration::from_secs(10), || {
        std::fs::read_to_string(home.state().join("engine.log")).is_ok_and(|log| log.contains("another engine owns"))
    });
    let newer = stand_in(&home, &["9.0.0"]);

    let client = client.wait_with_output().unwrap();

    assert_eq!((client.status.code(), String::from_utf8(client.stderr).unwrap().as_str()), (Some(6), NEWER_ENGINE));
    newer.join().unwrap();
}

#[test]
fn a_client_that_an_engine_playing_a_film_turns_away_never_uses_a_newer_one_it_finds_next() {
    let home = Home::new();
    let engines = stand_in(&home, &["0.1.0", "9.0.0"]);

    let run = home.run(&["status"]);

    assert_eq!((run.code, run.stderr.as_str()), (6, NEWER_ENGINE));
    engines.join().unwrap();
}

#[test]
fn a_sandboxed_client_refuses_to_start_an_engine() {
    let mut home = Home::new().with("CODEX_SANDBOX_NETWORK_DISABLED", "1").with("CODEX_THREAD_ID", "t-1");
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

/// A default-home engine with stand-ins for launchctl and systemctl, which record how they were
/// called, and the paths of its login service.
struct Service {
    home: Home,
    unit: PathBuf,
    calls: PathBuf,
}

impl Service {
    fn new() -> Self {
        let mut home = Home::new().with(hooks::ENGINE, "fake").with(hooks::HTTP_PORT, "0").with("CLAUDECODE", "1");
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
        let calls = dir.join("calls");
        for program in ["launchctl", "systemctl"] {
            let bin = home.stub(program, &format!("echo \"{program} $*\" >> '{}'", calls.display()));
            home.set(hooks::SERVICE_BIN, &bin.display().to_string());
        }
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
    let home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB");
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
    let home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB");
    assert_eq!(home.run(&["get", &magnet(10 << 30), "--dir", &home.downloads()]).code, 0);
    let wait = home.start(&["wait", "9b5212", "--timeout", "60s"]);

    let started = Instant::now();
    assert_eq!(home.run(&["engine", "stop"]).code, 0);
    let wait = wait.finish();

    assert!(started.elapsed() < Duration::from_secs(10), "wait took {:?}", started.elapsed());
    assert_eq!((wait.code, wait.stderr.as_str()), (6, "error[daemon_unavailable]: the engine stopped while waiting\n"));
}

#[test]
fn wait_follows_a_download_again_when_the_engine_comes_back() {
    let home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB");
    assert_eq!(home.run(&["get", &magnet(10 << 30), "--dir", &home.downloads()]).code, 0);
    let (pid, _) = home.engine().unwrap();
    let wait = home.start(&["wait", "9b5212", "--timeout", "8s", "--json"]);

    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("the killed engine is gone", Duration::from_secs(5), || !alive(pid));
    assert_eq!(home.run(&["status"]).code, 0);
    let wait = wait.finish();

    let last: serde_json::Value = serde_json::from_str(wait.stdout.lines().last().unwrap()).unwrap();
    assert_eq!((wait.code, wait.stderr.as_str()), (8, ""), "{}", wait.stdout);
    assert_eq!(last["outcome"], "timeout");
}

#[test]
fn wait_that_finds_a_newer_engine_after_a_restart_says_to_restart_this_torrential() {
    let mut home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB").with(hooks::VERSION, "0.1.0");
    assert_eq!(home.run(&["get", &magnet(10 << 30), "--dir", &home.downloads()]).code, 0);
    let (pid, _) = home.engine().unwrap();
    let wait = home.start(&["wait", "9b5212", "--timeout", "20s"]);

    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("the killed engine is gone", Duration::from_secs(5), || !alive(pid));
    home.set(hooks::VERSION, "0.1.1");
    assert_eq!(home.run(&["status"]).code, 0);
    let wait = wait.finish();

    assert_eq!(
        (wait.code, wait.stderr.as_str()),
        (6, "error[daemon_version]: the engine runs the newer torrential 0.1.1, so restart this torrential\n")
    );
}

#[test]
fn search_get_and_wait_rechecks_a_completed_download() {
    let home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "100GiB");
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
#[test]
fn play_json_names_the_video_and_its_subtitles_and_opens_nothing() {
    let home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB");
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
    let home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB");
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
    let home = Home::new().with(hooks::ENGINE, "fake");
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
    let mut home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB");
    let argv = home.path("argv");
    let bin = home.stub("mpv", &format!("printf '%s\\n' \"$@\" > {}.part\nmv {0}.part {0}", argv.display()));
    home.write_config("player: mpv\n");
    home.set("PATH", &format!("{}:/usr/bin:/bin", bin.display()));
    assert_eq!(home.run(&["search", "night", "of", "the", "living", "dead"]).code, 0);
    let run = home.run(&["play", "4c1e9a"]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (
            0,
            "4c1e9a night_of_the_living_dead\nvideo:     night_of_the_living_dead.mp4 (747 MiB)\nsubtitles: \
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
    let home = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "1MiB");
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
fn a_new_engine_keeps_its_state_where_only_its_user_can_change_it() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new().with(hooks::ENGINE, "fake");
    assert_eq!(home.run(&["status"]).code, 0);
    let state = std::fs::metadata(home.dir.path().join("th/state")).unwrap();
    assert_eq!(state.permissions().mode() & 0o777, 0o700);
    assert_eq!(home.run(&["daemon", "stop"]).code, 0);
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

#[test]
fn the_engine_writes_its_port_and_a_new_token_where_only_this_user_can_read_them() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    assert_eq!(home.run(&["status"]).code, 0);
    let first = Access::read(&home.token_file()).unwrap().unwrap();
    let mode = std::fs::metadata(home.token_file()).unwrap().permissions().mode();
    let web = home.json(&["engine", "status", "--json"])["address"].as_str().unwrap().to_string();
    assert_eq!(home.run(&["engine", "stop"]).code, 0);
    let after_stop = home.token_file().exists();
    assert_eq!(home.run(&["status"]).code, 0);
    let second = Access::read(&home.token_file()).unwrap().unwrap();

    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(web, format!("http://127.0.0.1:{}", first.port));
    assert!(first.token.len() == 64 && first.token.bytes().all(|b| b.is_ascii_hexdigit()), "{}", first.token);
    assert!(!after_stop, "a stopped engine removes its token file");
    assert_ne!(first.token, second.token, "each engine makes a new token");
}

#[test]
fn a_command_whose_token_file_names_a_port_another_program_took_starts_an_engine() {
    let home = Home::new();
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let left = Access { port: other.local_addr().unwrap().port(), token: "5a".repeat(32) };
    std::fs::create_dir_all(home.state()).unwrap();
    left.write(&home.token_file()).unwrap();
    // It closes each connection without a word, as a BitTorrent listener does with an HTTP request.
    std::thread::spawn(move || other.incoming().for_each(drop));

    let run = home.run(&["status"]);

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_ne!(Access::read(&home.token_file()).unwrap(), Some(left), "the new engine wrote its own");
}

#[test]
fn an_engine_stop_stops_an_engine_it_cannot_reach() {
    let home = Home::new();
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();
    // As an engine from before 0.6 leaves none.
    std::fs::remove_file(home.token_file()).unwrap();

    let stop = home.run(&["engine", "stop"]);

    assert_eq!((stop.code, stop.stdout), (0, format!("result: stopped\npid:    {pid}\n")));
    assert!(!alive(pid));
    assert!(home.log().contains("stopping on a signal"), "{}", home.log());
}

#[test]
fn a_command_that_finds_an_engine_it_cannot_reach_says_to_stop_it() {
    let home = Home::new();
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();
    std::fs::remove_file(home.token_file()).unwrap();

    let run = home.run(&["status", "--json"]);

    let state = home.state().display().to_string();
    assert_eq!(run.code, 6, "{}", run.stderr);
    assert_eq!(
        (run.json()["error"]["message"].as_str(), run.json()["next"].as_str()),
        (
            Some(format!("another engine (pid {pid}) holds {state} and does not answer").as_str()),
            Some("torrential engine stop")
        )
    );
    assert!(alive(pid), "the engine runs until the person stops it");
}

#[test]
fn an_engine_stop_after_the_engine_crashed_says_already_stopped_at_once() {
    let home = Home::new();
    assert_eq!(home.run(&["status"]).code, 0);
    let (pid, _) = home.engine().unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("the killed engine is gone", Duration::from_secs(5), || !alive(pid));
    let left_its_token = home.token_file().exists();

    let started = Instant::now();
    let stop = home.run(&["engine", "stop"]);

    assert!(left_its_token, "the crashed engine left its token file");
    assert_eq!((stop.code, stop.stdout.as_str()), (0, "result: already_stopped\n"));
    assert!(started.elapsed() < Duration::from_secs(5), "the stop took {:?}", started.elapsed());
}

#[test]
fn an_engine_stop_whose_engine_exits_before_it_answers_stops_the_engine_that_starts_next() {
    use std::process::Stdio;
    let home = Home::new();
    let leaving = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let access = Access { port: leaving.local_addr().unwrap().port(), token: "5a".repeat(32) };
    std::fs::create_dir_all(home.state()).unwrap();
    access.write(&home.token_file()).unwrap();
    let stop = home.process(&["engine", "stop"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let (mut taken, _) = leaving.accept().unwrap();
    let hello: Value = serde_json::from_slice(&read_post(&mut taken).1).unwrap();

    let log = home.dir.path().join("next.log");
    let mut next = home.process(&["daemon", "run"]).stderr(std::fs::File::create(&log).unwrap()).spawn().unwrap();
    let pid = next.id();
    // Reaped as it exits, so the stop sees it gone.
    let next = std::thread::spawn(move || next.wait().unwrap().code());
    eventually("the next engine is up", Duration::from_secs(30), || {
        std::fs::read_to_string(&log).unwrap_or_default().contains(" started, pid ")
    });
    drop((taken, leaving));
    let stop = stop.wait_with_output().unwrap();

    assert_eq!(hello["op"], "hello");
    assert_eq!(
        (String::from_utf8(stop.stdout).unwrap(), String::from_utf8(stop.stderr).unwrap()),
        (format!("result: stopped\npid:    {pid}\n"), String::new())
    );
    assert_eq!(next.join().unwrap(), Some(0));
}

const SKILL: &str = include_str!("../../../skills/torrential/SKILL.md");

impl Home {
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
    let home = Home::new().with(hooks::ENGINE, "fake");
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
    let home = Home::new().with(hooks::ENGINE, "fake");
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
    let mut home = Home::new().with(hooks::ENGINE, "fake").with(hooks::HTTP_PORT, "0");
    // Stands in for the browser, and records the address it was asked to open.
    let (opener, opened) = (home.dir.path().join("opener"), home.dir.path().join("opened"));
    std::fs::write(&opener, format!("#!/bin/sh\necho \"$1\" > '{}'\n", opened.display())).unwrap();
    std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
    home.set(hooks::OPENER, &opener.display().to_string());

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
