use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use tempfile::TempDir;
use torrential_engine::{
    AddTorrent, Alert, Config, ErrorKind, FileIndex, SelectedPieces, TorrentFormat, TorrentSource,
};

use crate::support::{Fixture, Node, PIECE, sha256_file, snapshot, total_size};

const FILES: [(&str, usize); 2] = [("a.bin", 40_000), ("b.bin", 30_000)];

fn downloaded(fixture: &Fixture) -> (Node, TempDir, torrential_core::TorrentId) {
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());
    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent), download.path());
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);
    leecher.finish(id);
    (leecher, download, id)
}

#[test]
fn recheck_reports_a_damaged_piece_and_downloading_repairs_it() {
    let fixture = Fixture::new("recheck", &FILES, 11);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let total = total_size(&torrent);
    let (mut leecher, download, id) = downloaded(&fixture);

    let piece_1_middle = PIECE as usize * 3 / 2;
    assert!(2 * PIECE as usize <= fixture.files["a.bin"].len(), "piece 1 lies inside a.bin");
    let damaged = fixture.path_in(download.path(), "a.bin");
    let mut file = OpenOptions::new().write(true).open(&damaged).unwrap();
    file.seek(SeekFrom::Start(piece_1_middle as u64)).unwrap();
    file.write_all(&[!fixture.files["a.bin"][piece_1_middle]]).unwrap();
    drop(file);

    leecher.session.resume(id).unwrap();
    leecher.session.force_recheck(id).unwrap();
    leecher.wait_for("recheck", |a| matches!(a, Alert::TorrentChecked { .. }));
    let progress = leecher.status(id).progress;
    assert_eq!(progress.file(FileIndex::new(0)).unwrap().verified, 40_000 - u64::from(PIECE));
    assert_eq!(progress.file(FileIndex::new(1)).unwrap().verified, 30_000);
    assert_eq!(progress.verified_selected_bytes(), total - u64::from(PIECE));
    assert_eq!(leecher.session.selected_pieces(id).unwrap(), SelectedPieces { selected: 5, verified: 4 });

    let mut repair = Node::new(Config::loopback());
    repair.seed(&torrent, fixture.save_path());
    leecher.session.connect_peer(id, repair.addr()).unwrap();
    leecher.finish(id);
    assert_eq!(sha256_file(&damaged), Some(fixture.sha256("a.bin")));
    assert_eq!(leecher.status(id).progress.verified_selected_bytes(), total);
}

#[test]
fn remove_keeps_the_files() {
    let fixture = Fixture::new("remove", &FILES, 13);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let (mut leecher, download, id) = downloaded(&fixture);
    let before = snapshot(download.path());

    leecher.session.remove(id).unwrap();
    let removed = leecher.wait_for("removed", |a| matches!(a, Alert::TorrentRemoved { .. }));
    assert_eq!(removed.torrent(), Some(torrent.info_hash()));
    assert_eq!(leecher.session.status(id).unwrap_err().kind(), ErrorKind::UnknownTorrent);
    assert_eq!(snapshot(download.path()), before);
    for file in fixture.files.keys() {
        assert_eq!(sha256_file(&fixture.path_in(download.path(), file)), Some(fixture.sha256(file)));
    }
}
