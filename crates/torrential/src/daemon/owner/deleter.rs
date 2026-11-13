//! The one place torrential takes away what a download wrote. A play's folder is deleted
//! through the plays folder as the engine opened it when it started. The files of a download in
//! a folder the person chose go to the Trash, and only while each is still the file it was when
//! the person asked. Nothing here deletes by a path, so a link, or a folder put in place of
//! another, cannot send a deletion anywhere else.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags};
use rustix::io::Errno;
use torrential_core::{Error, ErrorCode, TorrentId};

use super::super::log;
use super::super::store::{Store, Tombstone};
use super::trash::Trash;

/// Which file or folder something is, whatever path reaches it. A folder moved away keeps its
/// identity. A link, or another folder, put in its place has another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}

impl Identity {
    // A device number is signed on macOS.
    #[allow(clippy::unnecessary_cast)]
    fn of(stat: &rustix::fs::Stat) -> Self {
        Self { device: stat.st_dev as u64, inode: stat.st_ino }
    }
}

const STEM: usize = 16;

/// The name of a play's folder: the first 16 hex digits of its torrent's ID, a dash, and a
/// number. Always one part of a path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlayName(String);

impl PlayName {
    pub fn parse(name: &str) -> Option<Self> {
        let (stem, n) = name.split_once('-')?;
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        let ok = stem.len() == STEM
            && stem.bytes().all(hex)
            && (1..=10).contains(&n.len())
            && n.bytes().all(|b| b.is_ascii_digit());
        ok.then(|| Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn folder_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC
}

/// Opens the folder `name` in `dir`, never through a link.
fn open_folder(dir: &OwnedFd, name: impl rustix::path::Arg) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(dir, name, folder_flags() | OFlags::NOFOLLOW, Mode::empty())
}

const MOVED: &str = "the plays folder was moved or replaced since the engine started";

/// The plays folder as the engine opened it when it started.
struct Root {
    path: PathBuf,
    fd: OwnedFd,
    identity: Identity,
}

impl Root {
    /// Opens the plays folder at `path`, making it if it is not there. A link is refused.
    fn open(path: &Path) -> Result<Root, String> {
        let unusable = |why: String| format!("torrential cannot use the plays folder {}: {why}", path.display());
        std::fs::create_dir_all(path).map_err(|e| unusable(e.to_string()))?;
        let fd =
            rustix::fs::openat(CWD, path, folder_flags() | OFlags::NOFOLLOW, Mode::empty()).map_err(|e| match e {
                Errno::LOOP | Errno::NOTDIR => {
                    unusable("it is a link, and torrential deletes plays only in a folder of its own".into())
                }
                e => unusable(io::Error::from(e).to_string()),
            })?;
        let stat = rustix::fs::fstat(&fd).map_err(|e| unusable(io::Error::from(e).to_string()))?;
        Ok(Root { path: path.to_path_buf(), fd, identity: Identity::of(&stat) })
    }

    /// Fails once `path` no longer names the folder opened.
    fn intact(&self) -> Result<(), &'static str> {
        match rustix::fs::statat(CWD, &self.path, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if Identity::of(&stat) == self.identity => Ok(()),
            _ => Err(MOVED),
        }
    }

    /// The name of the play's folder at `dir`, when `dir` is a folder directly in the plays folder.
    fn name(&self, dir: &Path) -> Option<PlayName> {
        (dir.parent()? == self.path).then_some(())?;
        PlayName::parse(dir.file_name()?.to_str()?)
    }
}

/// What removing a download leaves to delete.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Doom {
    /// A play's folder, and everything in it, when it is still the folder made for the play.
    Play { dir: PathBuf, folder: Identity, bytes: u64 },
    /// The files a download wrote into a folder the person chose, as paths inside it.
    Files { dir: PathBuf, files: Vec<PathBuf> },
}

/// What one deletion did.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Report {
    pub deleted: Vec<PathBuf>,
    pub trashed: Vec<PathBuf>,
    /// What it left, and why.
    pub kept: Vec<(PathBuf, String)>,
}

/// A file as it was when the person asked to delete it: the folder it is in, and the file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pin {
    folder: Identity,
    file: Identity,
}

/// What the downloads on the list, and those the engine has yet to let go of, write: every
/// folder each is in, up to the root of the disk, and its files.
#[derive(Default)]
pub struct InUse {
    folders: HashSet<Identity>,
    files: HashSet<Identity>,
}

