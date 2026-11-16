//! Moves a file the person chose the folder for to their Trash, so nothing of theirs that
//! torrential takes away is gone for good.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::super::book::Identity;
use crate::folder::{self, Folder};

/// Where the files of a download in a folder the person chose go when they delete them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Trash {
    /// The Finder Trash, on macOS.
    Finder,
    /// The Recycle Bin, on Windows.
    RecycleBin,
    /// The freedesktop trash: `home` for a file on its disk, and otherwise the trash at the top of
    /// the disk the file is on.
    Freedesktop { home: PathBuf },
    /// Only the freedesktop trash in this folder. Tests use it, so they never touch the person's
    /// Trash.
    At(PathBuf),
}

/// Why a file stays where it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Stays {
    /// Another program has it open, which Windows lets stop a move. It can go once that program
    /// lets go.
    Busy(String),
    /// It stays for good, for this reason.
    For(String),
}

const NO_TRASH: &str = "there is no Trash on the disk it is on";
const NO_SAFE_MOVE: &str = "the disk it is on cannot move it to the Trash in one step that never replaces another file";
const REPLACED: &str = "it was replaced after you asked to delete it";
const BUSY: &str = "another program has it open, so torrential tries again once it lets go";

impl Trash {
    /// The person's Trash on this system. `data_home` is `$XDG_DATA_HOME`, or `~/.local/share`.
    pub fn system(data_home: &Path) -> Self {
        if cfg!(target_os = "macos") {
            Self::Finder
        } else if cfg!(windows) {
            Self::RecycleBin
        } else {
            Self::Freedesktop { home: data_home.join("Trash") }
        }
    }

    /// Moves the file `name` in `folder` to the Trash, while it is the file `file`. `path` is
    /// where the person knows it to be. A file that cannot go stays where it is, and says why.
    pub fn put(&self, folder: &Folder, name: &OsStr, path: &Path, file: Identity) -> Result<(), Stays> {
        match self {
            Self::Finder => finder(folder, name, file).map_err(Stays::For),
            Self::RecycleBin => recycle_bin::put(folder, name, file),
            Self::Freedesktop { home } => {
                let (trash, from) = freedesktop(home, folder, path).map_err(Stays::For)?;
                put_in(&trash, folder, name, &from, file, Folder::rename_new)
            }
            Self::At(dir) => {
                std::fs::create_dir_all(dir).map_err(|_| Stays::For(NO_TRASH.into()))?;
                put_in(&open(dir).map_err(Stays::For)?, folder, name, path, file, Folder::rename_new)
            }
        }
    }
}

/// A rename of a file to a new name that fails with `AlreadyExists` when the new name is taken.
/// Tests stand in for a disk that has none.
type Rename = fn(&Folder, &OsStr, &Folder, &OsStr) -> io::Result<()>;

/// Moves `name` in `from` to `to` in `into` by `rename`, in one step that never replaces what is
/// there: it fails with `AlreadyExists` when `to` is taken, and with `Unsupported` when the disk
/// has no such step, which Linux says as an invalid argument. There is no move in two steps, as a
/// second name and then the first removed, since it would remove for good a file saved at `name`
/// in between.
fn move_new(from: &Folder, name: &OsStr, into: &Folder, to: &OsStr, rename: Rename) -> io::Result<()> {
    match rename(from, name, into, to) {
        Err(e) if matches!(e.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported) => {
            Err(io::ErrorKind::Unsupported.into())
        }
        moved => moved,
    }
}

/// What a move to the Trash took when it was not the file checked: something took the file's
/// place before the move.
enum Replaced {
    /// It went back where it was.
    PutBack,
    /// Something else is at its name by now, so it stays in the Trash.
    Kept,
}

impl Replaced {
    fn why(self) -> String {
        match self {
            Replaced::PutBack => REPLACED.into(),
            Replaced::Kept => format!("{REPLACED}, and what took its place is in the Trash, as its name is taken now"),
        }
    }
}

/// Checks that a move took the file `file` to `to` in `into`, and puts anything else back at
/// `name` in `folder`, where it was.
fn check_moved(into: &Folder, to: &OsStr, folder: &Folder, name: &OsStr, file: Identity) -> Result<(), Replaced> {
    match into.entry(to) {
        Ok(entry) if entry.identity == file => Ok(()),
        _ => match move_new(into, to, folder, name, Folder::rename_new) {
            Ok(()) => Err(Replaced::PutBack),
            Err(_) => Err(Replaced::Kept),
        },
    }
}

