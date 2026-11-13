//! Where plays download, how they leave the plays folder, and what `rm` deletes.

use std::time::Duration;

use serde_json::json;
use torrential_core::protocol::{Change, PlayRequest, Request, Response};
use torrential_core::{Place, Target};

use super::super::admission::GIB;
use super::super::engine_port::FakeTorrentFile;
use super::super::owner::plays::Deletion;
use super::super::store::{Store, Tombstone};
use super::titles::{night, night_with};
use super::web::{magnet_link, post};
use super::{Harness, MIB, Outside, Player, prefix, torrent, written};

impl Harness {
    async fn rm(&self, refs: Vec<String>, delete: bool) -> Vec<Change> {
        match self.ask(Request::Remove { refs, delete }).await {
            Response::Changed { changes } => changes,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The name and state of each download, in the order status lists them.
    async fn named_states(&self) -> Vec<(String, &'static str)> {
        self.status().await.into_iter().map(|job| (job.name.unwrap_or_default(), job.status.name())).collect()
    }

    /// Plays `t` from the web app's search, into a folder of its own in the plays folder.
    async fn play_web(&self, t: &FakeTorrentFile) {
        self.swarm.publish(t.clone());
        let (status, body) = post(self, "/api/play", json!({ "ref": magnet_link(t) })).await;
        assert_eq!(status, 200, "{body}");
    }
}

fn named(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

async fn names(h: &Harness) -> Vec<String> {
    h.named_states().await.into_iter().map(|(name, _)| name).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_play_downloads_into_a_folder_of_its_own_and_leaves_a_folder_of_the_same_name_alone() {
    let (h, night) = night();
    let theirs = h.downloads().join("night_of_the_living_dead");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("night_of_the_living_dead.mp4"), "the person's own film").unwrap();
    let steps = h.play_title("tt0063350", Some(&night[3]), None).await;
    assert_eq!(steps.last().unwrap(), "playing tt0063350 a4a4a4 file 0 at null");
    let jobs = h.status().await;
    assert_eq!((&jobs[0].dir, jobs[0].place), (&h.play_folder(&night[3]), Place::Plays));
    let film = std::fs::read_to_string(theirs.join("night_of_the_living_dead.mp4")).unwrap();
    assert_eq!(film, "the person's own film");
}

#[tokio::test(flavor = "multi_thread")]
async fn get_and_a_play_by_hand_download_into_the_downloads_folder() {
    let h = Harness::new();
    let (kept, watched) =
        (torrent(0xb1, "kept", &[("kept.iso", MIB)]), torrent(0xb2, "watched", &[("watched.mp4", MIB)]));
    h.get(h.torrent_file(&kept), &[]).await.unwrap();
    let request = PlayRequest { target: h.torrent_file(&watched), dir: None, files: vec![0], setup_max_size: None };
    assert!(matches!(h.ask(Request::Play(request)).await, Response::Play { .. }));
    let places: Vec<_> = h.status().await.into_iter().map(|job| (job.dir, job.place)).collect();
    assert_eq!(places, [(h.downloads(), Place::Folder), (h.downloads(), Place::Folder)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn rm_deletes_a_plays_folder_and_the_files_of_a_download_in_the_persons_folder_only_when_asked() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let (film, iso) = (torrent(0xc1, "film", &[("film.mp4", MIB)]), torrent(0xc2, "iso", &[("disc.iso", MIB)]));
    h.swarm.publish(film.clone());
    post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    let folder = h.play_folder(&film);
    written(&folder.join("film/film.mp4"), MIB);
    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    let disc = h.downloads().join("iso/disc.iso");
    written(&disc, MIB);
    h.until("both are done", |jobs| jobs.iter().all(|job| job.status.name() == "done")).await;

    let removed = h.rm(vec![prefix(&film), prefix(&iso)], false).await;
    let deleted: Vec<_> = removed.into_iter().map(|change| (change.result, change.deleted)).collect();
    assert_eq!(deleted, [("removed".to_string(), vec![folder.clone()]), ("removed".to_string(), vec![])]);
    assert_eq!((folder.exists(), std::fs::read(&disc).unwrap().len()), (false, MIB as usize));

    h.get(h.torrent_file(&iso), &[]).await.unwrap();
    h.until("the disc is done again", |jobs| jobs[0].status.name() == "done").await;
    let removed = h.rm(vec![prefix(&iso)], true).await;
    assert_eq!(serde_json::to_value(&removed[0]).unwrap()["deleted"], json!([disc]));
    assert!(!h.downloads().join("iso").exists(), "the folder it leaves empty goes too");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_a_crash_cut_short_finishes_when_the_engine_starts_again() {
    let h = Harness::new();
    let film = torrent(0xd1, "film", &[("film.mp4", MIB)]);
    h.swarm.publish(film.clone());
    post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    let folder = h.play_folder(&film);
    written(&folder.join("film/film.mp4"), MIB);
    let database = h.dir.path().join("state.db");
    let h = h
        .restart_after(|| {
            let mut store = Store::open(&database).unwrap();
            let tomb = Tombstone { id: film.info_hash.id(), name: None, path: None, removed_at: 1 };
            store.remove(&tomb, Some(&Deletion::Play { folder: folder.clone() })).unwrap();
        })
        .await;
    assert_eq!((h.status().await.len(), folder.exists()), (0, false));
    let store = Store::open(&database).unwrap();
    assert_eq!(store.deletions().unwrap(), [], "it is done, so a later start has nothing to delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_play_watched_longest_ago_leaves_once_the_plays_folder_is_over_its_limit() {
    let (h, night) = night_with(Outside { plays_max: 3 * GIB, ..Outside::default() });
    h.swarm.set_rate(100 * GIB);
    assert_eq!(post(&h, "/api/library", json!({ "title": "tt0063350", "saved": true })).await.0, 200);
    h.play_title("tt0063350", Some(&night[3]), None).await;
    let got = torrent(0xe1, "got", &[("got.mp4", GIB)]);
    let older = torrent(0xe2, "older", &[("older.mp4", 2 * GIB)]);
    let newer = torrent(0xe3, "newer", &[("newer.mp4", 2 * GIB)]);
    for t in [&got, &older, &newer] {
        h.swarm.publish(t.clone());
    }
    post(&h, "/api/play", json!({ "ref": magnet_link(&got) })).await;
    h.get(Target::Prefix(prefix(&got)), &[]).await.unwrap();
    post(&h, "/api/play", json!({ "ref": magnet_link(&older) })).await;
    h.until("every play is done", |jobs| jobs.iter().all(|job| job.status.name() == "done")).await;
    // When a play was watched is kept to the second.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(h.status().await.len(), 3, "the plays that may leave take 2 GiB of 3 GiB");

    post(&h, "/api/play", json!({ "ref": magnet_link(&newer) })).await;
    h.until("the play watched longest ago leaves", |jobs| jobs.len() == 3).await;
    let named = ["night_of_the_living_dead", "got", "newer"].map(|name| (name.to_string(), "done"));
    h.until("newer is done", |jobs| jobs.iter().all(|job| job.status.name() == "done")).await;
    assert_eq!(h.named_states().await, named, "a saved title's play and one got by hand stay");
    assert_eq!([&older, &newer].map(|t| h.play_folder(t).exists()), [false, true]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_no_one_streams_pauses_and_streaming_it_again_resumes_it_but_a_saved_titles_play_goes_on() {
    let (h, night) = night_with(Outside { idle_after: Duration::from_secs(1), ..Outside::default() });
    h.swarm.set_rate(20 * MIB);
    post(&h, "/api/library", json!({ "title": "tt0063350", "saved": true })).await;
    h.play_title("tt0063350", Some(&night[3]), None).await;
    let film = torrent(0xf1, "film", &[("film.mkv", 2 * GIB)]);
    h.swarm.publish(film.clone());
    post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    let path = format!("/{}/0/film.mkv", film.info_hash.id().to_hex());
    // A range far past what has arrived keeps the stream open, as a player waiting for it does.
    let stream = |player: Player, path: String| {
        let mut player = player;
        tokio::spawn(async move { player.get(&path, "bytes=2000000000-2000000099").await })
    };

    let watching = stream(Player::connect(&h).await, path.clone());
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let downloading = ["night_of_the_living_dead", "film"].map(|name| (name.to_string(), "downloading"));
    assert_eq!(h.named_states().await, downloading, "someone streams the film");

    watching.abort();
    h.until("no one streams the film, so it pauses", |jobs| jobs[1].status.name() == "paused").await;
    let paused = [("night_of_the_living_dead".to_string(), "downloading"), ("film".to_string(), "paused")];
    assert_eq!(h.named_states().await, paused);

    let _watching = stream(Player::connect(&h).await, path);
    h.until("streaming the film again resumes it", |jobs| jobs[1].status.name() == "downloading").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn get_on_a_play_fails_when_it_cannot_note_that_the_play_is_the_persons_and_still_keeps_it() {
    let h = Harness::with_outside(
        super::search_port::Fake::new(vec![]),
        Outside { plays_max: 3 * GIB, ..Outside::default() },
    );
    h.swarm.set_rate(100 * GIB);
    let mine = torrent(0x61, "mine", &[("mine.mp4", 2 * GIB)]);
    h.play_web(&mine).await;
    h.until("it is done", |jobs| jobs[0].status.name() == "done").await;
    let database = rusqlite::Connection::open(h.dir.path().join("state.db")).unwrap();
    let refuse =
        "CREATE TRIGGER refuse BEFORE UPDATE OF by_hand ON plays BEGIN SELECT RAISE(ABORT, 'the disk is failing'); END";
    database.execute_batch(refuse).unwrap();

    let error = h.get(Target::Prefix(prefix(&mine)), &[]).await.unwrap_err();
    assert!(error.message.ends_with("the disk is failing"), "{error:?}");
    database.execute_batch("DROP TRIGGER refuse").unwrap();
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    h.play_web(&torrent(0x62, "next", &[("next.mp4", 2 * GIB)])).await;
    h.until("the next play is done", |jobs| jobs.len() == 2 && jobs.iter().all(|job| job.status.name() == "done"))
        .await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(names(&h).await, named(&["mine", "next"]), "the play got by hand stays");
}
