use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_cmd::Command;
use serde_json::Value;
use torrential_core::SearchQuery;
use torrential_core::protocol::{Request, WIRE_VERSION, encode_frame};

const BUILTIN: [&str; 8] =
    ["archlinux", "ubuntu", "debian", "archive.org", "academictorrents", "knaben", "apibay", "nyaa"];

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    delay: Duration,
}

fn ok(body: Vec<u8>) -> Reply {
    Reply { status: 200, body, delay: Duration::ZERO }
}

fn status(code: u16) -> Reply {
    Reply { status: code, body: b"no".to_vec(), delay: Duration::ZERO }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../search/tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn torrent(name: &str) -> Vec<u8> {
    let (length, piece) = (1usize << 20, 1usize << 18);
    let pieces = vec![0x5a; 20 * length / piece];
    let mut info =
        format!("d6:lengthi{length}e4:name{}:{name}12:piece lengthi{piece}e6:pieces{}:", name.len(), pieces.len())
            .into_bytes();
    info.extend_from_slice(&pieces);
    info.push(b'e');
    [b"d4:info".as_slice(), &info, b"e"].concat()
}

type Routes = Vec<(&'static str, Vec<Reply>)>;

struct Server {
    base: String,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(routes: impl FnOnce(&str) -> Routes) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(routes(&base));
        let paths: Arc<Mutex<Vec<String>>> = Arc::default();
        let counts: Arc<Mutex<HashMap<&'static str, usize>>> = Arc::default();
        let log = paths.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (routes, log, counts) = (routes.clone(), log.clone(), counts.clone());
                std::thread::spawn(move || answer(stream, &routes, &log, &counts));
            }
        });
        Server { base, paths }
    }

    fn asked(&self, prefix: &str) -> Vec<String> {
        self.paths.lock().unwrap().iter().filter(|p| p.starts_with(prefix)).cloned().collect()
    }
}

fn answer(mut stream: TcpStream, routes: &Routes, log: &Mutex<Vec<String>>, counts: &Mutex<HashMap<&str, usize>>) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let path = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("/").to_string();
    log.lock().unwrap().push(path.clone());
    let reply = match routes.iter().find(|(prefix, _)| path.starts_with(prefix)) {
        Some((prefix, replies)) => {
            let mut counts = counts.lock().unwrap();
            let n = counts.entry(prefix).or_insert(0);
            *n += 1;
            replies[(*n - 1).min(replies.len() - 1)].clone()
        }
        None => status(404),
    };
    std::thread::sleep(reply.delay);
    let head =
        format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.status, reply.body.len());
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&reply.body));
}

struct Home {
    dir: tempfile::TempDir,
    vars: Vec<(&'static str, String)>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| panic!("{e}: {}{}", self.stdout, self.stderr))
    }
}

const MAC: &str =
    r#"{"os": "macos", "display": {"width": 3024, "height": 1964, "hdr": true}, "arch": "arm64", "language": "en"}"#;

impl Home {
    fn new(server: &Server, enabled: &[&str], sources: &str) -> Home {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("th");
        let disabled: Vec<String> = BUILTIN.iter().filter(|s| !enabled.contains(s)).map(|s| format!("{s:?}")).collect();
        let endpoints: Vec<String> = enabled.iter().map(|s| format!("{s}={}/{s}/", server.base)).collect();
        let home = Home {
            vars: vec![
                ("HOME", dir.path().display().to_string()),
                ("TORRENTIAL_HOME", root.display().to_string()),
                ("TORRENTIAL_TEST_IDLE_TIMEOUT", "30s".into()),
                ("TORRENTIAL_TEST_NETWORK", "loopback".into()),
                ("TORRENTIAL_TEST_DISK", "roomy".into()),
                ("TORRENTIAL_TEST_ENDPOINTS", endpoints.join(",")),
                ("TORRENTIAL_TEST_MACHINE", MAC.into()),
            ],
            dir,
        };
        home.write_config(&format!("sources:\n  disabled: [{}]\n{sources}", disabled.join(", ")));
        home
    }

