//! Files written whole or not at all.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
        let denied = matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem);
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
    #[cfg(unix)]
    if let Some(permissions) = &kept {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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

/// Leaves only the owner of the file at `path` any access to it, whatever its folder grants, as
/// mode 0600 does on Unix.
#[cfg(windows)]
pub fn owner_only(path: &Path) -> io::Result<()> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };
    use windows::core::{BOOL, HSTRING, w};
    // Full access for the file's owner, and no entries inherited from the folder.
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;FA;;;OW)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(win32)?;
    let (mut present, mut dacl, mut defaulted) = (BOOL(0), std::ptr::null_mut::<ACL>(), BOOL(0));
    let set =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }.and_then(|()| {
            let info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
            unsafe {
                SetNamedSecurityInfoW(
                    &HSTRING::from(path.as_os_str()),
                    SE_FILE_OBJECT,
                    info,
                    None,
                    None,
                    Some(dacl),
                    None,
                )
            }
            .ok()
        });
    let _ = unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    set.map_err(win32)
}

/// The I/O error of a failed Windows call, with its Win32 code, so its kind says what failed. The
/// `windows` crate's own conversion keeps the HRESULT that wraps the code.
#[cfg(windows)]
pub fn win32(error: windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    // FACILITY_WIN32, failed.
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(error)
    }
}
