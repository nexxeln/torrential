//! The disk a download lands on: its free space, the part of it torrential leaves free, and what
//! is already there.

#[cfg(test)]
use std::collections::BTreeSet;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use crate::folder::{Folder, Identity};

const GIB: u64 = 1024 * 1024 * 1024;
const MIN_RESERVE: u64 = 2 * GIB;
const MAX_RESERVE: u64 = 10 * GIB;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Space {
    pub device: u64,
    pub total: u64,
    pub free: u64,
}

pub trait Disk: Send + Sync {
    fn space(&self, path: &Path) -> std::io::Result<Space>;
    /// The identities of the folder `path` names, or of the nearest one above it that exists, and
    /// of every folder above that.
    fn lineage(&self, path: &Path) -> std::io::Result<Vec<Identity>>;
    /// Which file or folder `path` names, following a link.
    fn identity(&self, path: &Path) -> std::io::Result<Identity>;
    fn exists(&self, path: &Path) -> bool;
    /// Whether anything is at `path`, without following a link, or why the disk cannot say.
    fn found(&self, path: &Path) -> std::io::Result<bool>;
    fn is_link(&self, path: &Path) -> bool;
    /// Space already allocated to a regular file, without following a symbolic link.
    fn allocated_file_bytes(&self, path: &Path) -> Option<u64>;
    /// Holds the folder `path` names open, following a link to it.
    fn open_folder(&self, path: &Path) -> std::io::Result<Folder>;
}

/// What torrential leaves free on a disk of `total` bytes.
pub fn reserve(total: u64) -> u64 {
    (total / 50).clamp(MIN_RESERVE, MAX_RESERVE)
}

pub struct RealDisk;

impl Disk for RealDisk {
    fn space(&self, path: &Path) -> std::io::Result<Space> {
        let existing = path.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
        let device = crate::folder::identity(existing)?.device;
        let (total, free) = total_and_free(existing)?;
        Ok(Space { device, total, free })
    }

    fn lineage(&self, path: &Path) -> std::io::Result<Vec<Identity>> {
        crate::folder::lineage(path)
    }

    fn identity(&self, path: &Path) -> std::io::Result<Identity> {
        crate::folder::identity(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn found(&self, path: &Path) -> std::io::Result<bool> {
        match path.symlink_metadata() {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn is_link(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink())
    }

    fn allocated_file_bytes(&self, path: &Path) -> Option<u64> {
        let meta = path.symlink_metadata().ok()?;
        meta.is_file().then(|| allocated(path, &meta).min(meta.len()))
    }

    fn open_folder(&self, path: &Path) -> std::io::Result<Folder> {
        Folder::open(path)
    }
}

/// The size of the disk `path` is on, and what of it this user may still write.
#[cfg(unix)]
fn total_and_free(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let block = stat.f_frsize as u64;
    Ok((stat.f_blocks as u64 * block, stat.f_bavail as u64 * block))
}

#[cfg(windows)]
fn total_and_free(path: &Path) -> std::io::Result<(u64, u64)> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let (mut free, mut total) = (0u64, 0u64);
    let path = windows::core::HSTRING::from(path.as_os_str());
    unsafe { GetDiskFreeSpaceExW(&path, Some(&mut free), Some(&mut total), None) }.map_err(crate::fsutil::win32)?;
    Ok((total, free))
}

/// What the file at `path` takes up on disk, which is less than its length while it is sparse.
#[cfg(unix)]
fn allocated(_: &Path, meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks().saturating_mul(512)
}

#[cfg(windows)]
fn allocated(path: &Path, meta: &std::fs::Metadata) -> u64 {
    use windows::Win32::Storage::FileSystem::{GetCompressedFileSizeW, INVALID_FILE_SIZE};
    let mut high = 0u32;
    let path = windows::core::HSTRING::from(path.as_os_str());
    let low = unsafe { GetCompressedFileSizeW(&path, Some(&mut high)) };
    if low == INVALID_FILE_SIZE && std::io::Error::last_os_error().raw_os_error() != Some(0) {
        return meta.len();
    }
    (u64::from(high) << 32) | u64::from(low)
}

pub struct RoomyDisk;

impl Disk for RoomyDisk {
    fn space(&self, _: &Path) -> std::io::Result<Space> {
        Ok(Space { device: 0, total: 1 << 50, free: 1 << 49 })
    }

    fn lineage(&self, path: &Path) -> std::io::Result<Vec<Identity>> {
        RealDisk.lineage(path)
    }

    fn identity(&self, path: &Path) -> std::io::Result<Identity> {
        RealDisk.identity(path)
    }

    fn exists(&self, path: &Path) -> bool {
        RealDisk.exists(path)
    }

    fn found(&self, path: &Path) -> std::io::Result<bool> {
        RealDisk.found(path)
    }

    fn is_link(&self, path: &Path) -> bool {
        RealDisk.is_link(path)
    }

    fn allocated_file_bytes(&self, path: &Path) -> Option<u64> {
        RealDisk.allocated_file_bytes(path)
    }

    fn open_folder(&self, path: &Path) -> std::io::Result<Folder> {
        RealDisk.open_folder(path)
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
    /// Folders where every look waits on a FIFO until a test lets it go, as macOS has a new program
    /// wait to open ~/Downloads until the person answers its prompt, and as a network share that
    /// dropped leaves every call waiting.
    stalled: std::collections::BTreeMap<PathBuf, PathBuf>,
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
            stalled: std::collections::BTreeMap::new(),
        })))
    }

    /// Makes every look at `folder`, or at anything in it, wait to open the FIFO it makes at `fifo`
    /// until the stall is let go.
    #[cfg(unix)]
    pub fn stall(&self, folder: impl Into<PathBuf>, fifo: PathBuf) -> Stall {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0, "mkfifo {}", fifo.display());
        let folder = folder.into();
        self.0.lock().unwrap().stalled.insert(folder.clone(), fifo.clone());
        Stall { disk: self.clone(), folder, fifo, writer: std::sync::Mutex::new(None) }
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

    /// Waits while `path` is in a stalled folder.
    fn answer(&self, path: &Path) -> std::io::Result<()> {
        let files = self.0.lock().unwrap();
        let stalled = files.stalled.iter().find(|(folder, _)| path.starts_with(folder)).map(|(_, fifo)| fifo.clone());
        drop(files);
        // Opening a FIFO to read waits until something opens it to write.
        stalled.map_or(Ok(()), |fifo| std::fs::File::open(fifo).map(drop))
    }
}

