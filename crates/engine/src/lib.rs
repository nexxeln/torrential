#![deny(unsafe_code)]

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
