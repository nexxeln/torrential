//! Films and series from the command line, against the fake engine and its fake catalog.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::{Value, json};
use torrential_core::protocol::{Request, WIRE_VERSION, encode_frame};

/// The archive.org release of Night of the Living Dead, which a play tries first.
const ARCHIVE: &str = "4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c";
/// Its DVD release on archive.org, which a play tries next.
const DVD: &str = "b7f20d5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c";

const MAC: &str =
    r#"{"os": "macos", "display": {"width": 3024, "height": 1964, "hdr": true}, "arch": "arm64", "language": "en"}"#;

struct Home {
    dir: tempfile::TempDir,
    vars: Vec<(&'static str, String)>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Home {
    /// A fake engine whose only player is a stub `mpv` that writes its arguments to `argv`, so no
    /// real player ever starts. ffmpeg is not on the path.
    fn new(vars: &[(&'static str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mpv = bin.join("mpv");
        let argv = dir.path().join("argv");
        std::fs::write(&mpv, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}.part\nmv {0}.part {0}\n", argv.display()))
            .unwrap();
        std::fs::set_permissions(&mpv, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dir.path().join("th/config")).unwrap();
        std::fs::write(dir.path().join("th/config/config.yaml"), "player: mpv\n").unwrap();
        let mut all = vec![
            ("HOME", dir.path().display().to_string()),
            ("TORRENTIAL_HOME", dir.path().join("th").display().to_string()),
            ("TORRENTIAL_TEST_ENGINE", "fake".to_string()),
            ("TORRENTIAL_TEST_FAKE_RATE", "1MiB".to_string()),
            ("TORRENTIAL_TEST_IDLE_TIMEOUT", "30s".to_string()),
            ("TORRENTIAL_TEST_NETWORK", "loopback".to_string()),
            ("TORRENTIAL_TEST_MACHINE", MAC.to_string()),
            ("TORRENTIAL_TEST_HTTP_PORT", "0".to_string()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ];
        all.extend(vars.iter().map(|(k, v)| (*k, v.to_string())));
        Self { dir, vars: all }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_torrential"));
        for key in ["CODEX_SANDBOX_NETWORK_DISABLED", "CODEX_THREAD_ID", "CODEX_CI", "TORRENTIAL_TEST_VERSION"] {
            cmd.env_remove(key);
        }
        cmd.args(args).envs(self.vars.iter().map(|(k, v)| (*k, v.as_str())));
        cmd.timeout(Duration::from_secs(60));
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

    fn json(&self, args: &[&str]) -> Value {
        let run = self.run(args);
        assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
        serde_json::from_str(&run.stdout).unwrap()
    }

    fn path(&self, rest: &str) -> PathBuf {
        self.dir.path().join(rest)
    }

    fn downloads(&self) -> Vec<String> {
        let doc = self.json(&["status", "--json"]);
        doc["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_str().unwrap()[..6].to_string()).collect()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

/// Puts `PORT` in place of the player port.
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

fn eventually(what: &str, within: Duration, done: impl Fn() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn titles_lists_the_1968_film_before_its_remake() {
    let home = Home::new(&[]);
    let run = home.run(&["titles", "night", "of", "the", "living", "dead"]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (
            0,
            "ID\tKIND\tYEAR\tRATING\tNAME\ntt0063350\tfilm\t1968\t7.8\tNight of the Living Dead\ntt0100258\tfilm\t1990\t6.9\t\
             Night of the Living Dead\n# 2 titles\nnext: torrential title tt0063350\n"
        ),
        "{}",
        run.stderr
    );
    let doc = home.json(&["titles", "night", "of", "the", "living", "dead", "--json"]);
    let titles: Vec<String> = doc["titles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| format!("{} {} {} {}", t["id"], t["kind"], t["name"], t["year"]))
        .collect();
    assert_eq!(
        titles,
        [
            r#""tt0063350" "film" "Night of the Living Dead" 1968"#,
            r#""tt0100258" "film" "Night of the Living Dead" 1990"#
        ]
    );
    assert_eq!((doc["type"].as_str(), doc["next"].as_str()), (Some("titles"), Some("torrential title tt0063350")));
}

#[test]
fn title_shows_a_film_or_a_series_and_what_plays_next() {
    let home = Home::new(&[]);
    let film = home.run(&["title", "tt0063350"]);
    assert_eq!(
        (film.code, film.stdout.as_str()),
        (
            0,
            "tt0063350 Night of the Living Dead (1968)\nkind:    film\nrating:  7.8\nruntime: 96 min\ngenres:  Horror, \
             Thriller\ncast:    Duane Jones, Judith O'Dea, Karl Hardman\nabout:   Strangers shelter in a farmhouse while \
             the dead walk outside.\nnext:    torrential play tt0063350\n"
        ),
        "{}",
        film.stderr
    );
    let doc = home.json(&["title", "tt0063350", "--json"]);
    let mut title = doc["title"].clone();
    title.as_object_mut().unwrap().remove("rating");
    assert_eq!(
        title,
        json!({
            "id": "tt0063350",
            "kind": "film",
            "name": "Night of the Living Dead",
            "year": 1968,
            "synopsis": "Strangers shelter in a farmhouse while the dead walk outside.",
            "genres": ["Horror", "Thriller"],
            "runtime_min": 96,
            "cast": ["Duane Jones", "Judith O'Dea", "Karl Hardman"],
            "art": {"poster": true, "background": true, "logo": true},
            "seasons": [],
            "saved": false,
            "watched": [],
            "next": {"video": "tt0063350", "resume_at": null},
            "downloads": []
        })
    );
    assert_eq!((doc["type"].as_str(), doc["next"].as_str()), (Some("title"), Some("torrential play tt0063350")));
    let series = home.run(&["title", "tt9999901"]);
    assert_eq!(
        series.stdout,
        "EPISODE\tAIRED\tWATCHED\tNAME\nS00E01\t2020-12-24\t-\tLights Out\nS01E01\t2019-05-01\t-\tPilot\nS01E02\t\
         2019-05-08\t-\tUndertow\nS01E03\t2019-05-15\t-\tFog Bell\nS02E01\t2020-05-01\t-\tReturn\nS02E02\t2100-01-01\t-\t\
         Tide Tables\nS02E03\t?\t-\tUntitled\n# tt9999901 Harbor Lights (2019) · series · rated 8.1 · Drama · 44 min\n# A \
         lighthouse keeper's family runs a harbor town's only inn.\n# cast: Ada Byrne\n# up next: S01E01 Pilot\nnext: \
         torrential play tt9999901:1:1\n"
    );
}

#[test]
fn sources_lists_a_films_releases_in_the_order_play_tries_them() {
    let home = Home::new(&[]);
    let doc = home.json(&["sources", "tt0063350", "--json"]);
    let releases: Vec<String> = doc["result"]["releases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| format!("{} {} {}", &r["id"].as_str().unwrap()[..6], r["span"].as_str().unwrap(), r["name"]))
        .collect();
    assert_eq!(
        releases,
        [
            r#"4c1e9a film "Night of the Living Dead (1968)""#,
            r#"b7f20d film "Night of the Living Dead (1968) [DVD]""#,
            r#"e3a915 film "Night.of.the.Living.Dead.1968.1080p.BluRay.x264""#,
            r#"09cd47 film "Night of the Living Dead 4K Restoration""#,
            r#"7a0c3f film "Night of the Living Dead (1968) colorized""#,
            r#"2d9b6a film "Night of the Living Dead 1968 720p WEB""#,
        ]
    );
    assert_eq!(
        (doc["type"].as_str(), doc["video"].as_str(), doc["title"]["id"].as_str(), doc["next"].as_str()),
        (Some("sources"), Some("tt0063350"), Some("tt0063350"), Some("torrential play tt0063350"))
    );
}

#[test]
fn get_moves_past_a_dead_release_and_names_the_one_that_started() {
    let home = Home::new(&[
        ("TORRENTIAL_TEST_DEAD", ARCHIVE),
        ("TORRENTIAL_TEST_TITLE_WAIT", "1s"),
        ("TORRENTIAL_TEST_FAKE_RATE", "100MiB"),
    ]);
    let doc = home.json(&["get", "tt0063350", "--json"]);
    let title = &doc["title"];
    assert_eq!(
        (title["video"].as_str(), title["name"].as_str(), title["release"]["id"].as_str()),
        (Some("tt0063350"), Some("Night of the Living Dead (1968)"), Some(DVD)),
        "{:#}",
        title["tried"]
    );
    let tried = title["tried"].as_array().unwrap();
    let tried: Vec<(&str, &str)> =
        tried.iter().map(|t| (t["release"].as_str().unwrap(), t["reason"]["reason"].as_str().unwrap())).collect();
    assert_eq!(tried, [(ARCHIVE, "no_data")]);
    assert_eq!(
        (doc["type"].as_str(), doc["reply"]["outcome"].as_str(), doc["reply"]["job"]["id"].as_str()),
        (Some("get"), Some("started"), Some(DVD))
    );
    assert_eq!(doc["reply"]["job"]["title"]["title"], "tt0063350");
    assert_eq!(doc["next"], "torrential wait b7f20d");
    assert!(!home.downloads().contains(&ARCHIVE[..6].to_string()), "the dead release was discarded");
}

#[test]
fn play_hands_a_title_to_the_player_as_play_does_a_torrent() {
    let home = Home::new(&[]);
    let run = home.run(&["play", "tt0063350"]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (
            0,
            "4c1e9a Night of the Living Dead (1968)\nsource:    Night of the Living Dead (1968)\nvideo:     \
             night_of_the_living_dead.mp4 (747.0 MiB)\nsubtitles: night_of_the_living_dead.srt\n# playing in mpv · the \
             download continues\nnext:      torrential status 4c1e9a\n"
        ),
        "{}",
        run.stderr
    );
    assert_eq!(
        run.stderr,
        "Finding sources of Night of the Living Dead (1968)\nFound 6 sources\nTrying 1 of 6: 4c1e9a Night of the Living \
         Dead (1968), 1.3 GiB\nPlaying 4c1e9a Night of the Living Dead (1968), 1.3 GiB\n"
    );
    let argv = home.path("argv");
    eventually("the player starts", Duration::from_secs(10), || argv.exists());
    assert_eq!(
        portless(&std::fs::read_to_string(&argv).unwrap()),
        format!(
            "--sub-file=http://127.0.0.1:PORT/{ARCHIVE}/3/night_of_the_living_dead.srt\n--slang=en\n\
             --force-media-title=Night of the Living Dead (1968)\n\
             http://127.0.0.1:PORT/{ARCHIVE}/0/night_of_the_living_dead.mp4\n"
        )
    );
}

#[test]
fn play_json_is_todays_play_document_with_the_title_and_opens_nothing() {
    let home = Home::new(&[]);
    let doc = home.json(&["play", "tt0063350", "--json"]);
    let streams: Vec<String> =
        doc["streams"].as_array().unwrap().iter().map(|s| portless(s["url"].as_str().unwrap())).collect();
    assert_eq!(
        streams,
        [
            format!("http://127.0.0.1:PORT/{ARCHIVE}/0/night_of_the_living_dead.mp4"),
            format!("http://127.0.0.1:PORT/{ARCHIVE}/3/night_of_the_living_dead.srt"),
        ]
    );
    assert_eq!(
        (doc["type"].as_str(), doc["name"].as_str(), doc["job"]["id"].as_str(), doc["next"].as_str()),
        (Some("play"), Some("Night of the Living Dead (1968)"), Some(ARCHIVE), Some("torrential status 4c1e9a"))
    );
    assert_eq!(
        (doc["title"]["video"].as_str(), doc["title"]["release"]["id"].as_str(), &doc["title"]["tried"]),
        (Some("tt0063350"), Some(ARCHIVE), &json!([]))
    );
    assert!(!home.path("argv").exists(), "no player opened");
}

#[test]
fn source_plays_only_the_release_it_names() {
    let home = Home::new(&[]);
    let doc = home.json(&["play", "tt0063350", "--source", "b7f2", "--json"]);
    assert_eq!((doc["job"]["id"].as_str(), &doc["title"]["tried"]), (Some(DVD), &json!([])));
    let missing = home.run(&["play", "tt0063350", "--source", "ffff"]);
    assert_eq!(
        (missing.code, missing.stderr.as_str()),
        (
            3,
            "error[not_found]: no source of Night of the Living Dead (1968) has an ID that starts with ffff\nhint: run \
             torrential sources tt0063350 to list them\nnext: torrential sources tt0063350\n"
        )
    );
}

#[test]
fn an_episode_reads_the_same_as_two_words_or_one() {
    let home = Home::new(&[]);
    let expected = "Finding sources of Harbor Lights S01E02\nFound 0 sources\nerror[no_source]: no torrent of Harbor \
                    Lights S01E02 was found\nhint: search the torrents for it by name\nnext: torrential search Harbor \
                    Lights S01E02\n";
    for words in [&["tt9999901", "s01e02"][..], &["tt9999901", "1x02"], &["tt9999901:1:2"]] {
        let run = home.run(&[&["play"], words].concat());
        assert_eq!((run.code, run.stderr.as_str()), (7, expected), "{words:?}");
    }
}

#[test]
fn a_bad_or_unknown_title_or_episode_says_how_to_find_one() {
    let home = Home::new(&[]);
    let errors = [
        (
            &["title", "tt12"][..],
            3,
            "error[invalid_ref]: \"tt12\" is not a title ID\nhint: run torrential titles <words> to find a title's ID, such as tt0063350\nnext: torrential titles <words>\n",
        ),
        (
            &["title", "tt0000001"],
            3,
            "error[not_found]: Cinemeta has no film or series tt0000001\nhint: run torrential titles <words> to find a title's ID\nnext: torrential titles <words>\n",
        ),
        (
            &["play", "tt9999901:4:1"],
            3,
            "error[not_found]: S04E01 is not an episode of Harbor Lights\nhint: run torrential title tt9999901 to list its episodes\nnext: torrential title tt9999901\n",
        ),
        (
            &["sources", "tt9999901", "s1"],
            3,
            "error[invalid_ref]: \"s1\" is not an episode\nhint: name an episode such as s03e05 or 3x05\n",
        ),
        (
            &["files", "tt0063350"],
            2,
            "error[usage]: files takes a torrent, not a title such as tt0063350\nhint: run torrential sources tt0063350 to list its torrents, and pass the ID of one\nnext: torrential sources tt0063350\n",
        ),
        (
            &["get", "tt0063350", "--dir", "/tmp"],
            2,
            "error[usage]: --dir takes a torrent, not a title such as tt0063350\nhint: run torrential sources tt0063350 to list its torrents, and pass the ID of one\nnext: torrential sources tt0063350\n",
        ),
    ];
    for (args, code, stderr) in errors {
        let run = home.run(args);
        assert_eq!((run.code, run.stderr.as_str()), (code, stderr), "{args:?}");
    }
    let json = home.run(&["files", "tt0063350", "--json"]);
    let doc: Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!((json.code, doc["error"]["code"].as_str()), (2, Some("usage")));
    assert_eq!(doc["next"], "torrential sources tt0063350");
}

fn send(socket: &mut UnixStream, message: &impl serde::Serialize) {
    socket.write_all(&encode_frame(message).unwrap()).unwrap();
}

fn receive(socket: &mut UnixStream) -> Option<Value> {
    let mut len = [0u8; 4];
    socket.read_exact(&mut len).ok()?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    socket.read_exact(&mut body).ok()?;
    Some(serde_json::from_slice(&body).unwrap())
}

#[test]
fn a_client_one_version_older_gets_the_version_error_from_the_engine() {
    let home = Home::new(&[]);
    assert_eq!(home.run(&["status"]).code, 0);
    let mut socket = UnixStream::connect(home.path("th/run/engine.sock")).unwrap();
    send(&mut socket, &Request::Hello { client_version: "old".into(), wire: WIRE_VERSION - 1 });
    assert_eq!(receive(&mut socket).unwrap()["wire"], WIRE_VERSION);
    send(&mut socket, &json!({"op": "status", "refs": []}));
    let reply = receive(&mut socket).unwrap();
    assert_eq!(
        reply["error"],
        json!({
            "code": "daemon_version",
            "message": format!("the engine uses protocol version {WIRE_VERSION}, but this client uses {}", WIRE_VERSION - 1),
            "hint": "run torrential engine stop, then run the command again",
            "next": "torrential engine stop"
        })
    );
}

/// Plays an engine one protocol version older at `socket`: it answers hello with its version,
/// refuses every request but shutdown as such an engine does, and records what it was sent.
fn older_engine(socket: &Path) -> std::thread::JoinHandle<Vec<String>> {
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(socket).unwrap();
    let gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    let mut gone = gone;
    gone.wait().unwrap();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut asked = Vec::new();
        let old = WIRE_VERSION - 1;
        while let Some(request) = receive(&mut conn) {
            let op = request["op"].as_str().unwrap().to_string();
            asked.push(op.clone());
            let reply = match op.as_str() {
                "hello" => {
                    json!({"type": "hello", "engine_version": env!("CARGO_PKG_VERSION"), "wire": old, "pid": pid})
                }
                "shutdown" => json!({"type": "ok"}),
                _ => json!({"type": "error", "error": {"code": "daemon_version",
                    "message": format!("the engine uses protocol version {old}, but this client uses {}", request["wire"])}}),
            };
            send(&mut conn, &reply);
            if op == "shutdown" {
                break;
            }
        }
        asked
    })
}

#[test]
fn a_client_one_version_newer_stops_the_older_engine_rather_than_ask_it_for_titles() {
    let home = Home::new(&[]);
    let old = older_engine(&home.path("th/run/engine.sock"));
    let run = home.run(&["titles", "harbor", "lights", "--json"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(old.join().unwrap(), ["hello", "shutdown"], "only hello and shutdown reached the older engine");
    let doc: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(doc["titles"][0]["id"], "tt9999901", "a new engine answered");
}
