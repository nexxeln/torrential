//! BEP 47 lets a torrent list symbolic links. libtorrent creates them as it sets up a torrent's
//! files, so the daemon relies on it keeping their targets inside the torrent's folder.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use sha1::Digest as _;
use torrential_engine::{AddTorrent, Config, Metainfo, TorrentSource};

use crate::support::Node;

fn string(s: &[u8]) -> Vec<u8> {
    [format!("{}:", s.len()).as_bytes(), s].concat()
}

fn list(items: &[&[u8]]) -> Vec<u8> {
    [b"l".as_slice(), &items.iter().flat_map(|i| string(i)).collect::<Vec<u8>>(), b"e"].concat()
}

fn link(path: &[u8], target: &[&[u8]]) -> Vec<u8> {
    [
        b"d".as_slice(),
        &string(b"attr"),
        &string(b"l"),
        &string(b"length"),
        b"i0e",
        &string(b"path"),
        &list(&[path]),
        &string(b"symlink path"),
        &list(target),
        b"e",
    ]
    .concat()
}

#[test]
fn a_torrents_symbolic_links_never_point_outside_its_folder() {
    let data = vec![7u8; 20_000];
    let pieces: Vec<u8> = data.chunks(16 * 1024).flat_map(|c| sha1::Sha1::digest(c).to_vec()).collect();
    let a = [b"d".as_slice(), &string(b"length"), b"i20000e", &string(b"path"), &list(&[b"a.bin"]), b"e"].concat();
    let files = [
        b"l".as_slice(),
        &a,
        &link(b"up", &[b"..", b"..", b"outside"]),
        &link(b"abs", &[b"/tmp"]),
        &link(b"sibling", &[b"..", b"other"]),
        &link(b"inside", &[b"a.bin"]),
        b"e",
    ]
    .concat();
    let info = [
        b"d".as_slice(),
        &string(b"files"),
        &files,
        &string(b"name"),
        &string(b"set"),
        &string(b"piece length"),
        b"i16384e",
        &string(b"pieces"),
        &string(&pieces),
        b"e",
    ]
    .concat();
    let torrent = [b"d".as_slice(), &string(b"info"), &info, b"e"].concat();
    let dir = tempfile::tempdir().unwrap();
    let mut node = Node::new(Config::loopback());
    let source = TorrentSource::Metainfo(Metainfo::from_bytes(torrent).unwrap());
    node.add(AddTorrent::new(source, dir.path()));
    node.idle(Duration::from_secs(2));

    let links: BTreeMap<String, PathBuf> = std::fs::read_dir(dir.path().join("set"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.symlink_metadata().unwrap().file_type().is_symlink())
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read_link(&p).unwrap()))
        .collect();
    let expected = [("abs", "abs"), ("inside", "a.bin"), ("sibling", "sibling"), ("up", "up")]
        .map(|(link, target)| (link.to_string(), PathBuf::from(target)));
    assert_eq!(links, BTreeMap::from(expected), "a target outside the folder becomes a link to itself");
}