/// NSFileManager's `trashItemAtURL`, as Finder moves a file to the Trash, never deletes: on a
/// disk with no Trash it fails.
#[cfg(target_os = "macos")]
fn finder(folder: &Folder, name: &OsStr, file: Identity) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSURL};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr::NonNull;
    const NS_FEATURE_UNSUPPORTED_ERROR: isize = 3328;
    let failed = |why: String| format!("it could not go to the Trash: {why}");
    // Where the folder checked is now, so what goes is the file found there.
    let place = folder.path().map_err(|e| failed(e.to_string()))?;
    let path = CString::new(place.join(name).as_os_str().as_bytes()).map_err(|e| failed(e.to_string()))?;
    objc2::rc::autoreleasepool(|_| {
        let path = NonNull::new(path.as_ptr().cast_mut()).expect("a C string is never null");
        // `path` is a C string that outlives the call.
        let url = unsafe { NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(path, false, None) };
        let mut moved = None;
        NSFileManager::defaultManager().trashItemAtURL_resultingItemURL_error(&url, Some(&mut moved)).map_err(
            |error| {
                if error.code() == NS_FEATURE_UNSUPPORTED_ERROR {
                    NO_TRASH.to_string()
                } else {
                    failed(error.localizedDescription().to_string())
                }
            },
        )?;
        let unchecked = "it is in the Trash, which did not say where, so torrential cannot check it was the file";
        let at = moved.and_then(|url| url.path()).map(|path| PathBuf::from(path.to_string())).ok_or(unchecked)?;
        let (Some(dir), Some(to)) = (at.parent(), at.file_name()) else { return Err(unchecked.into()) };
        check_moved(&open(dir).map_err(|_| unchecked)?, to, folder, name, file).map_err(Replaced::why)
    })
}

#[cfg(not(target_os = "macos"))]
fn finder(_: &Folder, _: &OsStr, _: Identity) -> Result<(), String> {
    Err(NO_TRASH.to_string())
}

/// The freedesktop trash for a file in `folder` at `path`, and the path to note it came from:
/// `home` when it is on the same disk, and otherwise the trash at the top of the file's disk,
/// which notes the path from that top folder.
#[cfg(unix)]
fn freedesktop(home: &Path, folder: &Folder, path: &Path) -> Result<(Folder, PathBuf), String> {
    let device = folder.identity().map_err(|_| NO_TRASH)?.device;
    let on_disk = |path: &Path| folder::identity(path).is_ok_and(|found| found.device == device);
    if home.ancestors().find(|folder| folder.exists()).is_some_and(on_disk) {
        std::fs::create_dir_all(home).map_err(|_| NO_TRASH)?;
        return Ok((open(home)?, path.to_path_buf()));
    }
    let real = path.parent().and_then(|parent| parent.canonicalize().ok()).ok_or(NO_TRASH)?;
    let top = real.ancestors().take_while(|folder| on_disk(folder)).last().ok_or(NO_TRASH)?;
    let from = real.strip_prefix(top).map_err(|_| NO_TRASH)?.join(path.file_name().ok_or(NO_TRASH)?);
    let top = open(top)?;
    let uid = crate::paths::uid();
    // An administrator's shared .Trash is used only when it is a real folder with the sticky bit.
    let shared = top
        .open_in(OsStr::new(".Trash"))
        .ok()
        .filter(|shared| rustix::fs::fstat(shared).is_ok_and(|stat| stat.st_mode & 0o1000 != 0))
        .and_then(|shared| make_folder(&shared, &uid.to_string()).ok());
    let trash = match shared {
        Some(trash) => trash,
        None => make_folder(&top, &format!(".Trash-{uid}"))?,
    };
    let on_its_disk = trash.identity().is_ok_and(|found| found.device == device);
    match rustix::fs::fstat(&trash) {
        Ok(stat) if stat.st_uid == uid && on_its_disk => Ok((trash, from)),
        _ => Err(NO_TRASH.into()),
    }
}

#[cfg(not(unix))]
fn freedesktop(_: &Path, _: &Folder, _: &Path) -> Result<(Folder, PathBuf), String> {
    Err(NO_TRASH.to_string())
}

