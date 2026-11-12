use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::bridge::{self, ffi};
use crate::error::EngineError;
use crate::identity::{InfoHash, TorrentId};

/// A file's position in the torrent's file list. Pad files keep their index
/// in libtorrent but never appear in this crate's file lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileIndex(u32);

impl FileIndex {
    pub const fn new(index: u32) -> Self {
        FileIndex(index)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub index: FileIndex,
    /// Relative to the save path. For a multi-file torrent it starts with the
    /// torrent's directory name.
    pub path: PathBuf,
    pub size: u64,
}

/// A magnet URI that libtorrent parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    uri: String,
    info_hash: InfoHash,
    name: Option<String>,
}

impl Magnet {
    pub fn parse(uri: &str) -> Result<Magnet, EngineError> {
        let parsed = ffi::parse_magnet(uri).map_err(bridge::error)?;
        Ok(Magnet {
            uri: uri.to_owned(),
            info_hash: bridge::info_hash(&parsed.hashes)?,
            name: (!parsed.name.is_empty()).then_some(parsed.name),
        })
    }

    pub fn info_hash(&self) -> InfoHash {
        self.info_hash
    }

    /// The `dn` display name, if the URI has one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn as_str(&self) -> &str {
        &self.uri
    }
}

/// The bytes of a `.torrent` file that libtorrent parsed.
#[derive(Clone, PartialEq, Eq)]
pub struct Metainfo {
    bytes: Vec<u8>,
    info_hash: InfoHash,
    name: String,
    piece_length: u32,
    files: Vec<FileEntry>,
    web_seeds: Vec<String>,
}

impl Metainfo {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Metainfo, EngineError> {
        let parsed = ffi::parse_metainfo(&bytes).map_err(bridge::error)?;
        Ok(Metainfo {
            info_hash: bridge::info_hash(&parsed.hashes)?,
            name: parsed.name,
            piece_length: parsed.piece_length,
            files: bridge::file_entries(parsed.files),
            web_seeds: parsed.web_seeds,
            bytes,
        })
    }

