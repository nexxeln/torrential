mod keys;
mod ranking;
mod render;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use torrential_core::protocol::{Event, FilesReply};
use torrential_core::{
    Arch, Category, Display, Error, ErrorCode, Flag, InfoHash, Job, JobStatus, Language, Machine, Os, Profile,
    Progress, Release, ReleaseId, SearchQuery, SearchResult, Selection, SourceOutcome, SourceReport, TorrentFile,
    TorrentId,
};

use crate::state::{Item, State};
use crate::update::{Effect, Msg};

pub const CLOCK: i64 = 1_790_000_000;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

pub fn id(short: &str) -> TorrentId {
    format!("{short}{}", "5d3b8e2f1c4a".repeat(3))[..40].parse().unwrap()
}

pub fn result_id(short: &str) -> ReleaseId {
    ReleaseId::Torrent(id(short))
}

fn job(short: &str, name: &str, status: JobStatus, size: u64, done: u64, rate: u64, days_ago: i64) -> Job {
    let dir = PathBuf::from("/Users/ada/Downloads/torrential");
    let eta = (rate > 0).then(|| (size - done).div_ceil(rate));
    Job {
        id: id(short),
        info_hash: InfoHash::V1(*id(short).as_bytes()),
        name: Some(name.into()),
        path: Some(dir.join(name)),
        dir,
        status,
        progress: Progress {
            size: Some(size),
            verified: done,
            piece_map: None,
            down_rate: rate,
            up_rate: 0,
            peers: if rate > 0 { 27 } else { 0 },
            seeds: 0,
            eta,
        },
        selection: Selection::All,
        added_at: CLOCK - days_ago * 86_400,
        title: None,
    }
}

pub fn jobs() -> Vec<Job> {
    let failed =
        Error::new(ErrorCode::StorageError, "cannot write openwebtext-2019/urlsf_subset07.tar: permission denied")
            .with_hint("check that the download folder is writable, then press p to resume");
    let mut jobs = vec![
        job("6f2c81", "wikipedia-en-2026-09-text", JobStatus::Downloading, 23 * GIB, 23 * GIB / 100 * 23, 10 * MIB, 0),
        job("a90d44", "openwebtext-2019", JobStatus::Error { error: failed }, 12 * GIB, 12 * GIB / 100 * 37, 0, 1),
        job("91fd3b", "stackexchange", JobStatus::Paused, 38 * GIB, 38 * GIB / 100 * 34, 0, 3),
        job(
            "5e02c8",
            "big_buck_bunny_1080p_h264.mov",
            JobStatus::Done { finished_at: CLOCK - 5 * 86_400 },
            691 * MIB,
            691 * MIB,
            0,
            5,
        ),
        job(
            "c7b9e3",
            "archlinux-2026.09.01-x86_64.iso",
            JobStatus::Done { finished_at: CLOCK - 30 * 86_400 },
            1372 * MIB,
            1372 * MIB,
            0,
            31,
        ),
    ];
    jobs[0].progress.piece_map =
        Some("4206224002006442024224020020406220240400262202424020022420000000".parse().unwrap());
    jobs[2].progress.piece_map =
        Some("2464024620426420420226440240426224206024246204224206240262420224".parse().unwrap());
    jobs
}

fn release(
    short: &str,
    name: &str,
    source: &str,
    flags: &[Flag],
    seeders: Option<u32>,
    size: u64,
    days: i64,
) -> Release {
    Release {
        id: result_id(short),
        name: name.into(),
        size: Some(size),
        seeders,
        leechers: None,
        published: Some(CLOCK - days * 86_400),
        category: Some(Category::Video),
        flags: flags.iter().copied().collect(),
        sources: vec![source.into()],
        magnet: None,
        torrent_url: None,
        info_url: None,
        span: None,
    }
}

pub fn night() -> Vec<Release> {
    vec![
        release(
            "4c1e9a",
            "Night of the Living Dead (1968)",
            "archive.org",
            &[Flag::PublicDomain],
            None,
            1366 * MIB,
            3_300,
        ),
        release(
            "b7f20d",
            "Night of the Living Dead (1968) [DVD]",
            "archive.org",
            &[Flag::PublicDomain],
            None,
            3487 * MIB,
            4_400,
        ),
        release(
            "e3a915",
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264",
            "knaben",
            &[Flag::Trusted],
            Some(214),
            7987 * MIB,
            1_100,
        ),
        release(
            "09cd47",
            "Night of the Living Dead 4K Restoration",
            "knaben",
            &[Flag::Official],
            Some(88),
            21_812 * MIB,
            700,
        ),
        release(
            "f61b2e",
            "Night.of.the.Living.Dead.2025.CAM.x264",
            "torrents-csv",
            &[Flag::Cam],
            Some(41),
            1452 * MIB,
            60,
        ),
        release(
            "7a0c3f",
            "Night of the Living Dead (1968) colorized",
            "archive.org",
            &[Flag::PublicDomain],
            None,
            734 * MIB,
            2_200,
        ),
        release(
            "c48e01",
            "Night of the Living Dead, original score FLAC",
            "knaben",
            &[Flag::CreativeCommons],
            Some(12),
            178 * MIB,
            1_500,
        ),
        release("2d9b6a", "Night of the Living Dead 1968 720p WEB", "torrents-csv", &[], Some(0), 2150 * MIB, 1_800),
    ]
}

fn report(source: &str, outcome: SourceOutcome) -> SourceReport {
    SourceReport { source: source.into(), outcome, elapsed_ms: 400 }
}

