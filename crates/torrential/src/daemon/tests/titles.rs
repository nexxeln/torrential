//! Playing a title: trying its sources in turn, the downloads that makes, and what becomes of one
//! given up.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use torrential_core::error::Details;
use torrential_core::protocol::{Request, Response, TitleRequest};
use torrential_core::title::Span;
use torrential_core::{
    AttemptEvent, Category, Check, Error, ErrorCode, Job, LocatorId, Player, Release, ReleaseId, TitleLink, TorrentId,
};

use super::super::admission::GIB;
use super::super::engine_port::{EngineEvent, Fake as FakeEngine, FakeTorrentFile, Swarm, parse_fake};
use super::super::owner::plays::Destination;
use super::super::owner::{Command, Finish, Hold, Origin, PlayCommand, Source, TitlePlay};
use super::super::search_port::{self, FakeSource};
use super::super::titles::attempt::{self, Containers, PlayTitle};
use super::super::titles::{Timing, catalog_port};
use super::{Gate, Gated, Harness, MIB, Outside, TIMING, prefix, torrent};

/// A search result for a test torrent, listed by `source`.
pub(super) fn listed(t: &FakeTorrentFile, source: &str, seeders: Option<u32>) -> Release {
    let hex = t.info_hash.id().to_hex();
    Release {
        id: ReleaseId::Torrent(t.info_hash.id()),
        name: t.name.clone(),
        size: Some(t.files.iter().map(|(_, size)| size).sum()),
        seeders,
        leechers: None,
        published: None,
        category: Some(Category::Video),
        flags: BTreeSet::new(),
        sources: vec![source.into()],
        magnet: Some(format!("magnet:?xt=urn:btih:{hex}&dn={}", t.name)),
        torrent_url: None,
        info_url: None,
        span: None,
        swarm: None,
        failed: None,
    }
}

/// Four releases of Night of the Living Dead (1968), in the order the search lists them. The
/// first never sends its file list, the second holds no video, no peer has any piece of the
/// third, and the fourth plays. The search also finds the 1990 remake, which is not this film.
pub(super) fn night() -> (Harness, [FakeTorrentFile; 4]) {
    night_with(Outside::default())
}

/// `night`, with what `outside` sets.
pub(super) fn night_with(outside: Outside) -> (Harness, [FakeTorrentFile; 4]) {
    let night = [
        torrent(0xa1, "Night.of.the.Living.Dead.1968.720p.BluRay.x264", &[("Night.1968.720p.mp4", 2 * GIB)]),
        torrent(0xa2, "Night.of.the.Living.Dead.1968.DVD9", &[("VIDEO_TS/NIGHT.iso", 4 * GIB)]),
        torrent(
            0xa3,
            "Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            &[("Night.1968.1080p.mp4", 3 * GIB), ("Night.1968.1080p.srt", 50_000)],
        ),
        torrent(
            0xa4,
            "night_of_the_living_dead",
            &[("night_of_the_living_dead.mp4", 700 * MIB), ("night_of_the_living_dead.srt", 50_000)],
        ),
    ];
    let remake = torrent(0xa5, "Night.of.the.Living.Dead.1990.1080p.BluRay.x264", &[("Night.1990.mkv", 9 * GIB)]);
    let mut catalog: Vec<Release> = night[..3].iter().map(|t| listed(t, "knaben", Some(40))).collect();
    catalog.push(listed(&night[3], "archive.org", None));
    catalog.push(listed(&remake, "knaben", Some(90)));
    let script = vec![FakeSource::answers("knaben", 0), FakeSource::answers("archive.org", 0)];
    let search = search_port::Fake::scripted(catalog, script, BTreeMap::new());
    let h = Harness::with_outside(search, outside);
    for t in &night[1..] {
        h.swarm.publish(t.clone());
    }
    h.swarm.withhold(night[2].info_hash.id(), u32::MAX);
    h.swarm.set_rate(GIB);
    (h, night)
}

/// One line of a title play, short enough to compare whole.
pub(super) fn step(line: &Value) -> String {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let short = |v: &Value| text(v).chars().take(6).collect::<String>();
    match line["type"].as_str().unwrap() {
        "resolved" => format!("resolved {} {}", text(&line["video"]), text(&line["title"]["name"])),
        "searched" => format!("searched {}", line["found"]),
        "trying" => format!("trying {} of {} {}", line["rank"], line["of"], text(&line["release"]["name"])),
        "check" => format!("check {}", text(&line["check"])),
        "skipped" => format!("skipped {} {}", short(&line["release"]), line["reason"]),
        "playing" => {
            let (job, file) = match line.get("play") {
                Some(play) => (short(&play["id"]), play["video"]["index"].clone()),
                None => (short(&line["job"]), line["file"].clone()),
            };
            format!("playing {} {job} file {file} at {}", text(&line["video"]), line["resume_at"])
        }
        "failed" => {
            let tried: Vec<String> = line["tried"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| format!("{} {}", short(&t["release"]), t["reason"]))
                .collect();
            format!("failed {}: {} {tried:?}", text(&line["error"]["code"]), text(&line["error"]["message"]))
        }
        other => panic!("a title play has no {other} step"),
    }
}

impl Harness {
    /// The command that plays `files` of `torrent` from its file, by hand or as a title play
    /// tests a source for `title`, and the hold that play has on the download.
    fn play_command(&self, t: &FakeTorrentFile, files: &[u32], title: Option<&str>) -> (PlayCommand, Hold) {
        let hold = Hold::fresh();
        let link = |title: &str| TitleLink { title: title.parse().unwrap(), span: Span::Film, release: None };
        let play = PlayCommand {
            source: Source::Torrent { metainfo: t.metainfo().unwrap(), origin: Origin::File },
            to: Destination::Folder(Some(self.downloads())),
            files: files.to_vec(),
            setup_max_size: None,
            title: title.map(|title| TitlePlay { link: link(title), hold }),
            finish: Finish::Stream,
        };
        (play, hold)
    }

    pub(super) async fn play_for(
        &self,
        t: &FakeTorrentFile,
        files: &[u32],
        title: Option<&str>,
    ) -> Result<(Job, Hold), Error> {
        let (play, hold) = self.play_command(t, files, title);
        let (job, _) = self.running.ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await?;
        Ok((job, hold))
    }

    /// Gives up a source under test, as a title play does when the source does not play or the
    /// play goes away.
    async fn give_up(&self, hold: Hold) {
        self.running.ctx.owner.ask(|reply| Command::Release { hold, reply }).await.unwrap();
    }

    /// Says a download plays, as a title play does once its video has bytes.
    async fn played(&self, t: &FakeTorrentFile) {
        let id = t.info_hash.id();
        self.running.ctx.owner.ask(|reply| Command::Played { id, reply }).await.unwrap();
    }

