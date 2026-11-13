//! The one place torrential deletes what a download wrote. A play's folder is deleted through
//! the plays folder as the engine opened it when it started. The files of a download in a
//! folder the person chose are first moved into a folder made for them beside the files, and
//! that folder is what gets deleted. Nothing here deletes by a path, so a link, or a folder put
//! in place of another, cannot send a deletion anywhere else.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags};
use rustix::io::Errno;
use torrential_core::{Error, ErrorCode, TorrentId};

use super::super::log;
use super::super::store::{Journaled, Store, Tombstone};

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

const QUARANTINE: &str = ".torrential-deleting-";

/// The name of a folder made beside a download's files, to move them into before they are
/// deleted. Always one part of a path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct QuarantineName(String);

impl QuarantineName {
    pub fn parse(name: &str) -> Option<Self> {
        let rest = name.strip_prefix(QUARANTINE)?;
        let ok = rest.len() == 16 && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        ok.then(|| Self(name.to_string()))
    }

    fn fresh() -> Self {
        static MADE: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        let mixed =
            nanos ^ (u64::from(std::process::id()) << 40) ^ MADE.fetch_add(1, Ordering::Relaxed).rotate_left(20);
        Self(format!("{QUARANTINE}{mixed:016x}"))
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
    /// A play's folder, and everything in it.
    Play { dir: PathBuf, bytes: u64 },
    /// The files a download wrote into a folder the person chose, as paths inside it, and the
    /// folders below it that they leave empty.
    Files { dir: PathBuf, files: Vec<PathBuf> },
}

/// What one deletion did.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Report {
    pub deleted: Vec<PathBuf>,
    /// What it left, and why.
    pub kept: Vec<(PathBuf, String)>,
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
    Play { key: Option<i64>, name: Option<PlayName>, dir: PathBuf, bytes: u64 },
    /// Files in a folder the person chose. Only in memory, so a crash before they are moved aside
    /// leaves whatever is at their names then alone.
    Files { dir: PathBuf, files: Vec<PathBuf> },
    /// A folder made in `dir` that holds only files moved there to delete, on record before the
    /// first one moved in. `moved` are the paths they had.
    Quarantine { key: i64, dir: PathBuf, name: QuarantineName, identity: Identity, moved: Vec<PathBuf>, after: Instant },
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
    pending: Vec<Pending>,
    /// How long files moved aside wait before they are deleted: no time, except in a test that
    /// stops the engine in between.
    pause: Duration,
}

