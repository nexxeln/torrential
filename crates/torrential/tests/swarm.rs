use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::Value;
use sha2::Digest;
use torrential_engine as lt;

const PIECE: u32 = 16 * 1024;
const SLOW: Option<NonZeroU32> = NonZeroU32::new(256 * 1024);
/// libtorrent waits `min_reconnect_time`, 60 seconds, before it dials a peer again after
/// disconnecting it, as on pause or a refused connection. With one seeder, nothing else fills
/// that gap.
const RECONNECT: &str = "4m";

struct Home {
    dir: tempfile::TempDir,
    peers: String,
    save_resume_every: Option<&'static str>,
    stall_after: Option<&'static str>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Home {
    fn new(seeder: &Seeder) -> Self {
        Self::seeded_by(&[seeder])
    }

    fn seeded_by(seeders: &[&Seeder]) -> Self {
        let peers = seeders.iter().map(|s| s.addr.to_string()).collect::<Vec<_>>().join(",");
        Self { dir: tempfile::tempdir().unwrap(), peers, save_resume_every: None, stall_after: None }
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
            .env("TORRENTIAL_TEST_PEERS", &self.peers)
            .env("TORRENTIAL_TEST_DISK", "roomy")
            .timeout(Duration::from_secs(300));
        if let Some(every) = self.save_resume_every {
            cmd.env("TORRENTIAL_TEST_SAVE_RESUME_EVERY", every);
        }
        if let Some(after) = self.stall_after {
            cmd.env("TORRENTIAL_TEST_STALL_AFTER", after);
        }
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

    fn ok(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        let run = self.run(&args);
        assert_eq!(run.code, 0, "torrential {args:?}: {}{}", run.stdout, run.stderr);
        serde_json::from_str(&run.stdout).unwrap()
    }

    fn job(&self, id: &str) -> Value {
        self.ok(&["status", id])["jobs"][0].clone()
    }

    fn until(&self, id: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let job = self.job(id);
            if done(&job) {
                return job;
            }
            assert!(Instant::now() < deadline, "timed out waiting until {what}: {job:#}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn pid(&self) -> i32 {
        self.ok(&["daemon", "status"])["pid"].as_i64().expect("the engine runs") as i32
    }

    fn kill_engine(&self) {
        let pid = self.pid();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "the killed engine is still alive");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn logged_addrs(&self) -> Vec<SocketAddr> {
        let log = std::fs::read_to_string(self.dir.path().join("th/state/engine.log")).unwrap_or_default();
        log.lines().filter_map(|l| l.split_once(" listening on ")?.1.strip_suffix(" (tcp)")?.parse().ok()).collect()
    }

    fn downloads(&self) -> PathBuf {
        self.dir.path().join("dl")
    }

    fn get(&self, target: &str, extra: &[&str]) -> Value {
        let dir = self.downloads().display().to_string();
        let mut args = vec!["get", target, "--dir", &dir];
        args.extend(extra);
        self.ok(&args)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    name: &'static str,
    files: BTreeMap<&'static str, Vec<u8>>,
    torrent: lt::Metainfo,
}

impl Fixture {
    fn new(name: &'static str, files: &[(&'static str, usize)], seed: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
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
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            contents.insert(file, bytes);
        }
        let piece = lt::PieceSize::new(PIECE).unwrap();
        let torrent = lt::create_torrent(&dir.path().join(name), piece, lt::TorrentFormat::V1).unwrap();
        Self { dir, name, files: contents, torrent }
    }

    fn id(&self) -> String {
        hex(self.torrent.info_hash().v1().unwrap().as_bytes())
    }

    fn short(&self) -> String {
        self.id()[..6].to_string()
    }

    fn magnet(&self) -> String {
        format!("magnet:?xt=urn:btih:{}&dn={}", self.id(), self.name)
    }

    fn torrent_file(&self) -> String {
        let path = self.dir.path().join(format!("{}.torrent", self.name));
        std::fs::write(&path, self.torrent.as_bytes()).unwrap();
        path.display().to_string()
    }

    fn total(&self) -> u64 {
        self.files.values().map(|b| b.len() as u64).sum()
    }

    fn downloaded(&self, home: &Home, file: &str) -> PathBuf {
        home.downloads().join(self.name).join(file)
    }

    fn intact(&self, home: &Home, file: &str) -> bool {
        std::fs::read(self.downloaded(home, file)).is_ok_and(|b| sha256(&b) == sha256(&self.files[file]))
    }
}

