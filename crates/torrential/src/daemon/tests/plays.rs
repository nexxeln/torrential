//! Where plays download, how they leave the plays folder, and what `rm` deletes.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use torrential_core::protocol::{Change, GetRequest, Kept, PlayRequest, Request, Response};
use torrential_core::{ErrorCode, Place, Target, TorrentId};

use super::super::admission::GIB;
use super::super::engine_port::{EngineEvent, FakeTorrentFile};
use super::super::owner::deleter::PlayName;
use super::super::store::{Store, Tombstone};
use super::titles::{night, night_with};
use super::web::{magnet_link, post};
use super::{Gate, Harness, MIB, Outside, Player, prefix, torrent, written};

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

    /// Gets `t` into `dir`, as `torrential get --dir` does.
    async fn get_into(&self, t: &FakeTorrentFile, dir: PathBuf) -> Result<(), torrential_core::Error> {
        let target = self.torrent_file(t);
        let request =
            GetRequest { target, dir: Some(dir), files: vec![], max_size: None, setup_max_size: None, dry_run: false };
        self.get_with(request).await.map(drop)
    }

    fn plays(&self) -> PathBuf {
        self.dir.path().join("plays")
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
            let name = PlayName::parse(folder.file_name().unwrap().to_str().unwrap()).unwrap();
            store.remove(&tomb, Some(&name)).unwrap();
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
async fn a_plays_folder_moved_away_and_replaced_by_a_link_while_the_engine_runs_has_nothing_deleted() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let film = torrent(0x11, "film", &[("film.mp4", MIB)]);
    h.play_web(&film).await;
    let folder = h.play_folder(&film);
    written(&folder.join("film/film.mp4"), MIB);
    h.until("the film is done", |jobs| jobs[0].status.name() == "done").await;
    let (moved, decoys) = (h.dir.path().join("plays.old"), h.dir.path().join("decoys"));
    std::fs::rename(h.plays(), &moved).unwrap();
    let decoy = decoys.join(folder.file_name().unwrap()).join("film/film.mp4");
    written(&decoy, 4);
    symlink(&decoys, h.plays()).unwrap();

    let removed = h.rm(vec![prefix(&film)], false).await;
    let refused = "the plays folder was moved or replaced since the engine started";
    assert_eq!(
        (removed[0].deleted.clone(), removed[0].kept.clone()),
        (vec![], vec![Kept { path: folder.clone(), reason: refused.into() }])
    );
    assert_eq!(std::fs::read(&decoy).unwrap().len(), 4, "the decoy behind the link stays");
    assert!(
        moved.join(folder.file_name().unwrap()).join("film/film.mp4").exists(),
        "and so does the folder moved away"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_refuses_a_folder_inside_the_plays_folder_even_through_a_link() {
    let h = Harness::new();
    std::fs::create_dir_all(h.plays()).unwrap();
    let films = h.dir.path().join("films");
    symlink(h.plays(), &films).unwrap();
    let iso = torrent(0x21, "iso", &[("disc.iso", MIB)]);
    let error = h.get_into(&iso, films.join("mine")).await.unwrap_err();
    let message = format!(
        "{} is inside {}, the folder torrential keeps plays in",
        films.join("mine").display(),
        h.plays().display()
    );
    assert_eq!((error.code, error.message), (ErrorCode::PathConflict, message));
    assert!(h.status().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rm_delete_keeps_a_file_another_download_writes_through_a_linked_folder() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let (theirs, ours) = (torrent(0x31, "set", &[("a.bin", MIB)]), torrent(0x32, "set", &[("a.bin", MIB)]));
    let alias = h.dir.path().join("alias");
    std::fs::create_dir_all(h.downloads()).unwrap();
    symlink(h.downloads(), &alias).unwrap();
    h.get_into(&theirs, h.downloads()).await.unwrap();
    h.get_into(&ours, alias.clone()).await.unwrap();
    let file = h.downloads().join("set/a.bin");
    written(&file, MIB);
    h.until("both are done", |jobs| jobs.iter().all(|job| job.status.name() == "done")).await;

    let removed = h.rm(vec![prefix(&ours)], true).await;
    let kept = Kept { path: alias.join("set/a.bin"), reason: "another download uses it".into() };
    assert_eq!((removed[0].deleted.clone(), removed[0].kept.clone()), (vec![], vec![kept]));
    assert_eq!(std::fs::read(&file).unwrap().len(), MIB as usize);
}

#[tokio::test(flavor = "multi_thread")]
async fn rm_delete_keeps_a_file_a_download_the_engine_has_not_let_go_of_writes() {
    let gate =
        Gate::holding(|event| matches!(event, EngineEvent::Removed { id } if *id == TorrentId::from_bytes([0x34; 20])));
    let h = Harness::with_outside(
        super::search_port::Fake::new(vec![]),
        Outside { gate: Some(gate.clone()), ..Outside::default() },
    );
    h.swarm.set_rate(100 * GIB);
    let (ours, theirs) = (torrent(0x33, "set", &[("a.bin", MIB)]), torrent(0x34, "set", &[("a.bin", 4 * GIB)]));
    let alias = h.dir.path().join("alias");
    std::fs::create_dir_all(h.downloads()).unwrap();
    symlink(h.downloads(), &alias).unwrap();
    h.get_into(&ours, alias.clone()).await.unwrap();
    h.until("ours is done", |jobs| jobs[0].status.name() == "done").await;
    h.swarm.set_rate(0);
    h.swarm.publish(theirs.clone());
    h.get_into(&theirs, h.downloads()).await.unwrap();
    let file = h.downloads().join("set/a.bin");
    written(&file, MIB);
    h.until("theirs downloads", |jobs| jobs[1].status.name() == "downloading").await;

    gate.close();
    h.rm(vec![prefix(&theirs)], false).await;
    gate.until_held(1).await;
    let removed = h.rm(vec![prefix(&ours)], true).await;
    let kept = Kept { path: alias.join("set/a.bin"), reason: "another download uses it".into() };
    assert_eq!((removed[0].deleted.clone(), removed[0].kept.clone()), (vec![], vec![kept]));
    assert_eq!(std::fs::read(&file).unwrap().len(), MIB as usize);
    gate.open();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_before_rm_delete_gets_to_the_files_keeps_what_is_at_their_names_after() {
    let gate = Gate::holding(|event| matches!(event, EngineEvent::Removed { .. }));
    let h = Harness::with_outside(
        super::search_port::Fake::new(vec![]),
        Outside { gate: Some(gate.clone()), ..Outside::default() },
    );
    let iso = torrent(0x41, "iso", &[("disc.iso", 4 * GIB)]);
    h.swarm.publish(iso.clone());
    h.get_into(&iso, h.downloads()).await.unwrap();
    let disc = h.downloads().join("iso/disc.iso");
    written(&disc, MIB);
    h.until("it downloads", |jobs| jobs[0].status.name() == "downloading").await;
    gate.close();
    h.rm(vec![prefix(&iso)], true).await;
    gate.until_held(1).await;

    let h = h.restart_after(|| std::fs::write(&disc, "the person's own file").unwrap()).await;
    h.until("the engine is back", |jobs| jobs.is_empty()).await;
    assert_eq!(std::fs::read_to_string(&disc).unwrap(), "the person's own file");
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_a_title_while_its_play_leaves_the_plays_folder_keeps_the_play_and_its_files() {
    let gate =
        Gate::holding(|event| matches!(event, EngineEvent::Removed { id } if *id == TorrentId::from_bytes([0xa4; 20])));
    let (h, night) = night_with(Outside { plays_max: 200 * MIB, gate: Some(gate.clone()), ..Outside::default() });
    h.swarm.set_rate(100 * MIB);
    h.play_title("tt0063350", Some(&night[3]), None).await;
    let folder = h.play_folder(&night[3]);
    written(&folder.join("night_of_the_living_dead/night_of_the_living_dead.mp4"), MIB);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    gate.close();
    h.play_web(&torrent(0x51, "film", &[("film.mp4", 2 * GIB)])).await;
    gate.until_held(1).await;

    assert_eq!(post(&h, "/api/library", json!({ "title": "tt0063350", "saved": true })).await.0, 200);
    gate.open();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(names(&h).await, named(&["night_of_the_living_dead", "film"]));
    assert!(folder.join("night_of_the_living_dead/night_of_the_living_dead.mp4").exists());
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

#[tokio::test(flavor = "multi_thread")]
async fn resume_all_starts_the_idle_clock_again_for_the_plays_it_resumes() {
    let h = Harness::with_outside(
        super::search_port::Fake::new(vec![]),
        Outside { idle_after: Duration::from_secs(3), ..Outside::default() },
    );
    h.swarm.set_rate(20 * MIB);
    h.play_web(&torrent(0x81, "film", &[("film.mkv", 2 * GIB)])).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while h.states().await != ["paused"] {
        assert!(std::time::Instant::now() < deadline, "no one streams it, so it pauses");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(matches!(h.ask(Request::Resume { refs: vec![], all: true }).await, Response::Changed { .. }));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(h.states().await, ["downloading"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_folder_that_cannot_be_deleted_yet_counts_toward_the_limit_and_goes_once_it_can() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let h = Harness::with_outside(
        super::search_port::Fake::new(vec![]),
        Outside { plays_max: 4 * MIB, ..Outside::default() },
    );
    h.swarm.set_rate(100 * GIB);
    let stuck = torrent(0x91, "stuck", &[("stuck.mp4", 3 * MIB)]);
    h.play_web(&stuck).await;
    let inner = h.play_folder(&stuck).join("stuck");
    written(&inner.join("stuck.mp4"), 3 * MIB);
    h.until("it is done", |jobs| jobs[0].status.name() == "done").await;
    let shut = |mode| std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(mode)).unwrap();
    shut(0o555);
    h.rm(vec![prefix(&stuck)], false).await;
    assert!(inner.join("stuck.mp4").exists());

    h.play_web(&torrent(0x92, "older", &[("older.mp4", MIB)])).await;
    h.until("older is done", |jobs| jobs.len() == 1 && jobs[0].status.name() == "done").await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    h.play_web(&torrent(0x93, "newer", &[("newer.mp4", MIB)])).await;
    h.until("the play watched longest ago leaves, for what is left to delete counts", |jobs| {
        jobs.len() == 1 && jobs[0].name.as_deref() == Some("newer")
    })
    .await;
    shut(0o755);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while h.play_folder(&stuck).exists() {
        assert!(std::time::Instant::now() < deadline, "the folder goes once it can be deleted");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
