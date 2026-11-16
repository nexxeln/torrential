//! What the end-to-end tests share: a home whose engine sees only the variables a test sets, and
//! a stub web server that stands in for the sources.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::Value;
use torrential_core::protocol::{Request, WIRE_VERSION, encode_frame};
use torrential_daemon::wire::{self, Access, Client};

#[path = "../../../daemon/src/env/hooks.rs"]
pub mod hooks;

/// The machine every engine in these tests runs on, so none probes the real one.
pub const MAC: &str =
    r#"{"os": "macos", "display": {"width": 3024, "height": 1964, "hdr": true}, "arch": "arm64", "language": "en"}"#;

/// What a command passes on from the test's own environment: how to run the binary and how to
/// spell file names. Everything else torrential reads comes from the home.
const PASSED_ON: [&str; 8] =
    ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH", "RUST_BACKTRACE"];

pub struct Home {
    pub dir: tempfile::TempDir,
    vars: Vec<(&'static str, String)>,
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| panic!("{e}: {}{}", self.stdout, self.stderr))
    }
}

impl Home {
    /// A home of its own on a loopback network.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let vars = vec![
            ("HOME", dir.path().display().to_string()),
            ("TORRENTIAL_HOME", dir.path().join("th").display().to_string()),
            (hooks::NETWORK, "loopback".to_string()),
            (hooks::MACHINE, MAC.to_string()),
        ];
        Self { dir, vars }
    }

    pub fn with(mut self, key: &'static str, value: &str) -> Self {
        self.set(key, value);
        self
    }

    pub fn set(&mut self, key: &'static str, value: &str) {
        self.unset(key);
        self.vars.push((key, value.to_string()));
    }

    pub fn unset(&mut self, key: &'static str) {
        self.vars.retain(|(k, _)| *k != key);
    }

    pub fn path(&self, rest: &str) -> PathBuf {
        self.dir.path().join(rest)
    }

    pub fn write_config(&self, text: &str) {
        let dir = self.path("th/config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), text).unwrap();
    }

    /// Writes an executable `bin/<name>` that runs `script`, and returns the folder.
    pub fn stub(&self, name: &str, script: &str) -> PathBuf {
        let bin = self.path("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(name), format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(bin.join(name), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        bin
    }

    pub fn process(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_torrential"));
        cmd.env_clear();
        for key in PASSED_ON {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.args(args).envs(self.vars.iter().map(|(k, v)| (*k, v.as_str())));
        cmd
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::from_std(self.process(args));
        cmd.timeout(Duration::from_secs(300));
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Run {
        let out = self.command(args).output().unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
        }
    }

    /// The JSON of a command that succeeds.
    pub fn json(&self, args: &[&str]) -> Value {
        let run = self.run(args);
        assert_eq!(run.code, 0, "torrential {args:?}: {}{}", run.stdout, run.stderr);
        run.json()
    }

    /// Where the engine writes its port and the token that lets this home's torrential in.
    pub fn token_file(&self) -> PathBuf {
        self.path("th/state/engine-token")
    }

    /// What the engine of this home answers `request`, sent as this torrential sends it.
    pub fn ask(&self, request: &Request) -> Value {
        self.ask_as(Client { version: env!("CARGO_PKG_VERSION"), wire: WIRE_VERSION }, request)
    }

    /// What the engine of this home answers `request` from `client`.
    pub fn ask_as(&self, client: Client, request: &Request) -> Value {
        let access = Access::read(&self.token_file()).unwrap().expect("an engine wrote its token file");
        serde_json::to_value(wire::ask(&access, client, request).unwrap()).unwrap()
    }

    /// An engine stand-in on 127.0.0.1 that this home's torrential finds by the token file. It answers
    /// each request with the reply `answer` gives, and stops after the one `answer` says is the last.
    /// It returns the op of each request it was sent.
    pub fn stand_in(
        &self,
        mut answer: impl FnMut(&Value) -> (Value, bool) + Send + 'static,
    ) -> std::thread::JoinHandle<Vec<String>> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let access = Access { port: listener.local_addr().unwrap().port(), token: "5a".repeat(32) };
        std::fs::create_dir_all(self.path("th/state")).unwrap();
        access.write(&self.token_file()).unwrap();
        std::thread::spawn(move || {
            let mut asked = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let (head, body) = read_post(&mut stream);
                assert!(head.contains(&format!("\r\nAuthorization: Bearer {}\r\n", access.token)), "{head}");
                let request: Value = serde_json::from_slice(&body).unwrap();
                let (reply, last) = answer(&request);
                asked.push(request["op"].as_str().unwrap().to_string());
                stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n").unwrap();
                stream.write_all(&encode_frame(&reply).unwrap()).unwrap();
                if last {
                    return asked;
                }
            }
            asked
        })
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.path("th/state/engine.log")).unwrap_or_default()
    }
}