struct Seeder {
    session: lt::Session,
    id: lt::TorrentId,
    addr: SocketAddr,
}

impl Seeder {
    fn start(fixture: &Fixture, rate: Option<NonZeroU32>) -> Self {
        Self::serving(fixture, rate, fixture.total())
    }

    fn lacking(fixture: &Fixture, file: &str, has: u64) -> Self {
        std::fs::remove_file(fixture.dir.path().join(fixture.name).join(file)).unwrap();
        Self::serving(fixture, None, has)
    }

    fn serving(fixture: &Fixture, rate: Option<NonZeroU32>, has: u64) -> Self {
        let config = lt::Config {
            listen: vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)],
            dht: lt::Dht::Off,
            lsd: false,
            upnp: false,
            natpmp: false,
            upload: rate.map_or(lt::UploadPolicy::Normal, lt::UploadPolicy::Capped),
            connections: lt::ConnectionLimits { multiple_per_ip: true, ..lt::ConnectionLimits::default() },
            ..lt::Config::default()
        };
        let mut session = lt::Session::new(config).unwrap();
        let source = lt::TorrentSource::Metainfo(fixture.torrent.clone());
        let id = session.add(lt::AddTorrent::new(source, fixture.dir.path())).unwrap().id();
        let deadline = Instant::now() + Duration::from_secs(20);
        let seeding = |s: &lt::Session| s.status(id).unwrap().progress.verified_selected_bytes() == has;
        let port = loop {
            if let (true, Some(port)) = (seeding(&session), session.listen_port().unwrap()) {
                break port;
            }
            assert!(Instant::now() < deadline, "the seeder never verified its files");
            std::thread::sleep(Duration::from_millis(20));
        };
        Self { session, id, addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port) }
    }

    fn uploaded(&self) -> u64 {
        self.session.status(self.id).unwrap().uploaded_payload
    }

    /// The upload count once the seeder has no peer left. A killed peer's uTP stream gets no
    /// reset, so the seeder keeps sending into it until the stream times out, seconds later.
    fn uploaded_once_alone(&self) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = self.session.status(self.id).unwrap();
            if status.peers == 0 {
                return status.uploaded_payload;
            }
            assert!(Instant::now() < deadline, "the seeder kept {} peers", status.peers);
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn verified(job: &Value) -> u64 {
    job["progress"]["verified"].as_u64().unwrap()
}

fn state(job: &Value) -> &str {
    job["state"].as_str().unwrap()
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, [u8; 32])> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            if meta.is_dir() {
                pending.push(path);
            } else {
                files.insert(
                    path.clone(),
                    (meta.len(), meta.modified().unwrap(), sha256(&std::fs::read(&path).unwrap())),
                );
            }
        }
    }
    files
}

const SET: [(&str, usize); 3] = [("a.bin", 300_000), ("b.bin", 200_000), ("docs/readme.txt", 70_000)];
const BIG: [(&str, usize); 2] = [("a.bin", 1_200_000), ("b.bin", 900_000)];

#[test]
fn a_torrent_file_downloads_and_every_file_matches_its_source() {
    let fixture = Fixture::new("set", &SET, 1);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    let get = home.get(&fixture.torrent_file(), &[]);
    assert_eq!(get["reply"]["outcome"], "started", "{get:#}");
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let job = home.job(&fixture.id());
    assert_eq!((state(&job), verified(&job)), ("done", fixture.total()));
    for file in fixture.files.keys() {
        assert!(fixture.intact(&home, file), "{file} matches its source");
    }
}

#[test]
fn a_magnet_downloads_and_every_file_matches_its_source() {
    let fixture = Fixture::new("set", &SET, 2);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    home.get(&fixture.magnet(), &[]);
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(state(&home.job(&fixture.id())), "done");
    for file in fixture.files.keys() {
        assert!(fixture.intact(&home, file), "{file} matches its source");
    }
}

