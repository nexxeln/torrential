use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use torrential_core::error::Details;
use torrential_core::protocol::DownloadPlan;
use torrential_core::units::{format_limit, format_size};
use torrential_core::{Error, ErrorCode, Selection, TorrentId};

use super::files;
use super::metainfo::Metainfo;
use crate::disk::{Disk, Space, reserve};

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_MAX_SIZE: u64 = 50 * GIB;

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
            let path = files::path(metainfo, file, dir);
            let untouched_link = file.size == 0 && disk.is_link(&path);
            (disk.exists(&path) && disk.allocated_file_bytes(&path).is_none() && !untouched_link).then_some(path)
        })
        .collect();
    let allocated_size = allocated_bytes(metainfo, dir, &wanted, disk);
    let space = disk.space(dir).map_err(|e| {
        Error::new(ErrorCode::StorageError, format!("cannot read free space at {}: {e}", dir.display()))
    })?;
    let links = links(metainfo, dir, &wanted, disk);
    let destination = files::root(dir, &metainfo.name);
    Ok(Plan { wanted, selected_size, allocated_size, destination, links, conflicts, space })
}

pub fn allocated_bytes(metainfo: &Metainfo, dir: &Path, wanted: &[bool], disk: &dyn Disk) -> u64 {
    metainfo
        .files
        .iter()
        .zip(wanted)
        .filter(|(_, w)| **w)
        .map(|(file, _)| disk.allocated_file_bytes(&files::path(metainfo, file, dir)).unwrap_or(0).min(file.size))
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
        let mut path = files::on_disk(dir);
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
            error.with_hint(format!("run {get} to allow it, or pick fewer files with --files")).with_next(get)
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
            torrential_core::units::count(n, "file"),
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
            torrential_core::units::count(n, "file"),
            destination.display(),
            torrential_core::text::sanitize(name)
        ),
    )
    .with_details(Details::Paths(paths.iter().map(|p| p.display().to_string()).collect()))
    .with_hint(hint)
}

/// The first of `paths` that is not free, with the error when the disk could not say. Only a
/// path the disk answers is not there is free: one it cannot read may hold a file.
pub fn taken<'a>(disk: &dyn Disk, paths: &'a [PathBuf]) -> Option<(&'a Path, Option<std::io::Error>)> {
    paths.iter().find_map(|path| match disk.found(path) {
        Ok(true) => Some((path.as_path(), None)),
        Ok(false) => None,
        Err(e) => Some((path.as_path(), Some(e))),
    })
}

/// Refuses the download a title play would make when any of the `paths` it writes is not free.
/// The play writes only where nothing is, so it cannot change a file that was there before it.
pub fn check_free(disk: &dyn Disk, paths: &[PathBuf], id: TorrentId) -> Result<(), Error> {
    let Some((path, unread)) = taken(disk, paths) else { return Ok(()) };
    let message = match unread {
        None => format!("{} already exists, and playing a title writes only where nothing is", path.display()),
        Some(e) => format!(
            "cannot see whether {} exists, and playing a title writes only where nothing is: {e}",
            path.display()
        ),
    };
    let get = format!("torrential get {}", &id.to_hex()[..6]);
    Err(Error::new(ErrorCode::PathConflict, message)
        .with_details(Details::Paths(vec![path.display().to_string()]))
        .with_hint(format!("move it away, or run {get} to download this torrent yourself")))
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
        ", but only {} is free and {} is kept in reserve",
        format_size(space.free),
        format_size(keep)
    ));
    Err(Error::new(ErrorCode::DiskFull, message)
        .with_details(Details::Space { required, available: space.free })
        .with_hint("free some space, or choose another disk with --dir"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake;
    use crate::disk::{FakeDisk, RealDisk};
    use torrential_core::InfoHash;

    const DISK: Space = Space { device: 1, total: 100 * GIB, free: 20 * GIB };

    fn torrent(size: u64) -> Metainfo {
        fake::TorrentFile {
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
            "needs 14.0 GiB more on this disk plus 5.0 GiB for other downloads, but only 20.0 GiB is free and 2.0 GiB is kept in reserve"
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
                Some("run torrential get 2d9b6a --max-size 3GiB to allow it, or pick fewer files with --files")
            )
        );
        let flagged = check_size(2_254_911_385, Cap::new(Some(GIB), Some(2_000_000_000)), None).unwrap_err();
        assert_eq!(flagged.message, "the selection is 2.1 GiB, over the 1 GiB cap");
    }

    // Windows makes links only for an administrator or in Developer Mode.
    #[cfg(unix)]
    #[test]
    fn links_below_the_download_folder_are_found_though_the_folder_itself_may_be_one() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let (real, outside) = (tmp.path().join("disk/Downloads"), tmp.path().join("outside"));
        std::fs::create_dir_all(real.join("set/docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let dir = tmp.path().join("Downloads");
        symlink(&real, &dir).unwrap();
        let set = fake::TorrentFile {
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

    // NTFS allocates the whole length of a file that grows, so it makes no sparse file unasked.
    #[cfg(unix)]
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
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, tmp.path().join("link")).unwrap();
            assert_eq!(RealDisk.allocated_file_bytes(&tmp.path().join("link")), None);
        }
    }
}
