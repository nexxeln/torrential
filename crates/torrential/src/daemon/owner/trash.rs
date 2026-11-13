//! Moves a file the person chose the folder for to their Trash, so nothing of theirs that
//! torrential takes away is gone for good.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, CWD, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

/// Where the files of a download in a folder the person chose go when they delete them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Trash {
    /// The Finder Trash, on macOS.
    Finder,
    /// The freedesktop trash: `home` for a file on its disk, and otherwise the trash at the top of
    /// the disk the file is on.
    Freedesktop { home: PathBuf },
    /// Only the freedesktop trash in this folder. Tests use it, so they never touch the person's
    /// Trash.
    At(PathBuf),
}

const NO_TRASH: &str = "there is no Trash on the disk it is on";

impl Trash {
    /// The person's Trash on this system. `data_home` is `$XDG_DATA_HOME`, or `~/.local/share`.
    pub fn system(data_home: &Path) -> Self {
        if cfg!(target_os = "macos") { Self::Finder } else { Self::Freedesktop { home: data_home.join("Trash") } }
    }

    /// Moves the file `name` in `folder` to the Trash. `path` is where the person knows it to be.
    /// A file that cannot go stays where it is, and the error says why.
    pub fn put(&self, folder: &OwnedFd, name: &OsStr, path: &Path) -> Result<(), String> {
        match self {
            Self::Finder => finder(folder, name),
            Self::Freedesktop { home } => {
                let (trash, from) = freedesktop(home, folder, path)?;
                put_in(&trash, folder, name, &from)
            }
            Self::At(dir) => {
                std::fs::create_dir_all(dir).map_err(|_| NO_TRASH)?;
                put_in(&open(CWD, dir)?, folder, name, path)
            }
        }
    }
}

/// NSFileManager's `trashItemAtURL`, as Finder moves a file to the Trash, never deletes: on a
/// disk with no Trash it fails.
#[cfg(target_os = "macos")]
fn finder(folder: &OwnedFd, name: &OsStr) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSURL};
    use std::ffi::CString;
    use std::ptr::NonNull;
    const NS_FEATURE_UNSUPPORTED_ERROR: isize = 3328;
    let failed = |why: String| format!("it could not go to the Trash: {why}");
    // Where the folder checked is now, so what goes is the file found there.
    let place = rustix::fs::getpath(folder).map_err(|e| failed(io::Error::from(e).to_string()))?;
    let mut path = place.into_bytes();
    path.push(b'/');
    path.extend_from_slice(name.as_bytes());
    let path = CString::new(path).map_err(|e| failed(e.to_string()))?;
    objc2::rc::autoreleasepool(|_| {
        let path = NonNull::new(path.as_ptr().cast_mut()).expect("a C string is never null");
        // `path` is a C string that outlives the call.
        let url = unsafe { NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(path, false, None) };
        NSFileManager::defaultManager().trashItemAtURL_resultingItemURL_error(&url, None).map_err(|error| {
            if error.code() == NS_FEATURE_UNSUPPORTED_ERROR {
                NO_TRASH.to_string()
            } else {
                failed(error.localizedDescription().to_string())
            }
        })
    })
}

#[cfg(not(target_os = "macos"))]
fn finder(_: &OwnedFd, _: &OsStr) -> Result<(), String> {
    Err(NO_TRASH.to_string())
}

/// The freedesktop trash for a file in `folder` at `path`, and the path to note it came from:
/// `home` when it is on the same disk, and otherwise the trash at the top of the file's disk,
/// which notes the path from that top folder.
fn freedesktop(home: &Path, folder: &OwnedFd, path: &Path) -> Result<(OwnedFd, PathBuf), String> {
    let device = rustix::fs::fstat(folder).map_err(|_| NO_TRASH)?.st_dev;
    let on_disk = |path: &Path| rustix::fs::stat(path).is_ok_and(|stat| stat.st_dev == device);
    if home.ancestors().find(|folder| folder.exists()).is_some_and(on_disk) {
        std::fs::create_dir_all(home).map_err(|_| NO_TRASH)?;
        return Ok((open(CWD, home)?, path.to_path_buf()));
    }
    let real = path.parent().and_then(|parent| parent.canonicalize().ok()).ok_or(NO_TRASH)?;
    let top = real.ancestors().take_while(|folder| on_disk(folder)).last().ok_or(NO_TRASH)?;
    let from = real.strip_prefix(top).map_err(|_| NO_TRASH)?.join(path.file_name().ok_or(NO_TRASH)?);
    let top = open(CWD, top)?;
    let uid = crate::paths::uid();
    // An administrator's shared .Trash is used only when it is a real folder with the sticky bit.
    let shared = open(&top, ".Trash")
        .ok()
        .filter(|shared| rustix::fs::fstat(shared).is_ok_and(|stat| stat.st_mode & 0o1000 != 0))
        .and_then(|shared| make_folder(&shared, &uid.to_string()).ok());
    let trash = match shared {
        Some(trash) => trash,
        None => make_folder(&top, &format!(".Trash-{uid}"))?,
    };
    match rustix::fs::fstat(&trash) {
        Ok(stat) if stat.st_uid == uid && stat.st_dev == device => Ok((trash, from)),
        _ => Err(NO_TRASH.into()),
    }
}

