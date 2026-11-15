//! The globs of `--files`, which pick the files of a download.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use torrential_core::{Error, ErrorCode};

/// The globs in `patterns`, matched without case, or a usage error naming the one that is not a glob.
pub fn compile(patterns: &[String]) -> Result<GlobSet, Error> {
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern).case_insensitive(true).literal_separator(false).build().map_err(|e| {
            Error::new(ErrorCode::Usage, format!("--files {pattern:?} is not a valid glob: {}", e.kind()))
                .with_hint("use a glob such as '*.mkv' or an exact path from torrential files")
        })?;
        set.add(glob);
    }
    set.build().map_err(|e| Error::new(ErrorCode::Usage, format!("--files patterns do not compile: {e}")))
}

/// `wanted`, unless no file matched `patterns`: then the error that names them and how to list the
/// paths of `id`, the download `name`.
pub fn some_match(wanted: Vec<bool>, patterns: &[String], name: &str, id: &str) -> Result<Vec<bool>, Error> {
    if wanted.contains(&true) {
        return Ok(wanted);
    }
    let quoted: Vec<String> = patterns.iter().map(|p| format!("{p:?}")).collect();
    Err(Error::new(ErrorCode::NoSelection, format!("no file in {name} matches {}", quoted.join(", ")))
        .with_hint(format!("run torrential files {} to see the paths", &id[..6]))
        .with_next(format!("torrential files {}", &id[..6])))
}