    /// Waits long enough for the engine to stop a download and the owner to put it back.
    async fn settled(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    /// Stops the engine where it is, lets `change` at the disk, and starts it again.
    pub(super) async fn restart_after(self, change: impl FnOnce()) -> Self {
        let Self { dir, running, swarm, disk, search, outside } = self;
        running.owner.abort();
        let _ = running.owner.await;
        drop(running.ctx);
        change();
        let engine = Box::new(super::Fake::new(swarm.clone()));
        let running = Self::boot(&dir, engine, super::parse_fake, &disk, &search, &outside);
        Self { dir, running, swarm, disk, search, outside }
    }

    /// Plays a title as `POST /api/title/play` does, and answers its steps.
    pub(super) async fn play_title(
        &self,
        video: &str,
        pin: Option<&FakeTorrentFile>,
        replace: Option<TorrentId>,
    ) -> Vec<String> {
        let ask = PlayTitle {
            video: video.parse().unwrap(),
            pin: pin.map(|t| ReleaseId::Torrent(t.info_hash.id())),
            replace,
            player: Player::External,
            containers: Containers::Browser,
            finish: Finish::Stream,
        };
        let mut steps = Vec::new();
        attempt::run(&self.running.ctx, ask, |event| steps.push(step(&serde_json::to_value(event).unwrap()))).await;
        steps
    }

    /// Gets the film to keep from the search, or from `pin` alone, as `torrential get tt0063350`
    /// does into the downloads folder, and answers each step as the web app reads it.
    async fn get_film(&self, pin: Option<&FakeTorrentFile>) -> Vec<Value> {
        let ask = PlayTitle {
            video: "tt0063350".parse().unwrap(),
            pin: pin.map(|t| ReleaseId::Torrent(t.info_hash.id())),
            replace: None,
            player: Player::External,
            containers: Containers::Browser,
            finish: Finish::Keep,
        };
        let mut steps = Vec::new();
        attempt::run(&self.running.ctx, ask, |event| steps.push(serde_json::to_value(event).unwrap())).await;
        steps
    }

    async fn job(&self, t: &FakeTorrentFile) -> Option<Job> {
        self.status().await.into_iter().find(|j| j.id == t.info_hash.id())
    }
}

/// Writes `path` as the engine would while it downloads. The fake engine writes nothing.
fn write(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"payload").unwrap();
}

/// Every file, folder, and link below `root`, relative to it and sorted.
fn tree(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            found.push(path.strip_prefix(root).unwrap().display().to_string());
            if path.symlink_metadata().unwrap().is_dir() {
                folders.push(path);
            }
        }
    }
    found.sort();
    found
}

fn parts(n: u8) -> String {
    format!(".{}.parts", format!("{n:02x}").repeat(20))
}

/// Everything below `root` with what it holds: a file's bytes, a link's target, `/` for a folder.
fn contents(root: &Path) -> Vec<(String, String)> {
    let holds = |name: &str| {
        let path = root.join(name);
        match path.symlink_metadata().unwrap().file_type() {
            kind if kind.is_symlink() => format!("-> {}", std::fs::read_link(&path).unwrap().display()),
            kind if kind.is_dir() => "/".to_string(),
            _ => String::from_utf8(std::fs::read(&path).unwrap()).unwrap(),
        }
    };
    tree(root).into_iter().map(|name| (holds(&name), name)).map(|(holds, name)| (name, holds)).collect()
}

fn pairs(there: &[(&str, &str)]) -> Vec<(String, String)> {
    there.iter().map(|(name, holds)| (name.to_string(), holds.to_string())).collect()
}

