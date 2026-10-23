use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use torrential_core::error::{Details, EXIT_PENDING};
use torrential_core::protocol::{Change, DownloadPlan, FilesReply, GetOutcome, GetReply, Response};
use torrential_core::{
    Error, ErrorCode, FileRisk, Flag, InfoHash, Job, JobStatus, LocatorId, Progress, Release, ReleaseId, SearchQuery,
    SearchResult, Selection, SourceOutcome, SourceReport, TorrentFile, TorrentId,
};

use super::*;
use crate::client::wait::Printer;
use crate::client::{Files, keep_matching};
use crate::output::{Mode, Typed, render};

const NOW: i64 = 1_792_314_723;
const DAY: i64 = 86_400;
const GIB: u64 = 1024 * 1024 * 1024;
const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

fn tid(hex: &str) -> TorrentId {
    format!("{hex}{}", "0".repeat(40 - hex.len())).parse().unwrap()
}

fn both(doc: &Doc, payload: &impl serde::Serialize) -> String {
    let json = render(Mode::Json, doc, payload);
    assert_eq!(json.matches('\n').count(), 1, "a JSON document is one line");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    format!("{}\n--- json\n{}\n", render(Mode::Plain, doc, payload), serde_json::to_string_pretty(&value).unwrap())
}

fn release(id: ReleaseId, name: &str) -> Release {
    Release {
        id,
        name: name.into(),
        size: None,
        seeders: None,
        leechers: None,
        published: None,
        category: None,
        flags: BTreeSet::new(),
        sources: vec!["knaben".into()],
        magnet: None,
        torrent_url: None,
        info_url: None,
    }
}

fn mac() -> torrential_core::Profile {
    use torrential_core::{Arch, Display, Language, Machine, Os, Profile};
    let display = Some(Display { width: 3024, height: 1964, hdr: true });
    let machine = Machine { os: Os::Macos, display, arch: Some(Arch::Arm64), language: Language::from_locale("en-IN") };
    Profile { detected: Some(machine), room: Some(400 * GIB), ..Profile::default() }
}

fn search_result(releases: Vec<Release>, total: u32, sources: Vec<SourceReport>) -> SearchResult {
    SearchResult {
        query: SearchQuery { profile: mac(), ..SearchQuery::new("arch linux iso") },
        releases,
        total,
        hidden_adult: 0,
        sources,
        elapsed_ms: 1432,
    }
}

fn report(source: &str, outcome: SourceOutcome) -> SourceReport {
    SourceReport { source: source.into(), outcome, elapsed_ms: 300 }
}

#[test]
fn search_with_a_failed_source() {
    let arch = Release {
        size: Some(1_640_497_152),
        seeders: Some(412),
        published: Some(NOW - 17 * DAY),
        flags: [Flag::Official].into(),
        sources: vec!["archlinux".into(), "knaben".into()],
        magnet: Some(format!("magnet:?xt=urn:btih:{ARCH}")),
        ..release(ReleaseId::Torrent(ARCH.parse().unwrap()), "archlinux-2026.10.01-x86_64.iso")
    };
    let older = Release {
        size: Some(1_503_238_553),
        seeders: Some(37),
        published: Some(NOW - 47 * DAY),
        ..release(ReleaseId::Torrent(tid("51e0c7")), "archlinux-2026.09.01-x86_64.iso")
    };
    let unknown = Release {
        sources: vec!["jackett".into()],
        ..release(ReleaseId::Locator(LocatorId::from_bytes([0xd4; 20])), "Arch Linux 2026.10.01 x86_64 ISO + checksums")
    };
    let hostile = Release {
        seeders: Some(3),
        size: Some(2 * GIB),
        published: Some(NOW - 3 * 365 * DAY),
        ..release(ReleaseId::Torrent(tid("9b5213")), "arch\u{1b}[31m linux\tISO\u{202e}gpj.exe")
    };
    let mut result = search_result(
        vec![arch, older, unknown, hostile],
        14,
        vec![
            report("archlinux", SourceOutcome::Ok { results: 1 }),
            report("knaben", SourceOutcome::Ok { results: 12 }),
            report("apibay", SourceOutcome::Http { status: 503 }),
            report("nyaa", SourceOutcome::Timeout),
        ],
    );
    result.hidden_adult = 1;
    let doc = search(&result, NOW);
    insta::assert_snapshot!(both(&doc, &Response::Search { result }));
}

