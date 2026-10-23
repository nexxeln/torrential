use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use torrential_core::error::Details;
use torrential_core::protocol::DownloadPlan;
use torrential_core::units::{format_limit, format_size};
use torrential_core::{Error, ErrorCode, Selection, TorrentId};

use super::engine_port::Metainfo;
use super::files;

pub const GIB: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_SIZE: u64 = 50 * GIB;
const MIN_RESERVE: u64 = 2 * GIB;
const MAX_RESERVE: u64 = 10 * GIB;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Space {
    pub device: u64,
    pub total: u64,
    pub free: u64,
}

pub trait Disk: Send {
    fn space(&self, path: &Path) -> std::io::Result<Space>;
    fn exists(&self, path: &Path) -> bool;
    fn is_link(&self, path: &Path) -> bool;
    /// Space already allocated to a regular file, without following a symbolic link.
    fn allocated_file_bytes(&self, path: &Path) -> Option<u64>;
}

pub fn reserve(total: u64) -> u64 {
    (total / 50).clamp(MIN_RESERVE, MAX_RESERVE)
}

#[derive(Clone, PartialEq, Debug)]
pub struct Plan {
    pub wanted: Vec<bool>,
    pub selected_size: u64,
    pub allocated_size: u64,
    pub destination: PathBuf,
    pub links: Vec<PathBuf>,
    pub conflicts: Vec<PathBuf>,
    pub space: Space,
}

impl Plan {
    pub fn needed(&self, verified: u64) -> u64 {
        self.selected_size.saturating_sub(self.allocated_size.max(verified))
    }

    pub fn public(&self, cap: Cap) -> DownloadPlan {
        DownloadPlan {
            selected_files: self.wanted.iter().filter(|w| **w).count() as u32,
            total_files: self.wanted.len() as u32,
            selected_size: self.selected_size,
            destination: self.destination.clone(),
            free_space: self.space.free,
            conflicts: self.links.iter().chain(&self.conflicts).cloned().collect(),
            max_size: Some(cap.bytes),
        }
    }
}

pub fn plan(metainfo: &Metainfo, selection: &Selection, dir: &Path, disk: &dyn Disk) -> Result<Plan, Error> {
    let wanted = files::wanted(metainfo, selection)?;
    let selected_size = metainfo.files.iter().zip(&wanted).filter(|(_, w)| **w).map(|(f, _)| f.size).sum();
    let conflicts = metainfo
        .files
        .iter()
        .zip(&wanted)
        .filter(|(_, w)| **w)
        .filter_map(|(file, _)| {
            let path = dir.join(files::relative(metainfo, file));
            let untouched_link = file.size == 0 && disk.is_link(&path);
            (disk.exists(&path) && disk.allocated_file_bytes(&path).is_none() && !untouched_link).then_some(path)
        })
        .collect();
    let allocated_size = allocated_bytes(metainfo, dir, &wanted, disk);
    let space = disk.space(dir).map_err(|e| {
        Error::new(ErrorCode::StorageError, format!("cannot read free space at {}: {e}", dir.display()))
    })?;
    let links = links(metainfo, dir, &wanted, disk);
    Ok(Plan { wanted, selected_size, allocated_size, destination: dir.join(&metainfo.name), links, conflicts, space })
}

pub fn allocated_bytes(metainfo: &Metainfo, dir: &Path, wanted: &[bool], disk: &dyn Disk) -> u64 {
    metainfo
        .files
        .iter()
        .zip(wanted)
        .filter(|(_, w)| **w)
        .map(|(file, _)| {
            disk.allocated_file_bytes(&dir.join(files::relative(metainfo, file))).unwrap_or(0).min(file.size)
        })
        .sum()
}

