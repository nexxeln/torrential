use std::path::PathBuf;

use tempfile::TempDir;
use torrential_engine::{
    AddTorrent, Alert, Config, FileEntry, FileIndex, FileSelection, Magnet, TorrentFormat, TorrentSource,
};

use crate::support::{Fixture, Node, bencode, expected_hashes, info_hash, magnet, sha256_file};

#[test]
fn hybrid_torrent_downloads_and_reports_both_hashes() {
    let fixture = Fixture::new("hybrid", &[("a.bin", 40_000), ("b.bin", 30_000), ("c.bin", 50_000)], 17);
    let torrent = fixture.torrent(TorrentFormat::Hybrid);
    let hashes = expected_hashes(torrent.as_bytes());
    let expected = info_hash(hashes, TorrentFormat::Hybrid);
    assert_eq!(torrent.info_hash(), expected);
    let entry = |index, path: &str, size| FileEntry { index: FileIndex::new(index), path: PathBuf::from(path), size };
    assert_eq!(
        torrent.files(),
        [entry(0, "hybrid/a.bin", 40_000), entry(2, "hybrid/b.bin", 30_000), entry(4, "hybrid/c.bin", 50_000),],
        "pad files keep their indices but are not listed"
    );

    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let magnet = Magnet::parse(&magnet(Some(hashes.0), Some(hashes.1))).unwrap();
    assert_eq!(magnet.info_hash(), expected);
    let mut add = AddTorrent::new(TorrentSource::Magnet(magnet), download.path());
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);

    let metadata = leecher.wait_for("metadata", |a| matches!(a, Alert::MetadataReceived { .. }));
    assert_eq!(metadata.torrent(), Some(expected));
    leecher.finish(id);
    assert_eq!(leecher.status(id).info_hash, expected);
    for file in fixture.files.keys() {
        assert_eq!(
            sha256_file(&fixture.path_in(download.path(), file)),
            Some(fixture.sha256(file)),
            "{file} matches the original"
        );
    }

    let cached = leecher.session.metainfo(id).unwrap();
    assert_eq!(cached.info_hash(), expected);
    for key in [&b"info"[..], b"piece layers"] {
        assert_eq!(
            bencode::dict_value(cached.as_bytes(), key),
            bencode::dict_value(torrent.as_bytes(), key),
            "{} matches the original torrent",
            String::from_utf8_lossy(key)
        );
    }
}

#[test]
fn a_hybrid_magnet_has_an_info_dictionary_when_metadata_arrives() {
    let fixture = Fixture::new("layers", &[("a.bin", 40_000), ("b.bin", 30_000)], 19);
    let torrent = fixture.torrent(TorrentFormat::Hybrid);
    let hashes = expected_hashes(torrent.as_bytes());
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let magnet = Magnet::parse(&magnet(Some(hashes.0), Some(hashes.1))).unwrap();
    let mut add = AddTorrent::new(TorrentSource::Magnet(magnet), download.path());
    add.files = FileSelection::metadata_only();
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);
    leecher.wait_for("metadata", |a| matches!(a, Alert::MetadataReceived { .. }));

    let info = leecher.session.info_dictionary(id).unwrap();
    assert_eq!(info.info_hash(), torrent.info_hash());
    assert_eq!(info.files(), torrent.files());
    assert_eq!(bencode::dict_value(info.as_bytes(), b"info"), bencode::dict_value(torrent.as_bytes(), b"info"));
    assert_eq!(bencode::dict_value(info.as_bytes(), b"piece layers"), None);
}
