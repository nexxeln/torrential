use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::Digest;
use torrential_engine as lt;

mod common;
use common::{Home, Reply, Server, hooks, ok};

const ITEM: &str = "LlamaDrama";
const FILES: [(&str, usize); 2] = [("a.bin", 100_000), ("b.bin", 60_000)];

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
        self.torrent.info_hash().id().to_hex()
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
            files.into_iter().map(|(name, body)| (format!("/items/{ITEM}/{name}"), vec![ok(body)])).collect()
        })
    }

    fn archive(&self, metadata: Reply, download: impl Fn(&str, &[u8]) -> Reply) -> Server {
        Server::start(|base| {
            let mut routes = vec![
                ("/archive.org/advancedsearch.php".into(), vec![ok(self.search_answer())]),
                (format!("/archive.org/download/{ITEM}/{ITEM}_archive.torrent"), vec![ok(self.torrent_file(base))]),
                (format!("/archive.org/metadata/{ITEM}"), vec![metadata]),
            ];
            for (name, body) in &self.files {
                routes.push((format!("/archive.org/download/{ITEM}/{name}"), vec![download(name, body)]));
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

impl Home {
    /// A home whose only source is archive.org, answered by `archive`.
    fn archived(archive: &Server) -> Self {
        let home = Home::new()
            .with(hooks::DISK, "roomy")
            .with(hooks::ENDPOINTS, &format!("archive.org={}/archive.org/", archive.base));
        let others = ["archlinux", "ubuntu", "debian", "academictorrents", "knaben", "apibay", "nyaa"];
        home.write_config(&format!("sources:\n  disabled: {others:?}\n"));
        home
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

#[test]
fn an_item_whose_files_redirect_to_different_servers_downloads_from_the_servers_its_metadata_names() {
    let item = Item::new();
    let (first, second) = (item.mirror(), item.mirror());
    let metadata =
        serde_json::json!({ "d1": first.base[7..], "d2": second.base[7..], "dir": format!("/items/{ITEM}") });
    let late = Reply::Bytes { body: metadata.to_string().into_bytes(), delay: Duration::from_secs(5) };
    let archive = item.archive(late, |name, _| {
        let server = if name == "a.bin" { &first } else { &second };
        Reply::Redirect(format!("{}/items/{ITEM}/{name}", server.base))
    });
    let home = Home::archived(&archive);

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
    let archive = item.archive(Reply::Status(503), |_, body| ok(body.to_vec()));
    let home = Home::archived(&archive);

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
