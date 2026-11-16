use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{FileTypeExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, GetFinalPathNameByHandleW, MOVE_FILE_FLAGS,
    MoveFileExW,
};
use windows::core::PCWSTR;

use super::{Entry, Identity, Kind};
use crate::fsutil::win32;

/// A folder held open without sharing delete, so it cannot be moved or deleted while held. What
/// is done in it goes through the path the system gives for the handle now.
pub struct Folder(File);

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // `file` is open for as long as the call runs.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.map_err(win32)?;
    Ok(info)
}

fn identity_of(info: &BY_HANDLE_FILE_INFORMATION) -> Identity {
    Identity {
        device: u64::from(info.dwVolumeSerialNumber),
        inode: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    }
}

fn kind_of(info: &BY_HANDLE_FILE_INFORMATION) -> Kind {
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        Kind::Other
    } else if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        Kind::Folder
    } else {
        Kind::File
    }
}

fn link() -> io::Error {
    io::Error::new(io::ErrorKind::NotADirectory, "it is a link or a file, not a folder")
}

/// Opens the folder at `path`, refusing anything else, and a link too unless `follow`.
fn open_folder(path: &Path, follow: bool) -> io::Result<Folder> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS.0;
    if !follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT.0;
    }
    let file = OpenOptions::new()
        .access_mode(FILE_GENERIC_READ.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(flags)
        .open(path)?;
    match kind_of(&information(&file)?) {
        Kind::Folder => Ok(Folder(file)),
        _ => Err(link()),
    }
}

/// Opens what `path` names to read its attributes, whoever else has it open.
fn attributes(path: &Path, follow: bool) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS.0;
    if !follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT.0;
    }
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(flags)
        .open(path)?;
    information(&file)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

impl Folder {
    /// Opens the folder at `path`, following any link on the way, as for a folder the person named.
    pub fn open(path: &Path) -> io::Result<Folder> {
        open_folder(path, true)
    }

    /// Opens the folder at `path`, refusing it when its last part is a link.
    pub fn open_nofollow(path: &Path) -> io::Result<Folder> {
        open_folder(path, false)
    }

    /// Opens the folder `name` in this one, never through a link.
    pub fn open_in(&self, name: &OsStr) -> io::Result<Folder> {
        open_folder(&self.at(name)?, false)
    }

    pub fn try_clone(&self) -> io::Result<Folder> {
        Ok(Folder(self.0.try_clone()?))
    }

    pub fn identity(&self) -> io::Result<Identity> {
        Ok(identity_of(&information(&self.0)?))
    }

    /// `name` in this folder, without following a link.
    pub fn entry(&self, name: &OsStr) -> io::Result<Entry> {
        let info = attributes(&self.at(name)?, false)?;
        let bytes = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
        Ok(Entry { identity: identity_of(&info), kind: kind_of(&info), bytes })
    }

    pub fn names(&self) -> io::Result<Vec<OsString>> {
        std::fs::read_dir(self.path()?)?.map(|entry| Ok(entry?.file_name())).collect()
    }

    /// Makes the folder `name`. Windows has no permission bits; the folder takes its parent's.
    pub fn make(&self, name: &OsStr, _mode: u16) -> io::Result<()> {
        std::fs::create_dir(self.at(name)?)
    }

    /// Makes the file `name`, failing when the name is taken.
    pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
        OpenOptions::new().write(true).create_new(true).open(self.at(name)?)
    }

    /// Removes the file or link `name`.
    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        let path = self.at(name)?;
        match std::fs::remove_file(&path) {
            // A link to a folder goes the way a folder does, and only the link goes.
            Err(e) if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink_dir()) => {
                std::fs::remove_dir(&path).map_err(|_| e)
            }
            removed => removed,
        }
    }

    /// Removes the folder `name` when it is empty.
    pub fn remove_folder(&self, name: &OsStr) -> io::Result<()> {
        std::fs::remove_dir(self.at(name)?)
    }

    /// Moves `name` to `to` in `into` in one step that never replaces what is there. It fails with
    /// `AlreadyExists` when `to` is taken.
    pub fn rename_new(&self, name: &OsStr, into: &Folder, to: &OsStr) -> io::Result<()> {
        let (from, to) = (wide(&self.at(name)?), wide(&into.at(to)?));
        // Both are paths ending in a nul, alive for the call.
        unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), MOVE_FILE_FLAGS(0)) }.map_err(win32)?;
        Ok(())
    }

    /// Where the folder is now, as a `\\?\` path.
    pub fn path(&self) -> io::Result<PathBuf> {
        let handle = HANDLE(self.0.as_raw_handle());
        let mut buffer = vec![0u16; 512];
        loop {
            // `buffer` is as long as the length passed.
            let length = unsafe { GetFinalPathNameByHandleW(handle, &mut buffer, FILE_NAME_NORMALIZED) } as usize;
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            if length < buffer.len() {
                return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
            }
            buffer.resize(length + 1, 0);
        }
    }

    /// Whether `name` is where another disk is mounted. Windows mounts a disk in a folder through
    /// a reparse point, which this module treats as a link and never goes into.
    pub fn mount_root(&self, _name: &OsStr) -> bool {
        false
    }

    fn at(&self, name: &OsStr) -> io::Result<PathBuf> {
        Ok(self.path()?.join(name))
    }
}

/// The identity of what `path` names, following links.
pub fn identity(path: &Path) -> io::Result<Identity> {
    Ok(identity_of(&attributes(path, true)?))
}

/// Whether opening a folder failed because a link, or a file, has its name.
pub fn is_link(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotADirectory
}

/// Whether another program holding a file open stopped it from being moved or deleted.
pub fn is_locked(e: &io::Error) -> bool {
    let locked = [ERROR_SHARING_VIOLATION.0, ERROR_LOCK_VIOLATION.0];
    e.raw_os_error().is_some_and(|code| locked.contains(&(code as u32)))
}

/// The identities of the folder `path` names, or of the nearest one above it that exists, and of
/// every folder above that, as the system finds them, whatever links or names lead there. Windows
/// resolves `..` by the path's text, so the nearest folder is found by dropping parts.
pub fn lineage(path: &Path) -> io::Result<Vec<Identity>> {
    let mut at = std::path::absolute(path)?;
    let real = loop {
        match std::fs::canonicalize(&at) {
            Ok(real) if real.is_dir() => break real,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory => {}
            Err(e) => return Err(e),
        }
        at = at.parent().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?.to_path_buf();
    };
    real.ancestors().map(|folder| identity(folder)).collect()
}
