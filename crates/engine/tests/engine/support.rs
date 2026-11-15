use std::collections::BTreeMap;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha1::Digest as _;
use tempfile::TempDir;
use torrential_core::{InfoHash, TorrentId};
use torrential_engine::{
    AddTorrent, Alert, Config, Metainfo, PieceSize, Session, TorrentFormat, TorrentSource, TorrentStatus, UploadPolicy,
    create_torrent,
};

pub const PIECE: u32 = 16 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(20);

pub fn rate(bytes_per_sec: u32) -> Option<NonZeroU32> {
    NonZeroU32::new(bytes_per_sec)
}

pub fn upload(policy: UploadPolicy) -> Config {
    Config { upload: policy, ..Config::loopback() }
}

pub struct Node {
    pub session: Session,
    pub log: Vec<Alert>,
    examined: usize,
    port: u16,
}

impl Node {
    pub fn new(config: Config) -> Node {
        let session = Session::new(config).expect("session starts");
        let mut node = Node { session, log: Vec::new(), examined: 0, port: 0 };
        node.wait_for("listen socket", |a| matches!(a, Alert::ListenSucceeded { .. }));
        node.port = node.session.listen_port().unwrap().expect("listening");
        node
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.port)
    }

    pub fn add(&mut self, add: AddTorrent) -> TorrentId {
        self.session.add(add).expect("torrent added").id()
    }

    pub fn seed(&mut self, metainfo: &Metainfo, save_path: &Path) -> TorrentId {
        let id = self.add(AddTorrent::new(TorrentSource::Metainfo(metainfo.clone()), save_path));
        self.wait_until("seeding", |s| {
            s.status(id).unwrap().progress.verified_selected_bytes() == total_size(metainfo)
        });
        id
    }

    /// Adds a torrent that downloads at most `bytes_per_sec`, so a test can catch it mid-download.
    pub fn add_slowly(&mut self, mut add: AddTorrent, bytes_per_sec: u32) -> TorrentId {
        add.paused = true;
        let id = self.add(add);
        self.session.set_download_limit(id, rate(bytes_per_sec)).unwrap();
        self.session.resume(id).unwrap();
        self.wait_for("resumed", |a| matches!(a, Alert::TorrentResumed { .. }));
        id
    }

    pub fn status(&self, id: TorrentId) -> TorrentStatus {
        self.session.status(id).expect("status")
    }

    fn pump(&mut self, timeout: Duration) {
        let alerts = self.session.wait_alerts(timeout).expect("alerts");
        self.log.extend(alerts);
    }

    pub fn wait_for(&mut self, what: &str, mut matches: impl FnMut(&Alert) -> bool) -> Alert {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(offset) = self.log[self.examined..].iter().position(&mut matches) {
                self.examined += offset + 1;
                return self.log[self.examined - 1].clone();
            }
            self.examined = self.log.len();
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; alerts seen: {:#?}", self.log);
            }
            self.pump(Duration::from_millis(100));
        }
    }

    pub fn wait_until(&mut self, what: &str, mut done: impl FnMut(&Session) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if done(&self.session) {
                return;
            }
            self.pump(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}; alerts seen: {:#?}", self.log);
    }

    /// `TorrentFinished` can precede libtorrent's disk writes; `TorrentPaused` follows them.
    pub fn finish(&mut self, id: TorrentId) {
        self.wait_for("finish", |a| matches!(a, Alert::TorrentFinished { .. }));
        self.session.pause(id).unwrap();
        self.wait_for("paused after finishing", |a| matches!(a, Alert::TorrentPaused { .. }));
    }

    pub fn idle(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(50));
        }
    }
}