#[test]
fn a_files_glob_downloads_only_the_matching_files() {
    let fixture = Fixture::new("set", &SET, 3);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    home.get(&fixture.magnet(), &["--files", "*.bin"]);
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let job = home.job(&fixture.id());
    assert_eq!((state(&job), job["progress"]["size"].as_u64()), ("done", Some(500_000)));
    assert!(fixture.intact(&home, "a.bin") && fixture.intact(&home, "b.bin"));
    assert!(!fixture.intact(&home, "docs/readme.txt"), "the unselected file was not downloaded");
    let files = home.ok(&["files", &fixture.short()]);
    let selected: Vec<(&str, bool)> = files["files"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["path"].as_str().unwrap(), f["selected"].as_bool().unwrap()))
        .collect();
    assert_eq!(selected, [("a.bin", true), ("b.bin", true), ("docs/readme.txt", false)]);
}

#[test]
fn a_removed_torrent_reuses_completed_files_without_a_peer() {
    let fixture = Fixture::new("set", &[("a.bin", 4 * PIECE as usize), ("b.bin", 4 * PIECE as usize)], 21);
    let seeder = Seeder::start(&fixture, None);
    let mut home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &["--files", "a.bin"]);
    assert_eq!(home.run(&["wait", &fixture.short()]).code, 0);
    home.ok(&["rm", &fixture.short()]);
    home.ok(&["daemon", "stop"]);
    let before = snapshot(&home.downloads());
    drop(seeder);
    home.peers.clear();

    home.get(&fixture.torrent_file(), &["--files", "a.bin"]);
    let wait = home.run(&["wait", &fixture.short(), "--timeout", "20s"]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(verified(&home.job(&fixture.id())), fixture.files["a.bin"].len() as u64);
    assert_eq!(snapshot(&home.downloads()), before, "hashing reused the files without rewriting them");
    assert!(!fixture.downloaded(&home, "b.bin").exists());

    home.ok(&["rm", &fixture.short()]);
    home.ok(&["daemon", "stop"]);
    let seeder = Seeder::start(&fixture, None);
    home.peers = seeder.addr.to_string();
    home.get(&fixture.magnet(), &["--files", "b.bin"]);
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(fixture.intact(&home, "b.bin"));
    assert_eq!(
        snapshot(&home.downloads()).get(&fixture.downloaded(&home, "a.bin")),
        before.get(&fixture.downloaded(&home, "a.bin")),
        "the previously downloaded file stays untouched when only the remaining file is selected"
    );
    assert_eq!(seeder.uploaded(), fixture.files["b.bin"].len() as u64);
}

#[test]
fn a_removed_partial_torrent_reuses_pieces_with_a_new_file_selection() {
    let fixture = Fixture::new("set", &[("a.bin", 1_200_000), ("b.bin", 900_000), ("docs/readme.txt", 70_000)], 22);
    let seeder = Seeder::start(&fixture, SLOW);
    let mut home = Home::new(&seeder);
    home.get(&fixture.magnet(), &["--files", "*.bin"]);
    home.until(&fixture.id(), "a quarter is verified", |j| verified(j) >= 500_000);
    home.ok(&["pause", &fixture.short()]);
    let before = verified(&home.job(&fixture.id()));
    let selected = (fixture.files["a.bin"].len() + fixture.files["b.bin"].len()) as u64;
    assert!(before < selected, "paused before completion");
    home.ok(&["rm", &fixture.short()]);
    home.ok(&["daemon", "stop"]);
    drop(seeder);
    let seeder = Seeder::start(&fixture, None);
    home.peers = seeder.addr.to_string();

    home.get(&fixture.magnet(), &["--files", "*.bin", "--files", "docs/readme.txt"]);
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    // The last selected piece overlaps the previously skipped readme. Re-adding
    // with that file selected may fetch this shared piece again; its bin bytes
    // count as verified in `before`, while its readme bytes do not.
    let shared = selected % u64::from(PIECE);
    assert!(
        seeder.uploaded() <= fixture.total() - before + shared,
        "the new download transferred {} bytes, more than the {} missing plus {shared} in the shared piece",
        seeder.uploaded(),
        fixture.total() - before
    );
    for file in fixture.files.keys() {
        assert!(fixture.intact(&home, file), "{file} matches its source");
    }
}

#[test]
fn a_magnet_hashes_existing_files_after_fetching_its_metadata() {
    let fixture = Fixture::new("set", &[("a.bin", 4 * PIECE as usize), ("b.bin", 4 * PIECE as usize)], 23);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    let path = fixture.downloaded(&home, "a.bin");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = fixture.files["a.bin"].clone();
    bytes[100] ^= 1;
    std::fs::write(&path, bytes).unwrap();

    home.get(&fixture.magnet(), &["--files", "a.bin"]);
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(fixture.intact(&home, "a.bin"), "the existing bytes were checked and the damaged piece repaired");
    assert_eq!(seeder.uploaded(), u64::from(PIECE), "only the damaged piece needed downloading");
    assert!(!fixture.downloaded(&home, "b.bin").exists(), "the unselected file stays unselected");
}