/// libtorrent never writes a zero-length file that already exists. Torrent
/// symlinks have zero length and targets confined to the torrent's folder.
pub fn links(metainfo: &Metainfo, dir: &Path, wanted: &[bool], disk: &dyn Disk) -> Vec<PathBuf> {
    let mut links = BTreeSet::new();
    for (file, _) in metainfo.files.iter().zip(wanted).filter(|(_, w)| **w) {
        let relative = files::relative(metainfo, file);
        let mut parts: Vec<_> = relative.components().collect();
        if file.size == 0 {
            parts.pop();
        }
        let mut path = dir.to_path_buf();
        for part in parts {
            path.push(part);
            if disk.is_link(&path) {
                links.insert(path);
                break;
            }
        }
    }
    links.into_iter().collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cap {
    pub bytes: u64,
    pub from_setup: bool,
}

impl Cap {
    pub fn new(flag: Option<u64>, setup: Option<u64>) -> Self {
        match (flag, setup) {
            (Some(bytes), _) => Self { bytes, from_setup: false },
            (None, Some(bytes)) => Self { bytes, from_setup: true },
            (None, None) => Self { bytes: DEFAULT_MAX_SIZE, from_setup: false },
        }
    }
}

pub fn check_size(selected: u64, cap: Cap, job: Option<TorrentId>) -> Result<(), Error> {
    if selected <= cap.bytes {
        return Ok(());
    }
    let from = if cap.from_setup { " from torrential setup" } else { "" };
    let allow = selected.div_ceil(GIB);
    let error = Error::new(
        ErrorCode::TooLarge,
        format!("the selection is {}, over the {} cap{from}", format_size(selected), format_limit(cap.bytes)),
    );
    Err(match job {
        None => error.with_hint(format!("add --max-size {allow}GiB to allow it, or pick fewer files with --files")),
        Some(id) => {
            let get = format!("torrential get {} --max-size {allow}GiB", &id.to_hex()[..6]);
            error.with_hint(format!("{get} allows it, or pick fewer files with --files")).with_next(get)
        }
    })
}

pub fn check_conflicts(plan: &Plan) -> Result<(), Error> {
    let dir = plan.destination.parent().unwrap_or(&plan.destination);
    check_links(&plan.links, dir, None)?;
    let n = plan.conflicts.len();
    if n == 0 {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::PathConflict,
        format!(
            "{} already {} under {}",
            crate::output::format::count(n, "file"),
            if n == 1 { "exists" } else { "exist" },
            plan.destination.display()
        ),
    )
    .with_details(Details::Paths(plan.conflicts.iter().map(|p| p.display().to_string()).collect()))
    .with_hint("choose another folder with --dir, or move the conflicting paths; only regular files can be reused"))
}

pub fn check_links(links: &[PathBuf], dir: &Path, retry: Option<&str>) -> Result<(), Error> {
    let what = match links {
        [] => return Ok(()),
        [link] => format!("{} is a symbolic link", link.display()),
        _ => format!("{} paths are symbolic links", links.len()),
    };
    let hint = match retry {
        None => "remove the link, or choose another folder with --dir".to_string(),
        Some(retry) => format!("remove the link, then run {retry}"),
    };
    Err(Error::new(ErrorCode::PathConflict, format!("{what}, and torrential writes only inside {}", dir.display()))
        .with_details(Details::Paths(links.iter().map(|p| p.display().to_string()).collect()))
        .with_hint(hint))
}

pub fn claimed(destination: &Path, owner: TorrentId, name: &str, paths: &[PathBuf], retry: Option<&str>) -> Error {
    let n = paths.len();
    let (are, files) = if n == 1 { ("is", "a file") } else { ("are", "files") };
    let other = &owner.to_hex()[..6];
    let hint = match retry {
        None => format!("choose another folder with --dir, or remove {other} with torrential rm {other}"),
        Some(retry) => format!("remove {other} with torrential rm {other}, then run {retry}"),
    };
    Error::new(
        ErrorCode::PathConflict,
        format!(
            "{} under {} {are} already {files} of download {other} ({})",
            crate::output::format::count(n, "file"),
            destination.display(),
            torrential_core::text::sanitize(name)
        ),
    )
    .with_details(Details::Paths(paths.iter().map(|p| p.display().to_string()).collect()))
    .with_hint(hint)
}