pub fn mac() -> Machine {
    Machine {
        os: Os::Macos,
        display: Some(Display { width: 3024, height: 1964, hdr: true }),
        arch: Some(Arch::Arm64),
        language: Language::from_locale("en-IN"),
    }
}

pub fn result(releases: Vec<Release>, sources: Vec<SourceReport>) -> SearchResult {
    let profile = Profile { detected: Some(mac()), ..Profile::default() };
    SearchResult {
        query: SearchQuery { profile, ..SearchQuery::new("night of the living dead") },
        total: releases.len() as u32,
        releases,
        hidden_adult: 0,
        sources,
        elapsed_ms: 1_512,
    }
}

pub fn final_sources() -> Vec<SourceReport> {
    vec![
        report("archlinux.org", SourceOutcome::Ok { results: 0 }),
        report("archive.org", SourceOutcome::Ok { results: 3 }),
        report("apibay", SourceOutcome::Http { status: 503 }),
        report("knaben", SourceOutcome::Ok { results: 3 }),
        report("torrents-csv", SourceOutcome::Ok { results: 2 }),
        report("nyaa", SourceOutcome::Timeout),
    ]
}

pub fn partial() -> (SearchResult, Vec<String>) {
    let releases = night().into_iter().filter(|r| r.sources[0] == "archive.org").collect();
    let sources = final_sources()[..2].to_vec();
    let pending = ["apibay", "knaben", "torrents-csv", "nyaa"].map(String::from).to_vec();
    (result(releases, sources), pending)
}

fn file(path: &str, size: u64, verified: u64, risky: bool) -> TorrentFile {
    TorrentFile {
        index: 0,
        path: path.into(),
        size,
        selected: true,
        verified,
        risk: risky.then_some(torrential_core::FileRisk::Executable),
    }
}

pub fn dvd_files() -> FilesReply {
    let files = [
        ("Night.mp4", 596_399_542),
        ("Night.asr.srt", 52_633),
        ("Night.gif", 325_837),
        ("VTS_01_1.mp4", 1_072_064_102),
        ("VTS_01_1_512Kb.mp4", 211_078_758),
        ("VTS_01_2.mp4", 1_035_573_657),
        ("VTS_01_2_512Kb.mp4", 204_157_747),
        ("VTS_01_3.mp4", 432_640_819),
        ("VTS_01_3_512Kb.mp4", 86_402_662),
        ("night_of_the_living_dead_files.xml", 10_035),
        ("extras/night_of_the_living_dead_meta.xml", 4_300),
        ("extras/setup.exe", 2_202_009),
    ];
    FilesReply {
        id: id("b7f20d").to_hex(),
        name: Some("night_of_the_living_dead_1968_dvd".into()),
        total_size: files.iter().map(|(_, s)| s).sum(),
        files: files.iter().map(|(p, s)| file(p, *s, 0, p.ends_with(".exe"))).collect(),
        metadata_source: "torrent_url".into(),
        elapsed_ms: 12,
    }
}

pub fn wikipedia_files() -> FilesReply {
    FilesReply {
        id: id("6f2c81").to_hex(),
        name: Some("wikipedia-en-2026-09-text".into()),
        total_size: 23 * GIB,
        files: vec![
            file("enwiki-20260901-pages-articles-multistream.xml.bz2", 21_900 * MIB, 5_290 * MIB, false),
            file("enwiki-20260901-pages-articles-multistream-index.txt.bz2", 254 * MIB, 0, false),
            file("enwiki-20260901-abstract.xml.gz", 1_126 * MIB, 0, false),
        ],
        metadata_source: "download".into(),
        elapsed_ms: 1,
    }
}

pub fn stackexchange_files() -> FilesReply {
    FilesReply {
        id: id("91fd3b").to_hex(),
        name: Some("stackexchange".into()),
        total_size: 2_000 * MIB,
        files: (1..=20).map(|i| file(&format!("part{i:02}.7z"), 100 * MIB, i * 5 * MIB, false)).collect(),
        metadata_source: "download".into(),
        elapsed_ms: 1,
    }
}

pub fn app(size: (u16, u16)) -> State {
    let now = Instant::now();
    State::new(now, CLOCK, size, Some(PathBuf::from("/Users/ada")))
}

pub fn with_downloads(size: (u16, u16)) -> State {
    let mut state = app(size);
    state.update(Msg::Watch(Event::Snapshot { jobs: jobs() }));
    state.update(Msg::FreeSpace(Some(412 * GIB + 300 * MIB)));
    state
}

pub fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

pub fn ctrl(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

pub fn press(state: &mut State, keys: &str) -> Vec<Effect> {
    keys.chars().flat_map(|c| state.update(key(KeyCode::Char(c)))).collect()
}

pub fn searched(size: (u16, u16)) -> State {
    let mut state = with_downloads(size);
    press(&mut state, "/");
    press(&mut state, "night of the living dead");
    let effects = state.update(key(KeyCode::Enter));
    let Some(Effect::Search { search, .. }) = effects.first() else { panic!("expected a search, got {effects:?}") };
    let search = *search;
    let (partial, pending) = partial();
    state.update(Msg::SearchUpdate { search, result: partial, pending });
    state.update(Msg::SearchDone { search, result: Ok(result(night(), final_sources())) });
    state
}

pub fn later(state: &mut State, ms: u64) {
    state.now += Duration::from_millis(ms);
}

pub fn cursor_name(state: &State) -> String {
    match state.cursor {
        Some(Item::Download(id)) => state.job(id).and_then(|j| j.name.clone()).unwrap(),
        Some(Item::Result(id)) => state.release(id).unwrap().name.clone(),
        None => "nothing".into(),
    }
}