    pub fn info_hash(&self) -> InfoHash {
        self.info_hash
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    /// The `url-list` URLs, as the file lists them.
    pub fn web_seeds(&self) -> &[String] {
        &self.web_seeds
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl std::fmt::Debug for Metainfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metainfo")
            .field("info_hash", &self.info_hash)
            .field("name", &self.name)
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// libtorrent resume data: the piece bitfield, file priorities, and save path
/// that let a torrent restart without rehashing its files.
#[derive(Clone, PartialEq, Eq)]
pub struct ResumeData {
    bytes: Vec<u8>,
    info_hash: InfoHash,
}

impl ResumeData {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<ResumeData, EngineError> {
        let parsed = ffi::parse_resume_data(&bytes).map_err(bridge::error)?;
        Ok(ResumeData { info_hash: bridge::info_hash(&parsed.hashes)?, bytes })
    }

    pub(crate) fn new(bytes: Vec<u8>, info_hash: InfoHash) -> Self {
        ResumeData { bytes, info_hash }
    }

    pub fn info_hash(&self) -> InfoHash {
        self.info_hash
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl std::fmt::Debug for ResumeData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeData").field("info_hash", &self.info_hash).field("bytes", &self.bytes.len()).finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentSource {
    Magnet(Magnet),
    Metainfo(Metainfo),
    Resume(ResumeData),
}

impl TorrentSource {
    pub fn info_hash(&self) -> InfoHash {
        match self {
            TorrentSource::Magnet(m) => m.info_hash(),
            TorrentSource::Metainfo(m) => m.info_hash(),
            TorrentSource::Resume(r) => r.info_hash(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSelection {
    All,
    /// Download only these files; every other file gets `FilePriority::Skip`,
    /// including files a magnet's metadata reveals later. An empty set fetches
    /// metadata only.
    Only(BTreeSet<FileIndex>),
}

impl FileSelection {
    pub fn metadata_only() -> Self {
        FileSelection::Only(BTreeSet::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilePriority {
    Skip,
    Low,
    Normal,
    High,
}

/// One torrent to add. The options apply to every source, so they override
/// the save path, paused flag, and file priorities stored in resume data.
#[derive(Debug, Clone)]
pub struct AddTorrent {
    pub source: TorrentSource,
    pub save_path: PathBuf,
    pub files: FileSelection,
    pub paused: bool,
    pub trackers: Vec<String>,
    pub web_seeds: Vec<String>,
    pub peers: Vec<SocketAddr>,
}

impl AddTorrent {
    /// All files, running, with no extra trackers, web seeds, or peers.
    pub fn new(source: TorrentSource, save_path: impl Into<PathBuf>) -> Self {
        AddTorrent {
            source,
            save_path: save_path.into(),
            files: FileSelection::All,
            paused: false,
            trackers: Vec::new(),
            web_seeds: Vec::new(),
            peers: Vec::new(),
        }
    }
}

/// The result of `Session::add`. Adding a torrent the session already has is
/// not an error, and it leaves the existing torrent's options untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    New(InfoHash),
    Existing(InfoHash),
}

impl Added {
    pub fn info_hash(&self) -> InfoHash {
        match self {
            Added::New(h) | Added::Existing(h) => *h,
        }
    }

    pub fn id(&self) -> TorrentId {
        self.info_hash().id()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    CheckingFiles,
    DownloadingMetadata,
    Downloading,
    /// Every selected piece is downloaded and verified.
    Finished,
    /// Every piece is downloaded and verified.
    Seeding,
    CheckingResumeData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentStatus {
    pub info_hash: InfoHash,
    pub name: String,
    pub save_path: PathBuf,
    pub state: TorrentState,
    /// Independent of `state`: a paused torrent keeps the state it had.
    pub paused: bool,
    pub error: Option<crate::NativeError>,
    /// Payload bytes per second.
    pub download_rate: u64,
    pub upload_rate: u64,
    /// Payload bytes since the torrent last started or resumed.
    pub downloaded_payload: u64,
    pub uploaded_payload: u64,
    pub peers: u32,
    pub seeds: u32,
    pub progress: Progress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    AwaitingMetadata,
    Known(Vec<FileProgress>),
}

impl Progress {
    /// Total size of the files not set to `FilePriority::Skip`.
    pub fn selected_bytes(&self) -> u64 {
        self.selected().map(|f| f.size).sum()
    }

    /// Verified bytes in the files not set to `FilePriority::Skip`.
    pub fn verified_selected_bytes(&self) -> u64 {
        self.selected().map(|f| f.verified).sum()
    }

    pub fn file(&self, index: FileIndex) -> Option<&FileProgress> {
        match self {
            Progress::AwaitingMetadata => None,
            Progress::Known(files) => files.iter().find(|f| f.index == index),
        }
    }

    fn selected(&self) -> impl Iterator<Item = &FileProgress> {
        let files = match self {
            Progress::AwaitingMetadata => &[][..],
            Progress::Known(files) => files.as_slice(),
        };
        files.iter().filter(|f| f.priority != FilePriority::Skip)
    }
}

/// The pieces that hold bytes of a file not set to `FilePriority::Skip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedPieces {
    pub selected: u32,
    /// Those that passed their hash check.
    pub verified: u32,
}

/// The pieces under a run of a file's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrittenPieces {
    pub pieces: u32,
    /// Those that passed their hash check and are in the file on disk.
    pub written: u32,
}

/// The selected pieces that have not passed their hash check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingPieces {
    pub missing: u32,
    /// Those that no connected peer or web seed has.
    pub unavailable: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileProgress {
    pub index: FileIndex,
    pub size: u64,
    pub priority: FilePriority,
    /// Bytes of this file inside pieces that passed their hash check. Blocks
    /// of a piece that has not been checked do not count.
    pub verified: u64,
}
