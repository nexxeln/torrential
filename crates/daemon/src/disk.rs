//! The disk a download lands on: its free space, the part of it torrential leaves free, and what
//! is already there.

#[cfg(test)]
use std::collections::BTreeSet;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

const GIB: u64 = 1024 * 1024 * 1024;
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

/// What torrential leaves free on a disk of `total` bytes.
pub fn reserve(total: u64) -> u64 {
    (total / 50).clamp(MIN_RESERVE, MAX_RESERVE)
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
    /// Folders whose bytes become free once they are gone.
    freeing: Vec<(PathBuf, u64)>,
    paths: BTreeSet<PathBuf>,
    links: BTreeSet<PathBuf>,
    allocated: std::collections::BTreeMap<PathBuf, u64>,
}

#[cfg(test)]
impl FakeDisk {
    pub fn new(space: Space) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(FakeFiles {
            space,
            freeing: Vec::new(),
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

    /// Adds `bytes` to what is free once the real folder `path` is gone, as deleting it would.
    pub fn frees(&self, path: impl Into<PathBuf>, bytes: u64) {
        self.0.lock().unwrap().freeing.push((path.into(), bytes));
    }
}

#[cfg(test)]
impl Disk for FakeDisk {
    fn space(&self, _: &Path) -> std::io::Result<Space> {
        let files = self.0.lock().unwrap();
        let freed: u64 = files.freeing.iter().filter(|(path, _)| !path.exists()).map(|(_, bytes)| bytes).sum();
        Ok(Space { free: files.space.free + freed, ..files.space })
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

    #[test]
    fn reserve_is_two_percent_between_two_and_ten_gib() {
        assert_eq!(reserve(10 * GIB), 2 * GIB);
        assert_eq!(reserve(256_000_000_000), 5_120_000_000);
        assert_eq!(reserve(1_000_000_000_000), 10 * GIB);
        assert_eq!(reserve(4_000_000_000_000), 10 * GIB);
    }
}
