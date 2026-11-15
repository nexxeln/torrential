use std::time::Duration;

use tempfile::TempDir;
use torrential_engine::{
    AddTorrent, Alert, Config, FileIndex, FilePriority, FileSelection, Magnet, SelectedPieces, TorrentFormat,
    TorrentSource,
};

use crate::support::{Fixture, Node, expected_hashes, magnet, sha256_file};

const FILES: [(&str, usize); 3] = [("a.bin", 40_000), ("b.bin", 30_000), ("c.bin", 50_000)];

#[test]
fn metadata_only_downloads_nothing_until_a_file_is_selected() {
    let fixture = Fixture::new("select", &FILES, 23);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let magnet = Magnet::parse(&magnet(Some(expected_hashes(torrent.as_bytes()).0), None)).unwrap();
    let mut add = AddTorrent::new(TorrentSource::Magnet(magnet), download.path());
    add.files = FileSelection::metadata_only();
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);
    leecher.wait_for("metadata", |a| matches!(a, Alert::MetadataReceived { .. }));
    leecher.idle(Duration::from_millis(500));

    let status = leecher.status(id);
    assert_eq!(status.downloaded_payload, 0);
    assert_eq!(status.progress.selected_bytes(), 0);
    assert_eq!(leecher.session.selected_pieces(id).unwrap(), SelectedPieces { selected: 0, verified: 0 });
    assert_eq!(leecher.session.files(id).unwrap(), torrent.files());

    leecher.session.set_file_priorities(id, &[(FileIndex::new(2), FilePriority::Normal)]).unwrap();
    leecher.wait_for("priorities applied", |a| matches!(a, Alert::FilePrioritiesApplied { .. }));
    leecher.finish(id);
    assert_eq!(sha256_file(&fixture.path_in(download.path(), "c.bin")), Some(fixture.sha256("c.bin")));
    let progress = leecher.status(id).progress;
    assert_eq!(progress.selected_bytes(), 50_000);
    assert_eq!(progress.verified_selected_bytes(), 50_000);
    assert_eq!(
        leecher.session.selected_pieces(id).unwrap(),
        SelectedPieces { selected: 4, verified: 4 },
        "c.bin spans pieces 4 to 7, and piece 4 also holds the end of b.bin"
    );
    assert_eq!(progress.file(FileIndex::new(0)).unwrap().priority, FilePriority::Skip);
}

#[test]
fn a_torrent_added_paused_waits_for_resume() {
    let fixture = Fixture::new("paused", &FILES, 29);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.paused = true;
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);
    leecher.idle(Duration::from_millis(500));
    let status = leecher.status(id);
    assert!(status.paused);
    assert_eq!(status.downloaded_payload, 0);

    leecher.session.resume(id).unwrap();
    let resumed = leecher.wait_for("resumed", |a| matches!(a, Alert::TorrentResumed { .. }));
    assert_eq!(resumed.torrent(), Some(torrent.info_hash()));
    leecher.finish(id);
    for file in fixture.files.keys() {
        assert_eq!(sha256_file(&fixture.path_in(download.path(), file)), Some(fixture.sha256(file)));
    }
}