pub struct Fixture {
    pub dir: TempDir,
    pub name: String,
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    pub fn new(name: &str, files: &[(&str, usize)], seed: u64) -> Fixture {
        let dir = TempDir::new().unwrap();
        let mut state = seed | 1;
        let mut contents = BTreeMap::new();
        for &(file, size) in files {
            let bytes: Vec<u8> = (0..size)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect();
            let path = dir.path().join(name).join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            contents.insert(file.to_owned(), bytes);
        }
        Fixture { dir, name: name.to_owned(), files: contents }
    }

    pub fn save_path(&self) -> &Path {
        self.dir.path()
    }

    pub fn torrent(&self, format: TorrentFormat) -> Metainfo {
        create_torrent(&self.dir.path().join(&self.name), PieceSize::new(PIECE).unwrap(), format)
            .expect("torrent created")
    }

    pub fn path_in(&self, save_path: &Path, file: &str) -> PathBuf {
        save_path.join(&self.name).join(file)
    }

    pub fn sha256(&self, file: &str) -> [u8; 32] {
        sha256(&self.files[file])
    }
}

pub fn total_size(metainfo: &Metainfo) -> u64 {
    metainfo.files().iter().map(|f| f.size).sum()
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

pub fn sha256_file(path: &Path) -> Option<[u8; 32]> {
    fs::read(path).ok().map(|b| sha256(&b))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn magnet(v1: Option<[u8; 20]>, v2: Option<[u8; 32]>) -> String {
    let mut uri = String::from("magnet:?");
    if let Some(v1) = v1 {
        uri += &format!("xt=urn:btih:{}&", hex(&v1));
    }
    if let Some(v2) = v2 {
        uri += &format!("xt=urn:btmh:1220{}&", hex(&v2));
    }
    uri.pop();
    uri
}

pub fn expected_hashes(torrent: &[u8]) -> ([u8; 20], [u8; 32]) {
    let info = bencode::dict_value(torrent, b"info").expect("info dictionary");
    (sha1::Sha1::digest(info).into(), sha256(info))
}

pub fn info_hash(hashes: ([u8; 20], [u8; 32]), format: TorrentFormat) -> InfoHash {
    match format {
        TorrentFormat::V1 => InfoHash::V1(hashes.0),
        TorrentFormat::Hybrid => InfoHash::Hybrid { v1: hashes.0, v2: hashes.1 },
    }
}

pub mod bencode {
    pub fn dict_value<'a>(data: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
        if data.first() != Some(&b'd') {
            return None;
        }
        let mut pos = 1;
        while data.get(pos)? != &b'e' {
            let key_end = skip(data, pos)?;
            let k = string(&data[pos..key_end])?;
            let value_end = skip(data, key_end)?;
            if k == key {
                return Some(&data[key_end..value_end]);
            }
            pos = value_end;
        }
        None
    }

    pub fn string(raw: &[u8]) -> Option<&[u8]> {
        let colon = raw.iter().position(|&b| b == b':')?;
        let len: usize = std::str::from_utf8(&raw[..colon]).ok()?.parse().ok()?;
        raw.get(colon + 1..colon + 1 + len)
    }

    fn skip(data: &[u8], pos: usize) -> Option<usize> {
        match *data.get(pos)? {
            b'i' => Some(pos + data[pos..].iter().position(|&b| b == b'e')? + 1),
            b'l' | b'd' => {
                let mut p = pos + 1;
                while *data.get(p)? != b'e' {
                    p = skip(data, p)?;
                }
                Some(p + 1)
            }
            b'0'..=b'9' => {
                let colon = pos + data[pos..].iter().position(|&b| b == b':')?;
                let len: usize = std::str::from_utf8(&data[pos..colon]).ok()?.parse().ok()?;
                Some(colon + 1 + len)
            }
            _ => None,
        }
    }
}

pub fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, [u8; 32])> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            if meta.is_dir() {
                pending.push(path);
            } else {
                let hash = sha256(&fs::read(&path).unwrap());
                files.insert(path, (meta.len(), meta.modified().unwrap(), hash));
            }
        }
    }
    files
}