impl InUse {
    /// Adds a download that writes `targets` in `dir`.
    pub fn add(&mut self, dir: &Path, targets: &[PathBuf]) -> io::Result<()> {
        self.folders.extend(lineage(dir)?);
        for target in targets {
            if let Ok(stat) = rustix::fs::stat(target) {
                self.files.insert(Identity::of(&stat));
            }
        }
        Ok(())
    }
}

/// The identities of the folder `path` names, or of the nearest one above it that exists, and of
/// every folder above that, as the system finds them, whatever links or names lead there.
pub fn lineage(path: &Path) -> io::Result<Vec<Identity>> {
    let start = if path.is_absolute() { "/" } else { "." };
    let mut fd = rustix::fs::openat(CWD, start, folder_flags(), Mode::empty())?;
    // How many parts deep `path` goes below `fd` into folders not made yet.
    let mut unmade = 0usize;
    for part in path.components() {
        match part {
            Component::Normal(name) if unmade == 0 => {
                match rustix::fs::openat(&fd, name, folder_flags(), Mode::empty()) {
                    Ok(next) => fd = next,
                    Err(Errno::NOENT | Errno::NOTDIR) => unmade = 1,
                    Err(e) => return Err(e.into()),
                }
            }
            Component::Normal(_) => unmade += 1,
            Component::ParentDir if unmade > 0 => unmade -= 1,
            Component::ParentDir => fd = rustix::fs::openat(&fd, "..", folder_flags(), Mode::empty())?,
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut lineage = Vec::new();
    loop {
        let identity = Identity::of(&rustix::fs::fstat(&fd)?);
        // The root's parent is itself.
        if lineage.last() == Some(&identity) {
            return Ok(lineage);
        }
        lineage.push(identity);
        fd = rustix::fs::openat(&fd, "..", folder_flags(), Mode::empty())?;
    }
}

/// One deletion and the download whose engine handle has to close first.
struct Pending {
    job: TorrentId,
    what: What,
    /// Run before, so its outcome is no longer news for an `rm`.
    ran: bool,
    /// What it kept the last time it failed, so each failure is said once.
    failed: Option<Vec<(PathBuf, String)>>,
}

enum What {
    /// A play's folder, on record from the download's removal on. `None` when the download's
    /// folder is not one directly in the plays folder.
    Play { key: Option<i64>, name: Option<PlayName>, dir: PathBuf, folder: Identity, bytes: u64 },
    /// Files in a folder the person chose, each as it was when they asked, or why it stays. Only
    /// in memory, so a stop before they go leaves them where they are.
    Files { dir: PathBuf, files: Vec<(PathBuf, Result<Pin, String>)> },
}

/// How a run of one deletion ended.
enum Outcome {
    Done(Report),
    /// It is tried again at the next run.
    Again(Report),
    /// Not done, and never tried again while the engine runs.
    Refused(Report),
}

/// Deletes what removing a download leaves to delete once the engine lets go of the download,
/// and keeps what is not done on record until it is.
pub struct Deleter {
    plays: Result<Root, String>,
    trash: Trash,
    pending: Vec<Pending>,
}

impl Deleter {
    /// Opens the plays folder at `plays` and picks up the deletions a stop cut short. When the
    /// folder there is not the one the engine used before, the plays it had are left as they
    /// are, as downloads in a folder the person chose.
    pub fn open(plays: &Path, trash: Trash, store: &mut Store) -> Result<Deleter, Error> {
        let root = Root::open(plays);
        let stopped = Error::new(
            ErrorCode::PathConflict,
            "the plays folder it was in was moved or replaced, so torrential stopped it",
        )
        .with_hint("resume it to go on downloading where it was, or remove it");
        match &root {
            Ok(root) => match store.plays_folder()? {
                Some(identity) if identity == root.identity => {}
                Some(_) => {
                    log(&format!(
                        "the plays folder {} is not the one the engine used before, so torrential deletes none of \
                         the plays it had there, and they stay, stopped, as downloads in a folder you chose",
                        plays.display()
                    ));
                    store.use_plays_folder(root.identity, &stopped)?;
                }
                None => store.use_plays_folder(root.identity, &stopped)?,
            },
            Err(why) => log(why),
        }
        let pending = store
            .deletions()?
            .into_iter()
            .map(|recorded| {
                let dir = plays.join(recorded.play.as_str());
                let (key, name, folder) = (Some(recorded.key), Some(recorded.play), recorded.folder);
                let what = What::Play { key, name, dir, folder, bytes: 0 };
                Pending { job: recorded.job, what, ran: true, failed: None }
            })
            .collect();
        Ok(Deleter { plays: root, trash, pending })
    }

    fn root(&self) -> Result<&Root, String> {
        let root = self.plays.as_ref().map_err(Clone::clone)?;
        root.intact().map_err(str::to_string)?;
        Ok(root)
    }

    /// Whether plays may be deleted now, or why not.
    pub fn can_delete_plays(&self) -> Result<(), String> {
        self.root().map(drop)
    }

    /// Makes a new, empty folder for a play of `id`, and answers where it is and which it is.
    pub fn make_play(&self, id: TorrentId) -> Result<(PathBuf, Identity), Error> {
        let refused = |why: &str| Error::new(ErrorCode::StorageError, format!("no play can start, for {why}"));
        let root = self.plays.as_ref().map_err(|why| refused(why))?;
        root.intact().map_err(|why| {
            refused(why).with_hint("put the folder back, or stop the engine and play again to use the folder there now")
        })?;
        for n in 0u32.. {
            let name = format!("{}-{n}", &id.to_hex()[..STEM]);
            let unmade = |e: Errno| {
                let message = format!("cannot make a folder in {}: {}", root.path.display(), io::Error::from(e));
                Error::new(ErrorCode::StorageError, message)
            };
            match rustix::fs::mkdirat(&root.fd, name.as_str(), Mode::from_raw_mode(0o755)) {
                Ok(()) => {}
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(unmade(e)),
            }
            let made = open_folder(&root.fd, name.as_str()).and_then(|folder| rustix::fs::fstat(&folder));
            return match made {
                Ok(stat) => Ok((root.path.join(name), Identity::of(&stat))),
                Err(e) => {
                    let _ = rustix::fs::unlinkat(&root.fd, name.as_str(), AtFlags::REMOVEDIR);
                    Err(unmade(e))
                }
            };
        }
        unreachable!("a free folder name turns up long before u32 runs out")
    }

    /// Removes the folder `make_play` made at `dir` for a play that did not start, if nothing is
    /// in it.
    pub fn unmake_play(&self, dir: &Path) {
        if let Ok(root) = self.root()
            && let Some(name) = root.name(dir)
        {
            let _ = rustix::fs::unlinkat(&root.fd, name.as_str(), AtFlags::REMOVEDIR);
        }
    }

    /// Refuses a folder for a download the person chose when it is the plays folder or inside it,
    /// whatever names or links lead there.
    pub fn outside_plays(&self, dir: &Path) -> Result<(), Error> {
        let Ok(root) = &self.plays else { return Ok(()) };
        if !lineage(dir).is_ok_and(|lineage| lineage.contains(&root.identity)) {
            return Ok(());
        }
        let message =
            format!("{} is inside {}, the folder torrential keeps plays in", dir.display(), root.path.display());
        Err(Error::new(ErrorCode::PathConflict, message).with_hint("choose another folder with --dir"))
    }

    /// Takes a download off record, and puts on record what of it to delete, both or neither.
    /// It is deleted once the engine lets go of the download.
    pub fn remove(&mut self, store: &mut Store, tombstone: &Tombstone, doom: Option<Doom>) -> Result<(), Error> {
        let job = tombstone.id;
        let what = match doom {
            None => {
                store.remove(tombstone, None)?;
                return Ok(());
            }
            Some(Doom::Play { dir, folder, bytes }) => {
                let name = self.plays.as_ref().ok().and_then(|root| root.name(&dir));
                let key = store.remove(tombstone, name.as_ref().map(|name| (name, folder)))?;
                What::Play { key, name, dir, folder, bytes }
            }
            Some(Doom::Files { dir, files }) => {
                store.remove(tombstone, None)?;
                What::Files { files: pin(&dir, files), dir }
            }
        };
        self.pending.push(Pending { job, what, ran: false, failed: None });
        Ok(())
    }

    /// Whether a deletion of a download not in `draining` is waiting to run.
    pub fn ready(&self, draining: &BTreeSet<TorrentId>) -> bool {
        self.pending.iter().any(|pending| !draining.contains(&pending.job))
    }

    /// The folder of a deletion still waiting for `job`.
    pub fn waiting_in(&self, job: TorrentId) -> Option<&Path> {
        self.pending.iter().find(|pending| pending.job == job && !pending.ran).map(|pending| match &pending.what {
            What::Play { dir, .. } | What::Files { dir, .. } => dir.as_path(),
        })
    }

    /// What the plays folders still to delete take up on disk.
    pub fn play_bytes(&self) -> u64 {
        self.pending.iter().map(|pending| if let What::Play { bytes, .. } = pending.what { bytes } else { 0 }).sum()
    }

    /// Runs each deletion whose download is not in `draining`. `in_use` is what other downloads
    /// write, or why that is not known. What fails is tried again at the next run. Answers what
    /// each deletion run for the first time did, for the `rm` that asked for it.
    pub fn run(
        &mut self,
        draining: &BTreeSet<TorrentId>,
        in_use: Result<&InUse, &str>,
        store: &mut Store,
    ) -> Vec<(TorrentId, Report)> {
        let mut reports = Vec::new();
        for mut pending in std::mem::take(&mut self.pending) {
            if draining.contains(&pending.job) {
                self.pending.push(pending);
                continue;
            }
            let (report, again) = match self.step(&mut pending.what, in_use) {
                Outcome::Done(report) | Outcome::Refused(report) => (report, false),
                Outcome::Again(report) => (report, true),
            };
            for path in &report.deleted {
                log(&format!("deleted {}", path.display()));
            }
            if pending.failed.as_ref() != Some(&report.kept) {
                for (path, why) in &report.kept {
                    log(&format!("kept {}, for {why}", path.display()));
                }
            }
            let key = match &pending.what {
                What::Play { key, .. } => *key,
                What::Files { .. } => None,
            };
            if again {
                pending.failed = Some(report.kept.clone());
            }
            if !pending.ran {
                reports.push((pending.job, report));
            }
            if again {
                pending.ran = true;
                self.pending.push(pending);
            } else if let Some(key) = key
                && let Err(error) = store.deleted(key)
            {
                // It runs again at the next start, and finds done what is done.
                log(&format!("cannot note that a deletion is done: {}", error.message));
            }
        }
        reports
    }

    fn step(&self, what: &mut What, in_use: Result<&InUse, &str>) -> Outcome {
        match what {
            What::Play { name: None, dir, .. } => {
                let why = "it is not a folder directly in the plays folder".to_string();
                Outcome::Refused(Report { kept: vec![(dir.clone(), why)], ..Report::default() })
            }
            What::Play { name: Some(name), dir, folder: made, bytes, .. } => {
                let kept = |why: String| Report { kept: vec![(dir.clone(), why)], ..Report::default() };
                let root = match self.root() {
                    Ok(root) => root,
                    Err(why) => return Outcome::Again(kept(why)),
                };
                let in_use = match in_use {
                    Ok(in_use) => in_use,
                    Err(why) => return Outcome::Again(kept(why.to_string())),
                };
                let folder = match open_folder(&root.fd, name.as_str()) {
                    Ok(folder) => folder,
                    Err(Errno::NOENT) => return Outcome::Done(Report::default()),
                    Err(Errno::LOOP | Errno::NOTDIR) => {
                        return Outcome::Refused(kept("it is a link or a file, not a folder".into()));
                    }
                    Err(e) => return Outcome::Again(kept(format!("it cannot be opened: {}", io::Error::from(e)))),
                };
                match rustix::fs::fstat(&folder) {
                    Ok(stat) if Identity::of(&stat) != *made => {
                        return Outcome::Refused(kept("it is not the folder torrential made for the play".into()));
                    }
                    Ok(stat) if in_use.folders.contains(&Identity::of(&stat)) => {
                        return Outcome::Again(kept("another download uses it".into()));
                    }
                    Ok(_) => {}
                    Err(e) => return Outcome::Again(kept(format!("it cannot be read: {}", io::Error::from(e)))),
                }
                let left = empty(&folder, &in_use.files, made.device);
                drop(folder);
                if left.why.is_none() && rustix::fs::unlinkat(&root.fd, name.as_str(), AtFlags::REMOVEDIR).is_ok() {
                    *bytes = 0;
                    return Outcome::Done(Report { deleted: vec![dir.clone()], ..Report::default() });
                }
                *bytes = left.bytes;
                let why = left.why.unwrap_or("it could not be deleted");
                Outcome::Again(kept(format!("{why}, so torrential tries again")))
            }
            What::Files { dir, files } => {
                let mut report = Report::default();
                let in_use = match in_use {
                    Ok(in_use) => in_use,
                    Err(why) => {
                        report.kept = files.iter().map(|(file, _)| (dir.join(file), why.to_string())).collect();
                        return Outcome::Again(report);
                    }
                };
                // The download's folder itself may be a link, as ~/Downloads often is.
                let folder = match rustix::fs::openat(CWD, dir.as_path(), folder_flags(), Mode::empty()) {
                    Ok(folder) => folder,
                    Err(Errno::NOENT) => return Outcome::Done(report),
                    Err(e) => {
                        let why = format!("the download's folder cannot be opened: {}", io::Error::from(e));
                        report.kept = files.iter().map(|(file, _)| (dir.join(file), why.clone())).collect();
                        return Outcome::Refused(report);
                    }
                };
                for (file, pinned) in files.iter() {
                    let path = dir.join(file);
                    let pin = match pinned {
                        Ok(pin) => pin,
                        Err(why) => {
                            report.kept.push((path, why.clone()));
                            continue;
                        }
                    };
                    let (parent, last) = match open_parent(&folder, file) {
                        Ok(Some(found)) => found,
                        Ok(None) => continue,
                        Err(why) => {
                            report.kept.push((path, why.into()));
                            continue;
                        }
                    };
                    if !rustix::fs::fstat(&parent).is_ok_and(|stat| Identity::of(&stat) == pin.folder) {
                        let why = "the folder it is in was moved or replaced after you asked to delete it";
                        report.kept.push((path, why.into()));
                        continue;
                    }
                    match rustix::fs::statat(&parent, last, AtFlags::SYMLINK_NOFOLLOW) {
                        Err(Errno::NOENT) => continue,
                        Err(e) => report.kept.push((path, format!("it cannot be read: {}", io::Error::from(e)))),
                        Ok(stat) if Identity::of(&stat) != pin.file => {
                            report.kept.push((path, "it was replaced after you asked to delete it".into()))
                        }
                        Ok(stat) if in_use.files.contains(&Identity::of(&stat)) => {
                            report.kept.push((path, "another download uses it".into()))
                        }
                        Ok(_) => match self.trash.put(&parent, last, &path) {
                            Ok(()) => report.trashed.push(path),
                            Err(why) => report.kept.push((path, why)),
                        },
                    }
                }
                for (file, _) in files.iter() {
                    prune(&folder, file);
                }
                Outcome::Done(report)
            }
        }
    }
}

/// Each of `files` in `dir` as it is now, or why it stays. Files not there are left out.
fn pin(dir: &Path, files: Vec<PathBuf>) -> Vec<(PathBuf, Result<Pin, String>)> {
    let folder = match rustix::fs::openat(CWD, dir, folder_flags(), Mode::empty()) {
        Ok(folder) => folder,
        Err(Errno::NOENT) => return vec![],
        Err(e) => {
            let why = format!("the download's folder cannot be opened: {}", io::Error::from(e));
            return files.into_iter().map(|file| (file, Err(why.clone()))).collect();
        }
    };
    let pinned = |file: &Path| -> Option<Result<Pin, String>> {
        let (parent, last) = match open_parent(&folder, file) {
            Ok(found) => found?,
            Err(why) => return Some(Err(why.into())),
        };
        let unreadable = |e: Errno| Some(Err(format!("it cannot be read: {}", io::Error::from(e))));
        let stat = match rustix::fs::statat(&parent, last, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return None,
            Err(e) => return unreadable(e),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Some(Err("it is not a regular file".into()));
        }
        match rustix::fs::fstat(&parent) {
            Ok(folder) => Some(Ok(Pin { folder: Identity::of(&folder), file: Identity::of(&stat) })),
            Err(e) => unreadable(e),
        }
    };
    files.into_iter().filter_map(|file| Some((file.clone(), pinned(&file)?))).collect()
}

/// What deleting everything in a folder left.
struct Left {
    /// What it takes up on disk.
    bytes: u64,
    why: Option<&'static str>,
}

/// Deletes everything in `folder` on the disk `device` without following a link, and keeps the
/// files `keep` names. It never goes into another disk mounted inside.
fn empty(folder: &OwnedFd, keep: &HashSet<Identity>, device: u64) -> Left {
    let mut left = Left { bytes: 0, why: None };
    fn fail(left: &mut Left, why: &'static str) {
        left.why.get_or_insert(why);
    }
    let names: Vec<Vec<u8>> = match rustix::fs::Dir::read_from(folder) {
        Ok(mut entries) => std::iter::from_fn(|| entries.read())
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_bytes().to_vec())
            .filter(|name| name != b"." && name != b"..")
            .collect(),
        Err(_) => {
            fail(&mut left, "it cannot be read");
            return left;
        }
    };
    for name in names {
        let name = OsStr::from_bytes(&name);
        let Ok(stat) = rustix::fs::statat(folder, name, AtFlags::SYMLINK_NOFOLLOW) else { continue };
        let directory = FileType::from_raw_mode(stat.st_mode) == FileType::Directory;
        if Identity::of(&stat).device != device || mount_root(folder, name) {
            fail(
                &mut left,
                if directory { "a folder in it is on another disk" } else { "a file in it is on another disk" },
            );
        } else if directory {
            match open_folder(folder, name) {
                Ok(inner) => {
                    let inside = empty(&inner, keep, device);
                    left.bytes += inside.bytes;
                    if let Some(why) = inside.why {
                        fail(&mut left, why);
                        continue;
                    }
                }
                Err(_) => {
                    fail(&mut left, "a folder in it cannot be opened");
                    continue;
                }
            }
            if rustix::fs::unlinkat(folder, name, AtFlags::REMOVEDIR).is_err() {
                fail(&mut left, "a folder in it could not be deleted");
            }
        } else if keep.contains(&Identity::of(&stat)) {
            left.bytes += stat.st_blocks as u64 * 512;
            fail(&mut left, "another download uses a file in it");
        } else if rustix::fs::unlinkat(folder, name, AtFlags::empty()).is_err() {
            left.bytes += stat.st_blocks as u64 * 512;
            fail(&mut left, "a file in it could not be deleted");
        }
    }
    left
}

/// Whether `name` in `folder` is where a file system, or a part of one, is mounted. Only Linux
/// tells, and only Linux mounts a part of the same disk, which needs root.
fn mount_root(folder: &OwnedFd, name: &OsStr) -> bool {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{StatxAttributes, StatxFlags};
        let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
        rustix::fs::statx(folder, name, flags, StatxFlags::empty()).is_ok_and(|found| {
            found.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT)
                && found.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (folder, name);
        false
    }
}

/// Opens the folder `inside` leads to below `folder` and names its last part, going down one
/// folder at a time without following a link. `None` when a folder on the way is missing.
fn open_parent<'a>(folder: &OwnedFd, inside: &'a Path) -> Result<Option<(OwnedFd, &'a OsStr)>, &'static str> {
    let leaves = "its path leaves the download's folder";
    let mut parts = Vec::new();
    for part in inside.components() {
        match part {
            Component::Normal(name) => parts.push(name),
            _ => return Err(leaves),
        }
    }
    let Some((last, folders)) = parts.split_last() else { return Err(leaves) };
    let mut fd = rustix::fs::openat(folder, ".", folder_flags(), Mode::empty())
        .map_err(|_| "the download's folder cannot be opened")?;
    for name in folders {
        fd = match open_folder(&fd, *name) {
            Ok(next) => next,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP | Errno::NOTDIR) => return Err("a folder on its path is a link"),
            Err(_) => return Err("a folder on its path cannot be opened"),
        };
    }
    Ok(Some((fd, last)))
}