#[test]
fn search_for_a_film_shows_the_quality_its_names_give() {
    let film = |hex: &str, name: &str, size: u64| Release {
        size: Some(size),
        seeders: Some(40),
        published: Some(NOW - 30 * DAY),
        category: Some(torrential_core::Category::Video),
        ..release(ReleaseId::Torrent(tid(hex)), name)
    };
    let mut result = search_result(
        vec![
            film("b42ef9", "Night of the Living Dead (1968)", 1_173_905_628),
            film("5c7c3c", "Night of the Living Dead (1968) 2160p UHD BluRay HDR10 x265", 59 * GIB),
            film("082e09", "Night Of The Living Dead 720p 1968 x264", 1_112_690_010),
        ],
        3,
        vec![report("archive.org", SourceOutcome::Ok { results: 3 })],
    );
    result.query = SearchQuery { profile: mac(), ..SearchQuery::new("night of the living dead 1968") };
    let doc = search(&result, NOW);
    insta::assert_snapshot!(render(Mode::Plain, &doc, &Response::Search { result }));
}

#[test]
fn film_and_software_searches_say_what_they_ranked_for() {
    use torrential_core::{Category, Preferences, Profile, Screen};
    let film = Release {
        category: Some(Category::Video),
        ..release(ReleaseId::Torrent(tid("082e09")), "Night Of The Living Dead 720p 1968")
    };
    let iso = release(ReleaseId::Torrent(ARCH.parse().unwrap()), "archlinux-2026.10.01-x86_64.iso");
    let book = release(ReleaseId::Torrent(tid("5c7c3c")), "Ubuntu Linux Bible, 11th Edition [PDF]");
    let detected = mac();
    let tv = Preferences { screen: Some(Screen::Tv4k), max_size: Some(8_000_000_000), ..Preferences::default() };
    let summary = |releases: Vec<Release>, category: Option<Category>, profile: &Profile| {
        let mut result = search_result(releases, 1, vec![report("archive.org", SourceOutcome::Ok { results: 1 })]);
        result.query.category = category;
        result.query.profile = profile.clone();
        search(&result, NOW).summary[1..].to_vec()
    };

    assert_eq!(
        summary(vec![film.clone()], None, &detected),
        ["ranked for this Mac: 1080p screen, shows HDR, plays HEVC, arm64, English"]
    );
    assert_eq!(
        summary(vec![film], None, &Profile { preferences: tv, ..detected.clone() }),
        ["ranked for this Mac: 2160p screen, up to 8 GB, arm64, English · overrides: screen tv-4k, max_size 8GB"]
    );
    assert_eq!(
        summary(Vec::new(), Some(Category::Video), &Profile::default()),
        ["ranked for this machine, not detected yet"]
    );
    assert_eq!(summary(vec![iso], None, &detected), ["ranked for this Mac: arm64"]);
    assert_eq!(summary(vec![book], None, &detected), Vec::<String>::new());
}

#[test]
fn search_with_no_results() {
    let result = search_result(Vec::new(), 0, vec![report("archlinux", SourceOutcome::Ok { results: 0 })]);
    insta::assert_snapshot!(both(&search(&result, NOW), &Response::Search { result }));
}

fn file(index: u32, path: &str, size: u64) -> TorrentFile {
    TorrentFile { index, path: path.into(), size, selected: false, verified: 0, risk: None }
}

fn film_files() -> FilesReply {
    let mut installer = file(3, "Codec Installer.exe", 1_200_000);
    installer.risk = Some(FileRisk::Executable);
    FilesReply {
        id: "6bc1af".to_string() + &"0".repeat(34),
        name: Some("night_of_the_living_dead_dvd".into()),
        total_size: 5_046_586_573,
        files: vec![
            file(0, "Night.asr.srt", 52_633),
            file(1, "Night.mp4", 596_399_542),
            file(2, "VTS_01_1.mp4", 262_668_288),
            installer,
        ],
        metadata_source: "torrent_url".into(),
        elapsed_ms: 412,
    }
}

#[test]
fn files_from_a_torrent_url_mark_executables() {
    let reply = film_files();
    insta::assert_snapshot!(both(&files(&reply, None), &Response::Files { files: reply }));
}

#[test]
fn files_matching_globs_list_only_those() {
    let mut reply = film_files();
    let patterns = vec!["*.MP4".to_string(), "Night.asr.srt".to_string()];
    let matched = keep_matching(&mut reply, &patterns).unwrap();
    let doc = files(&reply, Some(&matched));
    insta::assert_snapshot!(both(&doc, &Typed { kind: "files", body: Files { files: reply, matched: Some(matched) } }));
}

