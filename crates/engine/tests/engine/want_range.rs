use std::time::{Duration, Instant};

use tempfile::TempDir;
use torrential_engine::{
    AddTorrent, Config, FileIndex, FileSelection, TorrentFormat, TorrentSource, UploadPolicy, WrittenPieces,
};

use crate::support::{Fixture, Node, PIECE, TIMEOUT, loopback, rate, total_size};

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

#[test]
fn the_written_pieces_of_a_file_are_counted_without_asking_for_any() {
    let files = [("a.bin", MIB / 2), ("film.mkv", 2 * MIB), ("z.bin", MIB / 2)];
    let fixture = Fixture::new("film", &files, 62);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config { upload: UploadPolicy::Capped(rate(256 * 1024).unwrap()), ..loopback() });
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.files = FileSelection::Only([FileIndex::new(1)].into());
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);
    let film = FileIndex::new(1);
    let size = 2 * MIB as u64;
    let pieces = (size / u64::from(PIECE)) as u32;
    let whole = |leecher: &Node| leecher.session.written_pieces(id, film, 0, size, 1).unwrap()[0];
    assert_eq!(whole(&leecher), WrittenPieces { pieces, written: 0 }, "nothing has arrived yet");

    // The middle of the film arrives first, as when a player seeks there.
    let (offset, length) = (1_300_000, 100_000);
    let deadline = Instant::now() + TIMEOUT;
    while !leecher.session.want_range(id, film, offset, length).unwrap() {
        assert!(Instant::now() < deadline, "the range never arrived");
        leecher.idle(Duration::from_millis(50));
    }
    let middle = leecher.session.written_pieces(id, film, offset, length, 1).unwrap()[0];
    assert_eq!(middle.written, middle.pieces, "what want_range reports written is counted as written");
    let partial = whole(&leecher);
    assert!((middle.pieces..pieces).contains(&partial.written), "{partial:?}");
    let runs = leecher.session.written_pieces(id, film, 0, size, 200).unwrap();
    assert_eq!(runs.len(), 200);
    assert!(runs.iter().all(|run| (1..=2).contains(&run.pieces)), "a run is shorter than a piece: {runs:?}");
    let on_disk = std::fs::read(fixture.path_in(download.path(), "film.mkv")).unwrap();
    for (run, counted) in runs.iter().enumerate() {
        let (begin, end) = (run * size as usize / 200, (run + 1) * size as usize / 200);
        if counted.written == counted.pieces {
            assert_eq!(on_disk[begin..end], fixture.files["film.mkv"][begin..end], "run {run} reads back");
        }
    }

    leecher.finish(id);
    assert_eq!(whole(&leecher), WrittenPieces { pieces, written: pieces });
    let halves = leecher.session.written_pieces(id, film, 0, size, 2).unwrap();
    assert_eq!(halves, [WrittenPieces { pieces: pieces / 2, written: pieces / 2 }; 2]);
    let past_the_end = leecher.session.written_pieces(id, film, size, 1, 1).unwrap_err();
    assert_eq!(past_the_end.message(), "byte range 2097152+1 is outside the 2097152-byte file");
}