    fn write_config(&self, text: &str) {
        let dir = self.dir.path().join("th/config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), text).unwrap();
    }

    fn set(&mut self, key: &'static str, value: &str) {
        self.unset(key);
        self.vars.push((key, value.into()));
    }

    fn unset(&mut self, key: &'static str) {
        self.vars.retain(|(k, _)| *k != key);
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("torrential").unwrap();
        for key in ["CODEX_SANDBOX_NETWORK_DISABLED", "CODEX_THREAD_ID", "CODEX_CI", "TORRENTIAL_TEST_ENGINE"] {
            cmd.env_remove(key);
        }
        cmd.args(args).envs(self.vars.iter().map(|(k, v)| (*k, v.as_str()))).timeout(Duration::from_secs(60));
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

    fn path(&self, rest: &str) -> PathBuf {
        self.dir.path().join(rest)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

#[test]
fn search_ranks_what_the_sources_answered() {
    let server = Server::start(|_| {
        vec![
            ("/knaben/", vec![ok(fixture("knaben/v2-search-ubuntu.json"))]),
            ("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))]),
        ]
    });
    let home = Home::new(&server, &["knaben", "apibay"], "");

    let plain = home.run(&["search", "ubuntu", "--limit", "3"]);
    let json = home.run(&["search", "ubuntu", "--limit", "3", "--json"]).json();

    assert_eq!(plain.code, 0, "{}", plain.stderr);
    let table: Vec<String> = plain
        .stdout
        .lines()
        .take_while(|l| !l.starts_with('#'))
        .map(|l| l.split('\t').enumerate().filter(|(i, _)| *i != 3).map(|(_, c)| c).collect::<Vec<_>>().join(" | "))
        .collect();
    assert_eq!(
        table,
        [
            "ID | SEEDS | SIZE | SOURCE | FLAGS | NAME",
            "dafc8c | 143 | 6.1 GiB | knaben | - | ubuntu-26.04-desktop-amd64.iso",
            "4a3f5e | 52 | 5.8 GiB | knaben+1 | trusted | ubuntu-24.04.1-desktop-amd64.iso",
            "2c6b68 | 37 | 3.4 GiB | knaben+1 | trusted | Ubuntu 22.04 LTS",
        ]
    );
    let result = &json["result"];
    let ids: Vec<&str> = result["releases"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "dafc8c076ca2f3ed376eeae7c76a0d6be2415c45",
            "4a3f5e08bcef825718eda30637230585e3330599",
            "2c6b6858d61da9543d4231a71db4b1c9264b0685"
        ]
    );
    assert_eq!(result["total"], 158);
    let sources: Vec<String> =
        result["sources"].as_array().unwrap().iter().map(|s| format!("{} {}", s["source"], s["outcome"])).collect();
    assert_eq!(sources, [r#""knaben" "cached""#, r#""apibay" "cached""#]);
    assert_eq!((server.asked("/knaben/").len(), server.asked("/apibay/").len()), (1, 1));
}

#[test]
fn an_idle_engine_stays_up_while_its_search_answers_are_cached() {
    let server = Server::start(|_| vec![("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))])]);
    let mut home = Home::new(&server, &["apibay"], "");
    home.set("TORRENTIAL_TEST_IDLE_TIMEOUT", "1s");
    let engine = |home: &Home| home.run(&["daemon", "status", "--json"]).json()["pid"].clone();

    let first = home.run(&["search", "ubuntu"]);
    let pid = engine(&home);
    std::thread::sleep(Duration::from_secs(3));
    let after_idle = engine(&home);
    let again = home.run(&["search", "ubuntu", "--json"]).json();

    assert_eq!(first.code, 0, "{}", first.stderr);
    assert!(pid.is_u64(), "{pid}");
    assert_eq!(after_idle, pid, "the engine outlived its idle timeout");
    let sources: Vec<String> = again["result"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| format!("{} {}", s["source"], s["outcome"]))
        .collect();
    assert_eq!(sources, [r#""apibay" "cached""#]);
    assert_eq!(server.asked("/apibay/").len(), 1);
}

const NETINST: &str = "/debian/debian-cd/current/arm64/bt-cd/debian-13.7.0-arm64-netinst.iso.torrent";

fn debian(netinst: Vec<Reply>) -> impl FnOnce(&str) -> Routes {
    |_| {
        vec![
            (NETINST, netinst),
            ("/debian/debian-cd/current/arm64/bt-cd/", vec![ok(fixture("debian/debian-cd-current-arm64-bt-cd.html"))]),
            (
                "/debian/debian-cd/current/arm64/bt-dvd/",
                vec![ok(fixture("debian/debian-cd-current-arm64-bt-dvd.html"))],
            ),
            ("/debian/debian-cd/current/amd64/bt-cd/", vec![ok(fixture("debian/debian-cd-current-amd64-bt-cd.html"))]),
            (
                "/debian/debian-cd/current/amd64/bt-dvd/",
                vec![ok(fixture("debian/debian-cd-current-amd64-bt-dvd.html"))],
            ),
        ]
    }
}

#[test]
fn get_on_a_search_result_downloads_into_the_configured_folder() {
    let server = Server::start(debian(vec![ok(torrent("debian-13.7.0-arm64-netinst.iso"))]));
    let home = Home::new(&server, &["debian"], "");
    let isos = home.path("isos");
    home.write_config(&format!(
        "{}downloads:\n  dir: \"{}\"\n",
        std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap(),
        isos.display()
    ));

    let found = home.run(&["search", "debian", "13", "arm64", "netinst", "--json"]).json();
    let release = &found["result"]["releases"][0];
    let id = release["id"].as_str().unwrap();
    let get = home.run(&["get", &id[..6]]);
    let status = home.run(&["status", &id[..6], "--json"]).json();

    assert_eq!(
        (release["name"].as_str(), release["flags"].clone()),
        (Some("debian-13.7.0-arm64-netinst.iso"), serde_json::json!(["official"]))
    );
    assert_eq!(get.code, 0, "{}{}", get.stdout, get.stderr);
    assert!(
        get.stdout.starts_with(&format!("started {} debian-13.7.0-arm64-netinst.iso\n", &id[..6])),
        "{}",
        get.stdout
    );
    let job = &status["jobs"][0];
    assert_eq!((job["id"].as_str(), job["dir"].as_str()), (Some(id), isos.to_str()));
    assert_eq!(job["name"], "debian-13.7.0-arm64-netinst.iso");
    assert_eq!(server.asked(NETINST).len(), 2);
}

#[test]
fn private_indexer_links_never_reach_the_output() {
    let server = Server::start(|base| {
        let feed = String::from_utf8(fixture("torznab/jackett-ubuntu.xml"))
            .unwrap()
            .replace("http://127.0.0.1:9117", base)
            .replace(
                "<comments>https://privatehd.example/torrents/4471",
                "<comments>https://privatehd.example/t/4471?passkey=s3cret",
            );
        vec![
            ("/torznab/api", vec![ok(feed.into_bytes())]),
            ("/dl/privatehd/", vec![status(404), status(404), ok(torrent("ubuntu-private.iso"))]),
        ]
    });
    let sources = format!(
        "  torznab:\n    - name: jackett\n      url: {}/torznab/api\n      api_key_env: TEST_JACKETT_KEY\n",
        server.base
    );
    let mut home = Home::new(&server, &[], &sources);
    home.set("TEST_JACKETT_KEY", "k3y");

    let plain = home.run(&["search", "ubuntu"]);
    let json = home.run(&["search", "ubuntu", "--json"]);
    let locator: String = json.json()["result"]["releases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .find(|id| id.starts_with('u'))
        .unwrap()[..7]
        .to_string();
    let refused = home.run(&["get", &locator]);
    let refused_json = home.run(&["get", &locator, "--json"]);
    let started = home.run(&["get", &locator]);
    let job = started.stdout.split_whitespace().nth(1).unwrap_or_default().to_string();
    let status = home.run(&["status"]);
    let status_json = home.run(&["status", "--json"]);
    let files = home.run(&["files", &job]);
    let files_json = home.run(&["files", &job, "--json"]);

    assert_eq!(plain.code, 0, "{}", plain.stderr);
    assert_eq!(refused.code, 5);
    assert!(refused.stderr.contains("could not fetch the .torrent file: http 404"), "{}", refused.stderr);
    assert_eq!(refused_json.json()["error"]["code"], "fetch_failed");
    assert_eq!(started.code, 0, "{}{}", started.stdout, started.stderr);
    assert_eq!(files_json.json()["files"]["name"], "ubuntu-private.iso");
    let runs = [&plain, &json, &refused, &refused_json, &started, &status, &status_json, &files, &files_json];
    for run in runs {
        let out = format!("{}{}", run.stdout, run.stderr);
        assert!(
            !out.contains("k3y") && !out.contains("apikey") && !out.contains("s3cret"),
            "a private link reached the output: {out}"
        );
    }
    assert_eq!(server.asked("/torznab/api"), ["/torznab/api?t=search&q=ubuntu&apikey=k3y"]);
    assert_eq!(server.asked("/dl/privatehd/"), vec!["/dl/privatehd/?jackett_apikey=k3y&path=def&file=ubuntu"; 3]);
}

#[test]
fn a_private_locator_resolves_after_a_restart_with_the_key_from_the_environment() {
    let server = Server::start(|base| {
        let feed =
            String::from_utf8(fixture("torznab/jackett-ubuntu.xml")).unwrap().replace("http://127.0.0.1:9117", base);
        vec![("/torznab/api", vec![ok(feed.into_bytes())]), ("/dl/privatehd/", vec![ok(torrent("ubuntu-private.iso"))])]
    });
    let indexer =
        |key_env: &str| format!("  torznab:\n    - name: jackett\n      url: {}/torznab/api\n{key_env}", server.base);
    let mut home = Home::new(&server, &[], &indexer("      api_key_env: TEST_JACKETT_KEY\n"));
    let config = std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap();
    home.set("TEST_JACKETT_KEY", "k3y");
    let name = |run: &Run| run.json()["files"]["name"].as_str().map(String::from);

    let found = home.run(&["search", "ubuntu", "--json"]).json();
    let locator = found["result"]["releases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .find(|id| id.starts_with('u'))
        .unwrap()[..7]
        .to_string();
    let before = home.run(&["files", &locator, "--json"]);
    home.run(&["daemon", "stop"]);
    let on_disk: Vec<String> = walk(&home.path("th"))
        .into_iter()
        .filter(|path| std::fs::read(path).unwrap().windows(3).any(|w| w == b"k3y"))
        .map(|path| path.display().to_string())
        .collect();
    home.set("TEST_JACKETT_KEY", "n3w");
    let after = home.run(&["files", &locator, "--json"]);
    home.run(&["daemon", "stop"]);
    home.write_config(&config.replace("      api_key_env: TEST_JACKETT_KEY\n", ""));
    let keyless = home.run(&["files", &locator]);
    home.run(&["daemon", "stop"]);
    home.write_config(&config.replace(&indexer("      api_key_env: TEST_JACKETT_KEY\n"), ""));
    let removed = home.run(&["files", &locator]);
    home.run(&["daemon", "stop"]);
    home.write_config(&config);
    home.unset("TEST_JACKETT_KEY");
    let unset = home.run(&["files", &locator]);

    assert_eq!(name(&before), Some("ubuntu-private.iso".into()), "{}{}", before.stdout, before.stderr);
    assert_eq!(on_disk, Vec::<String>::new(), "the API key reached the disk");
    assert_eq!(name(&after), Some("ubuntu-private.iso".into()), "{}{}", after.stdout, after.stderr);
    assert_eq!(
        server.asked("/dl/privatehd/"),
        [
            "/dl/privatehd/?jackett_apikey=k3y&path=def&file=ubuntu",
            "/dl/privatehd/?path=def&file=ubuntu&jackett_apikey=n3w"
        ]
    );
    assert_eq!(
        (keyless.code, keyless.stderr.as_str()),
        (
            2,
            "error[usage]: this result's .torrent link needs an API key for the jackett indexer, which has none\n\
             hint: set api_key_env for jackett in the torrential config and run torrential engine stop, or search \
             again\n"
        )
    );
    assert_eq!(
        (removed.code, removed.stderr.as_str()),
        (
            2,
            "error[usage]: this result's .torrent link needs an API key for the jackett indexer, which is not \
             configured\nhint: add jackett back to sources.torznab in the torrential config and run torrential \
             engine stop, or search again\n"
        )
    );
    let config_file = home.path("th/config/config.yaml").display().to_string();
    assert_eq!(
        (unset.code, unset.stderr.as_str()),
        (
            2,
            &*format!(
                "error[usage]: {config_file} line 6: the \"jackett\" indexer takes its API key from TEST_JACKETT_KEY, \
                 which is not set\nhint: fix {config_file}\n"
            )
        )
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() { walk(&path) } else { vec![path] }
        })
        .collect()
}

fn send(socket: &mut UnixStream, request: &Request) {
    socket.write_all(&encode_frame(request).unwrap()).unwrap();
}

fn receive(socket: &mut UnixStream) -> Value {
    let mut len = [0u8; 4];
    socket.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    socket.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn a_streaming_search_sends_updates_before_the_result() {
    let server = Server::start(|_| {
        vec![
            ("/knaben/", vec![ok(fixture("knaben/v2-search-ubuntu.json"))]),
            ("/apibay/", vec![Reply { delay: Duration::from_millis(800), ..ok(fixture("apibay/q.php-ubuntu.json")) }]),
        ]
    });
    let home = Home::new(&server, &["knaben", "apibay"], "");
    assert_eq!(home.run(&["status"]).code, 0);

    let mut socket = UnixStream::connect(home.path("th/run/engine.sock")).unwrap();
    send(&mut socket, &Request::Hello { client_version: "test".into(), wire: WIRE_VERSION });
    receive(&mut socket);
    send(&mut socket, &Request::Search { query: SearchQuery::new("ubuntu"), stream: true });
    let mut frames = Vec::new();
    loop {
        let frame = receive(&mut socket);
        let kind = frame["event"].as_str().or(frame["type"].as_str()).unwrap().to_string();
        let sources: Vec<String> = frame["result"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| format!("{} {}", s["source"].as_str().unwrap(), s["outcome"].as_str().unwrap()))
            .collect();
        frames.push(format!("{kind}: {sources:?} total {} pending {}", frame["result"]["total"], frame["pending"]));
        if kind != "search_update" {
            break;
        }
    }

    assert_eq!(
        frames,
        [
            r#"search_update: ["knaben ok"] total 97 pending ["apibay"]"#,
            r#"search: ["knaben ok", "apibay ok"] total 158 pending null"#,
        ]
    );
}

#[test]
fn the_engine_refuses_a_config_that_does_not_parse() {
    let server = Server::start(|_| Vec::new());
    let home = Home::new(&server, &[], "  torznab: 3\n");

    let refused = home.run(&["status"]);
    let doctor = home.run(&["doctor", "--json"]).json();
    home.write_config("sources:\n  disabled: [nyaa]\n");
    let fixed = home.run(&["status"]);
    let fixed_doctor = home.run(&["doctor", "--json"]).json();

    let file = home.path("th/config/config.yaml").display().to_string();
    let message = format!("{file} line 3: expected sequence start");
    assert_eq!(refused.code, 2, "{}{}", refused.stdout, refused.stderr);
    assert!(refused.stderr.contains(&message), "{}", refused.stderr);
    let config = |doctor: &Value| {
        let check = doctor["checks"].as_array().unwrap().iter().find(|c| c["check"] == "config").unwrap();
        (check["status"].as_str().unwrap().to_string(), check["detail"].as_str().unwrap().to_string())
    };
    assert_eq!(config(&doctor), ("fail".to_string(), message));
    assert_eq!(fixed.code, 0, "{}", fixed.stderr);
    assert_eq!(config(&fixed_doctor), ("ok".to_string(), file));
}

#[test]
fn a_config_toml_is_named_rather_than_ignored() {
    let server = Server::start(|_| Vec::new());
    let home = Home::new(&server, &[], "");
    std::fs::remove_file(home.path("th/config/config.yaml")).unwrap();
    std::fs::write(home.path("th/config/config.toml"), "[downloads]\ndir = \"~/isos\"\n").unwrap();

    let refused = home.run(&["status"]);
    let doctor = home.run(&["doctor"]);

    let (toml, yaml) = (home.path("th/config/config.toml"), home.path("th/config/config.yaml"));
    let message = format!("{} is not read; torrential reads config.yaml", toml.display());
    assert_eq!(refused.code, 2, "{}{}", refused.stdout, refused.stderr);
    assert_eq!(
        refused.stderr,
        format!(
            "error[usage]: {message}\nhint: write your settings as YAML in {}, then delete config.toml\n",
            yaml.display()
        )
    );
    assert!(doctor.stdout.contains(&format!("config\tfail\t{message}\n")), "{}", doctor.stdout);
}

#[test]
fn doctor_network_asks_each_enabled_source() {
    let server = Server::start(|_| {
        vec![
            ("/knaben/", vec![status(503)]),
            ("/apibay/", vec![ok(fixture("apibay/q.php-zzqqxxnotarealthing987.json"))]),
            ("/nyaa/", vec![ok(fixture("nyaa/index-linux.xml"))]),
        ]
    });
    let home = Home::new(&server, &["knaben", "apibay", "nyaa"], "");

    let doctor = home.run(&["doctor", "--network", "--json"]).json();

    let sources: Vec<(String, String, String)> = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["check"].as_str().unwrap().starts_with("source"))
        .map(|c| {
            let detail = c["detail"].as_str().unwrap();
            let (outcome, took) = detail.rsplit_once(", ").unwrap();
            assert!(took.ends_with(" ms") && took[..took.len() - 3].parse::<u32>().is_ok(), "{detail}");
            (c["check"].as_str().unwrap().to_string(), c["status"].as_str().unwrap().to_string(), outcome.to_string())
        })
        .collect();
    let expected = [
        ("source knaben", "fail", "http 503"),
        ("source apibay", "ok", "0 results"),
        ("source nyaa", "ok", "30 results"),
    ];
    assert_eq!(sources, expected.map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string())));
    assert_eq!(server.asked("/nyaa/"), ["/nyaa/?page=rss&q=linux&c=0_0&f=0"]);
}

#[test]
fn files_on_an_archive_org_result_reads_its_torrent_url_not_the_bare_magnet() {
    let item = "/archive.org/download/night_of_the_living_dead_dvd/night_of_the_living_dead_dvd_archive.torrent";
    let server = Server::start(|_| {
        vec![
            (
                "/archive.org/advancedsearch.php",
                vec![ok(fixture("archive.org/advancedsearch.php-night-of-the-living-dead.json"))],
            ),
            (item, vec![ok(torrent("night_of_the_living_dead_dvd"))]),
            ("/archive.org/download/Night_Of_The_Living_Dead_raw_HD_WS/", vec![status(503)]),
        ]
    });
    let home = Home::new(&server, &["archive.org"], "");

    let found = home.run(&["search", "night", "of", "the", "living", "dead", "--limit", "50", "--json"]).json();
    let files = home.run(&["files", "6bc1af", "--json"]).json();
    let asked = std::time::Instant::now();
    let unreachable = home.run(&["files", "27f32a"]);
    let took = asked.elapsed();

    let release = found["result"]["releases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "6bc1af23a17fd62156ab2f0e41d3c86769c2c66c")
        .unwrap();
    assert_eq!(
        release["magnet"],
        "magnet:?xt=urn:btih:6bc1af23a17fd62156ab2f0e41d3c86769c2c66c&dn=Night%20of%20the%20Living%20Dead"
    );
    assert_eq!(
        (files["files"]["metadata_source"].as_str(), files["files"]["name"].as_str()),
        (Some("torrent_url"), Some("night_of_the_living_dead_dvd"))
    );
    assert_eq!(server.asked(item).len(), 1);
    assert_eq!(unreachable.code, 5, "{}", unreachable.stderr);
    assert!(
        unreachable.stderr.starts_with("error[fetch_failed]: could not fetch the .torrent file: http 503\n"),
        "{}",
        unreachable.stderr
    );
    assert!(took < Duration::from_secs(15), "took {took:?}");
}

#[test]
fn the_machine_and_preferences_rank_a_film_search_and_say_so() {
    let server = Server::start(|_| {
        vec![(
            "/archive.org/advancedsearch.php",
            vec![ok(fixture("archive.org/advancedsearch.php-night-of-the-living-dead-1968.json"))],
        )]
    });
    let home = Home::new(&server, &["archive.org"], "");
    let search = ["search", "night", "of", "the", "living", "dead", "1968", "--category", "video", "--limit", "6"];
    let shown = |run: &Run| -> Vec<String> {
        assert_eq!(run.code, 0, "{}", run.stderr);
        let lines: Vec<&str> = run.stdout.lines().collect();
        let header: Vec<&str> = lines[0].split('\t').collect();
        let columns: Vec<usize> =
            ["SIZE", "QUALITY", "NAME"].iter().filter_map(|c| header.iter().position(|h| h == c)).collect();
        let rows = lines.iter().take_while(|l| !l.starts_with('#')).map(|l| {
            let cells: Vec<&str> = l.split('\t').collect();
            columns.iter().map(|i| cells[*i]).collect::<Vec<_>>().join(" | ")
        });
        let summary = lines.iter().filter(|l| l.starts_with('#')).skip(1).map(|l| l.to_string());
        rows.chain(summary).collect()
    };

    let before = home.run(&search);
    assert_eq!(home.run(&["setup", "--screen", "laptop", "--max-size", "2GB", "--playback", "h264"]).code, 0);
    let after = home.run(&search);

    assert_eq!(
        shown(&before),
        [
            "SIZE | QUALITY | NAME",
            "3.4 GiB | 1080p | Night Of The Living Dead ( 1968) English FULL HD",
            "16.8 GiB | 1080p | Night of the Living Dead (1968) [P&M] 1080p Alternate Crop (16GB)",
            "3.4 GiB | 1080p | Night Of The Living Dead ( 1968) Doblada Al Español FULL HD",
            "1.0 GiB | 720p | Night Of The Living Dead 720p 1968",
            "54.7 GiB | 2160p | Night of the Living Dead (1968) (4k Remaster)",
            "6.4 GiB | 2160p | Night of the Living Dead (1968) [colorised, 4k, 60FPS]",
            "# ranked for this Mac: 1080p screen, shows HDR, plays HEVC, arm64, English",
        ]
    );
    assert_eq!(
        shown(&after),
        [
            "SIZE | QUALITY | NAME",
            "1.0 GiB | 720p | Night Of The Living Dead 720p 1968",
            "1.1 GiB | - | Night of the Living Dead (1968)",
            "1.1 GiB | - | Night of the Living Dead (1968)",
            "528.0 MiB | - | Night of the Living Dead (1968)",
            "1.8 GiB | - | Night of the Living Dead (1968)",
            "742.1 MiB | - | Night Of The Living Dead ( 1968)",
            "# ranked for this Mac: 1080p screen, no HDR, H.264 only, up to 2 GB, arm64, English · overrides: screen \
             laptop, max_size 2GB, playback h264",
        ]
    );
}
