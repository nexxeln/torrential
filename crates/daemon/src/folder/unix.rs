use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use super::{Entry, Identity, Kind};

pub struct Folder(OwnedFd);

const FOLDER: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC);

// A device number is signed on macOS.
#[allow(clippy::unnecessary_cast)]
fn identity_of(stat: &rustix::fs::Stat) -> Identity {
    Identity { device: stat.st_dev as u64, inode: stat.st_ino }
}

fn entry_of(stat: &rustix::fs::Stat) -> Entry {
    let kind = match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Folder,
        _ => Kind::Other,
    };
    Entry { identity: identity_of(stat), kind, bytes: stat.st_blocks as u64 * 512 }
}

impl Folder {
    /// Opens the folder at `path`, following any link on the way, as for a folder the person named.
    pub fn open(path: &Path) -> io::Result<Folder> {
        Ok(Folder(rustix::fs::openat(CWD, path, FOLDER, Mode::empty())?))
    }

    /// Opens the folder at `path`, refusing it when its last part is a link.
    pub fn open_nofollow(path: &Path) -> io::Result<Folder> {
        Ok(Folder(rustix::fs::openat(CWD, path, FOLDER | OFlags::NOFOLLOW, Mode::empty())?))
    }

    /// Opens the folder `name` in this one, never through a link.
    pub fn open_in(&self, name: &OsStr) -> io::Result<Folder> {
        Ok(Folder(rustix::fs::openat(&self.0, name, FOLDER | OFlags::NOFOLLOW, Mode::empty())?))
    }

    pub fn try_clone(&self) -> io::Result<Folder> {
        Ok(Folder(rustix::fs::openat(&self.0, ".", FOLDER, Mode::empty())?))
    }

    pub fn identity(&self) -> io::Result<Identity> {
        Ok(identity_of(&rustix::fs::fstat(&self.0)?))
    }

    /// `name` in this folder, without following a link.
    pub fn entry(&self, name: &OsStr) -> io::Result<Entry> {
        Ok(entry_of(&rustix::fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?))
    }

    pub fn names(&self) -> io::Result<Vec<OsString>> {
        let mut entries = rustix::fs::Dir::read_from(&self.0)?;
        let mut names = Vec::new();
        while let Some(entry) = entries.read() {
            let name = entry?.file_name().to_bytes().to_vec();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(&name).to_os_string());
            }
        }
        Ok(names)
    }

    /// Makes the folder `name` with the permission bits `mode`.
    pub fn make(&self, name: &OsStr, mode: u16) -> io::Result<()> {
        Ok(rustix::fs::mkdirat(&self.0, name, Mode::from_raw_mode(mode.into()))?)
    }

    /// Makes the file `name`, readable by the person alone, failing when the name is taken.
    pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(File::from(rustix::fs::openat(&self.0, name, flags, Mode::from_raw_mode(0o600))?))
    }

    /// Removes the file or link `name`.
    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(&self.0, name, AtFlags::empty())?)
    }

    /// Removes the folder `name` when it is empty.
    pub fn remove_folder(&self, name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(&self.0, name, AtFlags::REMOVEDIR)?)
    }

    /// Moves `name` to `to` in `into` in one step that never replaces what is there. It fails with
    /// `AlreadyExists` when `to` is taken.
    pub fn rename_new(&self, name: &OsStr, into: &Folder, to: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::renameat_with(&self.0, name, &into.0, to, RenameFlags::NOREPLACE)?)
    }

    /// Where the folder is now.
    #[cfg(target_os = "macos")]
    pub fn path(&self) -> io::Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::path::PathBuf::from(OsString::from_vec(rustix::fs::getpath(&self.0)?.into_bytes())))
    }

    /// Whether `name` is where a file system, or a part of one, is mounted. Only Linux tells, and
    /// only Linux mounts a part of the same disk, which needs root.
    pub fn mount_root(&self, name: &OsStr) -> bool {
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{StatxAttributes, StatxFlags};
            let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
            rustix::fs::statx(&self.0, name, flags, StatxFlags::empty()).is_ok_and(|found| {
                found.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT)
                    && found.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            false
        }
    }
}

#[cfg(test)]
impl From<OwnedFd> for Folder {
    fn from(fd: OwnedFd) -> Self {
        Folder(fd)
    }
}

impl AsFd for Folder {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// The identity of what `path` names, following links.
pub fn identity(path: &Path) -> io::Result<Identity> {
    Ok(identity_of(&rustix::fs::stat(path)?))
}

/// Whether opening a folder failed because a link, or a file, has its name.
pub fn is_link(e: &io::Error) -> bool {
    matches!(Errno::from_io_error(e), Some(Errno::LOOP | Errno::NOTDIR))
}

/// Whether another program holding a file open stopped it from being moved or deleted. Unix never
/// does.
pub fn is_locked(_: &io::Error) -> bool {
    false
}

/// The identities of the folder `path` names, or of the nearest one above it that exists, and of
/// every folder above that, as the system finds them, whatever links or names lead there.
pub fn lineage(path: &Path) -> io::Result<Vec<Identity>> {
    let start = if path.is_absolute() { "/" } else { "." };
    let mut fd = rustix::fs::openat(CWD, start, FOLDER, Mode::empty())?;
    // How many parts deep `path` goes below `fd` into folders not made yet.
    let mut unmade = 0usize;
    for part in path.components() {
        match part {
            Component::Normal(name) if unmade == 0 => match rustix::fs::openat(&fd, name, FOLDER, Mode::empty()) {
                Ok(next) => fd = next,
                Err(Errno::NOENT | Errno::NOTDIR) => unmade = 1,
                Err(e) => return Err(e.into()),
            },
            Component::Normal(_) => unmade += 1,
            Component::ParentDir if unmade > 0 => unmade -= 1,
            Component::ParentDir => fd = rustix::fs::openat(&fd, "..", FOLDER, Mode::empty())?,
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut lineage = Vec::new();
    loop {
        let identity = identity_of(&rustix::fs::fstat(&fd)?);
        // The root's parent is itself.
        if lineage.last() == Some(&identity) {
            return Ok(lineage);
        }
        lineage.push(identity);
        fd = rustix::fs::openat(&fd, "..", FOLDER, Mode::empty())?;
    }
}