#[test]
fn a_match_summary_counts_the_matches_and_their_size() {
    let mut reply = film_files();
    let matched = keep_matching(&mut reply, &["night.mp4".to_string()]).unwrap();
    let doc = files(&reply, Some(&matched));
    assert_eq!(
        doc.summary,
        ["1 of 4 files in night_of_the_living_dead_dvd match · 568.8 MiB · file list from the .torrent URL in 0.4s"]
    );
    assert_eq!(doc.next.as_deref(), Some("torrential get 6bc1af --files night.mp4"));
    let mut reply = film_files();
    let matched = keep_matching(&mut reply, &["Codec Installer.exe".to_string()]).unwrap();
    assert_eq!(
        files(&reply, Some(&matched)).next.as_deref(),
        Some("torrential get 6bc1af --files 'Codec Installer.exe'")
    );
}

#[test]
fn files_of_a_download_show_progress_per_file() {
    let selected = |f: TorrentFile, verified| TorrentFile { selected: true, verified, ..f };
    let reply = FilesReply {
        id: ARCH.into(),
        name: Some("debian-13.1.0-amd64-DVD".into()),
        total_size: 12 * GIB,
        files: vec![
            selected(file(0, "debian-13.1.0-amd64-DVD-1.iso", 4 * GIB), 4 * GIB),
            selected(file(1, "debian-13.1.0-amd64-DVD-2.iso", 4 * GIB), GIB),
            file(2, "debian-13.1.0-amd64-DVD-3.iso", 4 * GIB),
        ],
        metadata_source: "download".into(),
        elapsed_ms: 3,
    };
    insta::assert_snapshot!(both(&files(&reply, None), &Response::Files { files: reply }));
}

#[test]
fn a_one_file_torrent_counts_one_file() {
    let reply = FilesReply {
        id: ARCH.into(),
        name: Some("archlinux-2026.10.01-x86_64.iso".into()),
        total_size: GIB,
        files: vec![file(0, "archlinux-2026.10.01-x86_64.iso", GIB)],
        metadata_source: "peers".into(),
        elapsed_ms: 900,
    };
    assert_eq!(
        files(&reply, None).summary,
        ["1 file in archlinux-2026.10.01-x86_64.iso · 1.0 GiB · file list from peers in 0.9s"]
    );
}

fn job(hex: &str, name: Option<&str>, status: JobStatus, progress: Progress) -> Job {
    let id = tid(hex);
    let dir = PathBuf::from("/Users/ada/Downloads/torrential");
    Job {
        id,
        info_hash: InfoHash::V1(*id.as_bytes()),
        name: name.map(String::from),
        path: name.map(|n| dir.join(n)),
        dir,
        status,
        progress,
        selection: Selection::All,
        added_at: NOW - 3600,
    }
}

fn downloading() -> Progress {
    Progress {
        size: Some(1_640_497_152),
        verified: 50_855_411,
        piece_map: None,
        down_rate: 32_715_571,
        up_rate: 120_000,
        peers: 48,
        seeds: 31,
        eta: Some(49),
    }
}

fn get_reply(outcome: GetOutcome, job: Job, plan: Option<DownloadPlan>) -> GetReply {
    GetReply { outcome, job, plan, previous_selection: None }
}

fn plan(conflicts: Vec<PathBuf>) -> DownloadPlan {
    DownloadPlan {
        selected_files: 2,
        total_files: 255,
        selected_size: 596_452_175,
        destination: "/Users/ada/Movies/night_of_the_living_dead_dvd".into(),
        free_space: 410 * GIB,
        conflicts,
    }
}

