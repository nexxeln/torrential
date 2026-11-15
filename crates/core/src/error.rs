use std::fmt;

use serde::{Deserialize, Serialize, Serializer};

use crate::id::ReleaseId;

crate::words! {
    /// Every failure torrential reports. The code is the contract; messages are for people.
    #[derive(Hash)]
    #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
    pub enum ErrorCode {
        Usage = "usage",
        ConfirmationRequired = "confirmation_required",
        InvalidRef = "invalid_ref",
        InvalidTorrent = "invalid_torrent",
        NotFound = "not_found",
        AmbiguousRef = "ambiguous_ref",
        JobConflict = "job_conflict",
        PathConflict = "path_conflict",
        ProvidersUnavailable = "providers_unavailable",
        MetadataTimeout = "metadata_timeout",
        NetworkUnavailable = "network_unavailable",
        FetchFailed = "fetch_failed",
        SandboxNetworkBlocked = "sandbox_network_blocked",
        SandboxWriteBlocked = "sandbox_write_blocked",
        DaemonUnavailable = "daemon_unavailable",
        DaemonVersion = "daemon_version",
        StateCorrupt = "state_corrupt",
        DiskFull = "disk_full",
        TooLarge = "too_large",
        NoSelection = "no_selection",
        StorageError = "storage_error",
        VerificationFailed = "verification_failed",
        /// No connected peer or web seed had the pieces a download still needed, for long enough
        /// that the engine stopped it.
        NoSource = "no_source",
        Interrupted = "interrupted",
        /// A browser on the home network that is not one of the paired devices.
        NotPaired = "not_paired",
        /// Something only the machine running the engine may do, such as adding a device.
        LocalOnly = "local_only",
        Internal = "internal",
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One candidate listed by an `ambiguous_ref` error.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub kind: CandidateKind,
}

crate::words! {
    /// What an `ambiguous_ref` candidate is.
    #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
    pub enum CandidateKind {
        Download = "download",
        SearchResult = "search result",
    }
}

/// Structured context that lets an agent act on an error without parsing its message.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Details {
    Candidates(Vec<Candidate>),
    Sources(Vec<SourceReport>),
    Space {
        required: u64,
        available: u64,
    },
    Sandbox {
        signal: String,
        harness: Option<String>,
    },
    Versions {
        client: String,
        daemon: String,
    },
    Paths(Vec<String>),
    /// The releases a title play tried, and why each did not start.
    Tried(Vec<Tried>),
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Details>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Box<String>>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), hint: None, details: None, next: None }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(Box::new(next.into()));
        self
    }

    pub fn with_details(mut self, details: Details) -> Self {
        self.details = Some(details);
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum SourceOutcome {
    Ok {
        results: u32,
    },
    Cached {
        results: u32,
    },
    Timeout,
    Http {
        status: u16,
    },
    Connect {
        reason: String,
    },
    /// A response that did not parse.
    BadResponse {
        reason: String,
    },
    /// Most rows matched no query word, as when an API returns its latest uploads.
    Irrelevant,
}

impl SourceOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. } | Self::Cached { .. })
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SourceReport {
    pub source: String,
    #[serde(flatten)]
    pub outcome: SourceOutcome,
    pub elapsed_ms: u32,
}

/// Why a play moved past a source.
#[derive(Clone, PartialEq, Debug, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum Skip {
    NoFileList {
        waited_s: u32,
    },
    /// No file reads as this film or episode, or the largest is a sample.
    NoVideo,
    /// The video plays in a browser, ffmpeg is missing, and the file is not MP4, M4V, MOV, or WebM.
    Unplayable {
        container: String,
    },
    /// The video plays on a TV, which does not take its container.
    NotOnTv {
        container: String,
    },
    TooLarge {
        error: Error,
    },
    NoData {
        waited_s: u32,
    },
    Stalled,
    Failed {
        error: Error,
    },
}

/// Why a play moved past a source, as it goes on the wire: with `text`, which says it in plain
/// words. A read leaves the text out.
#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "Skip"))]
pub(crate) enum SkipWire<'a> {
    NoFileList { waited_s: u32, text: String },
    NoVideo { text: String },
    Unplayable { container: &'a str, text: String },
    NotOnTv { container: &'a str, text: String },
    TooLarge { error: &'a Error, text: String },
    NoData { waited_s: u32, text: String },
    Stalled { text: String },
    Failed { error: &'a Error, text: String },
}

impl Serialize for Skip {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = self.to_string();
        let wire = match self {
            Skip::NoFileList { waited_s } => SkipWire::NoFileList { waited_s: *waited_s, text },
            Skip::NoVideo => SkipWire::NoVideo { text },
            Skip::Unplayable { container } => SkipWire::Unplayable { container, text },
            Skip::NotOnTv { container } => SkipWire::NotOnTv { container, text },
            Skip::TooLarge { error } => SkipWire::TooLarge { error, text },
            Skip::NoData { waited_s } => SkipWire::NoData { waited_s: *waited_s, text },
            Skip::Stalled => SkipWire::Stalled { text },
            Skip::Failed { error } => SkipWire::Failed { error, text },
        };
        wire.serialize(serializer)
    }
}

/// Why a play moved past a source, in plain words.
impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Skip::NoFileList { waited_s } => write!(f, "no peer sent its file list in {waited_s}s"),
            Skip::NoVideo => f.write_str("none of its files is the video"),
            Skip::Unplayable { container } => write!(f, "its .{container} file needs ffmpeg, which is not installed"),
            Skip::NotOnTv { container } => write!(f, "the TV does not play its .{container} file"),
            Skip::TooLarge { error } | Skip::Failed { error } => f.write_str(&crate::text::sanitize(&error.message)),
            Skip::NoData { waited_s } => write!(f, "no data arrived in {waited_s}s"),
            Skip::Stalled => f.write_str("it stalled"),
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Tried {
    pub release: ReleaseId,
    pub reason: Skip,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_serialize_to_their_documented_names() {
        for code in ErrorCode::ALL {
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
        }
    }

    #[test]
    fn errors_serialize_with_optional_parts_omitted() {
        let e = Error::new(ErrorCode::NotFound, "no download matches 9b52").with_hint("run torrential status");
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"code":"not_found","message":"no download matches 9b52","hint":"run torrential status"}"#
        );
    }
}
