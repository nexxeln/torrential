use std::time::Duration;

use tempfile::TempDir;
use torrential_engine::{AddTorrent, Alert, Config, TorrentFormat, TorrentSource};

use crate::support::{Fixture, Node, snapshot, total_size};

#[test]
fn pause_closes_files_and_writes_stop() {
    let fixture = Fixture::new("pause", &[("a.bin", 200_000), ("b.bin", 280_000)], 3);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let total = total_size(&torrent);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.peers = vec![seeder.addr()];
    let id = leecher.add_slowly(add, 64 * 1024);
    leecher
        .wait_until("some verified pieces", |s| s.status(id).unwrap().progress.verified_selected_bytes() >= 64 * 1024);

    leecher.session.pause(id).unwrap();
    let paused = leecher.wait_for("paused", |a| matches!(a, Alert::TorrentPaused { .. }));
    assert_eq!(paused.torrent(), Some(torrent.info_hash()));
    let status = leecher.status(id);
    assert!(status.paused);
    let verified = status.progress.verified_selected_bytes();
    assert!(verified < total, "paused mid-download at {verified} of {total}");

    let before = snapshot(download.path());
    leecher.idle(Duration::from_millis(1500));
    assert_eq!(snapshot(download.path()), before, "no file changed after the pause");
    assert_eq!(leecher.status(id).progress.verified_selected_bytes(), verified);
}
