use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use torrential_core::protocol::{Change, FilesReply, GetOutcome, GetReply, GetRequest, Request, Response};
use torrential_core::{Error, ErrorCode, InfoHash, Job, JobStatus, Selection, Target};

use super::admission::{FakeDisk, GIB, Space};
use super::engine_port::{
    AddTorrent, EngineEvent, EnginePort, Fake, FakeTorrentFile, MetaFile, Metainfo, ParseTorrent, Swarm, disagreement,
    parse_fake,
};
use super::owner::OwnerConfig;
use super::search_port::{self, fake_catalog};
use super::store::Store;
use super::{Ports, Running, server, start};

const ROOMY: Space = Space { device: 1, total: 1000 * GIB, free: 500 * GIB };
const GIB_PIECE: u64 = GIB / 1024;
const STALL: Duration = Duration::from_secs(1);
const WRITES_STOP_WITHIN: Duration = Duration::from_secs(1);

struct Harness {
    dir: tempfile::TempDir,
    running: Running,
    swarm: Swarm,
    disk: FakeDisk,
    search: search_port::Fake,
}

fn config(dir: &std::path::Path) -> OwnerConfig {
    OwnerConfig {
        default_dir: dir.join("downloads"),
        probe_dir: dir.join("probes"),
        max_active: super::MAX_ACTIVE,
        idle_timeout: None,
        resume_on_start: false,
        save_resume_every: Duration::from_millis(100),
        stall_after: STALL,
        writes_stop_within: WRITES_STOP_WITHIN,
    }
}

impl Harness {
    fn new() -> Self {
        Self::with_catalog(fake_catalog())
    }

    fn with_catalog(catalog: Vec<torrential_core::Release>) -> Self {
        Self::with_search(search_port::Fake::new(catalog))
    }