#[test]
fn get_outcomes() {
    let movies = |mut j: Job| {
        j.dir = "/Users/ada/Movies".into();
        j.path = j.name.as_ref().map(|n| j.dir.join(n));
        j.selection = Selection::Patterns(vec!["Night.mp4".into(), "*.srt".into()]);
        j
    };
    let film = || movies(job("6bc1af", Some("night_of_the_living_dead_dvd"), JobStatus::Checking, Progress::default()));
    let cases = [
        (
            "started from a magnet",
            get_reply(GetOutcome::Started, job(ARCH, None, JobStatus::Metadata, Progress::default()), None),
        ),
        (
            "queued",
            get_reply(
                GetOutcome::Queued,
                movies(job("6bc1af", Some("night_of_the_living_dead_dvd"), JobStatus::Queued, Progress::default())),
                Some(plan(vec![])),
            ),
        ),
        (
            "existing",
            get_reply(
                GetOutcome::Existing,
                job(ARCH, Some("archlinux-2026.10.01-x86_64.iso"), JobStatus::Downloading, downloading()),
                None,
            ),
        ),
        (
            "existing and done",
            get_reply(
                GetOutcome::Existing,
                job(ARCH, Some("archlinux-2026.10.01-x86_64.iso"), JobStatus::Done { finished_at: NOW }, downloading()),
                None,
            ),
        ),
        (
            "updated",
            GetReply {
                previous_selection: Some(Selection::All),
                ..get_reply(GetOutcome::Updated, film(), Some(plan(vec![])))
            },
        ),
        ("planned in the default folder", get_reply(GetOutcome::Planned, film(), Some(plan(vec![])))),
        (
            "planned with conflicts",
            get_reply(
                GetOutcome::Planned,
                film(),
                Some(plan(vec!["/Users/ada/Movies/night_of_the_living_dead_dvd/Night.mp4".into()])),
            ),
        ),
    ];
    let mut out = String::new();
    for (name, reply) in cases {
        out.push_str(&format!(
            "=== {name}\n{}",
            both(&get(&reply, None), &Response::Get { reply: Box::new(reply.clone()) })
        ));
    }
    let reply = get_reply(GetOutcome::Planned, film(), Some(plan(vec![])));
    let doc = get(&reply, Some(Path::new("/Users/ada/Movies")));
    out.push_str(&format!("=== planned with --dir\n{}", both(&doc, &Response::Get { reply: Box::new(reply) })));
    insta::assert_snapshot!(out);
}

fn fleet() -> Vec<Job> {
    let full = |size| Progress { size: Some(size), verified: size, ..Progress::default() };
    vec![
        job(ARCH, Some("archlinux-2026.10.01-x86_64.iso"), JobStatus::Downloading, downloading()),
        job(
            "c32cdd",
            Some("stackexchange"),
            JobStatus::Interrupted,
            Progress { size: Some(99_535_000_000), verified: 79_130_000_000, ..Progress::default() },
        ),
        job(
            "6bc1af",
            Some("night_of_the_living_dead_dvd"),
            JobStatus::Done { finished_at: NOW - 600 },
            full(596_452_175),
        ),
        job(
            "e3a1f0",
            Some("ubuntu-24.04.5-desktop-amd64.iso"),
            JobStatus::Error {
                error: Error::new(ErrorCode::DiskFull, "needs 5.9 GiB more on this disk")
                    .with_hint("free some space, or choose another disk with --dir"),
            },
            Progress { size: Some(6_345_887_744), ..Progress::default() },
        ),
        job("d40a19", None, JobStatus::Metadata, Progress { peers: 4, ..Progress::default() }),
        job(
            "51e0c7",
            Some("archlinux-2026.09.01-x86_64.iso"),
            JobStatus::Queued,
            Progress { size: Some(1_503_238_553), ..Progress::default() },
        ),
        job(
            "a282c6",
            Some("Night Of The Living Dead"),
            JobStatus::Paused,
            Progress { size: Some(2_899_102_924), verified: 1_000_000_000, ..Progress::default() },
        ),
    ]
}

#[test]
fn status_lists_every_state() {
    let jobs = fleet();
    insta::assert_snapshot!(both(&super::jobs(&jobs), &Response::Jobs { jobs }));
}

#[test]
fn status_with_no_downloads() {
    insta::assert_snapshot!(both(&super::jobs(&[]), &Response::Jobs { jobs: Vec::new() }));
}

#[test]
fn status_of_one_download() {
    let mut out = String::new();
    for job in fleet().into_iter().take(4) {
        let name = job.status.name();
        out.push_str(&format!("=== {name}\n{}", both(&super::job(&job), &Response::Jobs { jobs: vec![job.clone()] })));
    }
    insta::assert_snapshot!(out);
}

fn change(hex: &str, name: &str, result: &str, path: Option<&str>) -> Change {
    Change { id: tid(hex).to_hex(), name: Some(name.into()), result: result.into(), path: path.map(PathBuf::from) }
}

