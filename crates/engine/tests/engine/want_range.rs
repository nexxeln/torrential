use std::time::{Duration, Instant};

use tempfile::TempDir;
use torrential_engine::{AddTorrent, Config, FileIndex, TorrentFormat, TorrentSource, UploadPolicy};

use crate::support::{Fixture, Node, TIMEOUT, loopback, rate, total_size};

const MIB: usize = 1024 * 1024;

#[test]
fn a_wanted_range_is_written_before_most_of_the_file() {
    let fixture = Fixture::new("film", &[("film.mkv", 2 * MIB)], 61);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config { upload: UploadPolicy::Capped(rate(256 * 1024).unwrap()), ..loopback() });
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);
    let (file, offset, length) = (FileIndex::new(0), 1_300_000, 100_000);
    assert!(!leecher.session.want_range(id, file, offset, length).unwrap(), "nothing has arrived yet");
    let deadline = Instant::now() + TIMEOUT;
    while !leecher.session.want_range(id, file, offset, length).unwrap() {
        assert!(Instant::now() < deadline, "the range never arrived");
        leecher.idle(Duration::from_millis(50));
    }

    let verified = leecher.status(id).progress.verified_selected_bytes();
    assert!(verified < total_size(&torrent) / 2, "{verified} bytes verified before the range was ready");
    let on_disk = std::fs::read(fixture.path_in(download.path(), "film.mkv")).unwrap();
    assert_eq!(on_disk[1_300_000..1_400_000], fixture.files["film.mkv"][1_300_000..1_400_000]);
    let past_the_end = leecher.session.want_range(id, file, 2 * MIB as u64, 1).unwrap_err();
    assert_eq!(past_the_end.message(), "byte range 2097152+1 is outside the 2097152-byte file");
}
