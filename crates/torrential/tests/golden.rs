//! What every `--json` command and every JSON `/api` route answers, against the fake engine with
//! its downloads held still. A change detector for moves that keep behavior the same: any change in
//! a field, a word, or an exit code shows as a snapshot diff. Times, ports, PIDs, pairing codes and
//! the test's own folder are taken out, so two runs give the same snapshots.
// Its stand-in mpv, ffmpeg and ffprobe are shell scripts.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Map, Value, json};

mod common;
use common::{Home, hooks};

/// The archive.org release of Night of the Living Dead.
const ARCHIVE: &str = "4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c";
/// The seeded downloads: one running, one failed, one paused, two done.
const RUNNING: &str = "6f2c81";
const FAILED: &str = "a90d44";
const PAUSED: &str = "91fd3b";
const DONE: &str = "5e02c8";
const ARCH: &str = "c7b9e3";

/// Keys whose values change from run to run: clock times, durations measured while running, and
/// what the operating system hands out.
const VOLATILE: &[&str] = &[
    "added_at",
    "address",
    "at",
    "checked_at",
    "created_at",
    "elapsed_ms",
    "expires_at",
    "finished_at",
    "http_port",
    "lan_address",
    "last_seen",
    "ms",
    "paired_at",
    "pid",
    "published",
    "qr",
    "saved_at",
    "started_at",
    "took_ms",
    "updated_at",
    "uptime",
    "watched_at",
];

impl Home {
    /// A fake engine whose downloads do not move, so a title play gives up on each release after a
    /// second. Its only player is a stub `mpv` that starts nothing. `ffmpeg` and `ffprobe` are stubs
    /// that fail, found before any the machine has, so what plays does not depend on the machine.
    /// Its TV search asks an address no TV answers.
    fn held() -> Self {
        let home = Home::new();
        for (program, script) in [("mpv", "exit 0"), ("ffmpeg", "exit 1"), ("ffprobe", "exit 1")] {
            home.stub(program, script);
        }
        home.write_config("player: mpv\n");
        let bin = home.path("bin").display().to_string();
        home.with(hooks::ENGINE, "fake")
            .with(hooks::FAKE_RATE, "0")
            .with(hooks::TITLE_WAIT, "1s")
            .with(hooks::HTTP_PORT, "0")
            .with(hooks::SSDP, "127.0.0.1:9")
            .with(hooks::OPENER, "/usr/bin/true")
            .with("NO_COLOR", "1")
            .with("PATH", &bin)
    }

    /// The exit code, stdout and stderr of `torrential <args>`, each read as JSON when it is.
    fn answer(&self, args: &[&str]) -> Value {
        let out = self.command(args).output().unwrap();
        let mut doc = Map::new();
        doc.insert("code".into(), json!(out.status.code()));
        for (name, bytes) in [("stdout", out.stdout), ("stderr", out.stderr)] {
            let text = String::from_utf8(bytes).unwrap();
            if !text.trim().is_empty() {
                doc.insert(name.into(), parse(text));
            }
        }
        self.redact(Value::Object(doc))
    }