#[test]
fn pause_resume_rm_and_verify() {
    let cases = [
        (
            ChangeCommand::Pause,
            vec![
                change(ARCH, "archlinux-2026.10.01-x86_64.iso", "paused", None),
                change("c32cdd", "stackexchange", "already_paused", None),
            ],
        ),
        (
            ChangeCommand::Resume,
            vec![
                change("c32cdd", "stackexchange", "resumed", None),
                change("6bc1af", "night_of_the_living_dead_dvd", "already_done", None),
            ],
        ),
        (
            ChangeCommand::Remove,
            vec![
                change(
                    ARCH,
                    "archlinux-2026.10.01-x86_64.iso",
                    "removed",
                    Some("/Users/ada/isos/archlinux-2026.10.01-x86_64.iso"),
                ),
                change("c32cdd", "stackexchange", "already_removed", Some("/Users/ada/datasets/stackexchange")),
            ],
        ),
        (ChangeCommand::Verify, vec![change("6bc1af", "night_of_the_living_dead_dvd", "checking", None)]),
    ];
    let mut out = String::new();
    for (command, list) in cases {
        out.push_str(&format!(
            "=== {command:?}\n{}",
            both(&changes(command, &list), &Response::Changed { changes: list.clone() })
        ));
    }
    insta::assert_snapshot!(out);
}

fn wait_transcript(mode: Mode, end: WaitEnd, frames: &[(u64, Job)], elapsed: u64) -> String {
    let printer = Printer { mode };
    let jobs: Vec<Job> = {
        let mut latest: Vec<Job> = Vec::new();
        for (_, job) in frames {
            match latest.iter_mut().find(|j| j.id == job.id) {
                Some(j) => *j = job.clone(),
                None => latest.push(job.clone()),
            }
        }
        latest
    };
    let latest = watched(jobs);
    let mut out = printer.header();
    for (at, job) in frames {
        let id = &latest.iter().find(|w| w.job.id == job.id).unwrap().id;
        out.push_str(&printer.row(Duration::from_secs(*at), id, job));
    }
    out.push_str(&printer.end(&end, Duration::from_secs(elapsed), &latest));
    out
}

#[test]
fn wait_until_done_timeout_error_and_stop() {
    let arch = |status, progress| job(ARCH, Some("archlinux-2026.10.01-x86_64.iso"), status, progress);
    let done = Progress { size: Some(1_640_497_152), verified: 1_640_497_152, ..Progress::default() };
    let later = Progress { verified: 1_240_000_000, eta: Some(12), ..downloading() };
    let failed = JobStatus::Error {
        error: Error::new(
            ErrorCode::DiskFull,
            "needs 1.5 GiB more on this disk, but only 900.0 MiB is free and 2.0 GiB stays in reserve",
        )
        .with_hint("free some space, or choose another disk with --dir"),
    };
    let cases = [
        (
            "done",
            WaitEnd::Done,
            vec![
                (0, arch(JobStatus::Downloading, downloading())),
                (44, arch(JobStatus::Done { finished_at: NOW }, done)),
            ],
            44,
        ),
        (
            "timeout",
            WaitEnd::Timeout,
            vec![(0, arch(JobStatus::Downloading, downloading())), (60, arch(JobStatus::Downloading, later))],
            100,
        ),
        (
            "error",
            WaitEnd::Failed { id: "9b5212".into() },
            vec![(0, arch(JobStatus::Metadata, Progress::default())), (3, arch(failed, Progress::default()))],
            3,
        ),
        ("stopped", WaitEnd::Stopped { id: "9b5212".into() }, vec![(0, arch(JobStatus::Interrupted, later))], 0),
        (
            "metadata",
            WaitEnd::Metadata,
            vec![(0, arch(JobStatus::Metadata, Progress::default())), (2, arch(JobStatus::Downloading, downloading()))],
            2,
        ),
    ];
    let mut out = String::new();
    for (name, end, frames, elapsed) in cases {
        let exit = end.exit_code();
        out.push_str(&format!(
            "=== {name}, exit {exit}\n{}",
            wait_transcript(Mode::Plain, end.clone(), &frames, elapsed)
        ));
        out.push_str(&format!("--- ndjson\n{}", wait_transcript(Mode::Json, end, &frames, elapsed)));
    }
    insta::assert_snapshot!(out);
}

