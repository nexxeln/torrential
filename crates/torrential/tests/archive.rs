use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::Value;
use sha2::Digest;
use torrential_engine as lt;

const ITEM: &str = "LlamaDrama";
const FILES: [(&str, usize); 2] = [("a.bin", 100_000), ("b.bin", 60_000)];
const OTHERS: [&str; 7] = ["archlinux", "ubuntu", "debian", "academictorrents", "knaben", "apibay", "nyaa"];

#[derive(Clone)]
enum Answer {
    Bytes { body: Vec<u8>, delay: Duration },
    Status(u16),
    Redirect(String),
}

fn bytes(body: Vec<u8>) -> Answer {
    Answer::Bytes { body, delay: Duration::ZERO }
}

struct Server {
    base: String,
}

impl Server {
    fn start(routes: impl FnOnce(&str) -> Vec<(String, Answer)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(routes(&base));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = routes.clone();
                std::thread::spawn(move || serve(stream, &routes));
            }
        });
        Server { base }
    }
}

fn serve(mut stream: TcpStream, routes: &[(String, Answer)]) {
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
        let head = String::from_utf8_lossy(&pending[..end]).to_lowercase();
        pending.drain(..end);
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let range = head.lines().find_map(|l| {
            let (first, last) = l.strip_prefix("range: bytes=")?.trim().split_once('-')?;
            Some((first.parse::<usize>().ok()?, last.parse::<usize>().ok()?))
        });
        let answer = routes.iter().find(|(prefix, _)| path.starts_with(&prefix.to_lowercase())).map(|(_, a)| a);
        let (head, body): (String, &[u8]) = match answer {
            Some(Answer::Bytes { body, delay }) => {
                std::thread::sleep(*delay);
                match range {
                    Some((first, last)) => (
                        format!("206 Partial\r\nContent-Range: bytes {first}-{last}/{}", body.len()),
                        &body[first..=last],
                    ),
                    None => ("200 OK".into(), body),
                }
            }
            Some(Answer::Redirect(to)) => (format!("302 Found\r\nLocation: {to}"), b""),
            Some(Answer::Status(code)) => (format!("{code} No"), b""),
            None => ("404 No".into(), b""),
        };
        let head = format!("HTTP/1.1 {head}\r\nContent-Length: {}\r\n\r\n", body.len());
        if stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body)).is_err() {
            return;
        }
    }
}

struct Item {
    _dir: tempfile::TempDir,
    files: Vec<(&'static str, Vec<u8>)>,
    torrent: lt::Metainfo,
}

impl Item {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut state = 0x9e37_79b9_u64;
        let mut files = Vec::new();
        for (name, size) in FILES {
            let bytes: Vec<u8> = (0..size)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect();
            let path = dir.path().join(ITEM).join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            files.push((name, bytes));
        }
        let piece = lt::PieceSize::new(16 * 1024).unwrap();
        let torrent = lt::create_torrent(&dir.path().join(ITEM), piece, lt::TorrentFormat::V1).unwrap();
        Self { _dir: dir, files, torrent }
    }