/// A command left running once it has printed its first line.
pub struct Running {
    child: std::process::Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    printed: String,
}

impl Running {
    pub fn finish(mut self) -> Run {
        self.stdout.read_to_string(&mut self.printed).unwrap();
        let mut stderr = String::new();
        self.child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        Run { code: self.child.wait().unwrap().code().unwrap_or(-1), stdout: self.printed, stderr }
    }
}

impl Home {
    /// Starts `torrential <args>` and returns once it prints, as `wait` does when it follows.
    pub fn start(&self, args: &[&str]) -> Running {
        use std::io::BufRead;
        use std::process::Stdio;
        let mut child = self.process(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut printed = String::new();
        stdout.read_line(&mut printed).unwrap();
        assert!(!printed.is_empty(), "torrential {args:?} printed nothing");
        Running { child, stdout, printed }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

pub fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

pub fn eventually(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Puts `PORT` in place of each loopback port.
pub fn portless(text: &str) -> String {
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

/// The head and the body of the next request on `stream`, which says how long its body is.
pub fn read_post(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut read = Vec::new();
    let end = loop {
        if let Some(i) = read.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "the request ended before its head did");
        read.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&read[..end]).to_string();
    let length: usize = head
        .lines()
        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ")?.trim().parse().ok())
        .unwrap_or(0);
    let mut body = read[end..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "the request ended before its body did");
        body.extend_from_slice(&chunk[..n]);
    }
    (head, body)
}

#[derive(Clone)]
pub enum Reply {
    Bytes { body: Vec<u8>, delay: Duration },
    Status(u16),
    Redirect(String),
}

pub fn ok(body: Vec<u8>) -> Reply {
    Reply::Bytes { body, delay: Duration::ZERO }
}

pub fn status(code: u16) -> Reply {
    Reply::Status(code)
}

/// Routes by path prefix, ignoring case. A route answers its replies in turn and then repeats its last.
pub type Routes = Vec<(String, Vec<Reply>)>;

/// A web server on loopback that answers by route, honors byte ranges, and keeps the paths it was asked.
pub struct Server {
    pub base: String,
    log: Arc<Mutex<Log>>,
}

#[derive(Default)]
struct Log {
    asked: Vec<String>,
    /// How many requests each route has answered, by its index.
    turns: HashMap<usize, usize>,
}

impl Server {
    pub fn start<P: Into<String>>(routes: impl FnOnce(&str) -> Vec<(P, Vec<Reply>)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Routes = routes(&base).into_iter().map(|(prefix, replies)| (prefix.into(), replies)).collect();
        let routes = Arc::new(routes);
        let log: Arc<Mutex<Log>> = Arc::default();
        let shared = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (routes, log) = (routes.clone(), shared.clone());
                std::thread::spawn(move || serve(stream, &routes, &log));
            }
        });
        Server { base, log }
    }

    pub fn asked(&self, prefix: &str) -> Vec<String> {
        self.log.lock().unwrap().asked.iter().filter(|p| p.starts_with(prefix)).cloned().collect()
    }
}

fn serve(mut stream: TcpStream, routes: &Routes, log: &Mutex<Log>) {
    let mut pending = Vec::new();
    loop {
        let end = loop {
            if let Some(i) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => pending.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&pending[..end]).to_string();
        pending.drain(..end);
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let range = head.to_lowercase().lines().find_map(|l| {
            let (first, last) = l.strip_prefix("range: bytes=")?.trim().split_once('-')?;
            Some((first.parse::<usize>().ok()?, last.parse::<usize>().ok()?))
        });
        let turn = {
            let mut log = log.lock().unwrap();
            log.asked.push(path.clone());
            let route = routes.iter().position(|(prefix, _)| path.to_lowercase().starts_with(&prefix.to_lowercase()));
            route.map(|i| {
                let n = log.turns.entry(i).or_insert(0);
                *n += 1;
                routes[i].1[(*n - 1).min(routes[i].1.len() - 1)].clone()
            })
        };
        let (head, body): (String, &[u8]) = match &turn {
            Some(Reply::Bytes { body, delay }) => {
                std::thread::sleep(*delay);
                match range {
                    Some((first, last)) => (
                        format!("206 Partial\r\nContent-Range: bytes {first}-{last}/{}", body.len()),
                        &body[first..=last],
                    ),
                    None => ("200 OK".into(), body),
                }
            }
            Some(Reply::Redirect(to)) => (format!("302 Found\r\nLocation: {to}"), b""),
            Some(Reply::Status(code)) => (format!("{code} No"), b""),
            None => ("404 No".into(), b""),
        };
        let head = format!("HTTP/1.1 {head}\r\nContent-Length: {}\r\n\r\n", body.len());
        if stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body)).is_err() {
            return;
        }
    }
}

/// Bytes from the search crate's recorded answers.
pub fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../search/tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}
