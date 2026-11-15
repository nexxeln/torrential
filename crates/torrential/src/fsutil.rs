//! Files written whole or not at all, and folders opened without following a link.

use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use sha2::{Digest, Sha256};
use torrential_core::{Error, ErrorCode};

use crate::env::Env;

/// Turns a failed write into the error a person can act on: a sandbox that blocks it, or a folder
/// that is not writable, with the command to run again.
pub struct Writes {
    pub sandboxed: bool,
    pub rerun: String,
}

impl Writes {
    pub fn new(env: &Env, rerun: String) -> Self {
        Self { sandboxed: env.sandbox_harness().is_some(), rerun }
    }

    pub fn error(&self, path: &Path, e: io::Error) -> Error {
        let denied = matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::EROFS));
        let message = format!("cannot write {}: {e}", path.display());
        if denied && self.sandboxed {
            Error::new(ErrorCode::SandboxWriteBlocked, message).with_hint(format!(
                "the sandbox blocks this write; ask the user to run it in a terminal: {}",
                self.rerun
            ))
        } else {
            Error::new(ErrorCode::StorageError, message)
                .with_hint(format!("make sure that folder is writable, then run {}", self.rerun))
        }
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let kept = std::fs::metadata(path).ok().map(|m| m.permissions());
    let partial = temporary(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    if let Some(permissions) = &kept {
        options.mode(permissions.mode());
    }
    options
        .open(&partial)
        .and_then(|mut file| {
            // The umask may have narrowed the mode `open` asked for.
            if let Some(permissions) = kept {
                file.set_permissions(permissions)?;
            }
            file.write_all(bytes)
        })
        .and_then(|()| std::fs::rename(&partial, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })
}

/// Removes the file at `path`. A file that is not there is already removed.
pub fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

pub fn temporary(path: &Path) -> PathBuf {
    let name = path.file_name().expect("written paths end in a name").to_string_lossy();
    path.with_file_name(format!(".{name}.{}.partial", std::process::id()))
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Opens the folder `name` in `dir`, never through a link.
pub fn open_folder(dir: impl AsFd, name: impl rustix::path::Arg) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}
