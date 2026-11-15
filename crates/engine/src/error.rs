use std::fmt;

use torrential_core::TorrentId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    kind: ErrorKind,
    torrent: Option<TorrentId>,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No torrent in the session has this id.
    UnknownTorrent,
    /// The call needs the torrent's info dictionary, or for `metainfo` its v2
    /// piece layers, which have not arrived.
    NoMetadata,
    /// The caller passed a value this crate or libtorrent rejected.
    InvalidInput,
    /// libtorrent or the C++ shim failed.
    Native,
}

impl EngineError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        EngineError { kind, torrent: None, message: message.into() }
    }

    pub(crate) fn about(self, torrent: TorrentId) -> Self {
        EngineError { torrent: Some(torrent), ..self }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The torrent the failed call named.
    pub fn torrent(&self) -> Option<TorrentId> {
        self.torrent
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ErrorKind::UnknownTorrent => "unknown torrent",
            ErrorKind::NoMetadata => "no metadata",
            ErrorKind::InvalidInput => "invalid input",
            ErrorKind::Native => "libtorrent error",
        };
        f.write_str(kind)?;
        if let Some(torrent) = self.torrent {
            write!(f, ": {torrent}")?;
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for EngineError {}

/// An error libtorrent reported in an alert or a torrent's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeError {
    /// The `error_category` name, such as `system` for OS errors.
    pub category: String,
    pub code: i32,
    pub message: String,
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} {})", self.message, self.category, self.code)
    }
}
