//! Downloads made by playing a title: their link to it, and what discarding one deletes.

use std::path::{Path, PathBuf};

use torrential_core::title::Span;
use torrential_core::{Error, ErrorCode, Job, Lease, TitleLink, TorrentId};

use super::super::admission::GIB;
use super::super::engine_port::FakeTorrentFile;
use super::super::owner::{Command, Created, Origin, PlayCommand, Source};
use super::super::store::{Discard, Store};
use super::{Harness, prefix, torrent};

impl Harness {
    /// Plays `files` of `torrent` from its file, for `title` when given, as a title play does.
    async fn play_for(&self, t: &FakeTorrentFile, files: &[u32], title: Option<&str>) -> Result<(Job, Created), Error> {
        let play = PlayCommand {
            source: Source::Torrent { metainfo: t.metainfo().unwrap(), origin: Origin::File },
            dir: Some(self.downloads()),
            files: files.to_vec(),
            setup_max_size: None,
            title: title.map(|title| TitleLink {
                title: title.parse().unwrap(),
                span: Span::Film,
                release: None,
                lease: Lease::Borrowed,
            }),
        };
        let (job, _, created) =
            self.running.ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await?;
        Ok((job, created))
    }

    async fn discard(&self, t: &FakeTorrentFile) -> Result<(), (ErrorCode, String)> {
        let id = t.info_hash.id();
        self.running.ctx.owner.ask(|reply| Command::Discard { id, reply }).await.map_err(|e| (e.code, e.message))
    }

    async fn restart_after(self, change: impl FnOnce(&Path)) -> Self {
        let Self { dir, running, swarm, disk, search, outside } = self;
        running.owner.abort();
        let _ = running.owner.await;
        drop(running.ctx);
        change(&dir.path().join("state.db"));
        let engine = Box::new(super::Fake::new(swarm.clone()));
        let running = Self::boot(&dir, engine, super::parse_fake, &disk, &search, &outside);
        Self { dir, running, swarm, disk, search, outside }
    }
}

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