/// Removes the folders on the way to `inside` below `folder` that are left empty, deepest first.
fn prune(folder: &OwnedFd, inside: &Path) {
    let mut at = inside.parent();
    while let Some(path) = at.filter(|p| !p.as_os_str().is_empty()) {
        let Ok(Some((parent, name))) = open_parent(folder, path) else { return };
        // Removes only an empty folder, and never through a link.
        if rustix::fs::unlinkat(&parent, name, AtFlags::REMOVEDIR).is_err() {
            return;
        }
        at = path.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Place {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        store: Store,
    }

    fn place() -> Place {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let store = Store::open(&root.join("state.db")).unwrap();
        Place { _tmp: tmp, root, store }
    }

    fn trash(root: &Path) -> Trash {
        Trash::At(root.join("trash"))
    }

    fn id(n: u8) -> TorrentId {
        TorrentId::from_bytes([n; 20])
    }

    fn tomb(n: u8) -> Tombstone {
        Tombstone { id: id(n), name: None, path: None, removed_at: 1 }
    }

    fn run(deleter: &mut Deleter, store: &mut Store, in_use: &InUse) -> Vec<Report> {
        deleter.run(&BTreeSet::new(), Ok(in_use), store).into_iter().map(|(_, report)| report).collect()
    }

    fn files(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_play_folder_name_is_always_one_part_of_a_path() {
        assert!(PlayName::parse("abababababababab-0").is_some());
        for name in
            ["abababababababab", "abababababababab-", "ABABABABABABABAB-0", "abab-0", "..", "a/b-0", "notes.txt"]
        {
            assert_eq!(PlayName::parse(name), None, "{name}");
        }
    }

    #[test]
    fn a_play_folder_goes_and_a_link_inside_it_is_removed_without_touching_what_it_points_to() {
        let Place { root, store, .. } = &mut place();
        let mut deleter = Deleter::open(&root.join("plays"), trash(root), store).unwrap();
        let outside = root.join("outside");
        write(&outside.join("films/keep.mp4"), "keep");
        write(&outside.join("keep.mp4"), "keep");
        let (folder, made) = deleter.make_play(id(1)).unwrap();
        assert_eq!(folder, root.join("plays/0101010101010101-0"));
        assert_eq!(deleter.make_play(id(1)).unwrap().0, root.join("plays/0101010101010101-1"));
        write(&folder.join("Film/film.mp4"), "film");
        symlink(outside.join("keep.mp4"), folder.join("Film/film.srt")).unwrap();
        symlink(outside.join("films"), folder.join("Film/extras")).unwrap();

        deleter.remove(store, &tomb(1), Some(Doom::Play { dir: folder.clone(), folder: made, bytes: 4 })).unwrap();
        assert_eq!(deleter.play_bytes(), 4);
        assert_eq!(
            run(&mut deleter, store, &InUse::default()),
            [Report { deleted: vec![folder.clone()], ..Report::default() }]
        );
        assert!(!folder.exists());
        assert_eq!(std::fs::read_to_string(outside.join("keep.mp4")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(outside.join("films/keep.mp4")).unwrap(), "keep");
        assert_eq!((deleter.play_bytes(), store.deletions().unwrap()), (0, vec![]));
    }

    #[test]
    fn a_play_folder_on_record_that_was_replaced_before_a_start_is_left_alone() {
        let Place { root, store, .. } = &mut place();
        let deleter = Deleter::open(&root.join("plays"), trash(root), store).unwrap();
        let (folder, made) = deleter.make_play(id(1)).unwrap();
        write(&folder.join("film.mp4"), "film");
        let name = PlayName::parse("0101010101010101-0").unwrap();
        store.remove(&tomb(1), Some((&name, made))).unwrap();
        drop(deleter);
        std::fs::rename(&folder, root.join("plays/moved")).unwrap();
        write(&folder.join("notes.txt"), "the person's own notes");

        let mut deleter = Deleter::open(&root.join("plays"), trash(root), store).unwrap();
        assert_eq!(run(&mut deleter, store, &InUse::default()), Vec::<Report>::new());
        assert_eq!(std::fs::read_to_string(folder.join("notes.txt")).unwrap(), "the person's own notes");
        assert_eq!(std::fs::read_to_string(root.join("plays/moved/film.mp4")).unwrap(), "film");
        assert_eq!(store.deletions().unwrap(), [], "it is not tried again");
    }

    /// A small disk image mounted at a folder until it is dropped, or `None` when the system
    /// cannot mount one.
    #[cfg(target_os = "macos")]
    struct Mounted(PathBuf);

    #[cfg(target_os = "macos")]
    impl Mounted {
        fn at(point: &Path, image: &Path) -> Option<Mounted> {
            let hdiutil = |args: &[&std::ffi::OsStr]| {
                std::process::Command::new("hdiutil").args(args).status().is_ok_and(|status| status.success())
            };
            std::fs::create_dir_all(point).unwrap();
            let size = ["create", "-quiet", "-size", "1m", "-fs", "HFS+", "-volname", "torrential-test"];
            let made = hdiutil(&[size.map(std::ffi::OsStr::new).as_slice(), &[image.as_os_str()]].concat());
            let attach = ["attach", "-quiet", "-nobrowse", "-mountpoint"].map(std::ffi::OsStr::new);
            (made && hdiutil(&[attach.as_slice(), &[point.as_os_str(), image.as_os_str()]].concat()))
                .then(|| Mounted(point.to_path_buf()))
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for Mounted {
        fn drop(&mut self) {
            let _ = std::process::Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&self.0).status();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_disk_mounted_inside_a_play_folder_is_left_alone() {
        let Place { root, store, .. } = &mut place();
        let mut deleter = Deleter::open(&root.join("plays"), trash(root), store).unwrap();
        let (folder, made) = deleter.make_play(id(1)).unwrap();
        write(&folder.join("film.mp4"), "film");
        let Some(_disk) = Mounted::at(&folder.join("extras"), &root.join("disk.dmg")) else {
            eprintln!("hdiutil cannot mount a disk image here, so this test proves nothing");
            return;
        };
        write(&folder.join("extras/keep.txt"), "on another disk");

        deleter.remove(store, &tomb(1), Some(Doom::Play { dir: folder.clone(), folder: made, bytes: 4 })).unwrap();
        let why = "a folder in it is on another disk, so torrential tries again";
        assert_eq!(
            run(&mut deleter, store, &InUse::default()),
            [Report { kept: vec![(folder.clone(), why.into())], ..Report::default() }]
        );
        assert_eq!(std::fs::read_to_string(folder.join("extras/keep.txt")).unwrap(), "on another disk");
        assert!(!folder.join("film.mp4").exists(), "what is on the play folder's own disk goes");
    }

    #[test]
    fn a_plays_folder_that_is_a_link_is_never_used() {
        let Place { root, store, .. } = &mut place();
        write(&root.join("disk/plays/0101010101010101-0/film.mp4"), "film");
        std::fs::create_dir(root.join("data")).unwrap();
        symlink(root.join("disk/plays"), root.join("data/plays")).unwrap();
        let mut deleter = Deleter::open(&root.join("data/plays"), trash(root), store).unwrap();
        let why = format!(
            "torrential cannot use the plays folder {}: it is a link, and torrential deletes plays only in a folder of its own",
            root.join("data/plays").display()
        );
        assert_eq!(deleter.make_play(id(2)).unwrap_err().message, format!("no play can start, for {why}"));

        let dir = root.join("data/plays/0101010101010101-0");
        let folder = Identity { device: 1, inode: 1 };
        deleter.remove(store, &tomb(1), Some(Doom::Play { dir: dir.clone(), folder, bytes: 4 })).unwrap();
        let why = "it is not a folder directly in the plays folder";
        let kept = Report { kept: vec![(dir, why.into())], ..Report::default() };
        assert_eq!(run(&mut deleter, store, &InUse::default()), [kept]);
        assert_eq!(std::fs::read_to_string(root.join("disk/plays/0101010101010101-0/film.mp4")).unwrap(), "film");
    }

    #[test]
    fn files_in_a_chosen_folder_go_to_the_trash_but_links_and_what_another_download_uses_stay() {
        let Place { root, store, .. } = &mut place();
        let dir = root.join("Downloads");
        let outside = root.join("outside");
        write(&dir.join("Set/a.mkv"), "a");
        write(&dir.join("Set/docs/b.txt"), "b");
        write(&dir.join("Set/shared.nfo"), "s");
        write(&dir.join("mine.txt"), "mine");
        write(&outside.join("c.srt"), "keep");
        write(&outside.join("extras/d.txt"), "keep");
        symlink(outside.join("c.srt"), dir.join("Set/c.srt")).unwrap();
        symlink(outside.join("extras"), dir.join("Set/extras")).unwrap();
        let mut in_use = InUse::default();
        in_use.add(&root.join("Other"), &[dir.join("Set/shared.nfo")]).unwrap();
        let mut deleter = Deleter::open(&root.join("plays"), trash(root), store).unwrap();
        let all = [
            "Set/a.mkv",
            "Set/docs/b.txt",
            "Set/c.srt",
            "Set/extras/d.txt",
            "Set/shared.nfo",
            "Set/gone.mkv",
            "../mine.txt",
        ];
        deleter.remove(store, &tomb(1), Some(Doom::Files { dir: dir.clone(), files: files(&all) })).unwrap();

        let reports = run(&mut deleter, store, &in_use);
        let kept = [
            ("Set/c.srt", "it is not a regular file"),
            ("Set/extras/d.txt", "a folder on its path is a link"),
            ("Set/shared.nfo", "another download uses it"),
            ("../mine.txt", "its path leaves the download's folder"),
        ]
        .map(|(file, why)| (dir.join(file), why.to_string()));
        let trashed = vec![dir.join("Set/a.mkv"), dir.join("Set/docs/b.txt")];
        assert_eq!(reports, [Report { trashed, kept: kept.to_vec(), ..Report::default() }]);
        assert_eq!(std::fs::read_to_string(root.join("trash/files/b.txt")).unwrap(), "b");
        assert!(!dir.join("Set/docs").exists(), "a folder it left empty goes");
        assert!(dir.join("Set").exists(), "a folder with something left in it stays");
        assert_eq!(std::fs::read_to_string(outside.join("c.srt")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(outside.join("extras/d.txt")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(dir.join("mine.txt")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(dir.join("Set/shared.nfo")).unwrap(), "s");
    }

    #[test]
    fn the_lineage_of_a_folder_not_made_yet_starts_where_its_path_really_goes() {
        let Place { root, .. } = &place();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        symlink(root.join("a/b"), root.join("link")).unwrap();
        let of = |path: &Path| lineage(path).unwrap();
        assert_eq!(of(&root.join("link/new/deeper")), of(&root.join("a/b")));
        assert_eq!(of(&root.join("a/new/../../a/b/new")), of(&root.join("a/b")));
        assert_eq!(of(&root.join("a/b/new/..")), of(&root.join("a/b")));
        assert_eq!(of(&root.join("a/b/new/../..")), of(&root.join("a")), "a .. past what is not made yet climbs");
        assert_eq!(of(Path::new("/")).len(), 1);
    }
}