impl Deleter {
    /// Opens the plays folder at `plays` and picks up the deletions a stop cut short. When the
    /// folder there is not the one the engine used before, the plays it had are left as they
    /// are, as downloads in a folder the person chose.
    pub fn open(plays: &Path, store: &mut Store, pause: Duration) -> Result<Deleter, Error> {
        let root = Root::open(plays);
        match &root {
            Ok(root) => match store.plays_folder()? {
                Some(identity) if identity == root.identity => {}
                Some(_) => {
                    log(&format!(
                        "the plays folder {} is not the one the engine used before, so torrential deletes none of \
                         the plays it had there, and they stay as downloads in a folder you chose",
                        plays.display()
                    ));
                    store.use_plays_folder(root.identity)?;
                }
                None => store.use_plays_folder(root.identity)?,
            },
            Err(why) => log(why),
        }
        let pending = store
            .deletions()?
            .into_iter()
            .map(|recorded| {
                let what = match recorded.what {
                    Journaled::Play(name) => {
                        let dir = plays.join(name.as_str());
                        What::Play { key: Some(recorded.key), name: Some(name), dir, bytes: 0 }
                    }
                    Journaled::Quarantine { dir, name, identity } => What::Quarantine {
                        key: recorded.key,
                        dir,
                        name,
                        identity,
                        moved: vec![],
                        after: Instant::now(),
                    },
                };
                Pending { job: recorded.job, what, ran: true, failed: None }
            })
            .collect();
        Ok(Deleter { plays: root, pending, pause })
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

    /// Makes a new, empty folder for a play of `id`.
    pub fn make_play(&self, id: TorrentId) -> Result<PathBuf, Error> {
        let root = self.root().map_err(|why| {
            Error::new(ErrorCode::StorageError, format!("no play can start, for {why}"))
                .with_hint("stop the engine and play again, and torrential uses the folder there then")
        })?;
        for n in 0u32.. {
            let name = format!("{}-{n}", &id.to_hex()[..STEM]);
            match rustix::fs::mkdirat(&root.fd, name.as_str(), Mode::from_raw_mode(0o755)) {
                Ok(()) => return Ok(root.path.join(name)),
                Err(Errno::EXIST) => continue,
                Err(e) => {
                    let message = format!("cannot make a folder in {}: {}", root.path.display(), io::Error::from(e));
                    return Err(Error::new(ErrorCode::StorageError, message));
                }
            }
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

    /// Takes a download off record, and puts on record what of it to delete, both or neither.
    /// It is deleted once the engine lets go of the download.
    pub fn remove(&mut self, store: &mut Store, tombstone: &Tombstone, doom: Option<Doom>) -> Result<(), Error> {
        let job = tombstone.id;
        let what = match doom {
            None => {
                store.remove(tombstone, None)?;
                return Ok(());
            }
            Some(Doom::Play { dir, bytes }) => {
                let name = self.plays.as_ref().ok().and_then(|root| root.name(&dir));
                let key = store.remove(tombstone, name.as_ref())?;
                What::Play { key, name, dir, bytes }
            }
            Some(Doom::Files { dir, files }) => {
                store.remove(tombstone, None)?;
                What::Files { dir, files }
            }
        };
        self.pending.push(Pending { job, what, ran: false, failed: None });
        Ok(())
    }

    /// Whether a deletion of a download not in `draining` is waiting to run.
    pub fn ready(&self, draining: &BTreeSet<TorrentId>) -> bool {
        let now = Instant::now();
        self.pending.iter().any(|pending| {
            !draining.contains(&pending.job) && !matches!(pending.what, What::Quarantine { after, .. } if after > now)
        })
    }

    /// The folder of a deletion still waiting for `job`.
    pub fn waiting_in(&self, job: TorrentId) -> Option<&Path> {
        self.pending.iter().find(|pending| pending.job == job && !pending.ran).map(|pending| match &pending.what {
            What::Play { dir, .. } | What::Files { dir, .. } | What::Quarantine { dir, .. } => dir.as_path(),
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
        let now = Instant::now();
        for mut pending in std::mem::take(&mut self.pending) {
            let later = matches!(pending.what, What::Quarantine { after, .. } if after > now);
            if draining.contains(&pending.job) || later {
                self.pending.push(pending);
                continue;
            }
            let job = pending.job;
            let (report, again) = match self.step(&mut pending.what, job, in_use, store) {
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
                What::Quarantine { key, .. } => Some(*key),
                What::Files { .. } => None,
            };
            if again {
                pending.failed = Some(report.kept.clone());
            }
            if !pending.ran {
                reports.push((pending.job, report));
            }
            match again {
                true => {
                    pending.ran = true;
                    self.pending.push(pending);
                }
                false => {
                    // A deletion left on record runs again at the next start, and finds done what is done.
                    if let Some(key) = key
                        && let Err(error) = store.deleted(key)
                    {
                        log(&format!("cannot note that a deletion is done: {}", error.message));
                    }
                }
            }
        }
        reports
    }

    fn step(&self, what: &mut What, job: TorrentId, in_use: Result<&InUse, &str>, store: &mut Store) -> Outcome {
        match what {
            What::Play { name: None, dir, .. } => {
                let why = "it is not a folder directly in the plays folder".to_string();
                Outcome::Refused(Report { deleted: vec![], kept: vec![(dir.clone(), why)] })
            }
            What::Play { name: Some(name), dir, bytes, .. } => {
                let kept = |why: String| Report { deleted: vec![], kept: vec![(dir.clone(), why)] };
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
                    Ok(stat) if in_use.folders.contains(&Identity::of(&stat)) => {
                        return Outcome::Again(kept("another download uses it".into()));
                    }
                    Ok(_) => {}
                    Err(e) => return Outcome::Again(kept(format!("it cannot be read: {}", io::Error::from(e)))),
                }
                let left = empty(&folder, &in_use.files);
                drop(folder);
                if left.why.is_none() && rustix::fs::unlinkat(&root.fd, name.as_str(), AtFlags::REMOVEDIR).is_ok() {
                    *bytes = 0;
                    return Outcome::Done(Report { deleted: vec![dir.clone()], kept: vec![] });
                }
                *bytes = left.bytes;
                let why = left.why.unwrap_or("it could not be deleted");
                Outcome::Again(kept(format!("{why}, so torrential tries again")))
            }
            What::Files { dir, files } => {
                let mut report = Report::default();
                let dir = dir.clone();
                let in_use = match in_use {
                    Ok(in_use) => in_use,
                    Err(why) => {
                        report.kept = files.iter().map(|file| (dir.join(file), why.to_string())).collect();
                        return Outcome::Again(report);
                    }
                };
                // The download's folder itself may be a link, as ~/Downloads often is.
                let folder = match rustix::fs::openat(CWD, &dir, folder_flags(), Mode::empty()) {
                    Ok(folder) => folder,
                    Err(Errno::NOENT) => return Outcome::Done(report),
                    Err(e) => {
                        let why = format!("the download's folder cannot be opened: {}", io::Error::from(e));
                        report.kept = files.iter().map(|file| (dir.join(file), why.clone())).collect();
                        return Outcome::Refused(report);
                    }
                };
                let mut aside: Option<(OwnedFd, i64, QuarantineName, Identity)> = None;
                let mut moved = Vec::new();
                for (n, file) in files.iter().enumerate() {
                    let path = dir.join(file);
                    let (parent, last) = match open_parent(&folder, file) {
                        Ok(Some(found)) => found,
                        Ok(None) => continue,
                        Err(why) => {
                            report.kept.push((path, why.into()));
                            continue;
                        }
                    };
                    match rustix::fs::statat(&parent, last, AtFlags::SYMLINK_NOFOLLOW) {
                        Err(Errno::NOENT) => continue,
                        Err(e) => {
                            report.kept.push((path, format!("it cannot be read: {}", io::Error::from(e))));
                            continue;
                        }
                        Ok(stat) if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile => {
                            report.kept.push((path, "it is not a regular file".into()));
                            continue;
                        }
                        Ok(stat) if in_use.files.contains(&Identity::of(&stat)) => {
                            report.kept.push((path, "another download uses it".into()));
                            continue;
                        }
                        Ok(_) => {}
                    }
                    if aside.is_none() {
                        match quarantine(&folder, job, &dir, store) {
                            Ok(made) => aside = Some(made),
                            Err(why) => {
                                report.kept.push((path, why));
                                continue;
                            }
                        }
                    }
                    let (into, ..) = aside.as_ref().expect("made above");
                    match rustix::fs::renameat(&parent, last, into, n.to_string()) {
                        Ok(()) => moved.push(path),
                        // Another disk is mounted inside the folder. There is nowhere on it to move
                        // the file to, so it is deleted where it is, without a record.
                        Err(Errno::XDEV) => match rustix::fs::unlinkat(&parent, last, AtFlags::empty()) {
                            Ok(()) => {
                                log(&format!(
                                    "deleted {} where it was, for it is on another disk than its folder",
                                    path.display()
                                ));
                                report.deleted.push(path);
                            }
                            Err(e) => {
                                report.kept.push((path, format!("it could not be deleted: {}", io::Error::from(e))))
                            }
                        },
                        Err(e) => {
                            report.kept.push((path, format!("it could not be moved aside: {}", io::Error::from(e))))
                        }
                    }
                }
                for file in files.iter() {
                    prune(&folder, file);
                }
                let Some((_, key, name, identity)) = aside else { return Outcome::Done(report) };
                *what = What::Quarantine { key, dir, name, identity, moved, after: Instant::now() + self.pause };
                if !self.pause.is_zero() {
                    return Outcome::Again(report);
                }
                match self.step(what, job, Ok(in_use), store) {
                    Outcome::Done(rest) => {
                        report.deleted.extend(rest.deleted);
                        Outcome::Done(report)
                    }
                    Outcome::Again(rest) => {
                        report.kept.extend(rest.kept);
                        Outcome::Again(report)
                    }
                    Outcome::Refused(rest) => {
                        report.kept.extend(rest.kept);
                        Outcome::Refused(report)
                    }
                }
            }
            What::Quarantine { dir, name, identity, moved, .. } => {
                let path = dir.join(name.as_str());
                let kept = |why: String| Report { deleted: vec![], kept: vec![(path.clone(), why)] };
                let folder = match rustix::fs::openat(CWD, &*dir, folder_flags(), Mode::empty()) {
                    Ok(folder) => folder,
                    Err(Errno::NOENT) => return Outcome::Done(Report::default()),
                    Err(e) => {
                        return Outcome::Again(kept(format!("its folder cannot be opened: {}", io::Error::from(e))));
                    }
                };
                let aside = match open_folder(&folder, name.as_str()) {
                    Ok(aside) => aside,
                    Err(Errno::NOENT) => return Outcome::Done(Report { deleted: moved.clone(), kept: vec![] }),
                    Err(Errno::LOOP | Errno::NOTDIR) => {
                        return Outcome::Refused(kept("it is not the folder torrential made".into()));
                    }
                    Err(e) => return Outcome::Again(kept(format!("it cannot be opened: {}", io::Error::from(e)))),
                };
                if !rustix::fs::fstat(&aside).is_ok_and(|stat| Identity::of(&stat) == *identity) {
                    return Outcome::Refused(kept("it is not the folder torrential made".into()));
                }
                let left = empty(&aside, &HashSet::new());
                drop(aside);
                if left.why.is_none() && rustix::fs::unlinkat(&folder, name.as_str(), AtFlags::REMOVEDIR).is_ok() {
                    let deleted = if moved.is_empty() { vec![path] } else { moved.clone() };
                    return Outcome::Done(Report { deleted, kept: vec![] });
                }
                let why = left.why.unwrap_or("it could not be deleted");
                Outcome::Again(kept(format!("{why}, so torrential tries again")))
            }
        }
    }
}

/// Makes a new folder in `folder` to move files into, and puts it on record before anything
/// moves in.
fn quarantine(
    folder: &OwnedFd,
    job: TorrentId,
    dir: &Path,
    store: &mut Store,
) -> Result<(OwnedFd, i64, QuarantineName, Identity), String> {
    let name = loop {
        let name = QuarantineName::fresh();
        match rustix::fs::mkdirat(folder, name.as_str(), Mode::from_raw_mode(0o700)) {
            Ok(()) => break name,
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(format!("there is no room to move it aside: {}", io::Error::from(e))),
        }
    };
    let unmade = |why: String| {
        let _ = rustix::fs::unlinkat(folder, name.as_str(), AtFlags::REMOVEDIR);
        why
    };
    let aside = open_folder(folder, name.as_str()).map_err(|e| unmade(io::Error::from(e).to_string()))?;
    let identity = rustix::fs::fstat(&aside)
        .map(|stat| Identity::of(&stat))
        .map_err(|e| unmade(io::Error::from(e).to_string()))?;
    let key = store
        .quarantined(job, dir, &name, identity)
        .map_err(|error| unmade(format!("the deletion cannot be put on record: {}", error.message)))?;
    Ok((aside, key, name, identity))
}

/// What deleting everything in a folder left.
struct Left {
    /// What it takes up on disk.
    bytes: u64,
    why: Option<&'static str>,
}

/// Deletes everything in `folder` without following a link, and keeps the files `keep` names.
fn empty(folder: &OwnedFd, keep: &HashSet<Identity>) -> Left {
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
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            match open_folder(folder, name) {
                Ok(inner) => {
                    let inside = empty(&inner, keep);
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
    use super::super::super::store::Recorded;
    use super::*;
    use std::os::unix::fs::symlink;

    const NOW: Duration = Duration::ZERO;

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
    fn a_play_folder_name_and_a_quarantine_name_are_each_one_part_of_a_path() {
        assert!(PlayName::parse("abababababababab-0").is_some());
        for name in
            ["abababababababab", "abababababababab-", "ABABABABABABABAB-0", "abab-0", "..", "a/b-0", "notes.txt"]
        {
            assert_eq!(PlayName::parse(name), None, "{name}");
        }
        let fresh = QuarantineName::fresh();
        assert_eq!(QuarantineName::parse(fresh.as_str()), Some(fresh.clone()));
        for name in [".torrential-deleting-", ".torrential-deleting-../../x", "Set"] {
            assert_eq!(QuarantineName::parse(name), None, "{name}");
        }
    }

    #[test]
    fn a_play_folder_goes_and_a_link_inside_it_is_removed_without_touching_what_it_points_to() {
        let Place { root, store, .. } = &mut place();
        let mut deleter = Deleter::open(&root.join("plays"), store, NOW).unwrap();
        let outside = root.join("outside");
        write(&outside.join("films/keep.mp4"), "keep");
        write(&outside.join("keep.mp4"), "keep");
        let folder = deleter.make_play(id(1)).unwrap();
        assert_eq!(folder, root.join("plays/0101010101010101-0"));
        assert_eq!(deleter.make_play(id(1)).unwrap(), root.join("plays/0101010101010101-1"));
        write(&folder.join("Film/film.mp4"), "film");
        symlink(outside.join("keep.mp4"), folder.join("Film/film.srt")).unwrap();
        symlink(outside.join("films"), folder.join("Film/extras")).unwrap();

        deleter.remove(store, &tomb(1), Some(Doom::Play { dir: folder.clone(), bytes: 4 })).unwrap();
        assert_eq!(deleter.play_bytes(), 4);
        assert_eq!(
            run(&mut deleter, store, &InUse::default()),
            [Report { deleted: vec![folder.clone()], kept: vec![] }]
        );
        assert!(!folder.exists());
        assert_eq!(std::fs::read_to_string(outside.join("keep.mp4")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(outside.join("films/keep.mp4")).unwrap(), "keep");
        assert_eq!((deleter.play_bytes(), store.deletions().unwrap()), (0, vec![]));
    }

    #[test]
    fn a_plays_folder_that_is_a_link_is_never_used() {
        let Place { root, store, .. } = &mut place();
        write(&root.join("disk/plays/0101010101010101-0/film.mp4"), "film");
        std::fs::create_dir(root.join("data")).unwrap();
        symlink(root.join("disk/plays"), root.join("data/plays")).unwrap();
        let mut deleter = Deleter::open(&root.join("data/plays"), store, NOW).unwrap();
        let why = format!(
            "torrential cannot use the plays folder {}: it is a link, and torrential deletes plays only in a folder of its own",
            root.join("data/plays").display()
        );
        assert_eq!(deleter.make_play(id(2)).unwrap_err().message, format!("no play can start, for {why}"));

        let dir = root.join("data/plays/0101010101010101-0");
        deleter.remove(store, &tomb(1), Some(Doom::Play { dir: dir.clone(), bytes: 4 })).unwrap();
        let kept =
            Report { deleted: vec![], kept: vec![(dir, "it is not a folder directly in the plays folder".into())] };
        assert_eq!(run(&mut deleter, store, &InUse::default()), [kept]);
        assert_eq!(std::fs::read_to_string(root.join("disk/plays/0101010101010101-0/film.mp4")).unwrap(), "film");
    }

    #[test]
    fn files_moved_aside_before_a_crash_are_deleted_at_the_next_start_and_what_is_at_their_names_then_stays() {
        let Place { root, store, .. } = &mut place();
        let dir = root.join("Downloads");
        write(&dir.join("Set/a.mkv"), "a");
        write(&dir.join("Set/docs/b.txt"), "b");
        let mut deleter = Deleter::open(&root.join("plays"), store, Duration::from_secs(3600)).unwrap();
        let doom = Doom::Files { dir: dir.clone(), files: files(&["Set/a.mkv", "Set/docs/b.txt"]) };
        deleter.remove(store, &tomb(1), Some(doom)).unwrap();
        assert_eq!(run(&mut deleter, store, &InUse::default()), [Report::default()]);
        let [Recorded { what: Journaled::Quarantine { name, .. }, .. }] = &store.deletions().unwrap()[..] else {
            panic!("one folder of files moved aside is on record")
        };
        let aside = dir.join(name.as_str());
        assert_eq!(std::fs::read_to_string(aside.join("0")).unwrap(), "a");
        assert!(!dir.join("Set").exists(), "the folders the files leave empty go");
        drop(deleter);

        write(&dir.join("Set/a.mkv"), "the person's own film");
        let mut deleter = Deleter::open(&root.join("plays"), store, NOW).unwrap();
        assert_eq!(run(&mut deleter, store, &InUse::default()), Vec::<Report>::new());
        assert!(!aside.exists());
        assert_eq!(std::fs::read_to_string(dir.join("Set/a.mkv")).unwrap(), "the person's own film");
        assert_eq!(store.deletions().unwrap(), []);
    }

    #[test]
    fn a_folder_of_files_moved_aside_that_was_replaced_is_left_alone() {
        let Place { root, store, .. } = &mut place();
        let dir = root.join("Downloads");
        write(&dir.join("a.mkv"), "a");
        let mut deleter = Deleter::open(&root.join("plays"), store, Duration::from_secs(3600)).unwrap();
        let doom = Doom::Files { dir: dir.clone(), files: files(&["a.mkv"]) };
        deleter.remove(store, &tomb(1), Some(doom)).unwrap();
        run(&mut deleter, store, &InUse::default());
        let [Recorded { what: Journaled::Quarantine { name, .. }, .. }] = &store.deletions().unwrap()[..] else {
            panic!("one folder of files moved aside is on record")
        };
        let aside = dir.join(name.as_str());
        std::fs::rename(&aside, root.join("elsewhere")).unwrap();
        write(&aside.join("mine.txt"), "mine");

        let mut deleter = Deleter::open(&root.join("plays"), store, NOW).unwrap();
        assert_eq!(run(&mut deleter, store, &InUse::default()), Vec::<Report>::new());
        assert_eq!(std::fs::read_to_string(aside.join("mine.txt")).unwrap(), "mine");
        assert_eq!(store.deletions().unwrap(), [], "it is not tried again");
    }

    #[test]
    fn files_in_a_chosen_folder_go_but_links_and_what_another_download_uses_stay() {
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
        let mut deleter = Deleter::open(&root.join("plays"), store, NOW).unwrap();
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
        assert_eq!(
            reports,
            [Report { deleted: vec![dir.join("Set/a.mkv"), dir.join("Set/docs/b.txt")], kept: kept.to_vec() }]
        );
        assert!(!dir.join("Set/docs").exists(), "a folder it left empty goes");
        assert!(dir.join("Set").exists(), "a folder with something left in it stays");
        assert_eq!(std::fs::read_to_string(outside.join("c.srt")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(outside.join("extras/d.txt")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(dir.join("mine.txt")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(dir.join("Set/shared.nfo")).unwrap(), "s");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.iter().all(|name| !name.to_string_lossy().starts_with(QUARANTINE)), "{left:?}");
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