#[cfg(test)]
impl Disk for FakeDisk {
    fn space(&self, path: &Path) -> std::io::Result<Space> {
        self.answer(path)?;
        let files = self.0.lock().unwrap();
        let freed: u64 = files.freeing.iter().filter(|(path, _)| !path.exists()).map(|(_, bytes)| bytes).sum();
        Ok(Space { free: files.space.free + freed, ..files.space })
    }

    fn lineage(&self, path: &Path) -> std::io::Result<Vec<Identity>> {
        self.answer(path)?;
        RealDisk.lineage(path)
    }

    fn identity(&self, path: &Path) -> std::io::Result<Identity> {
        self.answer(path)?;
        RealDisk.identity(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.answer(path).is_ok() && self.0.lock().unwrap().paths.contains(path)
    }

    fn found(&self, path: &Path) -> std::io::Result<bool> {
        self.answer(path)?;
        RealDisk.found(path)
    }

    fn is_link(&self, path: &Path) -> bool {
        self.answer(path).is_ok() && self.0.lock().unwrap().links.contains(path)
    }

    fn allocated_file_bytes(&self, path: &Path) -> Option<u64> {
        self.answer(path).ok()?;
        self.0.lock().unwrap().allocated.get(path).copied()
    }

    fn open_folder(&self, path: &Path) -> std::io::Result<Folder> {
        self.answer(path)?;
        RealDisk.open_folder(path)
    }
}

/// A folder where every look waits, until this is let go or dropped.
#[cfg(all(test, unix))]
pub struct Stall {
    disk: FakeDisk,
    folder: PathBuf,
    fifo: PathBuf,
    /// The FIFO open to write, which ends every wait to open it to read, those that come later too.
    writer: std::sync::Mutex<Option<std::fs::File>>,
}

#[cfg(all(test, unix))]
impl Stall {
    pub fn release(&self) {
        // Read and write, so the open itself never waits for a reader.
        let opened = std::fs::OpenOptions::new().read(true).write(true).open(&self.fifo).unwrap();
        self.writer.lock().unwrap().get_or_insert(opened);
        self.disk.0.lock().unwrap().stalled.remove(&self.folder);
    }
}

#[cfg(all(test, unix))]
impl Drop for Stall {
    fn drop(&mut self) {
        self.release();
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
