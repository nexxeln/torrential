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
pub use config::{ClientIdentity, Config, ConnectionLimits, Dht, DhtState, InterfaceName, Listen, UploadPolicy};
pub use create::{PieceSize, TorrentFormat, create_torrent};
pub use error::{EngineError, ErrorKind, NativeError};
pub use session::Session;
pub use torrent::{
    AddTorrent, Added, FileEntry, FileIndex, FilePriority, FileProgress, FileSelection, Magnet, Metainfo,
    MissingPieces, Progress, ResumeData, SelectedPieces, TorrentSource, TorrentState, TorrentStatus, WrittenPieces,
};

/// Where libtorrent reads and writes `path`. On Linux, unless `LANG` ends in `.UTF-8`, libtorrent
/// writes each character outside ASCII as `.`, so `Français.mkv` is `Fran.ais.mkv` on disk.
pub fn on_disk(path: &Path) -> PathBuf {
    from_native(bridge::ffi::native_path(path.as_os_str().as_encoded_bytes()))
}

#[cfg(unix)]
fn from_native(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// libtorrent widens UTF-8 paths on Windows itself, so it hands back what it was given.
#[cfg(windows)]
fn from_native(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}