/// Moves `name` in `folder` into the freedesktop trash `trash` by `rename`, noting that it came
/// from `from`, and puts it back when it was not the file `file`.
fn put_in(
    trash: &Folder,
    folder: &Folder,
    name: &OsStr,
    from: &Path,
    file: Identity,
    rename: Rename,
) -> Result<(), Stays> {
    let failed = |e: io::Error| match e.kind() {
        _ if folder::is_locked(&e) => Stays::Busy(BUSY.into()),
        io::ErrorKind::CrossesDevices => Stays::For(NO_TRASH.into()),
        io::ErrorKind::Unsupported => Stays::For(NO_SAFE_MOVE.into()),
        _ => Stays::For(format!("it could not go to the Trash: {e}")),
    };
    let files = make_folder(trash, "files").map_err(Stays::For)?;
    let info = make_folder(trash, "info").map_err(Stays::For)?;
    let text = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", escape(from), deletion_date());
    for n in 1u32.. {
        let named = numbered(name, n);
        let mut entry = named.clone();
        entry.push(".trashinfo");
        // The name is taken in info first, and only if no one has, as the spec has every trash do.
        let mut note = match info.create_new(&entry) {
            Ok(note) => note,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(failed(e)),
        };
        let moved = note.write_all(text.as_bytes()).and_then(|()| {
            drop(note);
            move_new(folder, name, &files, &named, rename)
        });
        match moved {
            Ok(()) => {
                return check_moved(&files, &named, folder, name, file).map_err(|replaced| {
                    if let Replaced::PutBack = replaced {
                        let _ = info.remove_file(&entry);
                    }
                    Stays::For(replaced.why())
                });
            }
            Err(e) => {
                let _ = info.remove_file(&entry);
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(failed(e));
                }
            }
        }
    }
    unreachable!("a free name turns up long before u32 runs out")
}

/// Opens the folder at `path`, never through a link at its last part.
fn open(path: &Path) -> Result<Folder, String> {
    Folder::open_nofollow(path).map_err(|_| NO_TRASH.to_string())
}

/// Opens the folder `name` in `dir`, making it, for the person alone, when it is not there.
fn make_folder(dir: &Folder, name: &str) -> Result<Folder, String> {
    match dir.make(name.as_ref(), 0o700) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(NO_TRASH.into()),
    }
    dir.open_in(name.as_ref()).map_err(|_| NO_TRASH.to_string())
}

/// `name` for the `n`th try: `a.mkv`, then `a.2.mkv`, `a.3.mkv`.
fn numbered(name: &OsStr, n: u32) -> OsString {
    let bytes = name.as_encoded_bytes();
    if n == 1 {
        return name.to_os_string();
    }
    let dot = bytes.iter().rposition(|&b| b == b'.').filter(|&at| at > 0).unwrap_or(bytes.len());
    let mut numbered = bytes[..dot].to_vec();
    numbered.extend_from_slice(format!(".{n}").as_bytes());
    numbered.extend_from_slice(&bytes[dot..]);
    // Split at an ASCII dot and joined with ASCII, so it is still the platform's encoding.
    unsafe { OsString::from_encoded_bytes_unchecked(numbered) }
}

