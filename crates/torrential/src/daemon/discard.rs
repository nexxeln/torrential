//! The files a discarded download leaves, and deleting them. Deleting is the one thing the engine
//! does that cannot be undone, so it touches only what the torrent itself wrote.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use torrential_core::InfoHash;

use super::engine_port::Metainfo;
use super::files;

/// What a download keeps in its folder, relative to it: each file of the torrent it selected, and
/// the parts file where libtorrent keeps pieces it shares with files that are not selected.
pub fn held(metainfo: &Metainfo, wanted: &[bool]) -> Vec<PathBuf> {
    let selected = metainfo.files.iter().zip(wanted).filter(|(_, w)| **w);
    let mut paths: Vec<PathBuf> = selected.map(|(file, _)| files::relative(metainfo, file)).collect();
    paths.push(PathBuf::from(parts_file(&metainfo.info_hash)));
    paths
}

/// libtorrent names the parts file after the torrent's v2 hash cut to 20 bytes, or after its v1
/// hash when it has no v2 one, and keeps it in the download folder.
fn parts_file(hash: &InfoHash) -> String {
    let best = match hash {
        InfoHash::V1(v1) => &v1[..],
        InfoHash::V2(v2) | InfoHash::Hybrid { v2, .. } => &v2[..20],
    };
    let hex: String = best.iter().map(|b| format!("{b:02x}")).collect();
    format!(".{hex}.parts")
}

/// Deletes each of `paths`, relative to `dir`, that is a plain file reached from `dir` through no
/// symbolic link, then each folder that leaves empty, up to but not including `dir`. Answers the
/// files it deleted.
pub fn delete(dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut deleted = Vec::new();
    let mut folders = BTreeSet::new();
    for relative in paths {
        let Some(path) = inside(dir, relative) else { continue };
        let plain = path.symlink_metadata().is_ok_and(|m| m.file_type().is_file());
        if plain && std::fs::remove_file(&path).is_ok() {
            folders.extend(path.ancestors().skip(1).take_while(|a| *a != dir).map(Path::to_path_buf));
            deleted.push(path);
        }
    }
    // The deepest first, so a folder empties before its parent is tried.
    for folder in folders.iter().rev() {
        let _ = std::fs::remove_dir(folder);
    }
    deleted
}

/// `dir` joined with `relative`, when `relative` names a place below `dir` and no folder on the
/// way there is a symbolic link.
fn inside(dir: &Path, relative: &Path) -> Option<PathBuf> {
    let mut path = dir.to_path_buf();
    let mut parts = relative.components().peekable();
    parts.peek()?;
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else { return None };
        path.push(name);
        if parts.peek().is_some() && !path.symlink_metadata().is_ok_and(|m| m.file_type().is_dir()) {
            return None;
        }
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::daemon::engine_port::FakeTorrentFile;

    fn torrent(hash: InfoHash, name: &str, files: &[&str]) -> Metainfo {
        let files = files.iter().map(|path| (path.to_string(), 1_000)).collect();
        FakeTorrentFile { info_hash: hash, name: name.into(), files }.metainfo().unwrap()
    }

    #[test]
    fn a_download_holds_its_selected_files_and_the_parts_file_named_by_its_best_hash() {
        let pack = torrent(InfoHash::V1([0xab; 20]), "Pack", &["a.mkv", "extras/b.mkv", "c.nfo"]);
        assert_eq!(
            held(&pack, &[true, true, false]),
            ["Pack/a.mkv", "Pack/extras/b.mkv", ".abababababababababababababababababababab.parts"].map(PathBuf::from)
        );
        let single = torrent(InfoHash::Hybrid { v1: [0x11; 20], v2: [0x22; 32] }, "film.mp4", &["film.mp4"]);
        let parts = format!(".{}.parts", "22".repeat(20));
        assert_eq!(held(&single, &[true]), [PathBuf::from("film.mp4"), PathBuf::from(parts)]);
        let v2 = torrent(InfoHash::V2([0x33; 32]), "Two", &["x.mp4"]);
        assert_eq!(held(&v2, &[false])[0], PathBuf::from(format!(".{}.parts", "33".repeat(20))));
    }

    #[test]
    fn deleting_stays_below_the_folder_and_never_follows_a_link() {
        let root = tempfile::tempdir().unwrap();
        let (dir, outside) = (root.path().join("downloads"), root.path().join("outside"));
        let write = |path: &Path| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"payload").unwrap();
        };
        for path in ["Pack/a.mkv", "Pack/extras/b.mkv", "Pack/keep.txt", "Gone/only.mkv", ".ab.parts"] {
            write(&dir.join(path));
        }
        write(&outside.join("secret.mkv"));
        write(&outside.join("folder/inner.mkv"));
        symlink(outside.join("secret.mkv"), dir.join("Linked.mkv")).unwrap();
        symlink(outside.join("folder"), dir.join("LinkedFolder")).unwrap();

        let asked = [
            "Pack/a.mkv",
            "Pack/extras/b.mkv",
            "Gone/only.mkv",
            ".ab.parts",
            "Linked.mkv",
            "LinkedFolder/inner.mkv",
            "../outside/secret.mkv",
            "/etc/hosts",
            "",
            "Pack/missing.mkv",
            "Pack",
        ]
        .map(PathBuf::from);
        let deleted = delete(&dir, &asked);
        assert_eq!(deleted, ["Pack/a.mkv", "Pack/extras/b.mkv", "Gone/only.mkv", ".ab.parts"].map(|p| dir.join(p)));

        let mut left: Vec<String> = walk(root.path());
        left.sort();
        assert_eq!(
            left,
            [
                "downloads",
                "downloads/Linked.mkv",
                "downloads/LinkedFolder",
                "downloads/Pack",
                "downloads/Pack/keep.txt",
                "outside",
                "outside/folder",
                "outside/folder/inner.mkv",
                "outside/secret.mkv",
            ]
        );
        assert_eq!(delete(&dir, &[PathBuf::from("Pack/keep.txt")]).len(), 1);
        assert!(dir.is_dir(), "the download folder itself stays even when it empties");
    }

    fn walk(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(folder) = stack.pop() {
            for entry in std::fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                found.push(path.strip_prefix(root).unwrap().display().to_string());
                if path.symlink_metadata().unwrap().is_dir() {
                    stack.push(path);
                }
            }
        }
        found
    }
}
