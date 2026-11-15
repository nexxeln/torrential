//! A `.torrent` file as the daemon reads it: checked to keep every path inside its folder.

use torrential_core::text::sanitize;
use torrential_core::{Error, ErrorCode, InfoHash, TorrentFile};

#[derive(Clone, PartialEq, Debug)]
pub struct Metainfo {
    pub info_hash: InfoHash,
    pub name: String,
    pub files: Vec<MetaFile>,
    pub single_file: bool,
    pub bytes: Vec<u8>,
    /// The `url-list` web seeds, as the `.torrent` lists them.
    pub web_seeds: Vec<String>,
    checked: (),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetaFile {
    pub path: String,
    pub size: u64,
}

impl Metainfo {
    pub fn new(
        info_hash: InfoHash,
        name: String,
        files: Vec<MetaFile>,
        single_file: bool,
        bytes: Vec<u8>,
    ) -> Result<Self, Error> {
        let escapes = |p: &str| p.is_empty() || p.starts_with('/') || p.split('/').any(|c| c == ".." || c.is_empty());
        if escapes(&name) || name.contains('/') || files.iter().any(|f| escapes(&f.path)) {
            return Err(Error::new(ErrorCode::InvalidTorrent, "the torrent has a file path outside its own folder")
                .with_hint("this torrent is malformed or malicious; do not download it"));
        }
        Ok(Self { info_hash, name, files, single_file, bytes, web_seeds: Vec::new(), checked: () })
    }

    pub fn with_web_seeds(self, web_seeds: Vec<String>) -> Self {
        Self { web_seeds, ..self }
    }

    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Its files with sanitized paths, nothing selected and nothing verified, for choosing what
    /// to play.
    pub fn listing(&self) -> Vec<TorrentFile> {
        (0..)
            .zip(&self.files)
            .map(|(index, f)| TorrentFile {
                index,
                path: sanitize(&f.path),
                size: f.size,
                selected: false,
                verified: 0,
                risk: None,
            })
            .collect()
    }
}

pub type ParseTorrent = fn(&[u8]) -> Result<Metainfo, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake;

    #[test]
    fn paths_that_leave_the_folder_are_refused() {
        let with = |name: &str, path: &str| {
            fake::TorrentFile { info_hash: InfoHash::V1([1; 20]), name: name.into(), files: vec![(path.into(), 1)] }
                .metainfo()
        };
        assert_eq!(with("set", "a/b.mkv").unwrap().files[0].path, "a/b.mkv");
        for (name, path) in
            [("set", "../etc/passwd"), ("set", "/etc/passwd"), ("..", "a"), ("a/b", "c"), ("set", "a//b")]
        {
            assert_eq!(with(name, path).unwrap_err().code, ErrorCode::InvalidTorrent, "{name} {path}");
        }
    }
}