pub fn check_disk(space: Space, needed: u64, others_remaining: u64) -> Result<(), Error> {
    let keep = reserve(space.total);
    let required = needed + others_remaining + keep;
    if required <= space.free {
        return Ok(());
    }
    let mut message = format!("needs {} more on this disk", format_size(needed));
    if others_remaining > 0 {
        message.push_str(&format!(" plus {} for other downloads", format_size(others_remaining)));
    }
    message.push_str(&format!(
        ", but only {} is free and {} stays in reserve",
        format_size(space.free),
        format_size(keep)
    ));
    Err(Error::new(ErrorCode::DiskFull, message)
        .with_details(Details::Space { required, available: space.free })
        .with_hint("free some space, or choose another disk with --dir"))
}

pub struct RealDisk;

impl Disk for RealDisk {
    fn space(&self, path: &Path) -> std::io::Result<Space> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let existing = path.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
        let device = std::fs::metadata(existing)?.dev();
        let c_path = std::ffi::CString::new(existing.as_os_str().as_bytes())?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let block = stat.f_frsize as u64;
        Ok(Space { device: device as u64, total: stat.f_blocks as u64 * block, free: stat.f_bavail as u64 * block })
    }

    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn is_link(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink())
    }

    fn allocated_file_bytes(&self, path: &Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        let meta = path.symlink_metadata().ok()?;
        meta.is_file().then(|| meta.blocks().saturating_mul(512).min(meta.len()))
    }
}

pub struct RoomyDisk;

impl Disk for RoomyDisk {
    fn space(&self, _: &Path) -> std::io::Result<Space> {
        Ok(Space { device: 0, total: 1 << 50, free: 1 << 49 })
    }

    fn exists(&self, path: &Path) -> bool {
        RealDisk.exists(path)
    }

    fn is_link(&self, path: &Path) -> bool {
        RealDisk.is_link(path)
    }

    fn allocated_file_bytes(&self, path: &Path) -> Option<u64> {
        RealDisk.allocated_file_bytes(path)
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct FakeDisk(std::sync::Arc<std::sync::Mutex<FakeFiles>>);

#[cfg(test)]
struct FakeFiles {
    space: Space,
    paths: BTreeSet<PathBuf>,
    links: BTreeSet<PathBuf>,
    allocated: std::collections::BTreeMap<PathBuf, u64>,
}

#[cfg(test)]
impl FakeDisk {
    pub fn new(space: Space) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(FakeFiles {
            space,
            paths: BTreeSet::new(),
            links: BTreeSet::new(),
            allocated: std::collections::BTreeMap::new(),
        })))
    }

    pub fn directory(&self, path: impl Into<PathBuf>) {
        let path = path.into();
        let mut files = self.0.lock().unwrap();
        files.paths.insert(path.clone());
        files.allocated.remove(&path);
    }

    pub fn file(&self, path: impl Into<PathBuf>, allocated: u64) {
        let path = path.into();
        let mut files = self.0.lock().unwrap();
        files.paths.insert(path.clone());
        files.allocated.insert(path, allocated);
    }

    pub fn link(&self, path: impl Into<PathBuf>) {
        let path = path.into();
        let mut files = self.0.lock().unwrap();
        files.paths.insert(path.clone());
        files.links.insert(path);
    }

    pub fn set_free(&self, free: u64) {
        self.0.lock().unwrap().space.free = free;
    }
}

#[cfg(test)]
impl Disk for FakeDisk {
    fn space(&self, _: &Path) -> std::io::Result<Space> {
        Ok(self.0.lock().unwrap().space)
    }

    fn exists(&self, path: &Path) -> bool {
        self.0.lock().unwrap().paths.contains(path)
    }

    fn is_link(&self, path: &Path) -> bool {
        self.0.lock().unwrap().links.contains(path)
    }