/// The title each download is linked to, in the order of the list.
fn titles(jobs: &[Job]) -> Vec<Option<String>> {
    jobs.iter().map(|j| j.title.as_ref().map(|link| link.title.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_play_links_the_download_it_makes_and_one_that_was_here_without_a_title() {
    let h = Harness::new();
    let made = torrent(70, "made", &[("made.mp4", GIB)]);
    let mine = torrent(71, "mine", &[("mine.mp4", GIB)]);
    let other = torrent(69, "other", &[("other.mp4", GIB)]);
    h.get(h.torrent_file(&mine), &[]).await.unwrap();
    h.get(h.torrent_file(&other), &[]).await.unwrap();
    h.play_for(&made, &[0], Some("tt0063350")).await.unwrap();
    h.play_for(&mine, &[0], Some("tt0063350")).await.unwrap();
    let (job, _) = h.play_for(&made, &[0], Some("tt0100258")).await.unwrap();
    assert_eq!(titles(&[job]), [Some("tt0063350".to_string())], "the first title to link a download keeps it");

    let h = h.crash_and_restart().await;
    let jobs = [h.job(&made).await.unwrap(), h.job(&mine).await.unwrap(), h.job(&other).await.unwrap()];
    assert_eq!(titles(&jobs), [Some("tt0063350".to_string()), Some("tt0063350".to_string()), None]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trial_given_up_with_nothing_on_disk_leaves_the_list_and_the_download_folder_as_it_was() {
    let h = Harness::new();
    let film = torrent(72, "film", &[("film.mp4", GIB)]);
    let theirs = h.downloads().join("holiday.mp4");
    std::fs::create_dir_all(h.downloads()).unwrap();
    std::fs::write(&theirs, b"the person's own film").unwrap();
    let (job, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    assert_eq!(titles(&[job]), [Some("tt0063350".to_string())]);
    h.until("the trial runs", |jobs| jobs[0].status.name() == "downloading").await;
    h.give_up(hold).await;
    h.until("the trial leaves the list", |jobs| jobs.is_empty()).await;
    assert_eq!(tree(&h.downloads()), ["holiday.mp4"]);
    assert_eq!(std::fs::read(&theirs).unwrap(), b"the person's own film");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trial_given_up_with_anything_of_it_on_disk_stays_on_the_list_paused_and_nothing_is_deleted() {
    let film = torrent(73, "film", &[("film.mp4", GIB), ("film.srt", 10)]);
    let parts = parts(73);
    for left in [vec!["film", "film/film.mp4"], vec!["film", "film/extras"], vec![parts.as_str()]] {
        let h = Harness::new();
        let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
        h.until("the trial runs", |jobs| jobs[0].status.name() == "downloading").await;
        let last = h.downloads().join(left[left.len() - 1]);
        std::fs::create_dir_all(last.parent().unwrap()).unwrap();
        match last.extension() {
            Some(_) => std::fs::write(&last, b"what the engine wrote").unwrap(),
            None => std::fs::create_dir(&last).unwrap(),
        }
        h.give_up(hold).await;
        let jobs = h.until("the trial is paused", |jobs| jobs[0].status.name() == "paused").await;
        h.settled().await;
        assert_eq!((h.states().await, titles(&jobs)), (vec!["paused"], vec![Some("tt0063350".to_string())]));
        assert_eq!(tree(&h.downloads()), left);
        if last.is_file() {
            assert_eq!(std::fs::read(&last).unwrap(), b"what the engine wrote");
        }
        let resume = Request::Resume { refs: vec![prefix(&film)], all: false };
        assert_eq!(h.changes(resume).await.unwrap(), [(prefix(&film), "resumed".into())], "it is the person's now");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trial_given_up_after_the_engine_verified_bytes_of_it_stays_on_the_list_paused() {
    let h = Harness::new();
    let film = torrent(74, "film", &[("film.mp4", GIB), ("film.srt", 10)]);
    h.swarm.set_rate(100 * MIB);
    let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.until("bytes arrive", |jobs| jobs[0].progress.verified > 0).await;
    h.give_up(hold).await;
    h.settled().await;
    assert_eq!(h.states().await, ["paused"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn giving_up_a_source_puts_its_download_back_as_the_play_found_it() {
    let h = Harness::new();
    let empty = torrent(72, "empty", &[("empty.mp4", GIB)]);
    let made = torrent(73, "made", &[("made.mp4", GIB)]);
    let mine = torrent(74, "mine", &[("mine.mp4", GIB)]);
    let paused = torrent(75, "paused", &[("paused.mp4", GIB)]);
    let stopped = torrent(76, "stopped", &[("stopped.mp4", GIB)]);
    let finished = torrent(77, "finished", &[("finished.mp4", 1_000)]);
    let watched = torrent(78, "watched", &[("watched.mp4", GIB)]);
    h.swarm.withhold(stopped.info_hash.id(), u32::MAX);
    for t in [&mine, &paused, &stopped] {
        h.get(h.torrent_file(t), &[]).await.unwrap();
    }
    let pause = Request::Pause { refs: vec![prefix(&paused)] };
    assert_eq!(h.changes(pause).await.unwrap(), [(prefix(&paused), "paused".into())]);
    let names = |jobs: &[Job], state: &str| -> Vec<String> {
        jobs.iter().filter(|j| j.status.name() == state).filter_map(|j| j.name.clone()).collect()
    };
    let before = h.until("one stops with no source", |jobs| names(jobs, "error") == ["stopped"]).await;
    let no_source = before.iter().find(|j| j.name.as_deref() == Some("stopped")).unwrap().status.clone();

    let mut holds = Vec::new();
    for t in [&empty, &made, &mine, &paused, &stopped, &finished, &watched] {
        holds.push(h.play_for(t, &[0], Some("tt0063350")).await.unwrap().1);
    }
    h.swarm.set_rate(MIB);
    h.until("the small one is done", |jobs| names(jobs, "done") == ["finished"]).await;
    h.swarm.set_rate(0);
    h.played(&watched).await;
    let files = ["finished/finished.mp4", "made/made.mp4", "mine/mine.mp4", "paused/paused.mp4", "watched/watched.mp4"];
    for file in files {
        write(&h.downloads().join(file));
    }
    for hold in holds {
        h.give_up(hold).await;
    }
    h.until("the trial with nothing on disk leaves the list", |jobs| names(jobs, "paused").len() == 2).await;
    h.settled().await;

    let mut jobs = h.status().await;
    jobs.sort_by_key(|j| j.name.clone());
    let states: Vec<(&str, &str)> = jobs.iter().map(|j| (j.name.as_deref().unwrap(), j.status.name())).collect();
    assert_eq!(
        states,
        [
            ("finished", "done"),
            ("made", "paused"),
            ("mine", "downloading"),
            ("paused", "paused"),
            ("stopped", "error"),
            ("watched", "downloading")
        ],
        "of the two downloads the play made and never played, the one with a file stays and the other is gone"
    );
    assert_eq!(jobs[4].status, no_source, "it stopped again on the error it had");
    let mut left = vec!["finished", "made", "mine", "paused", "watched"];
    left.extend(files);
    left.sort();
    assert_eq!(tree(&h.downloads()), left, "no file is deleted");
}

/// Something a person has in the download folder.
enum Theirs {
    File(&'static str, &'static str),
    Folder(&'static str),
    Link(&'static str),
}

/// A source, what a person has where it writes, the path a refusal names, and what the download
/// folder holds afterwards.
type Taken<'a> = (&'a FakeTorrentFile, &'a [Theirs], &'a str, &'a [(&'a str, &'a str)]);

#[tokio::test(flavor = "multi_thread")]
async fn a_title_play_makes_no_download_where_anything_already_is_and_leaves_it_as_it_was() {
    use Theirs::{File, Folder, Link};
    let film = torrent(90, "film", &[("film.mp4", 21), ("film.srt", 10)]);
    let solo = torrent(91, "solo.mp4", &[("solo.mp4", 21)]);
    let parts = parts(90);
    let cases: [Taken; 8] = [
        (
            &film,
            &[Folder("film"), File("film/film.mp4", "the person's own film")],
            "film",
            &[("film", "/"), ("film/film.mp4", "the person's own film")],
        ),
        (
            &film,
            &[Folder("film"), File("film/film.mp4", "another cut")],
            "film",
            &[("film", "/"), ("film/film.mp4", "another cut")],
        ),
        (&film, &[Folder("film")], "film", &[("film", "/")]),
        (&film, &[Link("film")], "film", &[("film", "-> ../elsewhere")]),
        (&film, &[File(".5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a.parts", "pieces")], &parts, &[(&parts, "pieces")]),
        (&solo, &[File("solo.mp4", "the person's own film")], "solo.mp4", &[("solo.mp4", "the person's own film")]),
        (&solo, &[File("solo.mp4", "another cut")], "solo.mp4", &[("solo.mp4", "another cut")]),
        (&solo, &[Link("solo.mp4")], "solo.mp4", &[("solo.mp4", "-> ../elsewhere")]),
    ];
    for (source, theirs, named, there) in cases {
        let h = Harness::new();
        let downloads = h.downloads();
        std::fs::create_dir_all(&downloads).unwrap();
        for thing in theirs {
            match thing {
                File(at, bytes) => std::fs::write(downloads.join(at), bytes).unwrap(),
                Folder(at) => std::fs::create_dir(downloads.join(at)).unwrap(),
                Link(at) => std::os::unix::fs::symlink("../elsewhere", downloads.join(at)).unwrap(),
            }
        }
        let refused = h.play_for(source, &[0], Some("tt0063350")).await.unwrap_err();
        let at = downloads.join(named);
        assert_eq!(
            (refused.code, refused.message, refused.details),
            (
                ErrorCode::PathConflict,
                format!("{} already exists, and playing a title writes only where nothing is", at.display()),
                Some(Details::Paths(vec![at.display().to_string()]))
            )
        );
        assert_eq!((h.states().await, contents(&downloads)), (Vec::new(), pairs(there)), "{named}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_or_get_by_hand_still_starts_over_a_file_that_is_there() {
    let h = Harness::new();
    let film = torrent(92, "film", &[("film.mp4", 21)]);
    let solo = torrent(93, "solo.mp4", &[("solo.mp4", 21)]);
    std::fs::create_dir_all(h.downloads().join("film")).unwrap();
    std::fs::write(h.downloads().join("film/film.mp4"), "the person's own film").unwrap();
    std::fs::write(h.downloads().join("solo.mp4"), "the person's own film").unwrap();
    assert_eq!(h.play(h.torrent_file(&film), &[0]).await.unwrap().job.id, film.info_hash.id());
    assert_eq!(h.get(h.torrent_file(&solo), &[]).await.unwrap().job.id, solo.info_hash.id());
    let (joined, _) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    assert_eq!(joined.id, film.info_hash.id(), "a title play joins a download that is on the list");
}

#[tokio::test(flavor = "multi_thread")]
async fn getting_a_title_skips_a_source_whose_folder_already_exists_and_gets_the_next() {
    let (h, night) = night();
    let theirs = h.downloads().join("Night.of.the.Living.Dead.1968.1080p.WEB.x264");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("Night.1968.1080p.mp4"), "the person's own film").unwrap();
    let steps = h.get_film(None).await;
    let third = night[2].info_hash.id().to_hex();
    let skipped = steps.iter().find(|s| s["type"] == "skipped" && s["release"] == third.as_str());
    assert_eq!(
        skipped.map(|s| &s["reason"]),
        Some(&serde_json::json!({
            "reason": "failed",
            "error": {
                "code": "path_conflict",
                "message": format!(
                    "{} already exists, and playing a title writes only where nothing is",
                    theirs.display()
                ),
                "hint": "move it away, or run torrential get a3a3a3 to download this torrent yourself",
                "details": { "paths": [theirs.display().to_string()] },
            }
        }))
    );
    assert_eq!(step(steps.last().unwrap()), "playing tt0063350 a4a4a4 file 0 at null");
    let left = h.until("the other trials are gone", |jobs| jobs.len() == 1).await;
    assert_eq!(left[0].id, night[3].info_hash.id(), "the source in the way never became a download");
    assert_eq!(contents(&theirs), pairs(&[("Night.1968.1080p.mp4", "the person's own film")]));
}

#[tokio::test(flavor = "multi_thread")]
async fn getting_a_title_skips_a_source_when_the_disk_cannot_say_that_its_path_is_free() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (h, night) = night();
    let downloads = h.downloads();
    let theirs = downloads.join("Night.of.the.Living.Dead.1968.1080p.WEB.x264");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("Night.1968.1080p.mp4"), "the person's own film").unwrap();
    let shut = |mode| std::fs::set_permissions(&downloads, std::fs::Permissions::from_mode(mode)).unwrap();
    shut(0o000);
    let steps = h.get_film(Some(&night[2])).await;
    shut(0o755);
    let reason = &steps[steps.len() - 2]["reason"];
    let message = format!(
        "cannot see whether {} exists, and playing a title writes only where nothing is: \
         Permission denied (os error 13)",
        theirs.display()
    );
    assert_eq!(
        (reason["reason"].as_str(), reason["error"]["code"].as_str(), reason["error"]["message"].as_str()),
        (Some("failed"), Some("path_conflict"), Some(message.as_str()))
    );
    assert_eq!(steps.last().unwrap()["type"], "failed", "the source is not played");
    assert_eq!(h.states().await, Vec::<&str>::new());
    assert_eq!(contents(&theirs), pairs(&[("Night.1968.1080p.mp4", "the person's own film")]));
}

fn pack() -> FakeTorrentFile {
    torrent(84, "Harbor.Lights.S01", &[("Harbor.Lights.S01E01.mp4", GIB), ("Harbor.Lights.S01E02.mp4", GIB)])
}

const PACK: [&str; 3] =
    ["Harbor.Lights.S01", "Harbor.Lights.S01/Harbor.Lights.S01E01.mp4", "Harbor.Lights.S01/Harbor.Lights.S01E02.mp4"];

#[tokio::test(flavor = "multi_thread")]
async fn a_get_by_hand_makes_a_download_under_test_the_persons() {
    for patterns in [&["*"][..], &[]] {
        let h = Harness::new();
        let (pack, downloads) = (pack(), h.downloads());
        let (_, creator) = h.play_for(&pack, &[0], Some("tt9999901")).await.unwrap();
        h.get(h.torrent_file(&pack), patterns).await.unwrap();
        h.until("it runs", |jobs| jobs[0].status.name() == "downloading").await;
        write(&downloads.join(PACK[1]));
        write(&downloads.join(PACK[2]));
        h.give_up(creator).await;
        h.settled().await;
        assert_eq!(
            (h.states().await, tree(&downloads)),
            (vec!["downloading"], PACK.map(String::from).to_vec()),
            "getting {patterns:?}"
        );
    }
}

/// What a download was before a title play tested it.
#[derive(Clone, Copy, Debug)]
enum Was {
    Made,
    Paused,
    Stopped,
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_a_person_names_in_a_command_during_a_trial_is_left_as_they_left_it() {
    use Was::{Made, Paused, Stopped};
    let cases = [
        (Made, "get", "downloading"),
        (Made, "pause", "paused"),
        (Made, "resume", "downloading"),
        (Made, "verify", "downloading"),
        (Paused, "get", "downloading"),
        (Paused, "pause", "paused"),
        (Paused, "resume", "downloading"),
        (Paused, "verify", "downloading"),
        (Stopped, "get", "downloading"),
        (Stopped, "resume", "downloading"),
    ];
    for (was, command, left) in cases {
        let h = Harness::new();
        let film = torrent(94, "film", &[("film.mp4", GIB)]);
        let refs = vec![prefix(&film)];
        match was {
            Made => {}
            Paused => {
                h.get(h.torrent_file(&film), &[]).await.unwrap();
                h.changes(Request::Pause { refs: refs.clone() }).await.unwrap();
            }
            Stopped => {
                h.swarm.withhold(film.info_hash.id(), u32::MAX);
                h.get(h.torrent_file(&film), &[]).await.unwrap();
                h.until("it stops with no source", |jobs| jobs[0].status.name() == "error").await;
                h.swarm.withhold(film.info_hash.id(), 0);
            }
        }
        let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
        h.until("the trial runs", |jobs| jobs[0].status.name() == "downloading").await;
        let changed = match command {
            "get" => h.get(h.torrent_file(&film), &[]).await.map(drop),
            "pause" => h.changes(Request::Pause { refs }).await.map(drop),
            "resume" => h.changes(Request::Resume { refs, all: false }).await.map(drop),
            _ => h.changes(Request::Verify { reference: prefix(&film), repair: false }).await.map(drop),
        };
        assert_eq!(changed, Ok(()), "{command}");
        h.give_up(hold).await;
        h.settled().await;
        h.until("a check ends", |jobs| jobs.iter().all(|j| j.status.name() != "checking")).await;
        assert_eq!(h.states().await, [left], "{command} on a download that was {was:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_for_every_download_leaves_a_trial_to_the_play_testing_it() {
    let h = Harness::new();
    let film = torrent(95, "film", &[("film.mp4", GIB)]);
    let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.until("the trial runs", |jobs| jobs[0].status.name() == "downloading").await;
    let resume = Request::Resume { refs: Vec::new(), all: true };
    assert_eq!(h.changes(resume).await.unwrap(), Vec::<(String, String)>::new());
    h.give_up(hold).await;
    h.until("the trial leaves the list", |jobs| jobs.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_of_another_title_holds_the_download_until_it_gives_up_too() {
    let h = Harness::new();
    let film = torrent(85, "film", &[("film.mp4", GIB)]);
    let (_, creator) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    let (_, other) = h.play_for(&film, &[0], Some("tt0100258")).await.unwrap();
    h.until("it runs", |jobs| jobs[0].status.name() == "downloading").await;
    h.give_up(creator).await;
    h.settled().await;
    assert_eq!(h.states().await, ["downloading"]);
    h.give_up(other).await;
    h.until("it leaves the list", |jobs| jobs.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_left_under_test_when_the_engine_stopped_stays_as_it_is() {
    let h = Harness::new();
    let film = torrent(86, "film", &[("film.mp4", GIB)]);
    h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    let h = h.crash_and_restart().await;
    let (_, later) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.give_up(later).await;
    h.settled().await;
    assert_eq!(h.states().await, ["downloading"], "the play that made it is gone, so it is no play's to remove");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_whose_answer_cannot_be_delivered_gives_its_source_up_at_once() {
    let h = Harness::new();
    let made = torrent(87, "made", &[("made.mp4", GIB)]);
    let mine = torrent(88, "mine", &[("mine.mp4", GIB)]);
    h.get(h.torrent_file(&mine), &[]).await.unwrap();
    let pause = Request::Pause { refs: vec![prefix(&mine)] };
    assert_eq!(h.changes(pause).await.unwrap(), [(prefix(&mine), "paused".into())]);
    for t in [&made, &mine] {
        let (play, _) = h.play_command(t, &[0], Some("tt0063350"));
        let (reply, gone) = tokio::sync::oneshot::channel();
        drop(gone);
        h.running.ctx.owner.tell(Command::Play { play: Box::new(play), reply });
    }
    let jobs = h.until("the download the play made is gone", |jobs| jobs.len() == 1).await;
    let states: Vec<(String, &str)> = jobs.iter().map(|j| (j.name.clone().unwrap(), j.status.name())).collect();
    assert_eq!(states, [("mine".to_string(), "paused")], "the other is paused again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hold_given_up_twice_or_never_taken_changes_nothing() {
    let h = Harness::new();
    let film = torrent(81, "film", &[("film.mp4", GIB)]);
    let (_, first) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    let (_, second) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.until("it runs", |jobs| jobs[0].status.name() == "downloading").await;
    h.give_up(Hold::fresh()).await;
    h.give_up(first).await;
    h.give_up(first).await;
    h.settled().await;
    assert_eq!(h.states().await, ["downloading"], "the second play still tests it");
    h.give_up(second).await;
    h.until("it leaves the list", |jobs| jobs.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_two_plays_test_stays_until_both_give_up_and_stays_for_good_once_it_plays() {
    let film = torrent(82, "film", &[("film.mp4", GIB)]);
    let shared = |written: bool| {
        let film = &film;
        async move {
            let h = Harness::new();
            let (_, first) = h.play_for(film, &[0], Some("tt0063350")).await.unwrap();
            let (_, second) = h.play_for(film, &[0], Some("tt0063350")).await.unwrap();
            h.until("it runs", |jobs| jobs[0].status.name() == "downloading").await;
            if written {
                write(&h.downloads().join("film/film.mp4"));
                write(&h.downloads().join(parts(82)));
            }
            (h, first, second)
        }
    };
    let there = || vec![parts(82), "film".to_string(), "film/film.mp4".to_string()];

    let (h, first, second) = shared(true).await;
    h.give_up(first).await;
    h.settled().await;
    assert_eq!((h.states().await, tree(&h.downloads())), (vec!["downloading"], there()), "the second still tests it");
    h.played(&film).await;
    h.give_up(second).await;
    h.settled().await;
    assert_eq!((h.states().await, tree(&h.downloads())), (vec!["downloading"], there()), "it plays, so it stays");

    let (h, first, second) = shared(true).await;
    h.played(&film).await;
    h.give_up(first).await;
    h.give_up(second).await;
    h.settled().await;
    assert_eq!((h.states().await, tree(&h.downloads())), (vec!["downloading"], there()), "it played first");

    let (h, first, second) = shared(true).await;
    h.give_up(second).await;
    h.give_up(first).await;
    h.settled().await;
    assert_eq!((h.states().await, tree(&h.downloads())), (vec!["paused"], there()), "neither played it");

    let (h, first, second) = shared(false).await;
    h.give_up(second).await;
    h.settled().await;
    assert_eq!(h.states().await, ["downloading"], "the first still tests it");
    h.give_up(first).await;
    h.until("neither played it and it wrote nothing, so it leaves the list", |jobs| jobs.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_downloaded_with_get_comes_in_plain_order_until_it_is_played() {
    let (h, night) = night();
    let id = night[3].info_hash.id();
    let ask = || TitleRequest { video: "tt0063350".parse().unwrap(), source: Some(prefix(&night[3])) };
    let Response::GetTitle { reply, .. } = h.ask(Request::GetTitle(ask())).await else { panic!("get answers a title") };
    assert_eq!((reply.job.id, h.swarm.streamed(id)), (id, false));
    let Response::PlayTitle { reply, .. } = h.ask(Request::PlayTitle(ask())).await else {
        panic!("play answers a title")
    };
    assert_eq!((reply.job.id, h.swarm.streamed(id)), (id, true));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_played_and_was_paused_is_paused_again_when_a_later_play_gives_it_up() {
    let h = Harness::new();
    let film = torrent(83, "film", &[("film.mp4", GIB)]);
    h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.played(&film).await;
    let pause = Request::Pause { refs: vec![prefix(&film)] };
    assert_eq!(h.changes(pause).await.unwrap(), [(prefix(&film), "paused".into())]);
    let (_, later) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.until("the later play resumes it", |jobs| jobs[0].status.name() == "downloading").await;
    h.give_up(later).await;
    assert_eq!(h.states().await, ["paused"]);
}

/// Starts a play of the film from `source` alone, and answers it once it waits for the source's
/// first bytes.
async fn testing(h: &Harness, source: &FakeTorrentFile) -> tokio::task::JoinHandle<()> {
    let ctx = h.running.ctx.clone();
    let ask = PlayTitle {
        video: "tt0063350".parse().unwrap(),
        pin: Some(ReleaseId::Torrent(source.info_hash.id())),
        replace: None,
        player: Player::External,
        containers: Containers::Browser,
        finish: Finish::Stream,
    };
    let (told, mut steps) = tokio::sync::mpsc::unbounded_channel();
    let play = tokio::spawn(async move {
        let report = move |event| drop(told.send(event));
        attempt::run(&ctx, ask, report).await
    });
    while let Some(event) = steps.recv().await {
        if matches!(event, AttemptEvent::Check { check: Check::FirstBytes }) {
            return play;
        }
    }
    panic!("the play ended before it waited for first bytes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_that_goes_away_leaves_the_source_another_play_tests_and_the_last_to_go_stops_it() {
    let patient = Timing { first_bytes: std::time::Duration::from_secs(10), ..TIMING };
    let (h, night) = night_with(Outside { timing: patient, ..Outside::default() });
    h.swarm.set_rate(0);
    let first = testing(&h, &night[3]).await;
    let second = testing(&h, &night[3]).await;
    let folder = h.play_folder(&night[3]);
    write(&folder.join("night_of_the_living_dead/night_of_the_living_dead.mp4"));
    let on_disk = ["night_of_the_living_dead", "night_of_the_living_dead/night_of_the_living_dead.mp4"];

    first.abort();
    let _ = first.await;
    h.settled().await;
    // The file written above is checked before the download goes on, and a slow machine is still checking here.
    h.until("its check ends", |jobs| jobs[0].status.name() == "downloading").await;
    assert_eq!((h.states().await, tree(&folder)), (vec!["downloading"], on_disk.map(String::from).to_vec()));

    second.abort();
    let _ = second.await;
    h.until("the last play to go pauses it", |jobs| jobs[0].status.name() == "paused").await;
    h.settled().await;
    assert_eq!((h.states().await, tree(&folder)), (vec!["paused"], on_disk.map(String::from).to_vec()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_played_by_hand_stays_as_it_is_when_the_play_testing_it_gives_up() {
    let h = Harness::new();
    let film = torrent(77, "film", &[("film.mp4", GIB)]);
    let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.play(super::magnet(&film), &[0]).await.unwrap();
    h.give_up(hold).await;
    h.settled().await;
    assert_eq!(h.states().await, ["downloading"]);
}

impl Harness {
    /// Keeps every write to the engine's state from landing until the answer is dropped, as
    /// another process holding the database does.
    fn lock_state(&self) -> rusqlite::Connection {
        let other = rusqlite::Connection::open(self.dir.path().join("state.db")).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        other
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_whose_playing_cannot_be_recorded_keeps_its_hold_and_can_still_give_the_source_up() {
    let h = Harness::new();
    let film = torrent(96, "film", &[("film.mp4", GIB)]);
    let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.until("the trial runs", |jobs| jobs[0].status.name() == "downloading").await;
    let locked = h.lock_state();
    let id = film.info_hash.id();
    let recorded = h.running.ctx.owner.ask(|reply| Command::Played { id, reply }).await;
    assert_eq!(recorded.map_err(|e| e.code), Err(ErrorCode::StateCorrupt));
    drop(locked);
    h.give_up(hold).await;
    h.until("the trial leaves the list", |jobs| jobs.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trial_whose_pause_cannot_be_recorded_is_put_back_at_the_next_release_or_when_the_engine_stops() {
    let film = torrent(97, "film", &[("film.mp4", GIB)]);
    let given_up_while_locked = || async {
        let h = Harness::new();
        let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
        h.until("the trial runs", |jobs| jobs[0].status.name() == "downloading").await;
        let locked = h.lock_state();
        h.give_up(hold).await;
        drop(locked);
        h.settled().await;
        assert_eq!(h.states().await, ["downloading"], "the pause did not land, so the trial still runs");
        h
    };

    let h = given_up_while_locked().await;
    h.give_up(Hold::fresh()).await;
    h.until("the trial leaves the list", |jobs| jobs.is_empty()).await;

    let h = given_up_while_locked().await;
    let (reply, stopped) = tokio::sync::oneshot::channel();
    h.running.ctx.owner.tell(Command::Shutdown { reply });
    stopped.await.unwrap();
    let h = h.restart_after(|| ()).await;
    assert_eq!(h.states().await, Vec::<&str>::new());
}

/// A trial given up while the engine has yet to say it stopped writing, so what the trial is
/// owed still waits.
async fn stopped_before_it_was_put_back(written: Option<&[u8]>) -> (Harness, PathBuf) {
    let gate = Gate::default();
    let engine = Gated { port: FakeEngine::new(Swarm::new(0)), gate: gate.clone() };
    let h = Harness::with_engine(|_| Box::new(engine), parse_fake);
    let film = torrent(80, "film", &[("film.mp4", GIB)]);
    let (_, hold) = h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.until("the trial runs", |jobs| jobs[0].status.name() == "downloading").await;
    let file = h.downloads().join("film/film.mp4");
    std::fs::create_dir_all(h.downloads()).unwrap();
    if let Some(bytes) = written {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, bytes).unwrap();
    }
    gate.close();
    h.give_up(hold).await;
    gate.until_held(1).await;
    (h, file)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_while_a_trial_waits_to_be_put_back_changes_nothing_on_disk() {
    let (h, file) = stopped_before_it_was_put_back(Some(b"what the engine wrote")).await;
    let h = h.restart_after(|| ()).await;
    assert_eq!(std::fs::read(&file).unwrap(), b"what the engine wrote");
    assert_eq!(
        (h.states().await, tree(&h.downloads())),
        (vec!["paused"], vec!["film".to_string(), "film/film.mp4".into()])
    );

    let (h, _) = stopped_before_it_was_put_back(None).await;
    let h = h.restart_after(|| ()).await;
    h.settled().await;
    assert_eq!(tree(&h.downloads()), Vec::<String>::new());
    assert_eq!(h.states().await, ["paused"], "what the trial was owed is forgotten, so it stays on the list");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_episode_in_a_season_pack_plays_the_file_whose_name_reads_as_it() {
    let pack = torrent(
        0xb1,
        "Harbor.Lights.S01.1080p.WEB.x264",
        &[
            ("Harbor.Lights.S01E01.1080p.WEB.x264.mp4", 900 * MIB),
            ("Harbor.Lights.S01E02.1080p.WEB.x264.mp4", 900 * MIB),
            ("Harbor.Lights.S01E02.1080p.WEB.x264.srt", 40_000),
            ("Harbor.Lights.S01E03.1080p.WEB.x264.mp4", 900 * MIB),
        ],
    );
    let other = torrent(0xb2, "Harbor.Lights.S02E01.1080p.WEB.x264", &[("Harbor.Lights.S02E01.mp4", 900 * MIB)]);
    let catalog = vec![listed(&other, "knaben", Some(30)), listed(&pack, "knaben", Some(20))];
    let h = Harness::with_search(search_port::Fake::scripted(
        catalog,
        vec![FakeSource::answers("knaben", 0)],
        BTreeMap::new(),
    ));
    h.swarm.publish(pack.clone());
    h.swarm.set_rate(GIB);
    assert_eq!(
        h.play_title("tt9999901:1:2", None, None).await,
        [
            "resolved tt9999901:1:2 Harbor Lights",
            "searched 1",
            "trying 1 of 1 Harbor.Lights.S01.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt9999901:1:2 b1b1b1 file 1 at null",
        ]
    );
    let job = h.job(&pack).await.unwrap();
    let link = job.title.unwrap();
    assert_eq!(
        (job.selection, link.title.as_str(), link.span.to_string()),
        (
            torrential_core::Selection::Patterns(vec![
                "Harbor.Lights.S01E02.1080p.WEB.x264.mp4".into(),
                "Harbor.Lights.S01E02.1080p.WEB.x264.srt".into()
            ]),
            "tt9999901",
            "s1".to_string()
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_made_by_hand_is_tried_first_and_kept_when_it_does_not_start() {
    let (h, night) = night();
    h.get(h.torrent_file(&night[2]), &[]).await.unwrap();
    let mine = h.downloads().join("Night.of.the.Living.Dead.1968.1080p.WEB.x264");
    write(&mine.join("Night.1968.1080p.mp4"));
    write(&mine.join("Night.1968.1080p.srt"));
    let steps = h.play_title("tt0063350", None, None).await;
    assert_eq!(
        steps,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 4",
            "trying 1 of 4 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            r#"skipped a3a3a3 {"reason":"no_data","waited_s":1}"#,
            "trying 2 of 4 Night.of.the.Living.Dead.1968.720p.BluRay.x264",
            "check file_list",
            r#"skipped a1a1a1 {"reason":"no_file_list","waited_s":1}"#,
            "trying 3 of 4 Night.of.the.Living.Dead.1968.DVD9",
            "check file_list",
            r#"skipped a2a2a2 {"reason":"no_video"}"#,
            "trying 4 of 4 night_of_the_living_dead",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 a4a4a4 file 0 at null",
        ]
    );
    assert_eq!(titles(&[h.job(&night[2]).await.unwrap()]), [Some("tt0063350".to_string())]);
    assert_eq!(tree(&mine), ["Night.1968.1080p.mp4", "Night.1968.1080p.srt"], "its files stay");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_here_that_holds_the_film_plays_before_any_search() {
    let (h, night) = night();
    h.play_for(&night[3], &[0], Some("tt0063350")).await.unwrap();
    assert_eq!(
        h.play_title("tt0063350", None, None).await,
        [
            "resolved tt0063350 Night of the Living Dead",
            "trying 1 of 1 night_of_the_living_dead",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 a4a4a4 file 0 at null",
        ]
    );
    assert_eq!(h.search.searches(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_whose_download_is_here_plays_while_the_catalog_search_is_unreachable() {
    let h = Harness::with_titles(Arc::new(catalog_port::Fake::without_search()));
    let film = torrent(0xa4, "night_of_the_living_dead", &[("night_of_the_living_dead.mp4", 700 * MIB)]);
    h.swarm.publish(film.clone());
    h.swarm.set_rate(GIB);
    h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.played(&film).await;
    assert_eq!(
        h.play_title("tt0063350", None, None).await,
        [
            "resolved tt0063350 Night of the Living Dead",
            "trying 1 of 1 night_of_the_living_dead",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 a4a4a4 file 0 at null",
        ]
    );
    assert_eq!(
        h.play_title("tt0100258", None, None).await,
        [
            "resolved tt0100258 Night of the Living Dead",
            "failed fetch_failed: cannot search IMDb or Cinemeta: no network []",
        ],
        "a title with no download here needs the search"
    );
    assert_eq!(h.search.searches(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_here_that_does_not_start_is_followed_by_the_search_and_never_tried_again() {
    let (h, night) = night();
    h.play_for(&night[2], &[0], Some("tt0063350")).await.unwrap();
    assert_eq!(
        h.play_title("tt0063350", None, None).await,
        [
            "resolved tt0063350 Night of the Living Dead",
            "trying 1 of 1 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            r#"skipped a3a3a3 {"reason":"no_data","waited_s":1}"#,
            "searched 4",
            "trying 2 of 4 Night.of.the.Living.Dead.1968.720p.BluRay.x264",
            "check file_list",
            r#"skipped a1a1a1 {"reason":"no_file_list","waited_s":1}"#,
            "trying 3 of 4 Night.of.the.Living.Dead.1968.DVD9",
            "check file_list",
            r#"skipped a2a2a2 {"reason":"no_video"}"#,
            "trying 4 of 4 night_of_the_living_dead",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 a4a4a4 file 0 at null",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_here_that_stopped_on_an_error_waits_for_the_search() {
    let (h, night) = night();
    h.play_for(&night[2], &[0], Some("tt0063350")).await.unwrap();
    h.until("it stops with no source", |jobs| jobs[0].status.name() == "error").await;
    assert_eq!(
        h.play_title("tt0063350", None, None).await[..3],
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 4",
            "trying 1 of 4 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_whose_file_list_arrives_in_time_plays_while_a_slow_disk_holds_up_the_engine() {
    let film = torrent(0xb7, "Night.of.the.Living.Dead.1968.1080p.WEB.x264", &[("Night.1968.1080p.mp4", 3 * GIB)]);
    let catalog = vec![listed(&film, "knaben", Some(40))];
    let search = search_port::Fake::scripted(catalog, vec![FakeSource::answers("knaben", 0)], BTreeMap::new());
    let gate = Gate::holding(|event| matches!(event, EngineEvent::Metadata { .. }));
    let h = Harness::with_outside(search, Outside { gate: Some(gate.clone()), ..Outside::default() });
    h.swarm.publish(film.clone());
    h.swarm.set_rate(GIB);
    gate.close();
    let slow_disk = async {
        gate.until_held(1).await;
        let locked = h.lock_state();
        gate.open();
        tokio::time::sleep(TIMING.file_list + std::time::Duration::from_millis(200)).await;
        drop(locked);
    };

    let (steps, ()) = tokio::join!(h.play_title("tt0063350", None, None), slow_disk);

    assert_eq!(
        steps,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 1",
            "trying 1 of 1 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 b7b7b7 file 0 at null",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_starts_on_a_lawful_catalog_at_once_and_does_not_wait_for_a_slow_index() {
    let dead = torrent(0xc1, "Night.of.the.Living.Dead.1968.DVDRip.x264", &[("Night.1968.mp4", 700 * MIB)]);
    let good = torrent(0xc2, "Night.of.the.Living.Dead.1968.1080p.WEB.x264", &[("Night.1968.1080p.mp4", 3 * GIB)]);
    let catalog = vec![listed(&dead, "archive.org", None), listed(&good, "knaben", Some(40))];
    let script = vec![
        FakeSource::answers("archive.org", 0),
        FakeSource::answers("knaben", 300),
        FakeSource::answers("nyaa", 5000),
    ];
    let h = Harness::with_search(search_port::Fake::scripted(catalog, script, BTreeMap::new()));
    h.swarm.publish(good.clone());
    h.swarm.set_rate(GIB);
    let started = std::time::Instant::now();
    assert_eq!(
        h.play_title("tt0063350", None, None).await,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 1",
            "trying 1 of 1 Night.of.the.Living.Dead.1968.DVDRip.x264",
            "check file_list",
            r#"skipped c1c1c1 {"reason":"no_file_list","waited_s":1}"#,
            "trying 2 of 2 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 c2c2c2 file 0 at null",
        ]
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(4), "it took {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn three_dead_swarms_in_a_row_cost_one_wait_for_a_file_list() {
    let name = |n: u8, screen: &str| (n, format!("Night.of.the.Living.Dead.1968.{screen}.WEB.x264"));
    let night = [name(0xd1, "480p"), name(0xd2, "576p"), name(0xd3, "720p"), name(0xd4, "1080p")]
        .map(|(n, name)| torrent(n, &name, &[("Night.1968.mp4", GIB)]));
    let catalog = night.iter().map(|t| listed(t, "knaben", Some(40))).collect();
    let search = search_port::Fake::scripted(catalog, vec![FakeSource::answers("knaben", 0)], BTreeMap::new());
    let file_list = std::time::Duration::from_secs(1);
    let h = Harness::with_outside(search, Outside { timing: Timing { file_list, ..TIMING }, ..Outside::default() });
    h.swarm.publish(night[3].clone());
    h.swarm.set_rate(GIB);
    let started = std::time::Instant::now();
    let steps = h.play_title("tt0063350", None, None).await;
    let took = started.elapsed();
    assert_eq!(
        steps,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 4",
            "trying 1 of 4 Night.of.the.Living.Dead.1968.480p.WEB.x264",
            "check file_list",
            r#"skipped d1d1d1 {"reason":"no_file_list","waited_s":1}"#,
            "trying 2 of 4 Night.of.the.Living.Dead.1968.576p.WEB.x264",
            "check file_list",
            r#"skipped d2d2d2 {"reason":"no_file_list","waited_s":1}"#,
            "trying 3 of 4 Night.of.the.Living.Dead.1968.720p.WEB.x264",
            "check file_list",
            r#"skipped d3d3d3 {"reason":"no_file_list","waited_s":1}"#,
            "trying 4 of 4 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 d4d4d4 file 0 at null",
        ]
    );
    assert!(took < 2 * file_list, "the play took {took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_download_a_trial_resumes_is_paused_again_when_it_does_not_start() {
    let (h, night) = night();
    h.get(h.torrent_file(&night[2]), &[]).await.unwrap();
    let pause = torrential_core::protocol::Request::Pause { refs: vec![prefix(&night[2])] };
    assert_eq!(h.changes(pause).await.unwrap(), [(prefix(&night[2]), "paused".into())]);
    assert_eq!(
        h.play_title("tt0063350", Some(&night[2]), None).await[1..7],
        [
            "searched 4",
            "trying 1 of 1 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            r#"skipped a3a3a3 {"reason":"no_data","waited_s":1}"#,
        ]
    );
    assert_eq!(h.states().await, ["paused"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_plays_another_source_and_then_pauses_the_one_that_stalled() {
    let (h, night) = night();
    h.swarm.withhold(night[2].info_hash.id(), 0);
    h.swarm.set_rate(100 * MIB);
    let first = h.play_title("tt0063350", None, None).await;
    assert_eq!(first.last().unwrap(), "playing tt0063350 a3a3a3 file 0 at null");
    let stalled = night[2].info_hash.id();
    assert_eq!(
        h.play_title("tt0063350", None, Some(stalled)).await,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 4",
            "trying 1 of 3 night_of_the_living_dead",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 a4a4a4 file 0 at null",
        ],
        "the sources that failed a moment ago go last"
    );
    let states: Vec<(String, &str)> =
        h.status().await.iter().map(|j| (j.id.to_hex()[..6].to_string(), j.status.name())).collect();
    assert_eq!(states, [("a3a3a3".to_string(), "paused"), ("a4a4a4".to_string(), "downloading")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_skips_a_release_whose_torrent_file_turns_out_to_be_the_stalled_download() {
    let stalled = torrent(0xe1, "Night.of.the.Living.Dead.1968.1080p.WEB.x264", &[("Night.1968.1080p.mp4", 3 * GIB)]);
    let other = torrent(0xe2, "night_of_the_living_dead", &[("night_of_the_living_dead.mp4", 700 * MIB)]);
    let url = "https://index.example/night.torrent";
    let by_link = Release {
        id: ReleaseId::Locator(LocatorId::from_bytes([0xee; 20])),
        magnet: None,
        torrent_url: Some(url.into()),
        ..listed(&stalled, "knaben", Some(40))
    };
    let catalog = vec![by_link, listed(&other, "archive.org", None)];
    let script = vec![FakeSource::answers("knaben", 0), FakeSource::answers("archive.org", 0)];
    let torrents = BTreeMap::from([(url.to_string(), stalled.bytes())]);
    let h = Harness::with_search(search_port::Fake::scripted(catalog, script, torrents));
    h.swarm.publish(stalled.clone());
    h.swarm.publish(other.clone());
    h.swarm.set_rate(100 * MIB);
    let first = h.play_title("tt0063350", None, None).await;
    assert_eq!(first.last().unwrap(), "playing tt0063350 e1e1e1 file 0 at null");
    assert_eq!(
        h.play_title("tt0063350", None, Some(stalled.info_hash.id())).await,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 2",
            "trying 1 of 2 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            r#"skipped ueeeee {"reason":"stalled"}"#,
            "trying 2 of 2 night_of_the_living_dead",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 e2e2e2 file 0 at null",
        ]
    );
    let states: Vec<(String, &str)> =
        h.status().await.iter().map(|j| (j.id.to_hex()[..6].to_string(), j.status.name())).collect();
    assert_eq!(states, [("e1e1e1".to_string(), "paused"), ("e2e2e2".to_string(), "downloading")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_with_no_other_source_fails_and_leaves_the_stalled_download_alone() {
    let (h, night) = night();
    h.swarm.set_rate(100 * MIB);
    let first = h.play_title("tt0063350", None, None).await;
    assert_eq!(first.last().unwrap(), "playing tt0063350 a4a4a4 file 0 at null");
    let before = h.job(&night[3]).await.unwrap();
    let steps = h.play_title("tt0063350", None, Some(night[3].info_hash.id())).await;
    assert_eq!(
        steps.last().unwrap(),
        concat!(
            "failed no_source: no other source of Night of the Living Dead (1968) plays now [",
            r#""a1a1a1 {\"reason\":\"no_file_list\",\"waited_s\":1}", "#,
            r#""a2a2a2 {\"reason\":\"no_video\"}", "#,
            r#""a3a3a3 {\"reason\":\"no_data\",\"waited_s\":1}"]"#
        )
    );
    let after = h.job(&night[3]).await.unwrap();
    assert_eq!(
        (after.status.name(), after.selection, after.title),
        (before.status.name(), before.selection, before.title)
    );
    h.until("the trials it made are gone", |jobs| jobs.len() == 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_source_that_fails_is_the_only_one_tried() {
    let (h, night) = night();
    assert_eq!(
        h.play_title("tt0063350", Some(&night[2]), None).await,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 4",
            "trying 1 of 1 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            r#"skipped a3a3a3 {"reason":"no_data","waited_s":1}"#,
            concat!(
                "failed no_source: this source of Night of the Living Dead (1968) does not play now ",
                r#"["a3a3a3 {\"reason\":\"no_data\",\"waited_s\":1}"]"#
            ),
        ]
    );
    h.until("the download the trial made leaves the list", |jobs| jobs.is_empty()).await;
}