#[test]
fn daemon_status_running_and_stopped() {
    let running = DaemonReport {
        engine: "running",
        pid: Some(4242),
        version: Some("0.1.0".into()),
        socket: "/tmp/torrential-501/engine.sock".into(),
        log: "/Users/ada/.local/state/torrential/engine.log".into(),
        downloads: Some(2),
    };
    let stopped = DaemonReport { engine: "stopped", pid: None, version: None, downloads: None, ..running.clone() };
    let mut out = String::new();
    for report in [running, stopped] {
        out.push_str(&both(&daemon_status(&report), &crate::output::Typed { kind: "daemon", body: &report }));
    }
    insta::assert_snapshot!(out);
}

#[test]
fn doctor_report() {
    let checks = vec![
        Check { check: "sandbox".into(), status: CheckStatus::Fail, detail: "CODEX_SANDBOX_NETWORK_DISABLED=1".into() },
        Check {
            check: "disk".into(),
            status: CheckStatus::Ok,
            detail: "410.2 GiB free in /Users/ada/Downloads/torrential".into(),
        },
        Check { check: "state".into(), status: CheckStatus::Ok, detail: "/Users/ada/.local/state/torrential".into() },
        Check {
            check: "engine".into(),
            status: CheckStatus::Ok,
            detail: "not running; the next command starts it".into(),
        },
        Check { check: "sources".into(), status: CheckStatus::Skipped, detail: "8 enabled".into() },
        Check { check: "source knaben".into(), status: CheckStatus::Fail, detail: "http 503, 211 ms".into() },
    ];
    let fix = Some(
        "in interactive Codex, rerun with sandbox_permissions \"require_escalated\" and prefix_rule [\"torrential\"]"
            .into(),
    );
    insta::assert_snapshot!(both(
        &doctor(&checks, fix),
        &crate::output::Typed { kind: "doctor", body: serde_json::json!({"checks": checks}) }
    ));
}

#[test]
fn completions_setup_is_valid_shell() {
    let setup = crate::completions::setup(crate::cli::Shell::Zsh);
    assert_eq!(crate::completions::script(&setup), "# add this line to ~/.zshrc\nsource <(COMPLETE=zsh torrential)\n");
}

#[test]
fn schema_document() {
    use clap::CommandFactory;
    let schema = crate::schema::build(&crate::cli::Cli::command(), "0.1.0".into());
    let value: serde_json::Value = serde_json::from_str(&crate::output::json_document(&schema, None)).unwrap();
    insta::assert_snapshot!(serde_json::to_string_pretty(&value).unwrap());
}

