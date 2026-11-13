use std::time::Duration;

use tempfile::TempDir;
use torrential_engine::{AddTorrent, Alert, TorrentFormat, TorrentSource};

use crate::support::{Fixture, Node, loopback, rate, sha256_file, total_size};

const LIMIT: u32 = 16 * 1024;

#[test]
fn a_download_limit_holds_a_torrent_back_until_it_is_lifted() {
    let fixture = Fixture::new("limited", &[("a.bin", 600_000)], 11);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.peers = vec![seeder.addr()];
    add.paused = true;
    let id = leecher.add(add);
    leecher.session.set_download_limit(id, rate(LIMIT)).unwrap();
    leecher.session.resume(id).unwrap();
    leecher.idle(Duration::from_secs(3));
    let verified = leecher.status(id).progress.verified_selected_bytes();
    assert!(verified <= 6 * u64::from(LIMIT), "3 s at 16 KiB/s verified {verified} bytes");

    leecher.session.set_download_limit(id, None).unwrap();
    leecher.finish(id);
    assert_eq!(sha256_file(&fixture.path_in(download.path(), "a.bin")), Some(fixture.sha256("a.bin")));
}

#[test]
fn the_largest_download_limit_still_holds_a_torrent_back() {
    let fixture = Fixture::new("largest", &[("a.bin", 100_000)], 13);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut node = Node::new(loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent), download.path());
    add.paused = true;
    let id = node.add(add);
    node.session.set_download_limit(id, rate(u32::MAX)).unwrap();
    // libtorrent reads i32::MAX as no limit at all.
    assert_eq!(node.session.download_limit(id).unwrap(), rate(2_147_483_646));
    node.session.set_download_limit(id, None).unwrap();
    assert_eq!(node.session.download_limit(id).unwrap(), None);
}

#[test]
fn a_torrent_added_again_from_resume_data_has_no_download_limit() {
    let fixture = Fixture::new("relimited", &[("a.bin", 2_000_000)], 12);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let total = total_size(&torrent);
    let mut seeder = Node::new(loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut first = Node::new(loopback());
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent.clone()), download.path());
    add.peers = vec![seeder.addr()];
    add.paused = true;
    let id = first.add(add);
    first.session.set_download_limit(id, rate(LIMIT)).unwrap();
    first.session.resume(id).unwrap();
    first.wait_until("a verified piece", |s| s.status(id).unwrap().progress.verified_selected_bytes() > 0);
    first.session.save_resume_data(id).unwrap();
    let Alert::ResumeDataSaved { data, .. } =
        first.wait_for("resume data", |a| matches!(a, Alert::ResumeDataSaved { .. }))
    else {
        unreachable!()
    };
    let verified = first.status(id).progress.verified_selected_bytes();
    assert!(verified < total / 4, "the limit held the first session to {verified} of {total} bytes");
    drop(first);

    // At 16 KiB/s the rest would take two minutes, longer than finish waits.
    let mut second = Node::new(loopback());
    let mut add = AddTorrent::new(TorrentSource::Resume(data), download.path());
    add.peers = vec![seeder.addr()];
    assert_eq!(second.add(add), id);
    second.finish(id);
    assert_eq!(sha256_file(&fixture.path_in(download.path(), "a.bin")), Some(fixture.sha256("a.bin")));
}
