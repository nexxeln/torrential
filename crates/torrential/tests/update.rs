use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use sha2::{Digest, Sha256};

const SKILL: &str = include_str!("../../../skills/torrential/SKILL.md");
const SCRIPT: &[u8] = b"#!/bin/sh\necho torrential 0.1.1\n";
const AGENT_VARS: [&str; 8] = [
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CODEX_THREAD_ID",
    "CODEX_CI",
    "CODEX_SANDBOX",
    "GEMINI_CLI",
    "CURSOR_AGENT",
    "AI_AGENT",
];

fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// The torrential under test, linked into a folder of its own. An engine that a release starts
/// runs from here, so whatever it replaces later leaves the build alone.
struct Built(tempfile::TempDir);

impl Built {
    fn new() -> Built {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("torrential");
        if std::fs::hard_link(env!("CARGO_BIN_EXE_torrential"), &link).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_torrential"), &link).unwrap();
        }
        Built(dir)
    }

    /// A release whose binary says it is torrential `says` and runs the engine as `runs`, or exits
    /// at once when asked to run the engine and `runs` is nothing.
    fn release(&self, says: &str, runs: Option<&str>) -> Vec<u8> {
        let real = self.0.path().join("torrential").display().to_string();
        let engine =
            runs.map_or_else(|| "exit 1".to_string(), |v| format!("TORRENTIAL_TEST_VERSION={v} exec '{real}' \"$@\""));
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"torrential {says}\" ;;\n  daemon) {engine} ;;\n  *) \
             TORRENTIAL_TEST_VERSION={says} exec '{real}' \"$@\" ;;\nesac\n"
        )
        .into_bytes()
    }
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn tarball(binary: &[u8]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(gz);
    for (name, bytes) in [("LICENSE", b"MIT\n".as_slice()), ("torrential", binary)] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

struct Server {
    base: String,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(version: &str, binary: &[u8], published_sum: Option<&str>) -> Server {
        Server::gated(version, binary, published_sum, None)
    }

    /// The first request whose path ends with the gate's text waits until the gate receives.
    fn gated(
        version: &str,
        binary: &[u8],
        published_sum: Option<&str>,
        mut gate: Option<(&'static str, mpsc::Receiver<()>)>,
    ) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let tarball = Arc::new(tarball(binary));
        let sum = published_sum.map_or_else(|| hex(&tarball), str::to_string);
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (log, location) = (asked.clone(), format!("{base}/tag/v{version}"));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let path = request_path(&stream);
                log.lock().unwrap().push(path.clone());
                if gate.as_ref().is_some_and(|(end, _)| path.ends_with(end)) {
                    let _ = gate.take().unwrap().1.recv();
                }
                let reply = match path.as_str() {
                    "/latest" => (302, vec![("Location", location.clone())], Vec::new()),
                    p if p.ends_with(".tar.gz") => (200, Vec::new(), tarball.to_vec()),
                    p if p.ends_with(".tar.gz.sha256") => {
                        let name = p.rsplit('/').next().unwrap().trim_end_matches(".sha256");
                        (200, Vec::new(), format!("{sum}  {name}\n").into_bytes())
                    }
                    _ => (404, Vec::new(), b"Not Found".to_vec()),
                };
                respond(stream, reply);
            }
        });
        Server { base, asked }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

fn request_path(mut stream: &TcpStream) -> String {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("/").to_string()
}

fn respond(mut stream: TcpStream, (status, headers, body): (u16, Vec<(&str, String)>, Vec<u8>)) {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&body));
}

