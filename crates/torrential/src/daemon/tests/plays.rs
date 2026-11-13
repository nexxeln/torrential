//! Where plays download, how they leave the plays folder, and what `rm` deletes.

use serde_json::json;
use torrential_core::Place;
use torrential_core::protocol::{Change, PlayRequest, Request, Response};

use super::super::admission::GIB;
use super::super::owner::plays::Deletion;
use super::super::store::{Store, Tombstone};
use super::titles::night;
use super::web::{magnet_link, post};
use super::{Harness, MIB, prefix, torrent, written};

impl Harness {
    async fn rm(&self, refs: Vec<String>, delete: bool) -> Vec<Change> {
        match self.ask(Request::Remove { refs, delete }).await {
            Response::Changed { changes } => changes,
            other => panic!("unexpected {other:?}"),
        }
    }
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