    fn id(&self) -> String {
        self.torrent.info_hash().v1().unwrap().as_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn torrent_file(&self, archive: &str) -> Vec<u8> {
        let plain = self.torrent.as_bytes();
        let seed = format!("{archive}/archive.org/download/");
        let list = format!("8:url-listl{}:{seed}e", seed.len());
        [&plain[..plain.len() - 1], list.as_bytes(), b"e"].concat()
    }

    fn search_answer(&self) -> Vec<u8> {
        let doc = serde_json::json!({ "response": { "docs": [{
            "identifier": ITEM, "title": "Llama Drama", "btih": self.id(), "item_size": 160_000, "mediatype": "movies",
        }]}});
        doc.to_string().into_bytes()
    }

    fn mirror(&self) -> Server {
        let files = self.files.clone();
        Server::start(|_| {
            files.into_iter().map(|(name, body)| (format!("/items/{ITEM}/{name}"), bytes(body))).collect()
        })
    }

    fn archive(&self, metadata: Answer, download: impl Fn(&str, &[u8]) -> Answer) -> Server {
        Server::start(|base| {
            let mut routes = vec![
                ("/archive.org/advancedsearch.php".into(), bytes(self.search_answer())),
                (format!("/archive.org/download/{ITEM}/{ITEM}_archive.torrent"), bytes(self.torrent_file(base))),
                (format!("/archive.org/metadata/{ITEM}"), metadata),
            ];
            for (name, body) in &self.files {
                routes.push((format!("/archive.org/download/{ITEM}/{name}"), download(name, body)));
            }
            routes
        })
    }

    fn intact(&self, home: &Home) -> bool {
        self.files.iter().all(|(name, body)| {
            let got = std::fs::read(home.downloads().join(ITEM).join(name)).unwrap_or_default();
            sha2::Sha256::digest(&got) == sha2::Sha256::digest(body)
        })
    }
}

struct Home {
    dir: tempfile::TempDir,
    archive: String,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Home {
    fn new(archive: &Server) -> Self {
        let home = Self { dir: tempfile::tempdir().unwrap(), archive: archive.base.clone() };
        let config = home.dir.path().join("th/config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("config.yaml"), format!("sources:\n  disabled: {OTHERS:?}\n")).unwrap();
        home
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("torrential").unwrap();
        for key in ["CODEX_SANDBOX_NETWORK_DISABLED", "CODEX_THREAD_ID", "CODEX_CI", "TORRENTIAL_TEST_ENGINE"] {
            cmd.env_remove(key);
        }
        cmd.args(args)
            .env("HOME", self.dir.path())
            .env("TORRENTIAL_HOME", self.dir.path().join("th"))
            .env("TORRENTIAL_TEST_IDLE_TIMEOUT", "60s")
            .env("TORRENTIAL_TEST_NETWORK", "loopback")
            .env("TORRENTIAL_TEST_DISK", "roomy")
            .env("TORRENTIAL_TEST_ENDPOINTS", format!("archive.org={}/archive.org/", self.archive))
            .timeout(Duration::from_secs(120));
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

    fn downloads(&self) -> PathBuf {
        self.dir.path().join("dl")
    }

    fn get(&self, item: &Item) -> Duration {
        let found = self.run(&["search", "llama", "drama", "--json"]);
        let found: Value = serde_json::from_str(&found.stdout).unwrap_or_else(|e| panic!("{e}: {}", found.stderr));
        assert_eq!(found["result"]["releases"][0]["id"].as_str(), Some(item.id().as_str()), "{found:#}");
        let dir = self.downloads().display().to_string();
        let asked = Instant::now();
        let get = self.run(&["get", &item.id()[..6], "--dir", &dir]);
        let took = asked.elapsed();
        assert_eq!(get.code, 0, "{}{}", get.stdout, get.stderr);
        took
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("th/state/engine.log")).unwrap_or_default()
    }

    fn logs(&self, line: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.log().contains(line) {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

#[test]
fn an_item_whose_files_redirect_to_different_servers_downloads_from_the_servers_its_metadata_names() {
    let item = Item::new();
    let (first, second) = (item.mirror(), item.mirror());
    let metadata =
        serde_json::json!({ "d1": first.base[7..], "d2": second.base[7..], "dir": format!("/items/{ITEM}") });
    let late = Answer::Bytes { body: metadata.to_string().into_bytes(), delay: Duration::from_secs(5) };
    let archive = item.archive(late, |name, _| {
        let server = if name == "a.bin" { &first } else { &second };
        Answer::Redirect(format!("{}/items/{ITEM}/{name}", server.base))
    });
    let home = Home::new(&archive);

    let took = home.get(&item);
    let wait = home.run(&["wait", &item.id()[..6], "--timeout", "60s"]);

    assert!(took < Duration::from_secs(4), "get waited {took:?} for the metadata API");
    assert_eq!(wait.code, 0, "{}{}\n{}", wait.stdout, wait.stderr, home.log());
    assert!(item.intact(&home), "the downloaded files differ from the item's");
    let added = format!("{} also downloads from {}/items/, {}/items/", &item.id()[..6], first.base, second.base);
    assert!(home.logs(&added), "{}", home.log());
}

#[test]
fn without_an_answer_from_the_metadata_api_an_item_downloads_from_the_torrents_own_web_seed() {
    let item = Item::new();
    let archive = item.archive(Answer::Status(503), |_, body| bytes(body.to_vec()));
    let home = Home::new(&archive);

    home.get(&item);
    let wait = home.run(&["wait", &item.id()[..6], "--timeout", "60s"]);

    assert_eq!(wait.code, 0, "{}{}\n{}", wait.stdout, wait.stderr, home.log());
    assert!(item.intact(&home), "the downloaded files differ from the item's");
    let kept = format!(
        "{} keeps the torrent's own web seeds: archive.org did not name the servers of {ITEM}: HTTP 503",
        &item.id()[..6]
    );
    assert!(home.logs(&kept), "{}", home.log());
}