struct Install {
    dir: tempfile::TempDir,
    vars: Vec<(&'static str, String)>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Install {
    fn new(server: &Server) -> Install {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_torrential"), dir.path().join("bin/torrential")).unwrap();
        let vars = vec![
            ("HOME", dir.path().display().to_string()),
            ("TORRENTIAL_HOME", dir.path().join("th").display().to_string()),
            ("TORRENTIAL_TEST_RELEASES", server.base.clone()),
            ("TORRENTIAL_TEST_VERSION", "0.1.0".to_string()),
            ("TORRENTIAL_TEST_NETWORK", "loopback".to_string()),
        ];
        Install { dir, vars }
    }

    fn set(&mut self, key: &'static str, value: &str) {
        self.vars.retain(|(k, _)| *k != key);
        self.vars.push((key, value.to_string()));
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin").canonicalize().unwrap()
    }

    fn exe(&self) -> PathBuf {
        self.bin().join("torrential")
    }

    fn command(&self, program: &Path, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new(program);
        for key in AGENT_VARS.iter().chain(&["CODEX_SANDBOX_NETWORK_DISABLED", "TORRENTIAL_TEST_ENGINE"]) {
            cmd.env_remove(key);
        }
        cmd.args(args).envs(self.vars.iter().map(|(k, v)| (*k, v.as_str())));
        cmd
    }

    fn run(&self, args: &[&str]) -> Run {
        let mut child = assert_cmd::Command::from_std(self.command(&self.exe(), args));
        child.timeout(Duration::from_secs(60));
        let out = child.output().unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
        }
    }

    fn files_in_bin(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.bin())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.dir.path().join("bin"), std::fs::Permissions::from_mode(0o755));
        let _ = self.command(Path::new(env!("CARGO_BIN_EXE_torrential")), &["daemon", "stop"]).output();
    }
}

fn assets(target: &str) -> [String; 2] {
    let asset = format!("/download/v0.1.1/torrential-{target}.tar.gz");
    [asset.clone(), format!("{asset}.sha256")]
}

#[test]
fn update_replaces_the_binary_with_the_verified_release() {
    let Some(target) = target() else { return };
    let server = Server::start("0.1.1", SCRIPT, None);
    let install = Install::new(&server);
    let old_inode = std::fs::metadata(install.exe()).unwrap().ino();

    let run = install.run(&["update"]);

    assert_eq!((run.code, run.stdout.as_str(), run.stderr.as_str()), (0, "Updated torrential 0.1.0 → 0.1.1.\n", ""));
    let installed = std::fs::metadata(install.exe()).unwrap();
    assert_eq!(std::fs::read(install.exe()).unwrap(), SCRIPT);
    assert_eq!(installed.permissions().mode() & 0o777, 0o755);
    assert_ne!(installed.ino(), old_inode, "a rename put the new file in place");
    assert_eq!(install.files_in_bin(), ["torrential"]);
    let [tarball, sum] = assets(target);
    assert_eq!(server.asked(), ["/latest".to_string(), tarball, sum]);
}

#[test]
fn update_refreshes_the_installed_skill_and_then_reports_the_latest() {
    if target().is_none() {
        return;
    }
    let built = Built::new();
    let server = Server::start("0.1.1", &built.release("0.1.1", Some("0.1.1")), None);
    let mut install = Install::new(&server);
    let copy = install.dir.path().join(".agents/skills/torrential");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), "the skill from 0.1.0\n").unwrap();
    let sidecar =
        serde_json::json!({"managed_by": "torrential", "version": "0.1.0", "sha256": hex(b"the skill from 0.1.0\n")});
    std::fs::write(copy.join(".torrential.json"), sidecar.to_string()).unwrap();

    let updated = install.run(&["update", "--json"]);
    install.set("TORRENTIAL_TEST_VERSION", "0.1.1");
    let again = install.run(&["update"]);
    let again_json = install.run(&["update", "--json"]);

    assert_eq!(
        updated.stdout,
        "{\"schema\":1,\"type\":\"update\",\"current\":\"0.1.0\",\"latest\":\"0.1.1\",\"result\":\"updated\",\"next\":null}\n"
    );
    assert_eq!(std::fs::read_to_string(copy.join("SKILL.md")).unwrap(), SKILL);
    assert_eq!((again.code, again.stdout.as_str()), (0, "torrential 0.1.1 is up to date.\n"));
    assert_eq!(
        again_json.stdout,
        "{\"schema\":1,\"type\":\"update\",\"current\":\"0.1.1\",\"latest\":\"0.1.1\",\"result\":\"up_to_date\",\"next\":\
         null}\n"
    );
}