#[test]
fn pause_stops_writes_and_resume_finishes() {
    let fixture = Fixture::new("big", &BIG, 4);
    let seeder = Seeder::start(&fixture, SLOW);
    let home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &[]);
    home.until(&fixture.id(), "a quarter is verified", |j| verified(j) >= fixture.total() / 4);
    let pause = home.ok(&["pause", &fixture.short()]);
    assert_eq!(pause["changes"][0]["result"], "paused");
    let paused = home.job(&fixture.id());
    let before = snapshot(&home.downloads());
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(snapshot(&home.downloads()), before, "no file changed after pause returned");
    let still = home.job(&fixture.id());
    assert_eq!((state(&still), verified(&still)), ("paused", verified(&paused)));
    assert!(verified(&still) < fixture.total(), "paused mid-download");
    let map = paused["progress"]["piece_map"].as_str().expect("a paused download shows its piece map");
    assert_eq!(still["progress"]["piece_map"], map);
    assert!(map.len() == 64 && map.bytes().all(|b| b.is_ascii_digit()), "every run holds selected pieces: {map}");
    let filled = map.bytes().map(|b| f64::from(b - b'0')).sum::<f64>() / (8.0 * 64.0);
    let share = verified(&paused) as f64 / fixture.total() as f64;
    assert!((filled - share).abs() < 0.07, "{map} fills {filled:.2}, but {share:.2} is verified");
    assert_eq!(home.ok(&["resume", &fixture.short()])["changes"][0]["result"], "resumed");
    let wait = home.run(&["wait", &fixture.short(), "--timeout", RECONNECT]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(home.job(&fixture.id())["progress"].get("piece_map"), None, "a done download has no map");
    for file in fixture.files.keys() {
        assert!(fixture.intact(&home, file), "{file} matches its source");
    }
}

#[test]
fn a_paused_download_keeps_its_file_progress_across_an_engine_restart() {
    let fixture = Fixture::new("big", &BIG, 9);
    let seeder = Seeder::start(&fixture, SLOW);
    let home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &[]);
    home.until(&fixture.id(), "a third is verified", |j| verified(j) >= fixture.total() / 3);
    assert_eq!(home.ok(&["pause", &fixture.short()])["changes"][0]["result"], "paused");
    let paused = verified(&home.job(&fixture.id()));
    let file_bytes = |files: Value| -> Vec<u64> {
        files["files"]["files"].as_array().unwrap().iter().map(|f| f["verified"].as_u64().unwrap()).collect()
    };
    let before = file_bytes(home.ok(&["files", &fixture.short()]));
    assert_eq!(before.iter().sum::<u64>(), paused, "the files hold the verified bytes: {before:?}");
    assert!(before.iter().all(|b| *b > 0), "both files have pieces: {before:?}");
    home.ok(&["daemon", "stop"]);
    let after = file_bytes(home.ok(&["files", &fixture.short()]));
    assert_eq!(after, before, "a new engine that has not loaded the paused download reports the same bytes");
    assert_eq!(state(&home.job(&fixture.id())), "paused");
}

#[test]
fn a_killed_engine_resumes_without_downloading_verified_pieces_again() {
    let fixture = Fixture::new("big", &BIG, 5);
    let seeder = Seeder::start(&fixture, SLOW);
    let home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &[]);
    let job = home.until(&fixture.id(), "a third is verified", |j| verified(j) >= fixture.total() / 3);
    let before_kill = verified(&job);
    let addr = home.logged_addrs().pop().expect("the engine logged its listen address");
    std::thread::sleep(Duration::from_secs(1));
    home.kill_engine();
    let sent_before_restart = seeder.uploaded_once_alone();

    assert_eq!(state(&home.job(&fixture.id())), "interrupted");
    assert_eq!(home.logged_addrs().last(), Some(&addr), "the new engine listens on the same port");
    assert_eq!(home.ok(&["resume", &fixture.short()])["changes"][0]["result"], "resumed");
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let resent = seeder.uploaded() - sent_before_restart;
    assert!(
        resent <= fixture.total() - before_kill,
        "the restarted engine downloaded {resent} bytes, more than the {} it had not verified",
        fixture.total() - before_kill
    );
    for file in fixture.files.keys() {
        assert!(fixture.intact(&home, file), "{file} matches its source");
    }
}

