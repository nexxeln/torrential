use std::fmt;

use serde::{Deserialize, Serialize};

use crate::release::SourceReport;
use crate::watching::Tried;

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
