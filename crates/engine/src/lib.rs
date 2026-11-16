#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

mod alert;
#[allow(unsafe_code)]
mod bridge;
mod config;
mod create;
mod error;
mod session;
mod torrent;

pub use alert::Alert;
pub use config::{ClientIdentity, Config, ConnectionLimits, Dht, DhtState, UploadPolicy};
pub use create::{PieceSize, TorrentFormat, create_torrent};
pub use error::{EngineError, ErrorKind, NativeError};
pub use session::Session;
pub use torrent::{
    AddTorrent, Added, FileEntry, FileIndex, FilePriority, FileProgress, FileSelection, Magnet, Metainfo,
    MissingPieces, Progress, ResumeData, SelectedPieces, TorrentSource, TorrentState, TorrentStatus, WrittenPieces,
};

/// Where libtorrent reads and writes `path`. On Linux, unless `LANG` ends in `.UTF-8`, libtorrent
/// writes each character outside ASCII as `.`, so `Français.mkv` is `Fran.ais.mkv` on disk.
#[cfg(unix)]
pub fn on_disk(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    PathBuf::from(OsString::from_vec(bridge::ffi::native_path(path.as_os_str().as_bytes())))
}

/// Where libtorrent reads and writes `path`. On Windows it widens UTF-8 paths, so that is `path`.
#[cfg(windows)]
pub fn on_disk(path: &Path) -> PathBuf {
    path.to_path_buf()
}