#[test]
fn a_killed_engine_trusts_its_last_periodic_save_instead_of_hashing_the_files() {
    let fixture = Fixture::new("big", &BIG, 8);
    let seeder = Seeder::start(&fixture, SLOW);
    let mut home = Home::new(&seeder);
    home.save_resume_every = Some("1s");
    home.get(&fixture.torrent_file(), &[]);
    home.until(&fixture.id(), "a third is verified", |j| verified(j) >= fixture.total() / 3);
    drop(seeder);
    std::thread::sleep(Duration::from_secs(3));
    let saved = verified(&home.job(&fixture.id()));
    home.kill_engine();
    for file in fixture.files.keys() {
        let path = fixture.downloaded(&home, file);
        if let Ok(meta) = std::fs::metadata(&path) {
            std::fs::write(&path, vec![0; meta.len() as usize]).unwrap();
        }
    }

    let job = home.job(&fixture.id());
    assert_eq!((state(&job), verified(&job)), ("interrupted", saved), "the save outlived the engine");
    assert_eq!(home.ok(&["resume", &fixture.short()])["changes"][0]["result"], "resumed");
    home.until(&fixture.id(), "the engine has it again", |j| state(j) == "downloading");
    let files = home.ok(&["files", &fixture.short()]);
    let in_engine: u64 =
        files["files"]["files"].as_array().unwrap().iter().map(|f| f["verified"].as_u64().unwrap()).sum();
    assert_eq!(in_engine, saved, "the engine kept the saved pieces, which a hash of the zeroed files would reject");
}

#[test]
fn a_download_whose_seeder_lacks_the_rest_stops_with_no_source_and_keeps_what_it_verified() {
    let fixture = Fixture::new("big", &BIG, 10);
    let a_alone = 73 * u64::from(PIECE);
    let seeder = Seeder::lacking(&fixture, "b.bin", a_alone);
    let mut home = Home::new(&seeder);
    home.stall_after = Some("3s");
    home.get(&fixture.torrent_file(), &[]);
    let wait = home.run(&["wait", &fixture.short(), "--timeout", "60s", "--json"]);
    assert_eq!(wait.code, 7, "{}{}", wait.stdout, wait.stderr);
    let result: Value = serde_json::from_str(wait.stdout.lines().last().unwrap()).unwrap();
    assert_eq!(result["outcome"], "error");
    let job = home.job(&fixture.id());
    assert_eq!((state(&job), job["error"]["code"].as_str()), ("error", Some("no_source")), "{job:#}");
    assert_eq!(verified(&job), a_alone, "nothing unverified counts, and nothing verified is lost");
    home.ok(&["daemon", "stop"]);
    assert_eq!(verified(&home.job(&fixture.id())), a_alone, "what it verified outlived the engine");
}

struct Locked {
    path: PathBuf,
    restore: u32,
}

impl Locked {
    /// `None` as root, which reads and writes past any mode.
    fn new(path: PathBuf, mode: u32) -> Option<Self> {
        if unsafe { libc::geteuid() } == 0 {
            return None;
        }
        let restore = std::fs::metadata(&path).unwrap().permissions().mode();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        Some(Self { path, restore })
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.restore));
    }
}

#[test]
fn after_a_storage_error_resume_downloads_what_never_reached_the_disk() {
    let fixture = Fixture::new("set", &[("a.bin", 4_000_000), ("b.bin", 300_000)], 14);
    let seeder = Seeder::start(&fixture, SLOW);
    let home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &["--files", "a.bin"]);
    home.until(&fixture.id(), "part of a.bin is verified", |j| verified(j) >= 512 * 1024);
    let Some(locked) = Locked::new(home.downloads().join(fixture.name), 0o555) else { return };
    home.get(&fixture.torrent_file(), &["--files", "*.bin"]);
    let wait = home.run(&["wait", &fixture.short(), "--timeout", "60s"]);
    drop(locked);
    assert_eq!(wait.code, 7, "libtorrent cannot create b.bin: {}{}", wait.stdout, wait.stderr);
    let job = home.job(&fixture.id());
    assert_eq!((state(&job), job["error"]["code"].as_str()), ("error", Some("storage_error")), "{job:#}");
    home.ok(&["daemon", "stop"]);
    home.ok(&["resume", &fixture.short()]);
    let wait = home.run(&["wait", &fixture.short(), "--timeout", "60s"]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(fixture.intact(&home, "a.bin"), "a.bin matches its source");
    assert!(fixture.intact(&home, "b.bin"), "b.bin pieces that passed their check but failed to write came again");
}