#[tokio::test(flavor = "multi_thread")]
async fn a_title_play_links_the_download_it_makes_and_borrows_one_that_existed() {
    let h = Harness::new();
    let made = torrent(70, "made", &[("made.mp4", GIB)]);
    let mine = torrent(71, "mine", &[("mine.mp4", GIB)]);
    h.get(h.torrent_file(&mine), &[]).await.unwrap();
    let (job, created) = h.play_for(&made, &[0], Some("tt0063350")).await.unwrap();
    let link = job.title.unwrap();
    assert_eq!((created, link.title.as_str(), link.lease), (Created::Yes, "tt0063350", Lease::Owned));
    let (job, created) = h.play_for(&mine, &[0], Some("tt0063350")).await.unwrap();
    assert_eq!((created, job.title.unwrap().lease), (Created::No, Lease::Borrowed));
    let (job, created) = h.play_for(&made, &[0], Some("tt0100258")).await.unwrap();
    let link = job.title.unwrap();
    assert_eq!((created, link.title.as_str(), link.lease), (Created::No, "tt0063350", Lease::Owned), "a link stays");

    let h = h.crash_and_restart().await;
    let leases: Vec<Option<Lease>> = h.status().await.iter().map(|j| j.title.as_ref().map(|t| t.lease)).collect();
    assert_eq!(leases, [Some(Lease::Owned), Some(Lease::Borrowed)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_refuses_a_download_a_title_play_does_not_own_or_that_finished_or_played() {
    let h = Harness::new();
    let own = torrent(72, "own", &[("own.mp4", GIB)]);
    let joined = torrent(73, "joined", &[("joined.mp4", GIB)]);
    let finished = torrent(74, "finished", &[("finished.mp4", 1_000)]);
    let watched = torrent(75, "watched", &[("watched.mp4", GIB)]);
    for t in [&own, &joined, &watched] {
        h.swarm.withhold(t.info_hash.id(), u32::MAX);
    }
    h.get(h.torrent_file(&own), &[]).await.unwrap();
    h.get(h.torrent_file(&joined), &[]).await.unwrap();
    let created = [
        h.play_for(&joined, &[0], Some("tt0063350")).await.unwrap().1,
        h.play_for(&finished, &[0], Some("tt0063350")).await.unwrap().1,
        h.play_for(&watched, &[0], Some("tt0063350")).await.unwrap().1,
    ];
    assert_eq!(created, [Created::No, Created::Yes, Created::Yes]);
    h.swarm.set_rate(100 * GIB);
    h.until("finished is done", |jobs| {
        jobs.iter().any(|j| j.name.as_deref() == Some("finished") && j.status.name() == "done")
    })
    .await;
    let id = watched.info_hash.id();
    h.running.ctx.owner.ask(|reply| Command::Played { id, reply }).await.unwrap();

    let refused = |t: &FakeTorrentFile, why: &str| {
        Err((ErrorCode::JobConflict, format!("{} {why}, so its files stay", prefix(t))))
    };
    assert_eq!(h.discard(&own).await, refused(&own, "was not made by playing a title"));
    assert_eq!(h.discard(&joined).await, refused(&joined, "was on this machine before a title play used it"));
    assert_eq!(h.discard(&finished).await, refused(&finished, "is finished"));
    assert_eq!(h.discard(&watched).await, refused(&watched, "has played"));
    let missing = torrent(76, "missing", &[("missing.mp4", GIB)]);
    assert_eq!(h.discard(&missing).await.unwrap_err().0, ErrorCode::NotFound);
    assert_eq!(h.status().await.len(), 4, "every refused download stays");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_played_by_hand_is_never_discarded() {
    let h = Harness::new();
    let film = torrent(77, "film", &[("film.mp4", GIB)]);
    h.play_for(&film, &[0], Some("tt0063350")).await.unwrap();
    h.play(super::magnet(&film), &[0]).await.unwrap();
    assert_eq!(
        h.discard(&film).await,
        Err((ErrorCode::JobConflict, format!("{} has played, so its files stay", prefix(&film))))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_deletes_only_what_the_download_selected_never_through_a_link_or_what_another_holds() {
    let h = Harness::new();
    let downloads = h.downloads();
    let outside = h.dir.path().join("outside");
    let failed = torrent(78, "trial", &[("shared.mp4", GIB)]);
    h.swarm.withhold(failed.info_hash.id(), u32::MAX);
    h.get(h.torrent_file(&failed), &[]).await.unwrap();
    h.until("the first download stops with no source", |jobs| jobs[0].status.name() == "error").await;

    let trial = torrent(
        79,
        "trial",
        &[("a.mp4", GIB), ("b.srt", 10), ("deep/c.mp4", 10), ("notes.nfo", 10), ("shared.mp4", GIB)],
    );
    h.play_for(&trial, &[0, 1, 2, 4], Some("tt0063350")).await.unwrap();
    h.until("the trial runs", |jobs| {
        jobs.iter().any(|j| j.name.as_deref() == Some("trial") && j.status.name() == "downloading")
    })
    .await;
    for file in ["trial/a.mp4", "trial/notes.nfo", "trial/shared.mp4", &parts(79), &parts(78)] {
        write(&downloads.join(file));
    }
    write(&outside.join("secret.srt"));
    write(&outside.join("deep/c.mp4"));
    std::os::unix::fs::symlink(outside.join("secret.srt"), downloads.join("trial/b.srt")).unwrap();
    std::os::unix::fs::symlink(outside.join("deep"), downloads.join("trial/deep")).unwrap();

    assert_eq!(h.discard(&trial).await, Ok(()));
    let mut left = Vec::new();
    for _ in 0..500 {
        left = tree(&downloads);
        if !left.contains(&parts(79)) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(left, [parts(78).as_str(), "trial", "trial/b.srt", "trial/deep", "trial/notes.nfo", "trial/shared.mp4"]);
    assert_eq!(tree(&outside), ["deep", "deep/c.mp4", "secret.srt"]);
    let ids: Vec<TorrentId> = h.status().await.into_iter().map(|j| j.id).collect();
    assert_eq!(ids, [failed.info_hash.id()]);
    let store = Store::open(&h.dir.path().join("state.db")).unwrap();
    assert_eq!(store.discards().unwrap(), []);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discard_a_crash_left_is_finished_when_the_engine_starts() {
    let h = Harness::new();
    let downloads = h.downloads();
    for file in ["left/a.mp4", "left/sub/b.srt", "kept.mp4"] {
        write(&downloads.join(file));
    }
    let h = h
        .restart_after(|database| {
            let left = Discard {
                id: TorrentId::from_bytes([9; 20]),
                dir: downloads.clone(),
                paths: [PathBuf::from("left/a.mp4"), PathBuf::from("left/sub/b.srt")].into(),
            };
            Store::open(database).unwrap().discard(&left, 0).unwrap();
        })
        .await;
    assert_eq!(tree(&h.downloads()), ["kept.mp4"]);
    assert_eq!(Store::open(&h.dir.path().join("state.db")).unwrap().discards().unwrap(), []);
}
