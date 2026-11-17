use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use torrential_core::TorrentId;
use torrential_engine::{
    AddTorrent, Alert, Config, Dht, DhtState, ErrorKind, FileIndex, FilePriority, FileSelection, Listen, Magnet,
    Metainfo, PieceSize, Progress, ResumeData, Session, TorrentFormat, TorrentSource,
};

use crate::support::{Fixture, Node, bencode, magnet};

#[test]
fn malformed_inputs_are_invalid_input() {
    let kind = |e: torrential_engine::EngineError| e.kind();
    assert_eq!(Magnet::parse("https://example.com/a.torrent").map_err(kind).unwrap_err(), ErrorKind::InvalidInput);
    assert_eq!(Metainfo::from_bytes(b"d4:infoi1ee".to_vec()).map_err(kind).unwrap_err(), ErrorKind::InvalidInput);
    assert_eq!(ResumeData::from_bytes(b"not bencode".to_vec()).map_err(kind).unwrap_err(), ErrorKind::InvalidInput);
    assert_eq!(PieceSize::new(24 * 1024).map_err(kind).unwrap_err(), ErrorKind::InvalidInput);
    assert_eq!(PieceSize::new(256 * 1024 * 1024).map_err(kind).unwrap_err(), ErrorKind::InvalidInput);
    assert_eq!(PieceSize::new(32 * 1024).unwrap().get(), 32_768);
    let garbage = Config { dht_state: Some(DhtState::from_bytes(b"garbage".to_vec())), ..Config::loopback() };
    assert_eq!(Session::new(garbage).map(|_| ()).map_err(kind).unwrap_err(), ErrorKind::InvalidInput);
}