/// `path` escaped as in a URL, as the spec says a trash info file holds it.
fn escape(path: &Path) -> String {
    let mut escaped = String::new();
    for &b in path.as_os_str().as_encoded_bytes() {
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
    let now = torrential_core::time::now() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // Each only reads `now` and writes `tm`.
    #[cfg(unix)]
    unsafe {
        libc::localtime_r(&now, &mut tm)
    };
    #[cfg(windows)]
    unsafe {
        libc::localtime_s(&mut tm, &now)
    };
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

/// The Recycle Bin, through the shell's file operation. A file the shell would delete for good,
/// as on a disk with no Recycle Bin or one too large for it, stays where it is instead.
#[cfg(windows)]
mod recycle_bin {
    use std::cell::Cell;
    use std::ffi::OsStr;
    use std::rc::Rc;

    use windows::Win32::Foundation::{E_ABORT, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        COPYENGINE_E_SHARING_VIOLATION_SRC, FOF_ALLOWUNDO, FOF_NO_UI, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE,
        FileOperation, IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
        SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
    };
    use windows::core::{HRESULT, HSTRING, PCWSTR, Ref, implement};

    use super::{BUSY, Identity, NO_TRASH, REPLACED, Stays};
    use crate::folder::Folder;

    /// What the shell said about the file, as the sink saw it.
    #[derive(Default)]
    struct Seen {
        refused: Cell<bool>,
        failed: Cell<HRESULT>,
    }

    /// Lets the shell delete only what goes to the Recycle Bin, and notes what it says.
    #[implement(IFileOperationProgressSink)]
    struct RecycleOnly(Rc<Seen>);

    impl IFileOperationProgressSink_Impl for RecycleOnly_Impl {
        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn FinishOperations(&self, _: HRESULT) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreMoveItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreCopyItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreDeleteItem(&self, flags: u32, _: Ref<IShellItem>) -> windows::core::Result<()> {
            if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
                self.0.refused.set(true);
                return Err(E_ABORT.into());
            }
            Ok(())
        }
        fn PostDeleteItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            done: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            if done.is_err() {
                self.0.failed.set(done);
            }
            Ok(())
        }
        fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostNewItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: &PCWSTR,
            _: u32,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// What the shell did with the file at `path`.
    enum Done {
        Recycled,
        Refused,
        Failed(HRESULT),
    }

    fn recycle(path: &HSTRING) -> windows::core::Result<Done> {
        let operation: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }?;
        unsafe { operation.SetOperationFlags(FOF_NO_UI | FOF_ALLOWUNDO | FOFX_EARLYFAILURE | FOFX_RECYCLEONDELETE) }?;
        let item: IShellItem = unsafe { SHCreateItemFromParsingName(path, None) }?;
        let seen = Rc::new(Seen::default());
        let sink: IFileOperationProgressSink = RecycleOnly(seen.clone()).into();
        unsafe { operation.DeleteItem(&item, &sink) }?;
        let performed = unsafe { operation.PerformOperations() };
        Ok(if seen.refused.get() {
            Done::Refused
        } else if seen.failed.get().is_err() {
            Done::Failed(seen.failed.get())
        } else {
            match performed {
                Ok(()) => Done::Recycled,
                Err(e) => Done::Failed(e.code()),
            }
        })
    }

    pub fn put(folder: &Folder, name: &OsStr, file: Identity) -> Result<(), Stays> {
        let failed = |why: String| Stays::For(format!("it could not go to the Recycle Bin: {why}"));
        match folder.entry(name) {
            Ok(entry) if entry.identity == file => {}
            Ok(_) => return Err(Stays::For(REPLACED.into())),
            Err(e) => return Err(failed(e.to_string())),
        }
        // The shell takes no \\?\ path.
        let full = folder.path().map_err(|e| failed(e.to_string()))?.join(name);
        let text = full.as_os_str().to_string_lossy();
        let plain = text.strip_prefix(r"\\?\").filter(|rest| !rest.starts_with("UNC\\")).unwrap_or(&text);
        let path = HSTRING::from(plain);
        // The shell's file operation needs a single-threaded apartment, so it runs on a thread of its own.
        let done = std::thread::spawn(move || {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok()?;
            let done = recycle(&path);
            unsafe { CoUninitialize() };
            done
        })
        .join()
        .map_err(|_| failed("the shell stopped".into()))?
        .map_err(|e| failed(e.message()))?;
        let busy = [
            COPYENGINE_E_SHARING_VIOLATION_SRC,
            ERROR_SHARING_VIOLATION.to_hresult(),
            ERROR_LOCK_VIOLATION.to_hresult(),
        ];
        match done {
            Done::Recycled => match folder.entry(name) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(failed("it is still there".into())),
            },
            Done::Refused => Err(Stays::For(NO_TRASH.into())),
            Done::Failed(code) if busy.contains(&code) => Err(Stays::Busy(BUSY.into())),
            Done::Failed(code) => Err(failed(windows::core::Error::from(code).message())),
        }
    }
}

#[cfg(not(windows))]
mod recycle_bin {
    use std::ffi::OsStr;

    use super::{Identity, NO_TRASH, Stays};
    use crate::folder::Folder;