    fn allocated_file_bytes(&self, path: &Path) -> Option<u64> {
        self.0.lock().unwrap().allocated.get(path).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::engine_port::FakeTorrentFile;
    use torrential_core::InfoHash;

    const DISK: Space = Space { device: 1, total: 100 * GIB, free: 20 * GIB };

    fn torrent(size: u64) -> Metainfo {
        FakeTorrentFile {
            info_hash: InfoHash::V1([7; 20]),
            name: "set".into(),
            files: vec![("a.bin".into(), size), ("b.bin".into(), GIB)],
        }
        .metainfo()
        .unwrap()
    }

    fn plan_for(size: u64, disk: &FakeDisk) -> Plan {
        plan(&torrent(size), &Selection::All, Path::new("/dl"), disk).unwrap()
    }

    #[test]
    fn reserve_is_two_percent_between_two_and_ten_gib() {
        assert_eq!(reserve(10 * GIB), 2 * GIB);
        assert_eq!(reserve(256_000_000_000), 5_120_000_000);
        assert_eq!(reserve(1_000_000_000_000), 10 * GIB);
        assert_eq!(reserve(4_000_000_000_000), 10 * GIB);
    }

    #[test]
    fn admits_what_fits_above_the_reserve() {
        let disk = FakeDisk::new(DISK);
        let plan = plan_for(17 * GIB, &disk);
        assert_eq!(plan.selected_size, 18 * GIB);
        assert_eq!(check_disk(plan.space, plan.selected_size, 0), Ok(()));
    }

    #[test]
    fn refuses_what_would_eat_the_reserve_counting_other_downloads() {
        let disk = FakeDisk::new(DISK);
        let plan = plan_for(13 * GIB, &disk);
        let err = check_disk(plan.space, plan.selected_size, 5 * GIB).unwrap_err();
        assert_eq!(err.code, ErrorCode::DiskFull);
        assert_eq!(
            err.message,
            "needs 14.0 GiB more on this disk plus 5.0 GiB for other downloads, but only 20.0 GiB is free and 2.0 GiB stays in reserve"
        );
        assert_eq!(err.details, Some(Details::Space { required: 21 * GIB, available: 20 * GIB }));
    }

    #[test]
    fn refuses_past_the_size_cap_unless_raised() {
        let disk = FakeDisk::new(Space { free: 500 * GIB, total: 1000 * GIB, ..DISK });
        let size = plan_for(60 * GIB, &disk).selected_size;
        let err = check_size(size, Cap::new(None, None), None).unwrap_err();
        assert_eq!(err.code, ErrorCode::TooLarge);
        assert_eq!(err.message, "the selection is 61.0 GiB, over the 50 GiB cap");
        assert_eq!(err.hint.as_deref(), Some("add --max-size 61GiB to allow it, or pick fewer files with --files"));
        assert_eq!(check_size(size, Cap::new(Some(61 * GIB), Some(GIB)), None), Ok(()));
    }

    #[test]
    fn a_refusal_names_a_cap_from_setup_and_the_download_it_stopped() {
        let id: TorrentId = format!("2d9b6a{}", "0".repeat(34)).parse().unwrap();
        let err = check_size(2_254_911_385, Cap::new(None, Some(2_000_000_000)), Some(id)).unwrap_err();
        assert_eq!(
            (err.message.as_str(), err.hint.as_deref()),
            (
                "the selection is 2.1 GiB, over the 2 GB cap from torrential setup",
                Some("torrential get 2d9b6a --max-size 3GiB allows it, or pick fewer files with --files")
            )
        );
        let flagged = check_size(2_254_911_385, Cap::new(Some(GIB), Some(2_000_000_000)), None).unwrap_err();
        assert_eq!(flagged.message, "the selection is 2.1 GiB, over the 1 GiB cap");
    }

    #[test]
    fn links_below_the_download_folder_are_found_though_the_folder_itself_may_be_one() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let (real, outside) = (tmp.path().join("disk/Downloads"), tmp.path().join("outside"));
        std::fs::create_dir_all(real.join("set/docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let dir = tmp.path().join("Downloads");
        symlink(&real, &dir).unwrap();
        let set = FakeTorrentFile {
            info_hash: InfoHash::V1([8; 20]),
            name: "set".into(),
            files: vec![("a.bin".into(), 5), ("docs/b.txt".into(), 5), ("link".into(), 0)],
        }
        .metainfo()
        .unwrap();
        let found = |wanted: &[bool]| links(&set, &dir, wanted, &RealDisk);
        assert_eq!(found(&[true, true, true]), Vec::<PathBuf>::new());
        symlink("a.bin", real.join("set/link")).unwrap();
        assert_eq!(found(&[true, true, true]), Vec::<PathBuf>::new(), "a torrent's own link, as libtorrent makes it");
        let plan = plan(&set, &Selection::Patterns(vec!["link".into()]), &dir, &RealDisk).unwrap();
        assert_eq!(check_conflicts(&plan), Ok(()), "libtorrent does not rewrite an existing zero-length link");
        std::fs::remove_dir(real.join("set/docs")).unwrap();
        symlink(&outside, real.join("set/docs")).unwrap();
        symlink(outside.join("a.bin"), real.join("set/a.bin")).unwrap();
        assert_eq!(found(&[true, true, true]), [dir.join("set/a.bin"), dir.join("set/docs")]);
        assert_eq!(found(&[false, false, true]), Vec::<PathBuf>::new(), "nothing is written through them");
        std::fs::rename(real.join("set"), tmp.path().join("elsewhere")).unwrap();
        symlink(tmp.path().join("elsewhere"), real.join("set")).unwrap();
        assert_eq!(found(&[false, false, true]), [dir.join("set")]);
        let err = check_links(&found(&[true, true, true]), &dir, None).unwrap_err();
        assert_eq!(
            (err.code, err.message, err.hint.as_deref()),
            (
                ErrorCode::PathConflict,
                format!(
                    "{} is a symbolic link, and torrential writes only inside {}",
                    dir.join("set").display(),
                    dir.display()
                ),
                Some("remove the link, or choose another folder with --dir")
            )
        );
    }

    #[test]
    fn existing_regular_files_are_reused_but_directories_conflict() {
        let disk = FakeDisk::new(DISK);
        disk.file("/dl/set/a.bin", GIB / 2);
        disk.file("/dl/set/b.bin", GIB);
        let plan = plan_for(GIB, &disk);
        assert!(plan.conflicts.is_empty());
        assert_eq!(check_conflicts(&plan), Ok(()));
        assert_eq!(plan.needed(0), GIB / 2);
        assert_eq!(plan.needed(GIB), GIB / 2, "verified and allocated bytes are not counted twice");
        assert_eq!(plan.needed(2 * GIB), 0);

        disk.directory("/dl/set/b.bin");
        let plan = plan_for(GIB, &disk);
        assert_eq!(plan.conflicts, [PathBuf::from("/dl/set/b.bin")]);
        assert_eq!(check_conflicts(&plan).unwrap_err().code, ErrorCode::PathConflict);
        let only_a =
            super::plan(&torrent(GIB), &Selection::Patterns(vec!["a.bin".into()]), Path::new("/dl"), &disk).unwrap();
        assert!(only_a.conflicts.is_empty());
        assert_eq!(only_a.needed(0), GIB / 2, "unselected files contribute no reusable space");
    }

    #[test]
    fn sparse_files_do_not_count_their_unallocated_length_as_reusable_space() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("set")).unwrap();
        let path = tmp.path().join("set/a.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(GIB).unwrap();
        let plan = plan(&torrent(GIB), &Selection::All, tmp.path(), &RealDisk).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.allocated_size, 0);
        assert_eq!(plan.needed(0), 2 * GIB);
        assert_eq!(RealDisk.allocated_file_bytes(&tmp.path().join("set")), None);
        std::os::unix::fs::symlink(&path, tmp.path().join("link")).unwrap();
        assert_eq!(RealDisk.allocated_file_bytes(&tmp.path().join("link")), None);
    }
}