#[test]
fn torrent_calls_name_what_is_missing() {
    let mut node = Node::new(Config::loopback());
    let save = TempDir::new().unwrap();

    let unknown = TorrentId::from_bytes([7; 20]);
    let err = node.session.pause(unknown).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnknownTorrent);
    assert_eq!(err.to_string(), "unknown torrent: 0707070707070707070707070707070707070707");
    assert_eq!(node.session.announce_to_dht(unknown).unwrap_err().kind(), ErrorKind::UnknownTorrent);
    assert_eq!(
        node.session.add_web_seed(unknown, "http://127.0.0.1:9/").unwrap_err().kind(),
        ErrorKind::UnknownTorrent
    );

    let lonely = Magnet::parse(&magnet(Some([9; 20]), None)).unwrap();
    let id = node.add(AddTorrent::new(TorrentSource::Magnet(lonely), save.path()));
    assert_eq!(node.status(id).progress, Progress::AwaitingMetadata);
    node.session.announce_to_dht(id).expect("a torrent announces with the DHT off");
    assert_eq!(node.session.files(id).unwrap_err().kind(), ErrorKind::NoMetadata);
    assert_eq!(node.session.metainfo(id).unwrap_err().kind(), ErrorKind::NoMetadata);
    let skip = [(FileIndex::new(0), FilePriority::Skip)];
    assert_eq!(node.session.set_file_priorities(id, &skip).unwrap_err().kind(), ErrorKind::NoMetadata);

    let fixture = Fixture::new("index", &[("a.bin", 20_000), ("b.bin", 20_000)], 19);
    let mut add = AddTorrent::new(TorrentSource::Metainfo(fixture.torrent(TorrentFormat::V1)), save.path());
    add.files = FileSelection::Only(BTreeSet::from([FileIndex::new(2)]));
    assert_eq!(node.session.add(add).unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn a_torrent_file_lists_its_web_seeds_as_written() {
    let fixture = Fixture::new("item", &[("a.bin", 20_000)], 23);
    let plain = fixture.torrent(TorrentFormat::V1);
    let seeds = "l29:https://archive.org/download/9:/6/items/e";
    let bytes = [&plain.as_bytes()[..plain.as_bytes().len() - 1], b"8:url-list", seeds.as_bytes(), b"e"].concat();
    let listed = Metainfo::from_bytes(bytes).unwrap();
    assert_eq!(listed.web_seeds(), ["https://archive.org/download/", "/6/items/"]);
    assert_eq!(listed.info_hash(), plain.info_hash());
    assert_eq!(plain.web_seeds(), [] as [String; 0]);
}

#[test]
fn saved_dht_state_keeps_the_node_id() {
    let config = || Config { dht: Dht::On { bootstrap_nodes: vec!["127.0.0.1:9".to_owned()] }, ..Config::loopback() };
    let node_id = |s: &Session| {
        let state = s.save_state().unwrap();
        let dht = bencode::dict_value(state.as_bytes(), b"dht state")?;
        bencode::dict_value(dht, b"node-id").map(<[u8]>::to_vec)
    };

    let mut first = Node::new(config());
    first.wait_until("a DHT node id", |s| node_id(s).is_some());
    let saved = node_id(&first.session).unwrap();
    let state = first.session.save_state().unwrap();
    drop(first);

    let mut restored = Node::new(Config { dht_state: Some(state), ..config() });
    restored.wait_until("a DHT node id", |s| node_id(s).is_some());
    assert_eq!(node_id(&restored.session), Some(saved.clone()));
    let state_again = restored.session.save_state().unwrap();

    let unbootstrapped =
        Config { dht: Dht::On { bootstrap_nodes: Vec::new() }, dht_state: Some(state_again), ..Config::loopback() };
    assert_eq!(Session::new(unbootstrapped).map(|_| ()).unwrap_err().kind(), ErrorKind::InvalidInput);

    let mut fresh = Node::new(config());
    fresh.wait_until("a DHT node id", |s| node_id(s).is_some());
    assert_ne!(node_id(&fresh.session), Some(saved), "a fresh session picks its own id");
}

#[test]
fn listen_failures_and_taken_ports() {
    let unassigned: SocketAddr = "192.0.2.1:6881".parse().unwrap();
    let mut session =
        Session::new(Config { listen: Listen::Addresses(vec![unassigned]), ..Config::loopback() }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let failure = loop {
        let alerts = session.wait_alerts(Duration::from_millis(100)).unwrap();
        if let Some(f) = alerts.into_iter().find(|a| matches!(a, Alert::ListenFailed { .. })) {
            break f;
        }
        assert!(Instant::now() < deadline, "no ListenFailed alert");
    };
    let Alert::ListenFailed { port, operation, error, .. } = failure else { unreachable!() };
    assert_eq!((port, operation.as_str(), error.category.as_str()), (6881, "sock_bind", "system"));
    assert_eq!(session.listen_port().unwrap(), None);

    let taken = Node::new(Config::loopback());
    let next = Node::new(Config { listen: Listen::Addresses(vec![taken.addr()]), ..Config::loopback() });
    assert_ne!(next.addr().port(), taken.addr().port(), "libtorrent moved to a free port");
}

/// Windows refuses a UDP port it reserves, as Hyper-V and WSL have it do, or one another account
/// holds on every address, even where TCP binds. libtorrent moves only off a port that is in use,
/// so it listened nowhere on that address: no peer could connect, and uTP and the DHT were off.
#[cfg(windows)]
#[test]
fn a_port_windows_refuses_for_udp_moves_to_one_that_binds() {
    let loopback = |port| SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let refused = |e: std::io::Error| e.kind() == std::io::ErrorKind::PermissionDenied;
    let Some(port) = (49152..=u16::MAX).find(|&port| {
        std::net::UdpSocket::bind(loopback(port)).is_err_and(refused)
            && std::net::TcpListener::bind(loopback(port)).is_ok()
    }) else {
        eprintln!("this machine refuses no UDP port that TCP binds, so there is nothing to check");
        return;
    };
    let node = Node::new(Config { listen: Listen::Addresses(vec![loopback(port)]), ..Config::loopback() });
    assert_ne!(node.addr().port(), port);
}

#[test]
fn every_interface_reports_its_address_and_scope() {
    let mut session = Session::new(Config {
        listen: Listen::Addresses(vec!["0.0.0.0:0".parse().unwrap(), "[::]:0".parse().unwrap()]),
        ..Config::loopback()
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut addresses = Vec::new();
    while Instant::now() < deadline {
        let alerts = session.wait_alerts(Duration::from_millis(100)).expect("every alert converts");
        addresses.extend(alerts.into_iter().filter_map(|a| match a {
            Alert::ListenSucceeded { address, .. } => Some(address),
            _ => None,
        }));
    }
    assert!(addresses.iter().any(|a| a.ip().is_loopback()), "{addresses:?}");
    for address in &addresses {
        if let SocketAddr::V6(v6) = address
            && v6.ip().segments()[0] & 0xffc0 == 0xfe80
        {
            assert_ne!(v6.scope_id(), 0, "link-local {v6} names its interface");
        }
    }
}