#[test]
fn a_checksum_mismatch_leaves_the_binary_untouched() {
    let Some(target) = target() else { return };
    let server = Server::start("0.1.1", SCRIPT, Some(&"0".repeat(64)));
    let install = Install::new(&server);
    let before = std::fs::read(install.exe()).unwrap();

    let run = install.run(&["update"]);

    assert_eq!(
        (run.code, run.stdout.as_str(), run.stderr.as_str()),
        (
            7,
            "",
            format!(
                "error[verification_failed]: torrential-{target}.tar.gz does not match its published checksum, so the \
                 installed torrential is unchanged\nhint: run torrential update again later; if it fails again, \
                 report it at https://github.com/nexxeln/torrential/issues\n"
            )
            .as_str()
        )
    );
    assert!(std::fs::read(install.exe()).unwrap() == before, "the old binary is still in place");
    assert_eq!(install.files_in_bin(), ["torrential"]);
}

#[test]
fn update_check_reports_without_installing() {
    let server = Server::start("0.1.1", SCRIPT, None);
    let mut install = Install::new(&server);
    let before = std::fs::read(install.exe()).unwrap();

    let text = install.run(&["update", "--check"]);
    let json = install.run(&["update", "--check", "--json"]);
    install.set("TORRENTIAL_TEST_VERSION", "0.1.1");
    let current = install.run(&["update", "--check", "--json"]);

    assert_eq!(text.stdout, "current: 0.1.0\nlatest:  0.1.1\npolicy:  auto\nnext:    torrential update\n");
    assert_eq!(
        json.stdout,
        "{\"schema\":1,\"type\":\"update_check\",\"current\":\"0.1.0\",\"latest\":\"0.1.1\",\"update_available\":true,\
         \"policy\":\"auto\",\"next\":\"torrential update\"}\n"
    );
    assert_eq!(
        current.stdout,
        "{\"schema\":1,\"type\":\"update_check\",\"current\":\"0.1.1\",\"latest\":\"0.1.1\",\"update_available\":\
         false,\"policy\":\"auto\",\"next\":null}\n"
    );
    assert!(std::fs::read(install.exe()).unwrap() == before, "--check installs nothing");
    assert_eq!(server.asked(), ["/latest", "/latest", "/latest"]);
}

#[test]
fn an_unwritable_folder_is_refused_before_the_download() {
    if target().is_none() || unsafe { libc::geteuid() } == 0 {
        return;
    }
    let server = Server::start("0.1.1", SCRIPT, None);
    let install = Install::new(&server);
    std::fs::set_permissions(install.bin(), std::fs::Permissions::from_mode(0o555)).unwrap();

    let run = install.run(&["update"]);

    let bin = install.bin().display().to_string();
    assert_eq!(
        (run.code, run.stderr),
        (
            7,
            format!(
                "error[storage_error]: cannot write {bin}: Permission denied (os error 13)\nhint: reinstall torrential \
                 in a folder you can write, such as ~/.local/bin, or run torrential update as a user who can write \
                 {bin}\n"
            )
        )
    );
    assert_eq!(server.asked(), ["/latest"]);
}

fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Install {
    fn agent_with_engine(server: &Server) -> Install {
        let mut install = Install::new(server);
        install.set("TORRENTIAL_TEST_ENGINE", "fake");
        install.set("CLAUDECODE", "1");
        install
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("th/state")
    }

    fn found(&self, latest: &str) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        std::fs::create_dir_all(self.state()).unwrap();
        let checked = serde_json::json!({"latest": latest, "checked_at": now});
        std::fs::write(self.state().join("update.json"), checked.to_string()).unwrap();
    }

    fn engine(&self) -> (i64, String) {
        self.running().expect("the engine is running")
    }

    /// The engine's process ID and version, or nothing while it restarts.
    fn running(&self) -> Option<(i64, String)> {
        let doc: serde_json::Value = serde_json::from_str(&self.run(&["daemon", "status", "--json"]).stdout).ok()?;
        Some((doc["pid"].as_i64()?, doc["version"].as_str()?.to_string()))
    }

    fn configure(&self, text: &str) {
        std::fs::create_dir_all(self.dir.path().join("th/config")).unwrap();
        std::fs::write(self.dir.path().join("th/config/config.yaml"), text).unwrap();
    }
}

