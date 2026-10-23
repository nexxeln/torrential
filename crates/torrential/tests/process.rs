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

#[test]
fn a_sandboxed_client_refuses_to_start_an_engine() {
    let mut home = Home::with(&[("CODEX_SANDBOX_NETWORK_DISABLED", "1"), ("CODEX_THREAD_ID", "t-1")]);
    let run = home.run(&["status"]);
    assert_eq!(run.code, 6);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "error[sandbox_network_blocked]: this sandbox blocks network access, so the engine could not reach any peer\n  \
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
fn search_get_and_wait_to_done_in_four_calls() {
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
    let again = home.run(&["get", "9b5212", "--dir", &home.downloads()]);
    assert!(again.stdout.starts_with("existing 9b5212"), "{}", again.stdout);
}

#[test]
fn a_killed_engine_comes_back_with_the_download_interrupted() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "1MiB")]);
    assert_eq!(home.run(&["get", &magnet(10 << 30), "--dir", &home.downloads()]).code, 0);
    let (pid, _) = home.engine().unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("the killed engine is gone", Duration::from_secs(5), || !alive(pid));
    let status = home.run(&["status", "9b5212"]);
    assert_eq!(status.code, 0, "{}", status.stderr);
    assert!(status.stdout.contains("state:    interrupted"), "{}", status.stdout);
    assert!(status.stdout.ends_with("next:     torrential resume 9b5212\n"), "{}", status.stdout);
    let resume = home.run(&["resume", "9b5212"]);
    assert_eq!(
        resume.stdout,
        "ID\tRESULT\tNAME\n9b5212\tresumed\tarchlinux-2026.10.01-x86_64.iso\nnext: torrential wait 9b5212\n"
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
    assert_eq!(again, ["c7b9e3 done", "5e02c8 done", "91fd3b paused", "a90d44 error", "6f2c81 interrupted"]);
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
    assert!(run.stderr.contains("torrential never resets state on its own"), "{}", run.stderr);
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
             # this binary carries the skill from torrential {version}\n"
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
    assert_eq!(
        help.stdout.lines().last(),
        Some("Agents: run torrential skill install to add the skill that explains torrential.")
    );
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
            "{mac}\nupdates:  notify\nfile:     {file}\n\
             # ranking for this Mac: 1080p screen, shows HDR, plays HEVC, arm64, English · overrides: none\n\
             next:     {flags}\n"
        )
    );
    assert_eq!(
        saved.stdout,
        format!(
            "saved\n{mac}\nupdates:  notify\nfile:     {file}\n\
             # ranking for this Mac: 2160p screen, up to 8 GB, arm64, English · overrides: screen tv-4k, max_size \
             8GB\n# get refuses a download over 8 GB unless --max-size allows it\nnext:     torrential search <words>\n"
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
fn get_max_size_raises_the_cap_of_a_magnet_that_stopped_over_it() {
    let home = Home::with(&[("TORRENTIAL_TEST_ENGINE", "fake"), ("TORRENTIAL_TEST_FAKE_RATE", "100MiB")]);
    assert_eq!(home.run(&["setup", "--max-size", "2GB"]).code, 0);
    assert_eq!(home.run(&["search", "night", "of", "the", "living", "dead", "1968", "720p"]).code, 0);
    let started = home.run(&["get", "2d9b6a", "--dir", &home.downloads()]);
    assert!(started.stdout.starts_with("started 2d9b6a"), "{}", started.stdout);
    let status = || home.run(&["status", "2d9b6a"]).stdout;
    eventually("the file list arrives", Duration::from_secs(30), || status().contains("state:    error"));
    let stopped = status();
    let refusal = "the selection is 2.1 GiB, over the 2 GB cap from torrential setup";
    let hint = "torrential get 2d9b6a --max-size 3GiB allows it, or pick fewer files with --files";
    let next = "torrential get 2d9b6a --max-size 3GiB";
    assert!(
        stopped.ends_with(&format!("error:    [too_large] {refusal}\n# hint: {hint}\nnext:     {next}\n")),
        "{stopped}"
    );

    let resumed = home.run(&["resume", "2d9b6a"]);
    assert_eq!(
        (resumed.code, resumed.stderr),
        (7, format!("error[too_large]: {refusal}\nhint: {hint}\nnext: {next}\n"))
    );
    let raised = home.run(&["get", "2d9b6a", "--max-size", "4GiB"]);
    let again = home.run(&["get", "2d9b6a", "--max-size", "4GiB"]);
    let paused = home.run(&["pause", "2d9b6a"]);
    let resumed = home.run(&["resume", "2d9b6a"]);

    assert!(raised.stdout.starts_with("started 2d9b6a"), "{}{}", raised.stdout, raised.stderr);
    assert!(again.stdout.starts_with("existing 2d9b6a"), "{}{}", again.stdout, again.stderr);
    let name = "Night of the Living Dead 1968 720p WEB";
    assert_eq!(paused.stdout, format!("ID\tRESULT\tNAME\n2d9b6a\tpaused\t{name}\nnext: torrential resume 2d9b6a\n"));
    assert_eq!(resumed.stdout, format!("ID\tRESULT\tNAME\n2d9b6a\tresumed\t{name}\nnext: torrential wait 2d9b6a\n"));
    let done = home.run(&["wait", "2d9b6a", "--timeout", "50s"]);
    assert_eq!(done.code, 0, "{}{}", done.stdout, done.stderr);
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