/// Moves `name` in `folder` into the freedesktop trash `trash`, noting that it came from `from`.
fn put_in(trash: &OwnedFd, folder: &OwnedFd, name: &OsStr, from: &Path) -> Result<(), String> {
    let failed = |e: Errno| match e {
        Errno::XDEV => NO_TRASH.to_string(),
        e => format!("it could not go to the Trash: {}", io::Error::from(e)),
    };
    let (files, info) = (make_folder(trash, "files")?, make_folder(trash, "info")?);
    let text = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", escape(from), deletion_date());
    for n in 1u32.. {
        let named = numbered(name, n);
        let mut entry = named.clone();
        entry.push(".trashinfo");
        // The name is taken in info first, and only if no one has, as the spec has every trash do.
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let note = match rustix::fs::openat(&info, &entry, flags, Mode::from_raw_mode(0o600)) {
            Ok(note) => note,
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(failed(e)),
        };
        let moved = match std::fs::File::from(note).write_all(text.as_bytes()) {
            Err(e) => Err(Errno::from_io_error(&e).unwrap_or(Errno::IO)),
            Ok(()) => match rustix::fs::renameat_with(folder, name, &files, &named, RenameFlags::NOREPLACE) {
                // A file system that cannot promise not to replace relies on the name taken in info.
                Err(Errno::INVAL | Errno::NOSYS) => rustix::fs::renameat(folder, name, &files, &named),
                moved => moved,
            },
        };
        match moved {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = rustix::fs::unlinkat(&info, &entry, AtFlags::empty());
                if e != Errno::EXIST {
                    return Err(failed(e));
                }
            }
        }
    }
    unreachable!("a free name turns up long before u32 runs out")
}

/// Opens the folder `name` in `dir`, never through a link.
fn open(dir: impl rustix::fd::AsFd, name: impl rustix::path::Arg) -> Result<OwnedFd, String> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(dir, name, flags, Mode::empty()).map_err(|_| NO_TRASH.to_string())
}

/// Opens the folder `name` in `dir`, making it, for the person alone, when it is not there.
fn make_folder(dir: &OwnedFd, name: &str) -> Result<OwnedFd, String> {
    match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(Errno::EXIST) => open(dir, name),
        Err(_) => Err(NO_TRASH.into()),
    }
}

/// `name` for the `n`th try: `a.mkv`, then `a.2.mkv`, `a.3.mkv`.
fn numbered(name: &OsStr, n: u32) -> OsString {
    let bytes = name.as_bytes();
    if n == 1 {
        return name.to_os_string();
    }
    let dot = bytes.iter().rposition(|&b| b == b'.').filter(|&at| at > 0).unwrap_or(bytes.len());
    let mut numbered = bytes[..dot].to_vec();
    numbered.extend_from_slice(format!(".{n}").as_bytes());
    numbered.extend_from_slice(&bytes[dot..]);
    OsString::from_vec(numbered)
}

/// `path` escaped as in a URL, as the spec says a trash info file holds it.
fn escape(path: &Path) -> String {
    let mut escaped = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()/".contains(&b) {
            escaped.push(b as char);
        } else {
            escaped.push_str(&format!("%{b:02X}"));
        }
    }
    escaped
}

/// Now in local time, as YYYY-MM-DDThh:mm:ss.
fn deletion_date() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r only reads `now` and writes `tm`.
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(path: &Path) -> OwnedFd {
        rustix::fs::openat(CWD, path, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap()
    }

    #[test]
    fn a_file_goes_to_a_freedesktop_trash_under_a_name_no_other_file_there_has() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, trash) = (tmp.path().join("Downloads"), Trash::At(tmp.path().join("trash")));
        std::fs::create_dir_all(&dir).unwrap();
        for (text, n) in [("first", 1), ("second", 2)] {
            std::fs::write(dir.join("a film.mkv"), text).unwrap();
            trash.put(&folder(&dir), OsStr::new("a film.mkv"), &dir.join("a film.mkv")).unwrap();
            assert!(!dir.join("a film.mkv").exists(), "{n}");
        }
        let files = tmp.path().join("trash/files");
        assert_eq!(std::fs::read_to_string(files.join("a film.mkv")).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(files.join("a film.2.mkv")).unwrap(), "second");
        let info = std::fs::read_to_string(tmp.path().join("trash/info/a film.2.mkv.trashinfo")).unwrap();
        let path = format!("Path={}/a%20film.mkv", dir.display());
        assert_eq!(info.lines().take(2).collect::<Vec<_>>(), ["[Trash Info]", path.as_str()]);
    }

    #[test]
    fn the_home_trash_takes_a_file_on_its_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Downloads");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("disc.iso"), "disc").unwrap();
        let trash = Trash::Freedesktop { home: tmp.path().join("share/Trash") };
        trash.put(&folder(&dir), OsStr::new("disc.iso"), &dir.join("disc.iso")).unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path().join("share/Trash/files/disc.iso")).unwrap(), "disc");
    }

    #[test]
    fn a_name_is_numbered_before_its_extension() {
        let named = |name: &str, n| numbered(OsStr::new(name), n).into_string().unwrap();
        assert_eq!(
            [named("a.mkv", 1), named("a.mkv", 2), named("notes", 3), named(".hidden", 2), named("a.b.c", 2)],
            ["a.mkv", "a.2.mkv", "notes.3", ".hidden.2", "a.b.2.c"]
        );
    }
}