#[test]
fn an_agent_hears_of_a_newer_version_once_a_day_and_json_output_never_changes() {
    let server = Server::start("0.1.1", SCRIPT, None);
    let install = Install::agent_with_engine(&server);
    install.configure("updates: notify\n");
    install.found("0.1.1");

    let json = install.run(&["status", "--json"]);
    let first = install.run(&["status"]);
    let second = install.run(&["status"]);

    assert_eq!((json.code, json.stderr.as_str()), (0, ""));
    assert!(json.stdout.starts_with("{\"schema\":1,\"type\":\"jobs\""), "{}", json.stdout);
    assert_eq!(
        first.stderr,
        "# update: torrential 0.1.1 is available (you have 0.1.0). Run torrential update to install it.\n"
    );
    assert_eq!(second.stderr, "");
    assert_eq!(server.asked(), Vec::<String>::new(), "a check from the last day needs no new one");
}

impl Install {
    fn checked(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.state().join("update.json")).unwrap()).unwrap()
    }

    fn downloads_of_the_release(server: &Server) -> usize {
        server.asked().iter().filter(|path| path.ends_with(".tar.gz")).count()
    }
}

#[test]
fn a_release_whose_binary_says_another_version_is_neither_installed_nor_tried_again() {
    if target().is_none() {
        return;
    }
    let built = Built::new();
    let server = Server::start("0.1.1", &built.release("0.1.0", Some("0.1.0")), None);
    let install = Install::agent_with_engine(&server);
    install.configure("updates: auto\n");
    install.found("0.1.1");
    let old_inode = std::fs::metadata(install.exe()).unwrap().ino();

    assert_eq!(install.run(&["status", "--json"]).code, 0);
    let (pid, _) = install.engine();
    eventually("the engine gave the release up", || install.checked()["install_failed_at"].is_i64());
    std::thread::sleep(Duration::from_secs(3));
    let after = (install.running(), Install::downloads_of_the_release(&server));
    assert_eq!(install.run(&["engine", "stop"]).code, 0);
    assert_eq!(install.run(&["status", "--json"]).code, 0);
    let (next_pid, next_version) = install.engine();
    std::thread::sleep(Duration::from_secs(3));

    assert_eq!(after, (Some((pid, "0.1.0".into())), 1), "the engine kept running and fetched the release once");
    assert_ne!(next_pid, pid);
    assert_eq!(next_version, "0.1.0");
    assert_eq!(Install::downloads_of_the_release(&server), 1, "a new engine does not try it again");
    assert_eq!(std::fs::metadata(install.exe()).unwrap().ino(), old_inode);
    assert_eq!(install.files_in_bin(), ["torrential"]);
    let log = std::fs::read_to_string(install.state().join("engine.log")).unwrap();
    assert!(log.contains("says it is torrential 0.1.0"), "{log}");
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl Install {
    /// Runs the engine the way the login service does: `daemon run --service`, started again each
    /// time it exits with the restart status. Its output goes to the engine log.
    fn supervise(&self) -> std::process::Child {
        let script = r#"while :; do "$0" daemon run --service >>"$1" 2>&1; s=$?; [ "$s" = 75 ] || exit "$s"; done"#;
        std::fs::create_dir_all(self.state()).unwrap();
        let (exe, log) = (self.exe().display().to_string(), self.state().join("engine.log").display().to_string());
        self.command(Path::new("/bin/sh"), &["-c", script, &exe, &log]).spawn().unwrap()
    }

    /// What the web app on `port` hears about versions, or nothing while no engine answers there.
    fn news(port: u16) -> Option<serde_json::Value> {
        let mut page = TcpStream::connect(("127.0.0.1", port)).ok()?;
        page.write_all(b"GET /api/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").ok()?;
        let mut answer = String::new();
        page.read_to_string(&mut answer).ok()?;
        serde_json::from_str(answer.split_once("\r\n\r\n")?.1).ok()
    }

    /// The ID of the test download that never finishes while `TORRENTIAL_TEST_FAKE_RATE` is 0.
    fn wikipedia(&self) -> String {
        let jobs: serde_json::Value = serde_json::from_str(&self.run(&["status", "--json"]).stdout).unwrap();
        let jobs = jobs["jobs"].as_array().unwrap();
        jobs.iter().find(|j| j["name"] == "wikipedia-en-2026-09-text").unwrap()["id"].as_str().unwrap().to_string()
    }
}

/// A player asking for the end of a file that has not arrived, so its stream stays open.
fn open_stream(port: u16, id: &str) -> (TcpStream, [u8; 12]) {
    let mut player = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let ask = format!("GET /{id}/0/enwiki.xml.bz2 HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=23514217000-\r\n\r\n");
    player.write_all(ask.as_bytes()).unwrap();
    let mut head = [0; 12];
    player.read_exact(&mut head).unwrap();
    (player, head)
}

#[test]
fn under_the_login_service_an_update_installs_while_a_film_plays_and_starts_once_it_ends() {
    let Some(target) = target() else { return };
    let (answer, gate) = mpsc::channel();
    let built = Built::new();
    let server = Server::gated("0.1.1", &built.release("0.1.1", Some("0.1.1")), None, Some(("/latest", gate)));
    let mut install = Install::agent_with_engine(&server);
    let port = free_port();
    install.set("TORRENTIAL_TEST_FAKE_RATE", "0");
    install.set("TORRENTIAL_TEST_HTTP_PORT", &port.to_string());
    let old_inode = std::fs::metadata(install.exe()).unwrap().ino();
    let mut service = install.supervise();
    eventually("the service started the engine", || install.running().is_some());
    let (pid, _) = install.engine();

    let (player, head) = open_stream(port, &install.wikipedia());
    answer.send(()).unwrap();
    eventually("the engine says the update waits", || {
        Install::news(port)
            .is_some_and(|news| news["notice"] == "torrential 0.1.1 is installed and starts when nothing is playing.")
    });
    std::thread::sleep(Duration::from_secs(1));
    let playing = (std::fs::read(install.exe()).unwrap(), install.running());
    drop(player);
    eventually("the service started 0.1.1", || install.running().is_some_and(|(now, v)| now != pid && v == "0.1.1"));
    let news = Install::news(port);
    install.set("TORRENTIAL_TEST_VERSION", "0.1.1");
    let stop = install.run(&["engine", "stop"]);
    let ended = service.wait().unwrap();

    assert_eq!(&head, b"HTTP/1.1 206");
    assert_eq!(playing.0, built.release("0.1.1", Some("0.1.1")), "the release was installed while the film played");
    assert_eq!(playing.1, Some((pid, "0.1.0".into())), "the engine kept its version until the film ended");
    assert_ne!(std::fs::metadata(install.exe()).unwrap().ino(), old_inode);
    assert_eq!(news, Some(serde_json::json!({"version": "0.1.1", "notice": "Updated torrential to 0.1.1."})));
    assert_eq!((stop.code, ended.code()), (0, Some(0)), "a stop the person asks for does not start it again");
    let [tarball, sum] = assets(target);
    assert_eq!(server.asked(), ["/latest".to_string(), tarball, sum]);
}

#[test]
fn without_the_login_service_an_installed_update_starts_with_the_next_command() {
    let Some(target) = target() else { return };
    let built = Built::new();
    let server = Server::start("0.1.1", &built.release("0.1.1", Some("0.1.1")), None);
    let mut install = Install::agent_with_engine(&server);
    let port = free_port();
    install.set("TORRENTIAL_TEST_HTTP_PORT", &port.to_string());
    install.found("0.1.1");

    let started = install.run(&["status", "--json"]);
    let (old_pid, _) = install.engine();
    eventually("the engine installed 0.1.1", || {
        Install::news(port).is_some_and(|news| {
            news["notice"] == "torrential 0.1.1 is installed and starts the next time the engine starts."
        })
    });
    let kept = install.running();
    install.set("TORRENTIAL_TEST_VERSION", "0.1.1");
    let first = install.run(&["status"]);
    let second = install.run(&["status"]);

    assert_eq!(started.code, 0);
    assert_eq!(std::fs::read(install.exe()).unwrap(), built.release("0.1.1", Some("0.1.1")));
    assert_eq!(kept, Some((old_pid, "0.1.0".into())), "the engine kept the version it started with");
    assert_eq!(first.stderr, "# updated torrential from 0.1.0 to 0.1.1\n");
    assert_eq!(second.stderr, "");
    let (pid, version) = install.engine();
    assert_eq!(version, "0.1.1", "the next command started the engine on the new version");
    assert_ne!(pid, old_pid);
    assert_eq!(server.asked(), assets(target), "a release found within the day needs no new check");
}
