//! The codes the CLI exits with, which scripts and agents read as its contract.

use torrential_core::ErrorCode;

pub const SUCCESS: u8 = 0;
pub const INTERNAL: u8 = 1;
pub const USAGE: u8 = 2;
pub const REFERENCE: u8 = 3;
pub const CONFLICT: u8 = 4;
pub const NETWORK: u8 = 5;
pub const ENVIRONMENT: u8 = 6;
pub const DOWNLOAD: u8 = 7;
/// A `wait` that hit its timeout while the download is still running. It is an observation, not
/// an error, so no `ErrorCode` has it.
pub const PENDING: u8 = 8;
pub const INTERRUPTED: u8 = 130;

/// Every exit code and what it means, as `torrential schema` lists them.
pub const MEANINGS: [(u8, &str); 10] = [
    (SUCCESS, "success, including an empty search"),
    (INTERNAL, "internal error"),
    (USAGE, "usage error"),
    (REFERENCE, "the reference matched nothing, matched several, or is malformed"),
    (CONFLICT, "conflict with an existing download or file"),
    (NETWORK, "network or search provider failure"),
    (ENVIRONMENT, "environment, sandbox, engine, or state failure"),
    (DOWNLOAD, "disk, size, selection, or verification failure, or a download that stopped with an error"),
    (PENDING, "still pending: the download is running or stopped without finishing"),
    (INTERRUPTED, "interrupted"),
];

pub const fn code(error: ErrorCode) -> u8 {
    use ErrorCode::*;
    match error {
        Internal => INTERNAL,
        Usage | ConfirmationRequired => USAGE,
        InvalidRef | InvalidTorrent | NotFound | AmbiguousRef => REFERENCE,
        JobConflict | PathConflict => CONFLICT,
        ProvidersUnavailable | MetadataTimeout | NetworkUnavailable | FetchFailed => NETWORK,
        SandboxNetworkBlocked
        | SandboxWriteBlocked
        | DaemonUnavailable
        | DaemonVersion
        | StateCorrupt
        | NotPaired
        | LocalOnly => ENVIRONMENT,
        DiskFull | TooLarge | NoSelection | StorageError | VerificationFailed | NoSource => DOWNLOAD,
        Interrupted => INTERRUPTED,
    }
}

/// Whether the same command may succeed if run again later.
pub const fn retryable(error: ErrorCode) -> bool {
    use ErrorCode::*;
    matches!(
        error,
        ProvidersUnavailable | MetadataTimeout | NetworkUnavailable | FetchFailed | DaemonUnavailable | Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_exits_with_a_listed_code_other_than_pending() {
        for error in ErrorCode::ALL {
            let code = code(error);
            assert!(MEANINGS.iter().any(|(listed, _)| *listed == code), "{error:?} exits with {code}");
            assert_ne!(code, PENDING, "{error:?}");
        }
        assert_eq!(
            [ErrorCode::AmbiguousRef, ErrorCode::PathConflict, ErrorCode::ProvidersUnavailable, ErrorCode::DiskFull]
                .map(code),
            [3, 4, 5, 7]
        );
    }
}
