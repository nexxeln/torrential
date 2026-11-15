use std::time::Instant;

use tempfile::TempDir;
use torrential_engine::{
    AddTorrent, Config, FileIndex, FileSelection, PieceSize, SelectedPieces, TorrentFormat, TorrentSource,
    TorrentState, create_torrent,
};

use crate::support::{Fixture, Node, PIECE};

const KIB: usize = 1024;

#[test]
fn slices_fill_as_pieces_arrive_and_skipped_files_stay_unwanted() {
    let files = [("a.bin", 256 * KIB), ("b.bin", 256 * KIB), ("c.bin", 512 * KIB)];
    let fixture = Fixture::new("slices", &files, 41);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(Config::loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.files = FileSelection::Only([FileIndex::new(0), FileIndex::new(2)].into());
    add.peers = vec![seeder.addr()];
    let id = leecher.add_slowly(add, 192 * 1024);
    let slices = |leecher: &Node| leecher.session.piece_slices(id, 64).unwrap();
    let piece = |selected, verified| SelectedPieces { selected, verified };

    leecher.wait_until("some pieces but not all", |s| {
        let verified: u32 = s.piece_slices(id, 64).unwrap().iter().map(|p| p.verified).sum();
        (8..40).contains(&verified)
    });
    let partial = slices(&leecher);
    let selected: Vec<u32> = partial.iter().map(|p| p.selected).collect();
    assert_eq!(selected, [[1; 16], [0; 16], [1; 16], [1; 16]].concat(), "b.bin is skipped");
    assert!(partial.iter().any(|p| *p == piece(1, 0)), "some selected pieces have not arrived: {partial:?}");
    assert!(partial.iter().any(|p| *p == piece(1, 1)), "some have: {partial:?}");

    leecher.finish(id);
    let done = [vec![piece(1, 1); 16], vec![piece(0, 0); 16], vec![piece(1, 1); 32]].concat();
    assert_eq!(slices(&leecher), done);
    assert_eq!(
        leecher.session.piece_slices(id, 3).unwrap(),
        [piece(16, 16), piece(10, 10), piece(22, 22)],
        "uneven runs of 21, 21, and 22 pieces"
    );
    let doubled: Vec<SelectedPieces> = done.iter().flat_map(|p| [*p, *p]).collect();
    assert_eq!(leecher.session.piece_slices(id, 128).unwrap(), doubled, "each piece covers two slices");
}

#[test]
#[ignore = "hashes a 1.6 GB sparse file"]
fn piece_slices_cost_at_100k_pieces() {
    const PIECES: u32 = 100_000;
    let data = TempDir::new().unwrap();
    std::fs::create_dir(data.path().join("big")).unwrap();
    let file = std::fs::File::create(data.path().join("big/zeros.bin")).unwrap();
    file.set_len(u64::from(PIECES) * u64::from(PIECE)).unwrap();
    let piece = PieceSize::new(PIECE).unwrap();
    let torrent = create_torrent(&data.path().join("big"), piece, TorrentFormat::V1).unwrap();
    let mut node = Node::new(Config::loopback());
    let empty = TempDir::new().unwrap();
    let id = node.add(AddTorrent::new(TorrentSource::Metainfo(torrent), empty.path()));
    node.wait_until("checked", |s| s.status(id).unwrap().state == TorrentState::Downloading);

    let per_call = |f: &dyn Fn()| {
        let mut batches: Vec<f64> = (0..7)
            .map(|_| {
                let start = Instant::now();
                (0..50).for_each(|_| f());
                start.elapsed().as_secs_f64() * 1e6 / 50.0
            })
            .collect();
        batches.sort_by(f64::total_cmp);
        (batches[3], batches[0], batches[6])
    };
    let slices = per_call(&|| {
        let selected: u32 = node.session.piece_slices(id, 64).unwrap().iter().map(|s| s.selected).sum();
        assert_eq!(selected, PIECES);
    });
    let status = per_call(&|| assert!(!node.status(id).paused));
    println!(
        "piece_slices(64) at {PIECES} pieces: median {:.0} us per call, range {:.0} to {:.0}",
        slices.0, slices.1, slices.2
    );
    println!("status for comparison: median {:.0} us per call, range {:.0} to {:.0}", status.0, status.1, status.2);
}