fn sample(code: ErrorCode) -> Error {
    use crate::daemon::admission::{self, Plan, Space};
    use crate::daemon::engine_port::FakeTorrentFile;
    let film = || {
        FakeTorrentFile {
            info_hash: InfoHash::V1([0x6b; 20]),
            name: "night".into(),
            files: vec![("Night.mp4".into(), 70 * GIB)],
        }
        .metainfo()
        .unwrap()
    };
    let disk = Space { device: 1, total: 100 * GIB, free: 3 * GIB };
    let plan = |conflicts| Plan {
        wanted: vec![true],
        selected_size: 70 * GIB,
        destination: "/Users/ada/Movies/night".into(),
        links: Vec::new(),
        conflicts,
        space: disk,
    };
    let ubuntu = |hex: &str| Release {
        name: "ubuntu-24.04.5-desktop-amd64.iso".into(),
        ..release(ReleaseId::Torrent(tid(hex)), "")
    };
    let given = |code, message: &str, hint: &str| Error::new(code, message).with_hint(hint);
    match code {
        ErrorCode::Usage => crate::usage_error(
            &<crate::cli::Cli as clap::Parser>::try_parse_from(["torrential", "wait", "9b5212", "--timeout", "soon"])
                .unwrap_err(),
            &["torrential".into(), "wait".into()],
        ),
        ErrorCode::ConfirmationRequired => given(code, "deleting files needs confirmation", "add --yes to confirm"),
        ErrorCode::InvalidRef => torrential_core::Target::parse("9b5", Path::new("/")).unwrap_err(),
        ErrorCode::InvalidTorrent => crate::daemon::engine_port::parse_fake(b"<html>").unwrap_err(),
        ErrorCode::NotFound => {
            let prefix = "9b52".parse().unwrap();
            crate::daemon::resolve::prefix(&prefix, &[], &[], crate::daemon::resolve::Scope::Downloads, "pause")
                .unwrap_err()
        }
        ErrorCode::AmbiguousRef => {
            let results =
                [ubuntu("e3a1f0"), Release { name: "Ubuntu 24.04.5 LTS Desktop (repack)".into(), ..ubuntu("e3a1c4") }];
            let prefix = "e3a1".parse().unwrap();
            crate::daemon::resolve::prefix(
                &prefix,
                &[],
                &results,
                crate::daemon::resolve::Scope::DownloadsAndResults,
                "get",
            )
            .unwrap_err()
        }
        ErrorCode::JobConflict => given(
            code,
            "d40a19 has no file list yet, so there is nothing to verify",
            "torrential wait d40a19 --until metadata",
        ),
        ErrorCode::PathConflict => {
            admission::check_conflicts(&plan(vec!["/Users/ada/Movies/night/Night.mp4".into()])).unwrap_err()
        }
        ErrorCode::ProvidersUnavailable => {
            given(code, "every search source failed", "try again in a minute, or pick a source with --source")
                .with_details(Details::Sources(vec![
                    report("knaben", SourceOutcome::Http { status: 502 }),
                    report("apibay", SourceOutcome::Timeout),
                ]))
        }
        ErrorCode::MetadataTimeout => given(
            code,
            "no peer sent the file list for 9b5212 in time",
            "try again later, or run torrential get 9b5212 to keep looking in the background",
        ),
        ErrorCode::NetworkUnavailable => {
            given(code, "no network route to any DHT node", "check the network connection, then run the command again")
        }
        ErrorCode::FetchFailed => given(
            code,
            "could not fetch https://archive.org/download/x/x_archive.torrent: http 404",
            "try the magnet link instead",
        ),
        ErrorCode::SandboxNetworkBlocked => crate::client::launch::sandbox_error(
            crate::client::launch::Blocked::Network { signal: "CODEX_SANDBOX_NETWORK_DISABLED=1".into() },
            &crate::env::Env::from_pairs([("CODEX_THREAD_ID", "t-1")]),
        ),
        ErrorCode::SandboxWriteBlocked => given(
            code,
            "cannot write /Users/ada/Library/LaunchAgents/dev.torrential.engine.plist: operation not permitted",
            "run torrential daemon install from a terminal outside any sandbox",
        ),
        ErrorCode::DaemonUnavailable => {
            let deep = format!("/tmp/{}", "d".repeat(120));
            crate::paths::Paths::resolve(
                &crate::env::Env::from_pairs([("HOME", "/Users/ada"), ("TORRENTIAL_HOME", deep.as_str())]),
                501,
                None,
            )
            .unwrap_err()
        }
        ErrorCode::DaemonVersion => {
            given(code, "the engine speaks wire 2, the client 1", "torrential daemon stop, then run the command again")
                .with_details(Details::Versions { client: "0.1.0".into(), daemon: "0.2.0".into() })
        }
        ErrorCode::StateCorrupt => given(
            code,
            "the state database /Users/ada/.local/state/torrential/state.db is damaged: file is not a database",
            "torrential never resets state on its own; move /Users/ada/.local/state/torrential/state.db aside to start fresh, or restore a backup",
        ),
        ErrorCode::DiskFull => admission::check_disk(disk, 70 * GIB, 0).unwrap_err(),
        ErrorCode::TooLarge => {
            admission::check_size(plan(vec![]).selected_size, admission::Cap::new(None, None), None).unwrap_err()
        }
        ErrorCode::NoSelection => {
            crate::daemon::files::wanted(&film(), &Selection::Patterns(vec!["*.mkv".into()])).unwrap_err()
        }
        ErrorCode::StorageError => given(
            code,
            "the disk holding /Users/ada/.local/state/torrential/state.db is full",
            "free some space on that disk",
        ),
        ErrorCode::VerificationFailed => given(
            code,
            "3 damaged pieces in the selected files",
            "torrential verify 6bc1af --repair downloads them again",
        ),
        ErrorCode::NoSource => given(
            code,
            "no peer or web seed has had the 2 pieces still missing (488.8 KiB) for 20m",
            "torrential resume 770e77 looks for them again; the 33.0 MiB verified stays",
        ),
        ErrorCode::Interrupted => given(code, "interrupted", "run the command again"),
        ErrorCode::Internal => Error::internal("the engine thread stopped"),
    }
}

