use tempfile::TempDir;
use torrential_engine::{AddTorrent, Alert, Config, TorrentFormat, TorrentSource, TorrentState};

use crate::support::{Fixture, Node, PIECE, bencode, sha256_file, total_size};

#[test]
fn resume_data_restarts_a_dropped_session_without_a_full_recheck() {
    let fixture = Fixture::new("resume", &[("a.bin", 200_000), ("b.bin", 280_000)], 5);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let total = total_size(&torrent);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());
    let download = TempDir::new().unwrap();

    let mut first = Node::new(Config::loopback());
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.peers = vec![seeder.addr()];
    let id = first.add_slowly(add, 64 * 1024);
    first
        .wait_until("some verified pieces", |s| s.status(id).unwrap().progress.verified_selected_bytes() >= 128 * 1024);
    let saved = first.status(id).progress.verified_selected_bytes();
    assert!(saved < total, "saved mid-download at {saved} of {total}");
    first.session.save_resume_data(id).unwrap();
    let Alert::ResumeDataSaved { data, .. } =
        first.wait_for("resume data", |a| matches!(a, Alert::ResumeDataSaved { .. }))
    else {
        unreachable!()
    };
    drop(first);
    let resumed = recorded_bytes(data.as_bytes(), total);
    assert!(resumed > 0, "the resume data records downloaded pieces");

    let mut second = Node::new(Config::loopback());
    let mut add = AddTorrent::new(TorrentSource::Resume(data), download.path());
    add.peers = vec![seeder.addr()];
    assert_eq!(second.add(add), id);
    second.finish(id);

    let states: Vec<(TorrentState, TorrentState)> = second
        .log
        .iter()
        .filter_map(|a| match a {
            Alert::StateChanged { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
        .collect();
    assert_eq!(states.first(), Some(&(TorrentState::CheckingResumeData, TorrentState::Downloading)));
    assert!(!states.iter().any(|&(_, to)| to == TorrentState::CheckingFiles), "no full recheck: {states:?}");
    let downloaded = second.status(id).downloaded_payload;
    assert!(
        downloaded <= total - resumed,
        "the restarted session downloaded {downloaded} bytes, more than the {} it lacked",
        total - resumed
    );
    for file in fixture.files.keys() {
        assert_eq!(
            sha256_file(&fixture.path_in(download.path(), file)),
            Some(fixture.sha256(file)),
            "{file} matches the original"
        );
    }
}

#[test]
fn a_save_if_progressed_skips_a_torrent_with_nothing_new() {
    let fixture = Fixture::new("unchanged", &[("a.bin", 100_000)], 9);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut node = Node::new(Config::loopback());
    let id = node.seed(&torrent, fixture.save_path());
    let answer = |node: &mut Node| {
        node.session.save_resume_data_if_progressed(id).unwrap();
        node.wait_for("an answer", |a| matches!(a, Alert::ResumeDataSaved { .. } | Alert::ResumeDataUnchanged { .. }))
    };
    assert!(matches!(answer(&mut node), Alert::ResumeDataSaved { .. }), "checking the files is progress");
    assert_eq!(answer(&mut node), Alert::ResumeDataUnchanged { torrent: torrent.info_hash() });
}

/// Bytes in the pieces the resume data marks as on disk. libtorrent writes
/// `pieces` as a bitfield, most significant bit first. It lists flushed
/// pieces only, so it can trail the verified count in the status.
fn recorded_bytes(resume: &[u8], total: u64) -> u64 {
    let raw = bencode::dict_value(resume, b"pieces").expect("pieces bitfield");
    let bits = bencode::string(raw).unwrap();
    let piece = u64::from(PIECE);
    let count = total.div_ceil(piece);
    (0..count)
        .filter(|&i| bits[(i / 8) as usize] & (0x80 >> (i % 8)) != 0)
        .map(|i| if i == count - 1 { total - i * piece } else { piece })
        .sum()
}
