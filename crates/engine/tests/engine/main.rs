mod download_limit;
mod hybrid;
// torrential binds torrent traffic to an interface only on Unix.
#[cfg(unix)]
mod interface;
mod missing_pieces;
mod parse;
mod pause;
mod piece_slices;
mod recheck;
mod resume_data;
mod selection;
mod support;
mod swarm;
// libtorrent makes symbolic links only on Unix.
#[cfg(unix)]
mod symlinks;
mod upload;
mod want_range;