#[test]
fn every_error_class() {
    let mut out = String::new();
    for code in ErrorCode::ALL {
        let error = sample(code);
        assert_eq!(error.code, code);
        let exit = code.exit_code();
        assert_ne!(exit, EXIT_PENDING);
        let plain = crate::output::error(Mode::Plain, &error);
        let json = crate::output::error(Mode::Json, &error);
        assert!(plain.stdout.is_empty() && json.stderr.is_empty(), "plain errors use stderr, JSON errors stdout");
        let value: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
        out.push_str(&format!(
            "=== {code}, exit {exit}\n{}--- json\n{}\n",
            plain.stderr,
            serde_json::to_string_pretty(&value).unwrap()
        ));
    }
    let limit = format!("over the {}-byte limit", crate::paths::MAX_SOCKET_PATH);
    insta::assert_snapshot!(out.replace(&limit, "over the <max>-byte limit"));
}

#[test]
fn skill_status_suggests_install_only_when_it_loses_no_edits() {
    use crate::skill::{Installed, State, Target};
    let row = |target: Target, state, version: Option<&str>| Installed {
        target,
        path: PathBuf::from(format!("/Users/ada/.{}/skills/torrential", target.name())),
        state,
        version: version.map(String::from),
    };
    let stale = [row(Target::Agents, State::Outdated, Some("0.0.9")), row(Target::Claude, State::Missing, None)];
    assert_eq!(
        skill_status(&stale, "0.1.0", "torrential skill install").plain(),
        "TARGET\tSTATE\tVERSION\tPATH\n\
         agents\toutdated\t0.0.9\t/Users/ada/.agents/skills/torrential\n\
         claude\tmissing\t-\t/Users/ada/.claude/skills/torrential\n\
         # this binary carries the skill from torrential 0.1.0\n\
         next: torrential skill install\n"
    );
    let edited = [row(Target::Agents, State::Modified, Some("0.1.0")), row(Target::Claude, State::Foreign, None)];
    assert_eq!(
        skill_status(&edited, "0.1.0", "torrential skill install").plain(),
        "TARGET\tSTATE\tVERSION\tPATH\n\
         agents\tmodified\t0.1.0\t/Users/ada/.agents/skills/torrential\n\
         claude\tforeign\t-\t/Users/ada/.claude/skills/torrential\n\
         # this binary carries the skill from torrential 0.1.0\n\
         # agents was edited after torrential wrote it\n\
         # claude was not written by torrential\n\
         # torrential skill install --force replaces them\n"
    );
    let check = |status, detail: &str| Check { check: "skill".into(), status, detail: detail.into() };
    assert_eq!(
        skill_check(&stale),
        (check(CheckStatus::Warn, "agents outdated, claude missing"), Some("torrential skill install".into()))
    );
    assert_eq!(skill_check(&edited), (check(CheckStatus::Warn, "agents modified, claude foreign"), None));
}

#[test]
fn a_tab_in_a_path_stays_inside_its_column() {
    use crate::skill::{self, AgentArg, Change as SkillChange, Installed, Scope, State, Target, TargetArgs};
    let project = Path::new("/src/my\trepo");
    let path = project.join(".agents/skills/torrential");
    let install =
        skill::command("install", &TargetArgs { agent: AgentArg::All, scope: Scope::Project }, false, project);
    let installed = [Installed { target: Target::Agents, path: path.clone(), state: State::Missing, version: None }];
    let changes = [SkillChange { target: Target::Agents, path, result: "installed" }];
    let report = DaemonReport {
        engine: "stopped",
        pid: None,
        version: None,
        socket: "/run/my\ttorrential/engine.sock".into(),
        log: "/state/my\ntorrential/engine.log".into(),
        downloads: None,
    };
    let named = job("9b5212", Some("arch\tlinux.iso"), JobStatus::Paused, Progress::default());

    assert_eq!(
        skill_status(&installed, "0.1.0", &install).plain(),
        "TARGET\tSTATE\tVERSION\tPATH\n\
         agents\tmissing\t-\t/src/my repo/.agents/skills/torrential\n\
         # this binary carries the skill from torrential 0.1.0\n\
         next: cd '/src/my repo' && torrential skill install --scope project\n"
    );
    assert_eq!(
        skill_changes(&changes).plain(),
        "TARGET\tRESULT\tPATH\nagents\tinstalled\t/src/my repo/.agents/skills/torrential\n"
    );
    assert_eq!(
        daemon_status(&report).plain(),
        "engine: stopped\nsocket: /run/my torrential/engine.sock\nlog:    /state/my torrential/engine.log\n"
    );
    let row: Vec<String> = wait_row(Duration::from_secs(3), "9b5212", &named).into_iter().map(|c| c.text).collect();
    assert_eq!(row.join("\t"), "3s\t9b5212\tpaused\t-\t-\t-\t-\t-\tarch linux.iso");
}
