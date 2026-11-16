//! A folder held open, so what is done in it happens in that folder whatever its path names
//! later, and never through a link inside it. Unix holds a file descriptor and works relative to
//! it. Windows holds a handle that refuses to let the folder be moved or deleted while it is open.

#[cfg(unix)]
#[path = "folder/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "folder/windows.rs"]
mod imp;

pub use imp::{Folder, identity, is_link, is_locked, lineage};

/// Which file or folder something is, whatever path reaches it. A folder moved away keeps its
/// identity. A link, or another folder, put in its place has another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Folder,
    /// A link, or anything else that is neither a file nor a folder.
    Other,
}

/// A name in a folder as it is now, without following a link.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub identity: Identity,
    pub kind: Kind,
    /// What it takes up on disk.
    pub bytes: u64,
}
