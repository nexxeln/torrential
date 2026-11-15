use tempfile::TempDir;
use torrential_engine::{
    AddTorrent, Alert, Config, FileIndex, FileSelection, MissingPieces, TorrentFormat, TorrentSource, UploadPolicy,
};

use crate::support::{Fixture, Node, rate};

const KIB: usize = 1024;

#[test]
fn pieces_no_connected_peer_has_are_missing_without_a_source() {
    let files = [("a.bin", 256 * KIB), ("b.bin", 256 * KIB), ("c.bin", 512 * KIB)];
    let fixture = Fixture::new("sources", &files, 43);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut partial = Node::new(Config::loopback());
    let partial_dir = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), partial_dir.path());
    add.files = FileSelection::Only([FileIndex::new(0)].into());
    add.peers = vec![seeder.addr()];
    let id = partial.add(add);
    partial.wait_for("a.bin alone", |a| matches!(a, Alert::TorrentFinished { .. }));
    drop(seeder);

    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.files = FileSelection::Only([FileIndex::new(0), FileIndex::new(2)].into());
    add.peers = vec![partial.addr()];
    leecher.add(add);
    leecher.wait_until("a.bin from the partial peer", |s| s.missing_pieces(id).unwrap().missing == 32);
    let stranded = MissingPieces { missing: 32, unavailable: 32 };
    assert_eq!(leecher.session.missing_pieces(id).unwrap(), stranded, "the only peer has none of c.bin");

    let mut slow = Node::new(Config { upload: UploadPolicy::Capped(rate(128 * 1024).unwrap()), ..Config::loopback() });
    slow.seed(&torrent, fixture.save_path());
    leecher.session.connect_peer(id, slow.addr()).unwrap();
    leecher.wait_until("a seeder with c.bin", |s| s.missing_pieces(id).unwrap().unavailable == 0);
    leecher.finish(id);
    assert_eq!(leecher.session.missing_pieces(id).unwrap(), MissingPieces { missing: 0, unavailable: 0 });
}
