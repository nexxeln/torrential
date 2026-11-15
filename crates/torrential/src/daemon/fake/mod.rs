//! The engine, swarm and search a test runs instead of libtorrent and the network, with the
//! listings and downloads they start from. They ship in the release binary because the end-to-end
//! tests drive it, and only an engine a test started uses them.

mod engine;
pub mod fixture;
mod search;

#[cfg(test)]
pub use engine::disagreement;
pub use engine::{Engine, Swarm, TorrentFile, land, parse};
#[cfg(test)]
pub use fixture::isos;
pub use search::{Search, Source};
