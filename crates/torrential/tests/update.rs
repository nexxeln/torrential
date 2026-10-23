use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
    let server = Server::start("0.1.1", &std::fs::read(env!("CARGO_BIN_EXE_torrential")).unwrap(), None);
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
    assert_eq!((again.code, again.stdout.as_str()), (0, "torrential 0.1.1 is the latest.\n"));
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

    assert_eq!(text.stdout, "current: 0.1.0\nlatest:  0.1.1\npolicy:  notify\nnext:    torrential update\n");
    assert_eq!(
        json.stdout,
        "{\"schema\":1,\"type\":\"update_check\",\"current\":\"0.1.0\",\"latest\":\"0.1.1\",\"update_available\":true,\
         \"policy\":\"notify\",\"next\":\"torrential update\"}\n"
    );
    assert_eq!(
        current.stdout,
        "{\"schema\":1,\"type\":\"update_check\",\"current\":\"0.1.1\",\"latest\":\"0.1.1\",\"update_available\":\
         false,\"policy\":\"notify\",\"next\":null}\n"
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
        let doc: serde_json::Value = serde_json::from_str(&self.run(&["daemon", "status", "--json"]).stdout).unwrap();
        (doc["pid"].as_i64().unwrap(), doc["version"].as_str().unwrap().to_string())
    }
}

#[test]
fn an_agent_hears_of_a_newer_version_once_a_day_and_json_output_never_changes() {
    let server = Server::start("0.1.1", SCRIPT, None);
    let install = Install::agent_with_engine(&server);
    install.found("0.1.1");

    let json = install.run(&["status", "--json"]);
    let first = install.run(&["status"]);
    let second = install.run(&["status"]);

    assert_eq!((json.code, json.stderr.as_str()), (0, ""));
    assert!(json.stdout.starts_with("{\"schema\":1,\"type\":\"jobs\""), "{}", json.stdout);
    assert_eq!(
        first.stderr,
        "# update: torrential 0.1.1 is out (you have 0.1.0). Run torrential update to install it.\n"
    );
    assert_eq!(second.stderr, "");
    assert_eq!(server.asked(), Vec::<String>::new(), "a check from the last day needs no new one");
}

#[test]
fn auto_mode_installs_a_found_release_and_the_next_command_says_so_once() {
    let Some(target) = target() else { return };
    let server = Server::start("0.1.1", &std::fs::read(env!("CARGO_BIN_EXE_torrential")).unwrap(), None);
    let mut install = Install::agent_with_engine(&server);
    std::fs::create_dir_all(install.dir.path().join("th/config")).unwrap();
    std::fs::write(install.dir.path().join("th/config/config.yaml"), "updates: auto\n").unwrap();
    install.found("0.1.1");
    let old_inode = std::fs::metadata(install.exe()).unwrap().ino();

    let started = install.run(&["status", "--json"]);
    let (old_pid, old_version) = install.engine();
    eventually("the engine installed 0.1.1", || install.state().join("updated.json").exists());
    install.set("TORRENTIAL_TEST_VERSION", "0.1.1");
    let first = install.run(&["status"]);
    let second = install.run(&["status"]);

    assert_eq!((started.code, started.stderr.as_str(), old_version.as_str()), (0, "", "0.1.0"));
    assert_ne!(std::fs::metadata(install.exe()).unwrap().ino(), old_inode, "the engine replaced the binary");
    assert_eq!(first.stderr, "# updated torrential from 0.1.0 to 0.1.1\n");
    assert_eq!(second.stderr, "");
    let (pid, version) = install.engine();
    assert_eq!(version, "0.1.1", "the next command restarted the engine on the new version");
    assert_ne!(pid, old_pid);
    let [tarball, sum] = assets(target);
    assert_eq!(server.asked(), ["/latest".to_string(), tarball, sum]);
}
