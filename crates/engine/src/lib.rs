#![deny(unsafe_code)]

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

mod alert;
#[allow(unsafe_code)]
mod bridge;
mod config;
mod create;
mod error;
mod identity;
mod session;
mod torrent;

pub use alert::{Alert, SessionStats, Transport};
pub use config::{ClientIdentity, Config, ConnectionLimits, Dht, DhtState, UploadPolicy};
pub use create::{PieceSize, TorrentFormat, create_torrent};
pub use error::{EngineError, ErrorKind, NativeError};
pub use identity::{InfoHash, InfoHashV1, InfoHashV2, TorrentId};
pub use session::Session;
pub use torrent::{
    AddTorrent, Added, FileEntry, FileIndex, FilePriority, FileProgress, FileSelection, Magnet, Metainfo,
    MissingPieces, Progress, ResumeData, SelectedPieces, TorrentSource, TorrentState, TorrentStatus, WrittenPieces,
};

/// The version of the linked libtorrent, such as `2.1.2.0`.
pub fn libtorrent_version() -> Result<String, EngineError> {
    bridge::ffi::libtorrent_version().map_err(bridge::error)
}

/// Where libtorrent reads and writes `path`. On Linux, unless `LANG` ends in `.UTF-8`, libtorrent
/// writes each character outside ASCII as `.`, so `Français.mkv` is `Fran.ais.mkv` on disk.
pub fn on_disk(path: &Path) -> PathBuf {
    PathBuf::from(OsString::from_vec(bridge::ffi::native_path(path.as_os_str().as_bytes())))
}