    fn with_search(search: search_port::Fake) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (swarm, disk) = (Swarm::new(0), FakeDisk::new(ROOMY));
        let running = Self::boot(&dir, Box::new(Fake::new(swarm.clone())), parse_fake, &disk, &search);
        Self { dir, running, swarm, disk, search }
    }

    fn with_engine(engine: impl FnOnce(&std::path::Path) -> Box<dyn EnginePort>, parse: ParseTorrent) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (swarm, disk, search) = (Swarm::new(0), FakeDisk::new(ROOMY), search_port::Fake::new(fake_catalog()));
        let running = Self::boot(&dir, engine(dir.path()), parse, &disk, &search);
        Self { dir, running, swarm, disk, search }
    }

    fn seeded() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("state.db")).unwrap();
        super::fixture::seed(&mut store, &dir.path().join("downloads"), 0).unwrap();
        drop(store);
        let (swarm, disk, search) = (Swarm::new(0), FakeDisk::new(ROOMY), search_port::Fake::new(fake_catalog()));
        let running = Self::boot(&dir, Box::new(Fake::new(swarm.clone())), parse_fake, &disk, &search);
        Self { dir, running, swarm, disk, search }
    }

    fn boot(
        dir: &tempfile::TempDir,
        engine: Box<dyn EnginePort>,
        parse: ParseTorrent,
        disk: &FakeDisk,
        search: &search_port::Fake,
    ) -> Running {
        let ports = Ports {
            engine,
            parse,
            search: Arc::new(search.clone()),
            disk: Box::new(disk.clone()),
            machine: crate::machine::Detector::detected(
                torrential_core::Machine {
                    os: torrential_core::Os::Linux,
                    display: None,
                    arch: Some(torrential_core::Arch::X86_64),
                    language: None,
                },
                dir.path().join("downloads"),
                Box::new(disk.clone()),
            ),
        };
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        start(&tokio::runtime::Handle::current(), store, ports, config(dir.path()), "0.1.0".into()).unwrap()
    }

    async fn crash_and_restart(self) -> Self {
        let Self { dir, running, swarm, disk, search } = self;
        running.owner.abort();
        let _ = running.owner.await;
        drop(running.ctx);
        let running = Self::boot(&dir, Box::new(Fake::new(swarm.clone())), parse_fake, &disk, &search);
        Self { dir, running, swarm, disk, search }
    }

    async fn ask(&self, request: Request) -> Response {
        server::handle(&self.running.ctx, request).await
    }

    async fn get(&self, target: Target, files: &[&str]) -> Result<GetReply, Error> {
        self.get_with(GetRequest {
            target,
            dir: Some(self.dir.path().join("downloads")),
            files: files.iter().map(|f| f.to_string()).collect(),
            max_size: None,
            setup_max_size: None,
            dry_run: false,
        })
        .await
    }

    async fn get_with(&self, request: GetRequest) -> Result<GetReply, Error> {
        match self.ask(Request::Get(request)).await {
            Response::Get { reply } => Ok(*reply),
            Response::Error { error } => Err(error),
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn changes(&self, request: Request) -> Result<Vec<(String, String)>, Error> {
        match self.ask(request).await {
            Response::Changed { changes } => {
                Ok(changes.into_iter().map(|c: Change| (c.id[..6].to_string(), c.result)).collect())
            }
            Response::Error { error } => Err(error),
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn status(&self) -> Vec<Job> {
        match self.ask(Request::Status { refs: Vec::new() }).await {
            Response::Jobs { jobs } => jobs,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn states(&self) -> Vec<&'static str> {
        self.status().await.iter().map(|j| j.status.name()).collect()
    }

    async fn until(&self, what: &str, done: impl Fn(&[Job]) -> bool) -> Vec<Job> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let jobs = self.status().await;
            if done(&jobs) {
                return jobs;
            }
            assert!(Instant::now() < deadline, "timed out waiting until {what}: {jobs:#?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn files(&self, prefix: &str) -> FilesReply {
        match self.ask(Request::Files { target: Target::Prefix(prefix.into()), timeout_ms: 1_000 }).await {
            Response::Files { files } => files,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn file_bytes(&self, t: &FakeTorrentFile) -> Vec<u64> {
        self.files(&prefix(t)).await.files.iter().map(|f| f.verified).collect()
    }

    fn torrent_file(&self, torrent: &FakeTorrentFile) -> Target {
        let path = self.dir.path().join(format!("{}.torrent", torrent.name));
        std::fs::write(&path, torrent.bytes()).unwrap();
        Target::TorrentFile(path)
    }

    fn downloads(&self) -> PathBuf {
        self.dir.path().join("downloads")
    }
}

#[derive(Clone, Default)]
struct Gate(Arc<Mutex<(bool, Vec<EngineEvent>)>>);

impl Gate {
    fn close(&self) {
        self.0.lock().unwrap().0 = true;
    }

    fn open(&self) {
        self.0.lock().unwrap().0 = false;
    }

    fn held(&self) -> usize {
        self.0.lock().unwrap().1.len()
    }

    async fn until_held(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.held() < n {
            assert!(Instant::now() < deadline, "the engine never paused");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct Gated<P> {
    port: P,
    gate: Gate,
}

impl<P: EnginePort> EnginePort for Gated<P> {
    fn add(&mut self, torrent: AddTorrent) -> Result<(), Error> {
        self.port.add(torrent)
    }

    fn select(&mut self, id: torrential_core::TorrentId, wanted: &[bool]) -> Result<(), Error> {
        self.port.select(id, wanted)
    }

    fn pause(&mut self, id: torrential_core::TorrentId) -> Result<(), Error> {
        self.port.pause(id)
    }

    fn resume(&mut self, id: torrential_core::TorrentId) -> Result<(), Error> {
        self.port.resume(id)
    }

    fn recheck(&mut self, id: torrential_core::TorrentId) -> Result<(), Error> {
        self.port.recheck(id)
    }

    fn save_resume_data(&mut self, id: torrential_core::TorrentId) -> Result<(), Error> {
        self.port.save_resume_data(id)
    }

    fn save_resume_data_if_progressed(&mut self, id: torrential_core::TorrentId) -> Result<(), Error> {
        self.port.save_resume_data_if_progressed(id)
    }

    fn remove(&mut self, id: torrential_core::TorrentId) -> Result<(), Error> {
        self.port.remove(id)
    }

    fn file_progress(&mut self, id: torrential_core::TorrentId) -> Result<Vec<u64>, Error> {
        self.port.file_progress(id)
    }

    fn poll(&mut self, max_wait: Duration) -> Vec<EngineEvent> {
        let events = self.port.poll(max_wait);
        let (closed, held) = &mut *self.gate.0.lock().unwrap();
        if !*closed {
            return held.drain(..).chain(events).collect();
        }
        let (paused, others): (Vec<_>, Vec<_>) =
            events.into_iter().partition(|e| matches!(e, EngineEvent::Paused { .. }));
        held.extend(paused);
        others
    }
}

fn torrent(n: u8, name: &str, files: &[(&str, u64)]) -> FakeTorrentFile {
    FakeTorrentFile {
        info_hash: InfoHash::V1([n; 20]),
        name: name.into(),
        files: files.iter().map(|(p, s)| (p.to_string(), *s)).collect(),
    }
}

fn magnet(torrent: &FakeTorrentFile) -> Target {
    Target::Magnet(format!("magnet:?xt=urn:btih:{}&dn={}", torrent.info_hash.id().to_hex(), torrent.name))
}

fn prefix(t: &FakeTorrentFile) -> String {
    t.info_hash.id().to_hex()[..6].to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_the_intent_commit_leaves_one_interrupted_download() {
    let h = Harness::new();
    let film = torrent(1, "film", &[("film.mkv", GIB)]);
    let reply = h.get(magnet(&film), &[]).await.unwrap();
    assert_eq!(reply.outcome, GetOutcome::Started);
    let h = h.crash_and_restart().await;
    let jobs = h.status().await;
    assert_eq!(jobs.len(), 1);
    assert_eq!((jobs[0].id, jobs[0].status.name()), (film.info_hash.id(), "interrupted"));
    assert_eq!(
        h.changes(Request::Resume { refs: vec![prefix(&film)], all: false }).await.unwrap(),
        [("010101".into(), "resumed".into())]
    );
    assert_eq!(h.states().await, ["metadata"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_get_brings_back_an_interrupted_download() {
    let h = Harness::new();
    let film = torrent(2, "film", &[("film.mkv", GIB)]);
    h.get(magnet(&film), &[]).await.unwrap();
    let h = h.crash_and_restart().await;
    assert_eq!(h.states().await, ["interrupted"]);
    assert_eq!(h.get(magnet(&film), &[]).await.unwrap().outcome, GetOutcome::Existing);
    assert_eq!(h.states().await, ["metadata"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_download_is_done_and_stays_done_after_a_restart() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let iso = torrent(2, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    let jobs = h.until("done", |jobs| jobs[0].status.name() == "done").await;
    assert_eq!(jobs[0].progress.verified, GIB);
    assert_eq!(jobs[0].path, Some(h.downloads().join("arch.iso")));
    let h = h.crash_and_restart().await;
    assert_eq!(h.states().await, ["done"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_is_idempotent_and_reports_a_changed_selection() {
    let h = Harness::new();
    let film = torrent(3, "film", &[("film.mkv", GIB), ("film.srt", 50_000), ("extras.mkv", GIB)]);
    h.swarm.publish(film.clone());
    assert_eq!(h.get(magnet(&film), &[]).await.unwrap().outcome, GetOutcome::Started);
    h.until("metadata arrived", |jobs| jobs[0].progress.size.is_some()).await;
    let again = h.get(magnet(&film), &[]).await.unwrap();
    assert_eq!(again.outcome, GetOutcome::Existing);
    let updated = h.get(Target::Prefix(prefix(&film)), &["film.*"]).await.unwrap();
    assert_eq!(updated.outcome, GetOutcome::Updated);
    assert_eq!(updated.previous_selection, Some(Selection::All));
    assert_eq!(updated.job.selection, Selection::Patterns(vec!["film.*".into()]));
    assert_eq!(updated.job.progress.size, Some(GIB + 50_000));
    assert_eq!(h.status().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_and_rm_are_idempotent_and_rm_keeps_the_path() {
    let h = Harness::new();
    let iso = torrent(4, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    let id = prefix(&iso);
    let pause = || Request::Pause { refs: vec![id.clone()] };
    assert_eq!(h.changes(pause()).await.unwrap(), [(id.clone(), "paused".into())]);
    assert_eq!(h.changes(pause()).await.unwrap(), [(id.clone(), "already_paused".into())]);
    assert_eq!(h.states().await, ["paused"]);
    let rm = || Request::Remove { refs: vec![id.clone()] };
    let Response::Changed { changes } = h.ask(rm()).await else { panic!("expected changes") };
    assert_eq!(
        (changes[0].result.as_str(), changes[0].path.clone()),
        ("removed", Some(h.downloads().join("arch.iso")))
    );
    let Response::Changed { changes } = h.ask(rm()).await else { panic!("expected changes") };
    assert_eq!(
        (changes[0].result.as_str(), changes[0].path.clone()),
        ("already_removed", Some(h.downloads().join("arch.iso")))
    );
    assert!(h.status().await.is_empty());
    let unknown = h.changes(Request::Remove { refs: vec!["abcdef".into()] }).await.unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ambiguous_prefix_lists_the_candidates() {
    let mut catalog = fake_catalog();
    let mut twin = catalog[1].clone();
    twin.id = torrential_core::ReleaseId::Torrent("e3a1c4aaaa000000000000000000000000000000".parse().unwrap());
    twin.name = "Ubuntu 24.04.5 desktop (repack)".into();
    catalog.push(twin);
    let h = Harness::with_catalog(catalog);
    let Response::Search { result } =
        h.ask(Request::Search { query: torrential_core::SearchQuery::new("ubuntu"), stream: false }).await
    else {
        panic!("expected results")
    };
    assert_eq!(result.releases.len(), 2);
    let err = h.get(Target::Prefix("e3a1".into()), &[]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousRef);
    let Some(torrential_core::error::Details::Candidates(candidates)) = err.details else {
        panic!("expected candidates")
    };
    let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["e3a1f0", "e3a1c4"]);
    assert_eq!(h.get(Target::Prefix("e3a1f0".into()), &[]).await.unwrap().outcome, GetOutcome::Started);
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_size_and_conflicting_directories_refuse_a_download() {
    let h = Harness::new();
    let big = torrent(5, "big", &[("a.bin", 40 * GIB), ("b.bin", 20 * GIB)]);
    let err = h.get(h.torrent_file(&big), &[]).await.unwrap_err();
    assert_eq!(
        (err.code, err.message.as_str()),
        (ErrorCode::TooLarge, "the selection is 60.0 GiB, over the 50 GiB cap")
    );
    let raised = GetRequest { max_size: Some(60 * GIB), ..request(&h, h.torrent_file(&big)) };
    h.disk.set_free(69 * GIB);
    let err = h.get_with(raised.clone()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::DiskFull);
    assert_eq!(err.details, Some(torrential_core::error::Details::Space { required: 70 * GIB, available: 69 * GIB }));
    h.disk.set_free(500 * GIB);
    h.disk.directory(h.downloads().join("big/b.bin"));
    let err = h.get_with(raised.clone()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PathConflict);
    assert_eq!(
        err.details,
        Some(torrential_core::error::Details::Paths(vec![h.downloads().join("big/b.bin").display().to_string()]))
    );
    assert!(h.status().await.is_empty());
    let only_a = GetRequest { files: vec!["a.bin".into()], ..raised };
    assert_eq!(h.get_with(only_a).await.unwrap().outcome, GetOutcome::Started);
}

#[tokio::test(flavor = "multi_thread")]
async fn existing_files_need_only_unallocated_space_and_are_not_assumed_verified() {
    let h = Harness::new();
    h.disk.set_free(12 * GIB);
    let first = torrent(52, "first", &[("a.bin", 10 * GIB)]);
    h.disk.file(h.downloads().join("first/a.bin"), 9 * GIB);
    let reply = h.get(h.torrent_file(&first), &[]).await.unwrap();
    assert_eq!(reply.outcome, GetOutcome::Started);
    assert_eq!(reply.job.progress.verified, 0, "only the engine's hash check can verify data");

    // The first download needs 1 GiB, not another 10 GiB. The second learns
    // its files from a magnet, exercising admission after metadata arrives too.
    let second = torrent(53, "second", &[("b.bin", 10 * GIB)]);
    h.disk.file(h.downloads().join("second/b.bin"), 9 * GIB);
    h.swarm.publish(second.clone());
    h.get(magnet(&second), &[]).await.unwrap();
    let jobs = h
        .until("both downloads are admitted", |jobs| {
            jobs.iter().all(|j| j.status.name() == "downloading" || j.status.name() == "error")
        })
        .await;
    assert!(jobs.iter().all(|j| j.status.name() == "downloading"), "{jobs:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_symbolic_link_stops_a_download_when_its_file_list_arrives_or_before_it_starts() {
    let h = Harness::new();
    let film = torrent(44, "film", &[("film.mkv", GIB)]);
    h.swarm.publish(film.clone());
    h.disk.link(h.downloads().join("film"));
    h.get(magnet(&film), &[]).await.unwrap();
    let jobs = h.until("the file list arrived", |jobs| jobs[0].status.name() == "error").await;
    let JobStatus::Error { error } = &jobs[0].status else { unreachable!() };
    assert_eq!(
        error.message,
        format!(
            "{} is a symbolic link, and torrential writes only inside {}",
            h.downloads().join("film").display(),
            h.downloads().display()
        )
    );
    h.changes(Request::Remove { refs: vec![prefix(&film)] }).await.unwrap();

    let busy: Vec<FakeTorrentFile> = (45..48).map(|n| torrent(n, &format!("busy{n}"), &[("f", GIB)])).collect();
    for t in &busy {
        h.get(h.torrent_file(t), &[]).await.unwrap();
    }
    let set = torrent(48, "set", &[("a.bin", GIB), ("docs/b.bin", GIB)]);
    assert_eq!(h.get(h.torrent_file(&set), &[]).await.unwrap().outcome, GetOutcome::Queued);
    h.disk.link(h.downloads().join("set/docs"));
    h.changes(Request::Remove { refs: vec![prefix(&busy[0])] }).await.unwrap();
    let jobs = h.until("the queued download stopped", |jobs| jobs[2].status.name() != "queued").await;
    let JobStatus::Error { error } = &jobs[2].status else { panic!("{jobs:#?}") };
    assert_eq!(
        (error.code, error.message.clone(), error.hint.as_deref()),
        (
            ErrorCode::PathConflict,
            format!(
                "{} is a symbolic link, and torrential writes only inside {}",
                h.downloads().join("set/docs").display(),
                h.downloads().display()
            ),
            Some("remove the link, then run torrential resume 303030")
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_cannot_take_files_another_download_claims_whatever_their_case() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let first = torrent(50, "pack", &[("same.bin", GIB)]);
    h.swarm.withhold(first.info_hash.id(), 2);
    h.get(h.torrent_file(&first), &[]).await.unwrap();
    h.until("the first stopped", |jobs| jobs[0].status.name() == "error").await;
    let second = torrent(51, "Pack", &[("same.bin", GIB)]);
    assert_eq!(h.get(h.torrent_file(&second), &[]).await.unwrap().outcome, GetOutcome::Started);
    let third = torrent(52, "PACK", &[("same.bin", GIB)]);
    let err = h.get(h.torrent_file(&third), &[]).await.unwrap_err();
    let downloads = h.downloads().display().to_string();
    assert_eq!(
        (err.code, err.message, err.hint.as_deref()),
        (
            ErrorCode::PathConflict,
            format!("1 file under {downloads}/PACK is already a file of download 333333 (Pack)"),
            Some("choose another folder with --dir, or remove 333333 with torrential rm 333333")
        )
    );

    h.swarm.withhold(first.info_hash.id(), 0);
    h.changes(Request::Resume { refs: vec![prefix(&first)], all: false }).await.unwrap();
    let stopped_again =
        |jobs: &[Job]| matches!(&jobs[0].status, JobStatus::Error { error } if error.code == ErrorCode::PathConflict);
    let jobs = h.until("the first stopped again", stopped_again).await;
    let JobStatus::Error { error } = &jobs[0].status else { unreachable!() };
    assert_eq!(
        (error.message.as_str(), error.hint.as_deref()),
        (
            format!("1 file under {downloads}/pack is already a file of download 333333 (Pack)").as_str(),
            Some("remove 333333 with torrential rm 333333, then run torrential resume 323232")
        )
    );
}

fn request(h: &Harness, target: Target) -> GetRequest {
    GetRequest {
        target,
        dir: Some(h.downloads()),
        files: Vec::new(),
        max_size: None,
        setup_max_size: None,
        dry_run: false,
    }
}

async fn over_its_setup_cap(h: &Harness, n: u8) -> FakeTorrentFile {
    let film = torrent(n, &format!("film{n}"), &[("film.mkv", 2 * GIB), ("extras.mkv", GIB)]);
    h.swarm.publish(film.clone());
    h.get_with(GetRequest { setup_max_size: Some(2 * GIB), ..request(h, magnet(&film)) }).await.unwrap();
    let id = film.info_hash.id();
    h.until("stopped", |jobs| jobs.iter().any(|j| j.id == id && j.status.name() == "error")).await;
    film
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cap_from_setup_is_named_when_the_file_list_arrives_over_it() {
    let h = Harness::new();
    over_its_setup_cap(&h, 41).await;
    let jobs = h.status().await;
    let JobStatus::Error { error } = &jobs[0].status else { panic!("expected an error, got {jobs:?}") };
    assert_eq!(
        (error.code, error.message.as_str(), error.hint.as_deref()),
        (
            ErrorCode::TooLarge,
            "the selection is 3.0 GiB, over the 2 GiB cap from torrential setup",
            Some("torrential get 292929 --max-size 3GiB allows it, or pick fewer files with --files")
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_runs_a_download_stopped_over_its_cap_once_it_fits() {
    let h = Harness::new();
    let film = over_its_setup_cap(&h, 41).await;
    let show = over_its_setup_cap(&h, 43).await;

    let again = h.get(Target::Prefix(prefix(&film)), &[]).await.unwrap_err();
    let fewer = h.get(Target::Prefix(prefix(&film)), &["film.mkv"]).await.unwrap();
    let raised = GetRequest { max_size: Some(4 * GIB), ..request(&h, Target::Prefix(prefix(&show))) };
    let started = h.get_with(raised.clone()).await.unwrap();
    let repeated = h.get_with(raised).await.unwrap();

    assert_eq!(again.code, ErrorCode::TooLarge);
    assert_eq!(
        (fewer.outcome, fewer.job.status.name(), fewer.plan.map(|p| p.selected_size)),
        (GetOutcome::Updated, "checking", Some(2 * GIB))
    );
    assert!(matches!(started.outcome, GetOutcome::Started | GetOutcome::Queued), "{:?}", started.outcome);
    assert_eq!(repeated.outcome, GetOutcome::Existing);
    let id = show.info_hash.id();
    h.until("the show runs", |jobs| jobs.iter().any(|j| j.id == id && j.status.name() == "checking")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_leaves_a_download_over_its_cap_stopped() {
    let h = Harness::new();
    let film = over_its_setup_cap(&h, 41).await;
    let iso = torrent(42, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.changes(Request::Pause { refs: vec![prefix(&iso)] }).await.unwrap();

    let one = h.changes(Request::Resume { refs: vec![prefix(&film)], all: false }).await.unwrap_err();
    let all = h.changes(Request::Resume { refs: Vec::new(), all: true }).await.unwrap();

    assert_eq!(
        (one.code, one.hint.as_deref()),
        (
            ErrorCode::TooLarge,
            Some("torrential get 292929 --max-size 3GiB allows it, or pick fewer files with --files")
        )
    );
    assert_eq!(all, [("2a2a2a".to_string(), "resumed".to_string())]);
    assert_eq!(h.status().await[0].status.name(), "error");
}

#[tokio::test(flavor = "multi_thread")]
async fn repair_leaves_a_download_over_its_cap_stopped() {
    let h = Harness::new();
    let film = over_its_setup_cap(&h, 41).await;

    let err = h.changes(Request::Verify { reference: prefix(&film), repair: true }).await.unwrap_err();

    assert_eq!(
        (err.code, err.hint.as_deref()),
        (
            ErrorCode::TooLarge,
            Some("torrential get 292929 --max-size 3GiB allows it, or pick fewer files with --files")
        )
    );
    assert_eq!(h.status().await[0].status.name(), "error");
}

#[tokio::test(flavor = "multi_thread")]
async fn other_downloads_on_the_same_disk_count_against_free_space() {
    let h = Harness::new();
    h.disk.set_free(90 * GIB);
    let first = torrent(6, "first", &[("x", 45 * GIB)]);
    let second = torrent(7, "second", &[("y", 45 * GIB)]);
    h.get(h.torrent_file(&first), &[]).await.unwrap();
    let err = h.get(h.torrent_file(&second), &[]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::DiskFull);
    assert!(err.message.contains("plus 45.0 GiB for other downloads"), "{}", err.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn three_downloads_run_at_once_and_the_rest_queue() {
    let h = Harness::new();
    let torrents: Vec<FakeTorrentFile> = (10..14).map(|n| torrent(n, &format!("t{n}"), &[("f", GIB)])).collect();
    let mut outcomes = Vec::new();
    for t in &torrents {
        outcomes.push(h.get(h.torrent_file(t), &[]).await.unwrap().outcome);
    }
    assert_eq!(outcomes, [GetOutcome::Started, GetOutcome::Started, GetOutcome::Started, GetOutcome::Queued]);
    h.until("three downloading", |jobs| jobs.iter().filter(|j| j.status.name() == "downloading").count() == 3).await;
    assert_eq!(h.states().await[3], "queued");
    h.changes(Request::Pause { refs: vec![prefix(&torrents[0])] }).await.unwrap();
    let jobs = h.until("the queued one started", |jobs| jobs[3].status.name() == "downloading").await;
    assert_eq!(
        jobs.iter().map(|j| j.status.name()).collect::<Vec<_>>(),
        ["paused", "downloading", "downloading", "downloading"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_gets_of_one_torrent_make_one_download() {
    let h = Harness::new();
    let film = torrent(20, "film", &[("film.mkv", GIB)]);
    let (a, b) = tokio::join!(h.get(magnet(&film), &[]), h.get(magnet(&film), &[]));
    let mut outcomes = [a.unwrap().outcome, b.unwrap().outcome];
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(outcomes, [GetOutcome::Existing, GetOutcome::Started]);
    assert_eq!(h.status().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn both_hashes_of_a_hybrid_torrent_name_one_download() {
    let h = Harness::new();
    let v2 = [0x77; 32];
    let hybrid = FakeTorrentFile {
        info_hash: InfoHash::Hybrid { v1: [0x21; 20], v2 },
        name: "hybrid".into(),
        files: vec![("hybrid".into(), GIB)],
    };
    h.swarm.publish(hybrid.clone());
    h.get(magnet(&hybrid), &[]).await.unwrap();
    h.until("metadata arrived", |jobs| jobs[0].progress.size.is_some()).await;
    let by_v2 = Target::Magnet(format!("magnet:?xt=urn:btmh:1220{}", "77".repeat(32)));
    assert_eq!(h.get(by_v2, &[]).await.unwrap().outcome, GetOutcome::Existing);
    assert_eq!(h.status().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_downloads_that_turn_out_to_be_one_hybrid_torrent_converge() {
    let h = Harness::new();
    let hybrid = FakeTorrentFile {
        info_hash: InfoHash::Hybrid { v1: [0x31; 20], v2: [0x32; 32] },
        name: "hybrid".into(),
        files: vec![("hybrid".into(), GIB)],
    };
    let by_v2 = Target::Magnet(format!("magnet:?xt=urn:btmh:1220{}", "32".repeat(32)));
    let original = h.get(by_v2, &[]).await.unwrap().job.id;
    h.get(Target::Magnet(format!("magnet:?xt=urn:btih:{}", "31".repeat(20))), &[]).await.unwrap();
    assert_eq!(h.status().await.len(), 2);
    h.swarm.publish(hybrid);
    let jobs = h.until("one job is left", |jobs| jobs.len() == 1).await;
    assert_eq!(jobs[0].id, original);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pattern_matching_no_file_stops_the_download_when_metadata_arrives() {
    let h = Harness::new();
    let film = torrent(22, "film", &[("film.mp4", GIB)]);
    h.swarm.publish(film.clone());
    h.get(magnet(&film), &["*.mkv"]).await.unwrap();
    let jobs = h.until("stopped", |jobs| jobs[0].status.name() == "error").await;
    let JobStatus::Error { error } = &jobs[0].status else { unreachable!() };
    assert_eq!((error.code, error.message.as_str()), (ErrorCode::NoSelection, "no file in film matches \"*.mkv\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn files_fetch_metadata_from_peers_and_get_reuses_it() {
    let h = Harness::new();
    let film = torrent(23, "film", &[("film.mkv", GIB), ("setup.exe", 1000)]);
    h.swarm.publish(film.clone());
    let Response::Files { files } = h.ask(Request::Files { target: magnet(&film), timeout_ms: 5_000 }).await else {
        panic!("expected files")
    };
    assert_eq!(files.metadata_source, "peers");
    let paths: Vec<(&str, bool)> = files.files.iter().map(|f| (f.path.as_str(), f.risk.is_some())).collect();
    assert_eq!(paths, [("film.mkv", false), ("setup.exe", true)]);
    let dry = GetRequest { files: vec!["*.mkv".into()], dry_run: true, ..request(&h, magnet(&film)) };
    let reply = h.get_with(dry).await.unwrap();
    assert_eq!(reply.outcome, GetOutcome::Planned);
    assert_eq!(reply.plan.map(|p| (p.selected_files, p.total_files, p.selected_size)), Some((1, 2, GIB)));
    assert!(h.status().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn files_without_peers_time_out() {
    let h = Harness::new();
    let lonely = torrent(24, "lonely", &[("x", 1)]);
    let response = h.ask(Request::Files { target: magnet(&lonely), timeout_ms: 200 }).await;
    let Response::Error { error } = response else { panic!("expected an error, got {response:?}") };
    assert_eq!(error.code, ErrorCode::MetadataTimeout);
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_reports_damage_and_repair_downloads_it_again() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let iso = torrent(25, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    h.swarm.damage(iso.info_hash.id(), 3);
    let verify = |repair| Request::Verify { reference: prefix(&iso), repair };
    assert_eq!(h.changes(verify(false)).await.unwrap(), [(prefix(&iso), "checking".into())]);
    let jobs = h.until("damage reported", |jobs| jobs[0].status.name() == "error").await;
    let JobStatus::Error { error } = &jobs[0].status else { unreachable!() };
    assert_eq!(
        (error.code, error.message.as_str()),
        (ErrorCode::VerificationFailed, "3 damaged pieces in the selected files")
    );
    h.swarm.damage(iso.info_hash.id(), 3);
    h.changes(verify(true)).await.unwrap();
    h.until("repaired", |jobs| jobs[0].status.name() == "done").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repair_counts_the_damaged_pieces_as_missing_until_they_download_again() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let iso = torrent(28, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    h.swarm.set_rate(0);
    h.swarm.damage(iso.info_hash.id(), 3);
    h.changes(Request::Verify { reference: prefix(&iso), repair: true }).await.unwrap();
    let jobs = h.until("repairing", |jobs| jobs[0].status.name() == "downloading").await;
    assert_eq!(jobs[0].progress.verified, GIB - 3 * GIB_PIECE);
    h.swarm.set_rate(100 * GIB);
    let jobs = h.until("repaired", |jobs| jobs[0].status.name() == "done").await;
    assert_eq!(jobs[0].progress.verified, GIB);
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_after_a_failed_verify_downloads_the_damaged_pieces_again() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let iso = torrent(29, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    h.swarm.set_rate(0);
    h.swarm.damage(iso.info_hash.id(), 2);
    h.changes(Request::Verify { reference: prefix(&iso), repair: false }).await.unwrap();
    let jobs = h.until("damage reported", |jobs| jobs[0].status.name() == "error").await;
    assert_eq!(jobs[0].progress.verified, GIB - 2 * GIB_PIECE);
    h.changes(Request::Resume { refs: vec![prefix(&iso)], all: false }).await.unwrap();
    let jobs = h.until("downloading", |jobs| jobs[0].status.name() == "downloading").await;
    assert_eq!(jobs[0].progress.verified, GIB - 2 * GIB_PIECE);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_keeps_the_progress_of_the_last_periodic_save() {
    let h = Harness::new();
    h.swarm.set_rate(GIB / 2);
    let iso = torrent(30, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.until("a quarter is verified", |jobs| jobs[0].progress.verified >= GIB / 4).await;
    h.swarm.set_rate(0);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = h.status().await[0].progress.verified;
    let h = h.crash_and_restart().await;
    let jobs = h.status().await;
    assert_eq!((jobs[0].status.name(), jobs[0].progress.verified), ("interrupted", before));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_keeps_the_piece_map_of_the_last_save_and_done_drops_it() {
    let h = Harness::new();
    h.swarm.set_rate(GIB / 2);
    let set = torrent(31, "set", &[("a.bin", GIB), ("b.bin", GIB)]);
    h.get(h.torrent_file(&set), &["a.bin"]).await.unwrap();
    h.until("a quarter is verified", |jobs| jobs[0].progress.verified >= GIB / 4).await;
    h.swarm.set_rate(0);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let map = h.status().await[0].progress.piece_map.expect("a running download has a piece map").to_string();
    assert_eq!(map[32..], "-".repeat(32), "b.bin is not selected");
    let h = h.crash_and_restart().await;
    let jobs = h.status().await;
    assert_eq!((jobs[0].status.name(), jobs[0].progress.piece_map.map(|m| m.to_string())), ("interrupted", Some(map)));
    h.swarm.set_rate(100 * GIB);
    h.changes(Request::Resume { refs: vec![prefix(&set)], all: false }).await.unwrap();
    let jobs = h.until("done", |jobs| jobs[0].status.name() == "done").await;
    assert_eq!(jobs[0].progress.piece_map, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_downloads_files_agree_with_its_piece_map() {
    let h = Harness::new();
    h.swarm.set_rate(GIB / 2);
    let set = torrent(32, "set", &[("a.bin", GIB), ("b.bin", GIB / 3), ("c.bin", GIB / 5)]);
    h.get(h.torrent_file(&set), &["a.bin", "c.bin"]).await.unwrap();
    h.until("a third is verified", |jobs| jobs[0].progress.verified >= GIB / 3).await;
    h.swarm.set_rate(0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let progress = h.status().await[0].progress;
    let files = h.file_bytes(&set).await;
    let map = progress.piece_map.expect("a running download has a piece map");
    let wanted = [true, false, true];
    assert_eq!(disagreement(&set.metainfo().unwrap(), &wanted, progress.verified, &files, &map), None, "{map}");
    assert!(files[2] > 0, "c.bin, last in the torrent, gets pieces too: {files:?}");
}

fn assert_files_agree(job: &Job, reply: &FilesReply) {
    let list = reply.files.iter().map(|f| MetaFile { path: f.path.clone(), size: f.size }).collect();
    let metainfo = Metainfo::new(job.info_hash, reply.name.clone().unwrap(), list, false, Vec::new()).unwrap();
    let wanted: Vec<bool> = reply.files.iter().map(|f| f.selected).collect();
    let bytes: Vec<u64> = reply.files.iter().map(|f| f.verified).collect();
    let map = job.progress.piece_map.expect("an unfinished download has a piece map");
    let wrong = disagreement(&metainfo, &wanted, job.progress.verified, &bytes, &map);
    assert_eq!(wrong, None, "{:?} {} {map}", job.name, job.status.name());
}

#[tokio::test(flavor = "multi_thread")]
async fn each_unfinished_test_download_reports_file_bytes_that_match_its_progress() {
    let h = Harness::seeded();
    let mut checked = Vec::new();
    for job in h.status().await.iter().filter(|j| !matches!(j.status, JobStatus::Done { .. })) {
        assert_files_agree(job, &h.files(&job.id.to_hex()[..6]).await);
        checked.push((job.name.clone().unwrap(), job.status.name()));
    }
    let expected =
        [("stackexchange", "paused"), ("openwebtext-2019", "error"), ("wikipedia-en-2026-09-text", "interrupted")];
    assert_eq!(checked, expected.map(|(name, state)| (name.to_string(), state)));
    h.changes(Request::Resume { refs: vec!["6f2c81".into()], all: false }).await.unwrap();
    let jobs =
        h.until("wikipedia is in the engine", |jobs| jobs.iter().any(|j| j.status.name() == "downloading")).await;
    let running = jobs.iter().find(|j| j.status.name() == "downloading").unwrap();
    assert_files_agree(running, &h.files("6f2c81").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_download_keeps_its_file_bytes_across_a_restart() {
    let h = Harness::new();
    h.swarm.set_rate(GIB / 2);
    let set = torrent(33, "set", &[("a.bin", GIB), ("b.bin", GIB / 3), ("c.bin", GIB / 5)]);
    h.get(h.torrent_file(&set), &["a.bin", "c.bin"]).await.unwrap();
    h.until("a third is verified", |jobs| jobs[0].progress.verified >= GIB / 3).await;
    h.changes(Request::Pause { refs: vec![prefix(&set)] }).await.unwrap();
    let paused = h.file_bytes(&set).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let h = h.crash_and_restart().await;
    let job = h.status().await.remove(0);
    assert_eq!((job.status.name(), paused[0] + paused[2]), ("paused", job.progress.verified));
    assert_eq!(h.file_bytes(&set).await, paused, "the restarted engine reports the bytes it saved at the pause");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_whose_missing_pieces_have_no_source_stops_and_resume_looks_again() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let film = torrent(34, "film", &[("film.mp4", GIB), ("film.ogv", GIB)]);
    h.swarm.withhold(film.info_hash.id(), 2);
    h.get(h.torrent_file(&film), &["film.mp4"]).await.unwrap();
    let jobs = h.until("stopped", |jobs| jobs[0].status.name() == "error").await;
    let JobStatus::Error { error } = &jobs[0].status else { unreachable!() };
    let piece = 2 * GIB_PIECE;
    assert_eq!(jobs[0].progress.verified, GIB - 2 * piece);
    assert_eq!(
        (error.code, error.message.as_str(), error.hint.as_deref()),
        (
            ErrorCode::NoSource,
            "no peer or web seed has had the 2 pieces still missing (4.0 MiB) for 1s",
            Some("torrential resume 222222 looks for them again; the 1020.0 MiB verified stays")
        )
    );
    let verify = || h.changes(Request::Verify { reference: prefix(&film), repair: false });
    while verify().await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let jobs = h.until("checked", |jobs| jobs[0].status.name() == "error").await;
    let JobStatus::Error { error } = &jobs[0].status else { unreachable!() };
    assert_eq!(error.code, ErrorCode::NoSource, "pieces that never arrived are not damage");
    h.swarm.withhold(film.info_hash.id(), 0);
    h.changes(Request::Resume { refs: vec![prefix(&film)], all: false }).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repair_after_no_source_gives_its_sources_a_full_window_again() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let film = torrent(36, "film", &[("film.mp4", GIB)]);
    h.swarm.withhold(film.info_hash.id(), 2);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("stopped", |jobs| jobs[0].status.name() == "error").await;
    let repaired = Instant::now();
    let verify = || h.changes(Request::Verify { reference: prefix(&film), repair: true });
    while let Err(error) = verify().await {
        assert_eq!(error.code, ErrorCode::JobConflict, "only busy while its stop is saved");
        assert!(repaired.elapsed() < Duration::from_secs(5), "the stop was never saved");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    h.until("downloading again", |jobs| jobs[0].status.name() == "downloading").await;
    let jobs = h.until("stopped again", |jobs| jobs[0].status.name() == "error").await;
    let JobStatus::Error { error } = &jobs[0].status else { unreachable!() };
    assert_eq!(error.code, ErrorCode::NoSource);
    assert!(repaired.elapsed() >= STALL, "stopped again after {:?}", repaired.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_whose_save_fails_leaves_the_engine_with_the_error_that_stopped_it() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let film = torrent(37, "film", &[("film.mp4", GIB)]);
    let id = film.info_hash.id();
    h.swarm.withhold(id, 2);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("all but the withheld pieces", |jobs| jobs[0].progress.verified == GIB - 2 * GIB_PIECE).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.swarm.refuse_saves(id, Error::new(ErrorCode::StorageError, "libtorrent did not save its state: disk full"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.swarm.refused_saves(id) == 0 {
        assert!(Instant::now() < deadline, "the stop never tried to save");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let jobs = h.status().await;
    let JobStatus::Error { error } = &jobs[0].status else { panic!("{jobs:#?}") };
    assert_eq!((error.code, jobs[0].progress.verified), (ErrorCode::NoSource, GIB - 2 * GIB_PIECE));
    assert_eq!(h.swarm.refused_saves(id), 1, "it left the engine instead of saving again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_gets_nothing_from_peers_that_have_its_pieces_keeps_waiting() {
    let h = Harness::new();
    let film = torrent(35, "film", &[("film.mp4", GIB)]);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("downloading", |jobs| jobs[0].status.name() == "downloading").await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let jobs = h.status().await;
    assert_eq!((jobs[0].status.name(), jobs[0].progress.verified), ("downloading", 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_with_only_a_torrent_url_is_fetched_once_found() {
    let iso = torrent(27, "debian.iso", &[("debian.iso", GIB)]);
    let url = "https://cdimage.example/debian.iso.torrent";
    let row = torrential_core::Release {
        id: torrential_core::ReleaseId::Locator(torrential_core::LocatorId::from_bytes([0xab; 20])),
        name: "debian.iso".into(),
        magnet: None,
        torrent_url: Some(url.into()),
        ..fake_catalog().remove(0)
    };
    let h = Harness::with_catalog(vec![row]);
    h.search.serve_torrent(url, iso.bytes());
    let Response::Search { result } =
        h.ask(Request::Search { query: torrential_core::SearchQuery::new("debian"), stream: false }).await
    else {
        panic!("expected results")
    };
    assert_eq!(result.releases[0].id.to_ref(), format!("u{}", "ab".repeat(20)));
    let reply = h.get(Target::Prefix("uabab".into()), &[]).await.unwrap();
    assert_eq!((reply.job.id, reply.plan.map(|p| p.selected_size)), (iso.info_hash.id(), Some(GIB)));
    let Response::Files { files } =
        h.ask(Request::Files { target: Target::Prefix(prefix(&iso)), timeout_ms: 1_000 }).await
    else {
        panic!("expected files")
    };
    assert_eq!(files.metadata_source, "download");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_id_of_a_search_result_fetches_its_torrent_as_a_prefix_does() {
    let bunny = torrent(40, "BigBuckBunny_124", &[("BigBuckBunny_124.mp4", GIB), ("BigBuckBunny_124.ogv", GIB)]);
    let id = bunny.info_hash.id().to_hex();
    let url = "https://archive.org/download/BigBuckBunny_124/BigBuckBunny_124_archive.torrent";
    let row = torrential_core::Release {
        id: torrential_core::ReleaseId::Torrent(bunny.info_hash.id()),
        name: "Big Buck Bunny".into(),
        magnet: Some(format!("magnet:?xt=urn:btih:{id}&dn=Big%20Buck%20Bunny")),
        torrent_url: Some(url.into()),
        ..fake_catalog().remove(0)
    };
    let h = Harness::with_catalog(vec![row]);
    h.search.serve_torrent(url, bunny.bytes());
    let Response::Search { result } =
        h.ask(Request::Search { query: torrential_core::SearchQuery::new("bunny"), stream: false }).await
    else {
        panic!("expected results")
    };
    assert_eq!(result.releases[0].id.to_ref(), id);
    let full = Target::parse(&id, std::path::Path::new("/")).unwrap();
    let reply = h.get(full, &["*.mp4"]).await.unwrap();
    assert_eq!(
        (reply.outcome, reply.job.name.as_deref(), reply.plan.map(|p| p.selected_size)),
        (GetOutcome::Started, Some("BigBuckBunny_124"), Some(GIB))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_search_does_not_delay_a_pause() {
    let h = Harness::new();
    let iso = torrent(26, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.search.set_delay(Duration::from_secs(3));
    let ctx = h.running.ctx.clone();
    let search = tokio::spawn(async move {
        server::handle(&ctx, Request::Search { query: torrential_core::SearchQuery::new("arch"), stream: false }).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    h.changes(Request::Pause { refs: vec![prefix(&iso)] }).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "pause took {:?}", started.elapsed());
    assert!(!search.is_finished());
    search.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streaming_search_sends_results_as_sources_answer() {
    use search_port::FakeSource;
    let listed = |hex: &str, name: &str, source: &str| torrential_core::Release {
        id: torrential_core::ReleaseId::Torrent(hex.repeat(20).parse().unwrap()),
        name: name.into(),
        sources: vec![source.into()],
        ..fake_catalog().remove(0)
    };
    let catalog = vec![listed("aa", "dead \u{1b}[31mred", "slow"), listed("bb", "dead fast", "quick")];
    let script = vec![
        FakeSource::answers("quick", 0),
        FakeSource::fails("broken", 20, torrential_core::SourceOutcome::Http { status: 503 }),
        FakeSource::answers("slow", 60),
    ];
    let h = Harness::with_search(search_port::Fake::scripted(catalog, script, Default::default()));
    let (client, served) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(server::connection(h.running.ctx.clone(), served));
    let (mut rd, mut wr) = client.into_split();
    let hello = Request::Hello { client_version: "0.1.0".into(), wire: torrential_core::protocol::WIRE_VERSION };
    crate::wire::write_async(&mut wr, &hello).await.unwrap();
    crate::wire::read_async::<Response>(&mut rd).await.unwrap();
    let search = Request::Search { query: torrential_core::SearchQuery::new("dead"), stream: true };
    crate::wire::write_async(&mut wr, &search).await.unwrap();
    let mut frames = Vec::new();
    loop {
        let frame: serde_json::Value = crate::wire::read_async(&mut rd).await.unwrap().unwrap();
        let names: Vec<&str> =
            frame["result"]["releases"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        let sources: Vec<String> = frame["result"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| format!("{} {}", s["source"].as_str().unwrap(), s["outcome"].as_str().unwrap()))
            .collect();
        let kind = frame["event"].as_str().or(frame["type"].as_str()).unwrap().to_string();
        frames.push(format!("{kind}: {names:?} {sources:?} pending {}", frame["pending"]));
        if kind != "search_update" {
            break;
        }
    }
    assert_eq!(
        frames,
        [
            r#"search_update: ["dead fast"] ["quick ok"] pending ["broken","slow"]"#,
            r#"search_update: ["dead fast"] ["quick ok", "broken http"] pending ["slow"]"#,
            r#"search: ["dead red", "dead fast"] ["quick ok", "broken http", "slow ok"] pending null"#,
        ]
    );
    let reply = h.get(Target::Prefix("bbbbbb".into()), &[]).await.unwrap();
    assert_eq!(reply.outcome, GetOutcome::Started);
}

#[tokio::test(flavor = "multi_thread")]
async fn watching_every_download_includes_ones_started_later() {
    let h = Harness::new();
    let before = torrent(30, "before.iso", &[("before.iso", GIB)]);
    h.get(h.torrent_file(&before), &[]).await.unwrap();
    let (client, served) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(server::connection(h.running.ctx.clone(), served));
    let (mut rd, mut wr) = client.into_split();
    let hello = Request::Hello { client_version: "0.1.0".into(), wire: torrential_core::protocol::WIRE_VERSION };
    crate::wire::write_async(&mut wr, &hello).await.unwrap();
    crate::wire::read_async::<Response>(&mut rd).await.unwrap();
    crate::wire::write_async(&mut wr, &Request::Watch { refs: Vec::new() }).await.unwrap();
    let first: torrential_core::protocol::Event = crate::wire::read_async(&mut rd).await.unwrap().unwrap();
    assert!(matches!(first, torrential_core::protocol::Event::Snapshot { ref jobs } if jobs.len() == 1));
    let later = torrent(31, "later.iso", &[("later.iso", GIB)]);
    h.get(h.torrent_file(&later), &[]).await.unwrap();
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match crate::wire::read_async(&mut rd).await.unwrap().unwrap() {
                torrential_core::protocol::Event::Job { job } if job.id == later.info_hash.id() => return job.name,
                _ => continue,
            }
        }
    })
    .await
    .expect("an event for the later download");
    assert_eq!(seen.as_deref(), Some("later.iso"));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_pause_returns_once_writes_stop_or_fails_when_they_do_not() {
    let gate = Gate::default();
    let engine_gate = gate.clone();
    let h = Harness::with_engine(|_| Box::new(Gated { port: Fake::new(Swarm::new(0)), gate: engine_gate }), parse_fake);
    let iso = torrent(60, "arch.iso", &[("arch.iso", GIB)]);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.until("downloading", |jobs| jobs[0].status.name() == "downloading").await;
    let pause = || {
        let ctx = h.running.ctx.clone();
        let refs = vec![prefix(&iso)];
        tokio::spawn(async move { server::handle(&ctx, Request::Pause { refs }).await })
    };
    gate.close();
    let first = pause();
    gate.until_held(1).await;
    let second = pause();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!((first.is_finished(), second.is_finished()), (false, false), "writes have not stopped");
    gate.open();
    let result = |response| match response {
        Response::Changed { changes } => changes[0].result.clone(),
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        (result(first.await.unwrap()), result(second.await.unwrap())),
        ("paused".to_string(), "already_paused".to_string())
    );

    h.changes(Request::Resume { refs: vec![prefix(&iso)], all: false }).await.unwrap();
    h.until("downloading again", |jobs| jobs[0].status.name() == "downloading").await;
    gate.close();
    let started = Instant::now();
    let err = h.changes(Request::Pause { refs: vec![prefix(&iso)] }).await.unwrap_err();
    assert!(started.elapsed() >= WRITES_STOP_WITHIN, "waited {:?}", started.elapsed());
    assert_eq!(
        (err.code, err.message.as_str(), err.hint.as_deref()),
        (
            ErrorCode::JobConflict,
            "writes to 3c3c3c did not stop within 1s",
            Some("run torrential pause 3c3c3c again; it returns once writes stop")
        )
    );
    gate.open();
    let again = h.changes(Request::Pause { refs: vec![prefix(&iso)] }).await.unwrap();
    assert_eq!(again, [("3c3c3c".to_string(), "already_paused".to_string())]);
}

struct Seeder {
    session: torrential_engine::Session,
    id: torrential_engine::TorrentId,
    addr: std::net::SocketAddr,
    torrent: Vec<u8>,
    _files: tempfile::TempDir,
}

impl Seeder {
    const PIECE: u32 = 16 * 1024;

    fn start(files: &[(&str, u8)]) -> Self {
        use torrential_engine as lt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("set")).unwrap();
        for (name, byte) in files {
            std::fs::write(dir.path().join("set").join(name), vec![*byte; Self::PIECE as usize]).unwrap();
        }
        let piece = lt::PieceSize::new(Self::PIECE).unwrap();
        let torrent = lt::create_torrent(&dir.path().join("set"), piece, lt::TorrentFormat::V1).unwrap();
        let config = lt::Config {
            listen: vec![std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 0)],
            dht: lt::Dht::Off,
            lsd: false,
            upnp: false,
            natpmp: false,
            connections: lt::ConnectionLimits { multiple_per_ip: true, ..lt::ConnectionLimits::default() },
            ..lt::Config::default()
        };
        let mut session = lt::Session::new(config).unwrap();
        let source = lt::TorrentSource::Metainfo(torrent.clone());
        let id = session.add(lt::AddTorrent::new(source, dir.path())).unwrap().id();
        let total = u64::from(Self::PIECE) * files.len() as u64;
        let deadline = Instant::now() + Duration::from_secs(20);
        let port = loop {
            let seeding = session.status(id).unwrap().progress.verified_selected_bytes() == total;
            if let (true, Some(port)) = (seeding, session.listen_port().unwrap()) {
                break port;
            }
            assert!(Instant::now() < deadline, "the seeder never verified its files");
            std::thread::sleep(Duration::from_millis(20));
        };
        let addr = std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port);
        Self { session, id, addr, torrent: torrent.into_bytes(), _files: dir }
    }

    fn engine(&self, gate: &Gate) -> impl FnOnce(&std::path::Path) -> Box<dyn EnginePort> {
        let (peers, gate) = (vec![self.addr], gate.clone());
        move |dir| {
            let state = dir.join("state");
            std::fs::create_dir_all(&state).unwrap();
            let network = super::libtorrent::Network::Loopback { peers };
            let search = Arc::new(search_port::Fake::new(Vec::new()));
            let port = super::libtorrent::Libtorrent::start(&state, network, search, tokio::runtime::Handle::current())
                .unwrap();
            Box::new(Gated { port, gate })
        }
    }

    /// Dials the engine, which skips a peer for a minute after a pause disconnects it.
    fn dial(&mut self, h: &Harness) {
        let port: u16 =
            std::fs::read_to_string(h.dir.path().join("state/listen.port")).unwrap().trim().parse().unwrap();
        let _ =
            self.session.connect_peer(self.id, std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_selection_widened_while_the_engine_finishes_is_done_only_once_the_new_files_verify() {
    let mut seeder = Seeder::start(&[("a.bin", 1), ("b.bin", 2)]);
    let gate = Gate::default();
    let h = Harness::with_engine(seeder.engine(&gate), super::libtorrent::parse);
    let file = h.dir.path().join("set.torrent");
    std::fs::write(&file, &seeder.torrent).unwrap();
    gate.close();
    let id = h.get(Target::TorrentFile(file), &["a.bin"]).await.unwrap().job.id;
    gate.until_held(1).await;
    let short = id.to_hex()[..6].to_string();
    let widened = h.get(Target::Prefix(short), &["*.bin"]).await.unwrap();
    assert_eq!((widened.outcome, widened.job.progress.size), (GetOutcome::Updated, Some(32 * 1024)));
    gate.open();
    let deadline = Instant::now() + Duration::from_secs(30);
    let job = loop {
        let job = h.status().await.remove(0);
        if job.status.name() == "done" || Instant::now() > deadline {
            break job;
        }
        seeder.dial(&h);
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let b = std::fs::read(h.downloads().join("set/b.bin")).unwrap_or_default();
    assert_eq!(
        (job.status.name(), job.progress.verified, b == vec![2; 16 * 1024]),
        ("done", 32 * 1024, true),
        "done means every selected file arrived"
    );
}