#[test]
fn a_check_that_cannot_read_a_file_fails_and_keeps_what_was_verified() {
    let fixture = Fixture::new("set", &SET, 15);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &[]);
    assert_eq!(home.run(&["wait", &fixture.short()]).code, 0);
    let Some(locked) = Locked::new(fixture.downloaded(&home, "a.bin"), 0o000) else { return };
    assert_eq!(home.ok(&["verify", &fixture.short()])["changes"][0]["result"], "checking");
    let job = home.until(&fixture.id(), "the check stops", |j| state(j) != "checking");
    drop(locked);
    assert_eq!(job["error"]["code"], "storage_error", "{job:#}");
    home.ok(&["daemon", "stop"]);
    assert_eq!(verified(&home.job(&fixture.id())), fixture.total(), "a check that stopped took nothing back");
    home.ok(&["verify", &fixture.short()]);
    let job = home.until(&fixture.id(), "the check passes", |j| state(j) != "checking");
    assert_eq!(state(&job), "done", "{job:#}");
}

#[test]
fn a_download_whose_only_seeder_leaves_keeps_waiting() {
    let fixture = Fixture::new("big", &BIG, 12);
    let seeder = Seeder::start(&fixture, SLOW);
    let mut home = Home::new(&seeder);
    home.stall_after = Some("2s");
    home.get(&fixture.torrent_file(), &[]);
    home.until(&fixture.id(), "a third is verified", |j| verified(j) >= fixture.total() / 3);
    drop(seeder);
    let wait = home.run(&["wait", &fixture.short(), "--timeout", "8s"]);
    assert_eq!(wait.code, 8, "with nothing connected there is nothing to judge: {}{}", wait.stdout, wait.stderr);
    assert_eq!(state(&home.job(&fixture.id())), "downloading");
}

#[test]
fn verify_reports_a_corrupted_byte_and_repair_fixes_it() {
    let fixture = Fixture::new("set", &SET, 6);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &[]);
    assert_eq!(home.run(&["wait", &fixture.short()]).code, 0);
    let damaged = fixture.downloaded(&home, "a.bin");
    let offset = 3 * u64::from(PIECE) + 100;
    let mut file = OpenOptions::new().write(true).open(&damaged).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&[!fixture.files["a.bin"][offset as usize]]).unwrap();
    drop(file);

    assert_eq!(home.ok(&["verify", &fixture.short()])["changes"][0]["result"], "checking");
    let job = home.until(&fixture.id(), "the check reports", |j| state(j) != "checking");
    assert_eq!(job["error"]["code"], "verification_failed", "{job:#}");
    assert_eq!(job["error"]["message"], "1 damaged piece in the selected files");
    assert_eq!(verified(&job), fixture.total() - u64::from(PIECE), "the damaged piece no longer counts");
    assert!(!fixture.intact(&home, "a.bin"), "verify alone repairs nothing");

    assert_eq!(home.ok(&["verify", &fixture.short(), "--repair"])["changes"][0]["result"], "checking");
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(fixture.intact(&home, "a.bin"), "repair downloaded the damaged piece again");
}

#[test]
fn a_failed_check_outlives_the_engine_and_resume_repairs_the_damage() {
    let fixture = Fixture::new("set", &SET, 11);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    home.get(&fixture.torrent_file(), &[]);
    assert_eq!(home.run(&["wait", &fixture.short()]).code, 0);
    let damaged = fixture.downloaded(&home, "a.bin");
    let offset = 3 * u64::from(PIECE) + 100;
    let mut file = OpenOptions::new().write(true).open(&damaged).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&[!fixture.files["a.bin"][offset as usize]]).unwrap();
    drop(file);
    assert_eq!(home.ok(&["verify", &fixture.short()])["changes"][0]["result"], "checking");
    home.until(&fixture.id(), "the check reports", |j| state(j) == "error");

    home.ok(&["daemon", "stop"]);
    let job = home.job(&fixture.id());
    let short = fixture.total() - u64::from(PIECE);
    assert_eq!((state(&job), verified(&job)), ("error", short), "the check outlived the engine: {job:#}");
    let files = home.ok(&["files", &fixture.short()]);
    let bytes: u64 = files["files"]["files"].as_array().unwrap().iter().map(|f| f["verified"].as_u64().unwrap()).sum();
    assert_eq!(bytes, short, "the files hold what the check verified");
    assert_eq!(home.ok(&["resume", &fixture.short()])["changes"][0]["result"], "resumed");
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(fixture.intact(&home, "a.bin"), "resume downloaded the damaged piece again");
}