    pub fn put(_: &Folder, _: &OsStr, _: Identity) -> Result<(), Stays> {
        Err(Stays::For(NO_TRASH.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(path: &Path) -> Folder {
        Folder::open(path).unwrap()
    }

    fn identity(path: &Path) -> Identity {
        folder::identity(path).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    }

    /// A Downloads folder holding `film.mkv`, and a trash whose `files` already has a file of
    /// that name, left there without its note.
    fn left_in_trash() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, trash) = (tmp.path().join("Downloads"), tmp.path().join("trash"));
        std::fs::create_dir_all(trash.join("files")).unwrap();
        std::fs::create_dir_all(trash.join("info")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(trash.join("files/film.mkv"), "left there").unwrap();
        (tmp, dir, trash)
    }

    #[test]
    fn a_file_left_in_the_trash_without_its_note_is_never_replaced() {
        let (_tmp, dir, trash) = left_in_trash();
        let film = dir.join("film.mkv");
        std::fs::write(&film, "film").unwrap();
        let file = identity(&film);
        put_in(&folder(&trash), &folder(&dir), OsStr::new("film.mkv"), &film, file, Folder::rename_new).unwrap();
        let read = |name: &str| std::fs::read_to_string(trash.join("files").join(name)).unwrap();
        assert_eq!([read("film.mkv"), read("film.2.mkv")], ["left there", "film"]);
        assert_eq!(names(&trash.join("info")), ["film.2.mkv.trashinfo"]);
        assert!(!film.exists());
    }

    #[test]
    fn a_file_stays_where_it_is_on_a_disk_with_no_rename_that_never_replaces() {
        let (_tmp, dir, trash) = left_in_trash();
        let film = dir.join("film.mkv");
        std::fs::write(&film, "the download's film").unwrap();
        // Linux says INVAL to a rename that never replaces on a disk that has none.
        let disk: Rename = |_, _, _, _| Err(io::ErrorKind::InvalidInput.into());
        let why = put_in(&folder(&trash), &folder(&dir), OsStr::new("film.mkv"), &film, identity(&film), disk);
        let refused = "the disk it is on cannot move it to the Trash in one step that never replaces another file";
        assert_eq!(why, Err(Stays::For(refused.to_string())));
        assert_eq!(std::fs::read_to_string(&film).unwrap(), "the download's film");
        assert_eq!(names(&trash.join("files")), ["film.mkv"]);
        assert_eq!(std::fs::read_to_string(trash.join("files/film.mkv")).unwrap(), "left there");
        assert_eq!(names(&trash.join("info")), Vec::<String>::new());
    }

    #[test]
    fn a_file_put_in_place_of_the_one_checked_goes_back_where_it_was() {
        let (_tmp, dir, trash) = left_in_trash();
        let film = dir.join("film.mkv");
        std::fs::write(&film, "the download's film").unwrap();
        let checked = identity(&film);
        // An editor saves by writing a new file and renaming it over the old one.
        std::fs::write(dir.join(".film.mkv.tmp"), "the person's own film").unwrap();
        std::fs::rename(dir.join(".film.mkv.tmp"), &film).unwrap();
        let why = put_in(&folder(&trash), &folder(&dir), OsStr::new("film.mkv"), &film, checked, Folder::rename_new);
        assert_eq!(why, Err(Stays::For("it was replaced after you asked to delete it".to_string())));
        assert_eq!(std::fs::read_to_string(&film).unwrap(), "the person's own film");
        assert_eq!(names(&trash.join("files")), ["film.mkv"]);
        assert_eq!(names(&trash.join("info")), Vec::<String>::new());
    }

    // The note holds a Unix path.
    #[cfg(unix)]
    #[test]
    fn a_file_goes_to_a_freedesktop_trash_under_a_name_no_other_file_there_has() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, trash) = (tmp.path().join("Downloads"), Trash::At(tmp.path().join("trash")));
        std::fs::create_dir_all(&dir).unwrap();
        for (text, n) in [("first", 1), ("second", 2)] {
            std::fs::write(dir.join("a film.mkv"), text).unwrap();
            let file = identity(&dir.join("a film.mkv"));
            trash.put(&folder(&dir), OsStr::new("a film.mkv"), &dir.join("a film.mkv"), file).unwrap();
            assert!(!dir.join("a film.mkv").exists(), "{n}");
        }
        let files = tmp.path().join("trash/files");
        assert_eq!(std::fs::read_to_string(files.join("a film.mkv")).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(files.join("a film.2.mkv")).unwrap(), "second");
        let info = std::fs::read_to_string(tmp.path().join("trash/info/a film.2.mkv.trashinfo")).unwrap();
        let path = format!("Path={}/a%20film.mkv", dir.display());
        assert_eq!(info.lines().take(2).collect::<Vec<_>>(), ["[Trash Info]", path.as_str()]);
    }

    // Only Linux has a freedesktop trash.
    #[cfg(unix)]
    #[test]
    fn the_home_trash_takes_a_file_on_its_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Downloads");
        std::fs::create_dir_all(&dir).unwrap();
        let disc = dir.join("disc.iso");
        std::fs::write(&disc, "disc").unwrap();
        let trash = Trash::Freedesktop { home: tmp.path().join("share/Trash") };
        trash.put(&folder(&dir), OsStr::new("disc.iso"), &disc, identity(&disc)).unwrap();
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

    #[cfg(windows)]
    #[test]
    fn a_file_goes_to_the_recycle_bin_unless_another_program_holds_it() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let film = tmp.path().join("film.mkv");
        std::fs::write(&film, "film").unwrap();
        // A media player opens a file sharing reads and writes, but not deletion.
        let held = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&film).unwrap();
        let put = || Trash::RecycleBin.put(&folder(tmp.path()), OsStr::new("film.mkv"), &film, identity(&film));
        assert_eq!(put(), Err(Stays::Busy(BUSY.into())));
        assert_eq!(std::fs::read_to_string(&film).unwrap(), "film");
        drop(held);
        assert_eq!(put(), Ok(()));
        assert!(!film.exists());
    }
}