    /// Starts the engine and waits for the fake engine's first stats of the running download, which
    /// come a second after it starts.
    fn settle(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let out = self.command(&["status", RUNNING, "--json"]).output().unwrap();
            let doc: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            if doc["jobs"][0]["progress"]["peers"] == 3 {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "no stats for {RUNNING}: {doc}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The engine's web port, from `torrential web --json`.
    fn port(&self) -> u16 {
        let out = self.command(&["web", "--json"]).output().unwrap();
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        let url = doc["url"].as_str().unwrap();
        url.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn redact(&self, value: Value) -> Value {
        let home = self.dir.path();
        let real = home.canonicalize().unwrap();
        redact(value, &[real.display().to_string(), home.display().to_string()])
    }
}

/// JSON, a stream of JSON lines as an array, or else the text.
fn parse(text: String) -> Value {
    if let Ok(value) = serde_json::from_str(&text) {
        return value;
    }
    let lines: Result<Vec<Value>, _> =
        text.lines().filter(|line| !line.trim().is_empty()).map(serde_json::from_str).collect();
    lines.map(Value::Array).unwrap_or(Value::String(text))
}

fn redact(value: Value, home: &[String]) -> Value {
    match value {
        Value::Object(map) => {
            // What the disk has free, how far a check has got, and the pairing link with its code
            // and this machine's name.
            let disk = map.get("check").is_some_and(|check| check == "disk");
            let checking = map.get("state").is_some_and(|state| state == "checking");
            Value::Object(
                map.into_iter()
                    .map(|(key, value)| {
                        let value = match (key.as_str(), value) {
                            (_, Value::Null) => Value::Null,
                            (key, _) if VOLATILE.contains(&key) => json!("[redacted]"),
                            ("detail", _) if disk => json!("[redacted]"),
                            ("progress", Value::Object(mut progress)) if checking => {
                                for key in ["verified", "piece_map"] {
                                    progress.entry(key).and_modify(|value| *value = json!("[redacted]"));
                                }
                                Value::Object(progress)
                            }
                            ("code", Value::String(code)) if code.bytes().all(|b| b.is_ascii_digit()) => {
                                json!("[redacted]")
                            }
                            ("url", Value::String(url)) if !url.starts_with("http://127.0.0.1:") => json!("[redacted]"),
                            (_, value) => redact(value, home),
                        };
                        (key, value)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(|item| redact(item, home)).collect()),
        Value::String(text) => Value::String(redact_text(&text, home)),
        other => other,
    }
}

/// Takes the test's folder, the version, every loopback port and every PID out of `text`.
fn redact_text(text: &str, home: &[String]) -> String {
    let mut text = home.iter().fold(text.to_string(), |text, home| text.replace(home.as_str(), "[home]"));
    text = text.replace(env!("CARGO_PKG_VERSION"), "[version]");
    for (before, number) in [("127.0.0.1:", "[port]"), ("pid ", "[pid]")] {
        text = after_each(&text, before, number);
    }
    text
}

/// Puts `number` in place of the digits after each `before` in `text`.
fn after_each(text: &str, before: &str, number: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(before) {
        let (head, after) = rest.split_at(at + before.len());
        out.push_str(head);
        let digits = after.trim_start_matches(|c: char| c.is_ascii_digit());
        out.push_str(if digits.len() < after.len() { number } else { "" });
        rest = digits;
    }
    out + rest
}

/// Sends one HTTP/1.0 request to the engine's web app and answers its status and body. A stream of
/// lines answers its last line; the event stream answers its first event.
fn http(home: &Home, port: u16, method: &str, path: &str, body: Option<Value>) -> Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let typed = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    write!(
        stream,
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n{typed}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = Vec::new();
    let mut buf = [0; 8192];
    let events = path.starts_with("/api/events");
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        answer.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&answer);
        if events && text.split_once("\r\n\r\n").is_some_and(|(_, body)| body.contains("\n\n")) {
            break;
        }
    }
    let text = String::from_utf8(answer).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.lines().next().unwrap().trim_start_matches("HTTP/1.1 ").trim_start_matches("HTTP/1.0 ");
    let content_type = head
        .lines()
        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-type: ").map(str::to_string))
        .unwrap_or_default();
    let body = if events {
        let first = body.split("\n\n").next().unwrap();
        let data = first.lines().filter_map(|line| line.strip_prefix("data: ")).collect::<String>();
        serde_json::from_str(&data).unwrap_or(Value::String(first.to_string()))
    } else if content_type.contains("ndjson") {
        let last = body.lines().rfind(|line| !line.trim().is_empty()).unwrap_or_default();
        serde_json::from_str(last).unwrap_or(Value::String(last.to_string()))
    } else {
        serde_json::from_str(body).unwrap_or(Value::String(body.to_string()))
    };
    home.redact(json!({ "status": status, "type": content_type, "body": body }))
}

fn full_id(home: &Home, short: &str) -> String {
    let out = home.command(&["status", short, "--json"]).output().unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    doc["jobs"][0]["id"].as_str().map(str::to_string).unwrap_or_else(|| panic!("no job {short}: {doc}"))
}

/// Checks every snapshot, then fails with the names of those that changed, so one run shows each
/// change.
fn snapshots(answers: impl IntoIterator<Item = (String, Value)>) {
    let changed: Vec<String> = answers
        .into_iter()
        .filter(|(name, value)| {
            std::panic::catch_unwind(|| insta::assert_json_snapshot!(name.as_str(), value)).is_err()
        })
        .map(|(name, _)| name)
        .collect();
    assert!(changed.is_empty(), "changed: {}", changed.join(", "));
}

#[test]
fn every_command_answers_in_json_as_it_did() {
    let home = Home::held();
    home.settle();
    let steps: &[(&str, &[&str])] = &[
        ("search", &["search", "night", "of", "the", "living", "dead", "--json"]),
        ("search_limit", &["search", "ubuntu", "--limit", "3", "--json"]),
        ("titles", &["titles", "night", "of", "the", "living", "dead", "--json"]),
        ("title_film", &["title", "tt0063350", "--json"]),
        ("title_series", &["title", "tt9999901", "--json"]),
        ("title_unknown", &["title", "tt0000001", "--json"]),
        ("sources", &["sources", "tt0063350", "--json"]),
        ("files", &["files", &ARCHIVE[..6], "--json"]),
        ("get_dry_run", &["get", &ARCHIVE[..6], "--dry-run", "--json"]),
        ("status", &["status", "--json"]),
        ("status_one", &["status", RUNNING, "--json"]),
        ("status_unknown", &["status", "ffffff", "--json"]),
        ("wait_done", &["wait", DONE, "--json"]),
        ("wait_timeout", &["wait", RUNNING, "--timeout", "1s", "--json"]),
        ("wait_metadata", &["wait", RUNNING, "--until", "metadata", "--json"]),
        ("pause", &["pause", RUNNING, "--json"]),
        ("resume", &["resume", RUNNING, "--json"]),
        ("resume_failed", &["resume", FAILED, "--json"]),
        ("verify", &["verify", ARCH, "--json"]),
        ("rm", &["rm", PAUSED, "--json"]),
        ("play_title", &["play", "tt0063350", "--json"]),
        ("play_download", &["play", DONE, "--json"]),
        ("devices", &["devices", "--json"]),
        ("devices_add", &["devices", "add", "--json"]),
        ("devices_remove_unknown", &["devices", "remove", "3f9a0c2e", "--json"]),
        ("tv", &["tv", "--json"]),
        ("tv_pause", &["tv", "pause", "--json"]),
        ("setup", &["setup", "--json"]),
        ("engine_status", &["engine", "status", "--json"]),
        ("web", &["web", "--json"]),
        ("doctor", &["doctor", "--json"]),
        ("schema", &["schema"]),
    ];
    snapshots(steps.iter().map(|(name, args)| (format!("cli_{name}"), home.answer(args))));
}

#[test]
fn every_api_route_answers_as_it_did() {
    let home = Home::held();
    home.settle();
    let port = home.port();
    let running = full_id(&home, RUNNING);
    let done = full_id(&home, DONE);
    let film = |path: &str| format!("{path}?id={done}&file=0");
    let steps: Vec<(&str, &str, String, Option<Value>)> = vec![
        ("version", "GET", "/api/version".into(), None),
        ("me", "GET", "/api/me".into(), None),
        ("devices", "GET", "/api/devices".into(), None),
        ("devices_add", "POST", "/api/devices/add".into(), Some(json!({}))),
        ("devices_remove_unknown", "POST", "/api/devices/remove".into(), Some(json!({ "id": "3f9a0c2e" }))),
        ("tvs", "GET", "/api/tvs".into(), None),
        ("tvs_search", "POST", "/api/tvs".into(), Some(json!({}))),
        ("cast_no_tv", "POST", "/api/cast".into(), Some(json!({ "tv": "nowhere", "video": "tt0063350" }))),
        ("cast_control_no_tv", "POST", "/api/cast/control".into(), Some(json!({ "tv": "nowhere", "action": "pause" }))),
        ("titles", "GET", "/api/titles?q=night%20of%20the%20living%20dead".into(), None),
        ("title", "GET", "/api/title?id=tt0063350".into(), None),
        ("title_series", "GET", "/api/title?id=tt9999901".into(), None),
        ("title_sources", "GET", "/api/title/sources?video=tt0063350".into(), None),
        ("home", "GET", "/api/home".into(), None),
        ("library_save", "POST", "/api/library".into(), Some(json!({ "title": "tt0063350", "saved": true }))),
        (
            "watched",
            "POST",
            "/api/watched".into(),
            Some(json!({ "video": "tt0063350", "position": 200.5, "duration": 5760.0 })),
        ),
        ("home_after", "GET", "/api/home".into(), None),
        ("search", "POST", "/api/search".into(), Some(json!({ "q": "night of the living dead" }))),
        ("search_empty", "POST", "/api/search".into(), Some(json!({ "q": " " }))),
        ("jobs", "GET", "/api/jobs".into(), None),
        ("events", "GET", "/api/events".into(), None),
        ("files", "POST", "/api/files".into(), Some(json!({ "ref": ARCHIVE }))),
        ("media", "GET", film("/api/media"), None),
        ("pieces", "GET", film("/api/pieces"), None),
        ("subtitles_online", "GET", film("/api/subtitles/online"), None),
        ("play", "POST", "/api/play".into(), Some(json!({ "ref": done }))),
        ("title_play", "POST", "/api/title/play".into(), Some(json!({ "video": "tt0063350" }))),
        ("pause", "POST", "/api/pause".into(), Some(json!({ "refs": [running] }))),
        ("resume", "POST", "/api/resume".into(), Some(json!({ "refs": [running] }))),
        ("remove", "POST", "/api/remove".into(), Some(json!({ "refs": [PAUSED] }))),
        ("ffmpeg", "POST", "/api/ffmpeg".into(), Some(json!({}))),
        ("unknown", "GET", "/api/nothing".into(), None),
    ];
    snapshots(
        steps
            .into_iter()
            .map(|(name, method, path, body)| (format!("api_{name}"), http(&home, port, method, &path, body))),
    );
}
