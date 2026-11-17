use std::time::{Duration, Instant};

use tempfile::TempDir;
use torrential_engine::{AddTorrent, Config, InterfaceName, Listen, Session, TorrentFormat, TorrentSource};

use crate::support::{Fixture, Node};

#[cfg(target_os = "linux")]
const LOOPBACK: &str = "lo";
#[cfg(target_os = "macos")]
const LOOPBACK: &str = "lo0";

fn bound_to(name: &str) -> Config {
    Config { listen: Listen::Interface { name: InterfaceName::new(name).unwrap(), port: 0 }, ..Config::loopback() }
}

#[test]
fn a_session_bound_to_an_interface_downloads_through_it() {
    let fixture = Fixture::new("bound", &[("a.bin", 40_000)], 3);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut leecher = Node::new(bound_to(LOOPBACK));
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent), download.path());
    add.peers = vec![seeder.addr()];
    let id = leecher.add(add);
    leecher.finish(id);

    let got = std::fs::read(fixture.path_in(download.path(), "a.bin")).unwrap();
    assert_eq!(got, fixture.files["a.bin"]);
}

#[test]
fn a_session_bound_to_a_missing_interface_neither_listens_nor_reaches_a_peer() {
    let fixture = Fixture::new("unbound", &[("a.bin", 40_000)], 4);
    let torrent = fixture.torrent(TorrentFormat::V1);
    let mut seeder = Node::new(Config::loopback());
    seeder.seed(&torrent, fixture.save_path());

    let mut session = Session::new(bound_to("tr-missing0")).unwrap();
    let download = TempDir::new().unwrap();
    let mut add = AddTorrent::new(TorrentSource::Metainfo(torrent), download.path());
    add.peers = vec![seeder.addr()];
    let id = session.add(add).unwrap().id();
    // An unbound session connects to a peer on 127.0.0.1 well within this.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        session.wait_alerts(Duration::from_millis(100)).unwrap();
        assert_eq!(session.status(id).unwrap().peers, 0, "no connection leaves by another interface");
    }
    assert_eq!(session.listen_port().unwrap(), None);
}