#[test]
fn wait_exits_8_on_a_short_timeout_and_0_when_done() {
    let fixture = Fixture::new("big", &BIG, 7);
    let seeder = Seeder::start(&fixture, SLOW);
    let home = Home::new(&seeder);
    home.get(&fixture.magnet(), &[]);
    let short = home.run(&["wait", &fixture.short(), "--timeout", "1s"]);
    assert_eq!(short.code, 8, "{}{}", short.stdout, short.stderr);
    let wait = home.run(&["wait", &fixture.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(state(&home.job(&fixture.id())), "done");
}

#[test]
fn a_symbolic_link_inside_the_download_folder_is_refused_and_nothing_lands_where_it_points() {
    let fixture = Fixture::new("set", &SET, 16);
    let seeder = Seeder::start(&fixture, None);
    let home = Home::new(&seeder);
    let outside = home.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(home.downloads()).unwrap();
    let link = home.downloads().join("set");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let dir = home.downloads().display().to_string();
    let get = home.run(&["get", &fixture.torrent_file(), "--dir", &dir, "--json"]);
    std::thread::sleep(Duration::from_secs(2));
    let landed: Vec<PathBuf> = snapshot(&outside).into_keys().collect();
    assert_eq!((get.code, landed), (4, Vec::new()), "{}{}", get.stdout, get.stderr);
    let error = &serde_json::from_str::<Value>(&get.stdout).unwrap()["error"];
    assert_eq!(error["code"], "path_conflict");
    assert_eq!(
        error["message"],
        format!("{} is a symbolic link, and torrential writes only inside {dir}", link.display())
    );
    assert_eq!(error["hint"], "remove the link, or choose another folder with --dir");
    assert_eq!(home.ok(&["status"])["jobs"], Value::Array(Vec::new()));
}

#[test]
fn a_download_to_files_a_queued_download_will_write_is_refused() {
    let first = Fixture::new("pack", &[("same.bin", 300_000)], 17);
    let second = Fixture::new("pack", &[("same.bin", 300_000)], 18);
    let (a, b) = (Seeder::start(&first, None), Seeder::start(&second, None));
    let home = Home::seeded_by(&[&a, &b]);
    let blockers: Vec<String> = (1..=3u8).map(|n| hex(&[n; 20])).collect();
    for hash in &blockers {
        assert_eq!(home.get(&format!("magnet:?xt=urn:btih:{hash}"), &[])["reply"]["outcome"], "started");
    }
    assert_eq!(home.get(&first.torrent_file(), &[])["reply"]["outcome"], "queued");
    let dir = home.downloads().display().to_string();
    let refused = home.run(&["get", &second.torrent_file(), "--dir", &dir, "--json"]);
    for hash in &blockers {
        home.ok(&["rm", &hash[..6]]);
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let jobs = loop {
        let jobs = home.ok(&["status"])["jobs"].as_array().unwrap().clone();
        if jobs.iter().all(|j| state(j) == "done") || Instant::now() > deadline {
            break jobs;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let finished: Vec<(&str, &str)> = jobs.iter().map(|j| (j["id"].as_str().unwrap(), state(j))).collect();
    assert_eq!(
        (refused.code, finished, first.intact(&home, "same.bin")),
        (4, vec![(first.id().as_str(), "done")], true),
        "{}{}",
        refused.stdout,
        refused.stderr
    );
    let error = &serde_json::from_str::<Value>(&refused.stdout).unwrap()["error"];
    assert_eq!(error["code"], "path_conflict");
    assert_eq!(
        error["message"],
        format!("1 file under {dir}/pack is already a file of download {} (pack)", first.short())
    );
    assert_eq!(
        error["hint"],
        format!("choose another folder with --dir, or remove {0} with torrential rm {0}", first.short())
    );
}
