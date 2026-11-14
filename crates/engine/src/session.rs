use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::time::Duration;

use crate::alert::Alert;
use crate::bridge::{self, ffi};
use crate::config::{Config, Dht, DhtState};
use crate::error::{EngineError, ErrorKind};
use crate::identity::TorrentId;
use crate::torrent::{
    AddTorrent, Added, FileEntry, FileIndex, FilePriority, Metainfo, MissingPieces, SelectedPieces, TorrentStatus,
    WrittenPieces,
};

/// One libtorrent session, which serves every torrent.
///
/// One thread owns a `Session`. It is `Send`, so it can be created and moved
/// to that thread, and not `Sync`, so no two threads call it at once.
///
/// ```compile_fail
/// fn shared<T: Sync>() {}
/// shared::<torrential_engine::Session>();
/// ```
///
/// Calls that change a torrent return once libtorrent has queued the change,
/// and most post a confirming alert. Dropping the session blocks until
/// libtorrent has shut down.
pub struct Session {
    native: cxx::UniquePtr<ffi::Session>,
}

impl Session {
    pub fn new(config: Config) -> Result<Session, EngineError> {
        if let (Dht::On { bootstrap_nodes }, Some(_)) = (&config.dht, &config.dht_state)
            && bootstrap_nodes.is_empty()
        {
            return Err(EngineError::new(ErrorKind::InvalidInput, "dht_state needs at least one DHT bootstrap node"));
        }
        let native = ffi::new_session(&bridge::settings(&config)).map_err(bridge::error)?;
        Ok(Session { native })
    }

    pub fn add(&mut self, torrent: AddTorrent) -> Result<Added, EngineError> {
        let params = bridge::add_params(&torrent)?;
        let result = self.native.pin_mut().add_torrent(&params).map_err(bridge::error)?;
        let info_hash = bridge::info_hash(&result.hashes)?;
        Ok(if result.existed { Added::Existing(info_hash) } else { Added::New(info_hash) })
    }

    /// `Alert::TorrentPaused` follows once disk writes finish and the files are
    /// closed. Pausing a paused torrent changes nothing and posts no alert.
    pub fn pause(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().pause(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    pub fn resume(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().resume(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    /// Sets the listed files and leaves the rest as they are. Needs metadata.
    /// `Alert::FilePrioritiesApplied` confirms the change. A disk failure posts
    /// `Alert::FileError` and `Alert::TorrentError` instead.
    pub fn set_file_priorities(
        &mut self,
        id: TorrentId,
        priorities: &[(FileIndex, FilePriority)],
    ) -> Result<(), EngineError> {
        let records: Vec<ffi::PriorityRecord> = priorities
            .iter()
            .map(|&(index, priority)| ffi::PriorityRecord { index: index.get(), priority: bridge::priority(priority) })
            .collect();
        let r = self.native.pin_mut().set_file_priorities(id.as_bytes(), &records);
        r.map_err(|e| with_id(e, id))
    }

    /// Hashes every file on disk again once the torrent is running; a paused
    /// torrent waits for `resume`. `Alert::TorrentChecked` follows, and selected
    /// pieces that fail are downloaded again once the torrent has peers.
    pub fn force_recheck(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().force_recheck(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    /// Asks the DHT for peers now, without waiting for its periodic announce.
    pub fn announce_to_dht(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().announce_to_dht(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    /// Asks for resume data, including the info dictionary.
    /// `Alert::ResumeDataSaved` or `Alert::ResumeDataFailed` follows. The data
    /// lists only pieces already written to disk, which can trail the verified
    /// count until the torrent is paused.
    pub fn save_resume_data(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().save_resume_data(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    /// Like `save_resume_data`, but only if the torrent downloaded or checked
    /// data since its last save. Otherwise `Alert::ResumeDataUnchanged`
    /// follows.
    pub fn save_resume_data_if_progressed(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().save_resume_data_if_progressed(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    /// Removes the torrent and keeps its files. `Alert::TorrentRemoved`
    /// follows.
    pub fn remove(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().remove(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    pub fn connect_peer(&mut self, id: TorrentId, peer: SocketAddr) -> Result<(), EngineError> {
        let peer = bridge::endpoint(&peer);
        let r = self.native.pin_mut().connect_peer(id.as_bytes(), &peer.address, peer.port);
        r.map_err(|e| with_id(e, id))
    }

    /// Adds a web seed to a torrent the session already has. libtorrent ignores one it has, and
    /// saves it in the resume data.
    pub fn add_web_seed(&mut self, id: TorrentId, url: &str) -> Result<(), EngineError> {
        let r = self.native.pin_mut().add_web_seed(id.as_bytes(), url);
        r.map_err(|e| with_id(e, id))
    }

    /// The info dictionary, plus piece layers for v2 and hybrid torrents, as
    /// a `.torrent` file. Trackers and web seeds are not included.
    pub fn metainfo(&self, id: TorrentId) -> Result<Metainfo, EngineError> {
        let bytes = self.native.metainfo(id.as_bytes()).map_err(|e| with_id(e, id))?;
        Metainfo::from_bytes(bytes)
    }

    /// The info dictionary alone, as a `.torrent` file. Unlike `metainfo` it
    /// is ready when `Alert::MetadataReceived` arrives: libtorrent asks peers
    /// for v2 piece layers only after that, and it accepts a v2 or hybrid
    /// `.torrent` without them.
    pub fn info_dictionary(&self, id: TorrentId) -> Result<Metainfo, EngineError> {
        let bytes = self.native.info_dictionary(id.as_bytes()).map_err(|e| with_id(e, id))?;
        Metainfo::from_bytes(bytes)
    }

    /// Needs metadata.
    pub fn files(&self, id: TorrentId) -> Result<Vec<FileEntry>, EngineError> {
        let records = self.native.files(id.as_bytes()).map_err(|e| with_id(e, id))?;
        Ok(bridge::file_entries(records))
    }

    pub fn status(&self, id: TorrentId) -> Result<TorrentStatus, EngineError> {
        let record = self.native.status(id.as_bytes()).map_err(|e| with_id(e, id))?;
        bridge::status(record)
    }

    /// Needs metadata. Unlike `status`, this counts whole pieces, so a damaged
    /// piece that spans two files counts once.
    pub fn selected_pieces(&self, id: TorrentId) -> Result<SelectedPieces, EngineError> {
        let mut whole = self.piece_slices(id, 1)?;
        whole.pop().ok_or_else(|| EngineError::new(ErrorKind::Native, "the torrent has no pieces"))
    }

    /// Needs metadata. Cuts the pieces into `slices` runs of equal length, in
    /// order, and counts each run's pieces like `selected_pieces`. With fewer
    /// pieces than slices, a piece counts in every slice it covers. The cost
    /// grows with the number of pieces: one pass over the piece bitfield.
    pub fn piece_slices(&self, id: TorrentId, slices: u32) -> Result<Vec<SelectedPieces>, EngineError> {
        let counts = self.native.piece_slices(id.as_bytes(), slices).map_err(|e| with_id(e, id))?;
        Ok(counts.into_iter().map(|c| SelectedPieces { selected: c.selected, verified: c.verified }).collect())
    }

    /// Needs metadata. Counts whole pieces like `selected_pieces`. A peer or web seed counts for a
    /// piece once it is connected and has said it has that piece. libtorrent stops counting a web
    /// seed for a piece that spans a file the seed redirected elsewhere, or that failed its hash
    /// check from that seed, so such a piece can be missing with no source while seeds are connected.
    pub fn missing_pieces(&self, id: TorrentId) -> Result<MissingPieces, EngineError> {
        let counts = self.native.missing_pieces(id.as_bytes()).map_err(|e| with_id(e, id))?;
        Ok(MissingPieces { missing: counts.missing, unavailable: counts.unavailable })
    }

    /// Needs metadata. Asks peers for the pieces under `length` bytes of `file` from `offset` before
    /// any other piece, the earliest first, then for the rest of the file in order. True once every
    /// one of them is verified and written, so reading that range of the file returns the torrent's
    /// bytes. Ask again until then: each call renews the request.
    pub fn want_range(
        &mut self,
        id: TorrentId,
        file: FileIndex,
        offset: u64,
        length: u64,
    ) -> Result<bool, EngineError> {
        let r = self.native.pin_mut().want_range(id.as_bytes(), file.get(), offset, length);
        r.map_err(|e| with_id(e, id))
    }

    /// Needs metadata. Cuts `length` bytes of `file` from `offset` into `runs` runs of equal length,
    /// in order, and counts the pieces under each run and how many of them are verified and
    /// written, so reading them from the file returns the torrent's bytes. With fewer pieces than
    /// runs, a piece counts in every run it covers. Unlike `want_range`, this asks peers for nothing.
    pub fn written_pieces(
        &self,
        id: TorrentId,
        file: FileIndex,
        offset: u64,
        length: u64,
        runs: u32,
    ) -> Result<Vec<WrittenPieces>, EngineError> {
        let counts = self.native.written_pieces(id.as_bytes(), file.get(), offset, length, runs);
        let counts = counts.map_err(|e| with_id(e, id))?;
        Ok(counts.into_iter().map(|c| WrittenPieces { pieces: c.selected, written: c.verified }).collect())
    }

    /// Picks pieces in file order instead of rarest first, for a torrent read while it downloads.
    pub fn download_in_order(&mut self, id: TorrentId) -> Result<(), EngineError> {
        let r = self.native.pin_mut().download_in_order(id.as_bytes());
        r.map_err(|e| with_id(e, id))
    }

    /// Caps how fast this torrent downloads, in bytes per second. `None` lifts the cap. Resume
    /// data does not keep it, so a torrent added again downloads as fast as it can.
    pub fn set_download_limit(&mut self, id: TorrentId, limit: Option<NonZeroU32>) -> Result<(), EngineError> {
        let r = self.native.pin_mut().set_download_limit(id.as_bytes(), limit.map_or(0, NonZeroU32::get));
        r.map_err(|e| with_id(e, id))
    }

    /// How fast this torrent may download, in bytes per second. `None` when it has no cap.
    pub fn download_limit(&self, id: TorrentId) -> Result<Option<NonZeroU32>, EngineError> {
        let r = self.native.download_limit(id.as_bytes()).map(NonZeroU32::new);
        r.map_err(|e| with_id(e, id))
    }

    /// Asks for session counters. `Alert::SessionStats` delivers them.
    pub fn post_session_stats(&mut self) -> Result<(), EngineError> {
        self.native.pin_mut().post_session_stats().map_err(bridge::error)
    }

    /// The DHT routing table and node ids, for `Config::dht_state`.
    pub fn save_state(&self) -> Result<DhtState, EngineError> {
        let bytes = self.native.save_state().map_err(bridge::error)?;
        Ok(DhtState::from_bytes(bytes))
    }

    /// The TCP port peers should use for the first listen socket: the port a
    /// UPnP or NAT-PMP mapping gave, if one exists, otherwise the bound port.
    /// `None` while no socket is open.
    pub fn listen_port(&self) -> Result<Option<u16>, EngineError> {
        let port = self.native.listen_port().map_err(bridge::error)?;
        Ok((port != 0).then_some(port))
    }

    /// Waits up to `timeout` for alerts, then returns the queued ones this
    /// crate models. The list can be empty before the timeout, because other
    /// libtorrent alerts and alerts about removed torrents are dropped.
    pub fn wait_alerts(&mut self, timeout: Duration) -> Result<Vec<Alert>, EngineError> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let records = self.native.pin_mut().wait_alerts(millis).map_err(bridge::error)?;
        records.into_iter().map(bridge::alert).collect()
    }
}

fn with_id(e: cxx::Exception, id: TorrentId) -> EngineError {
    let e = bridge::error(e);
    match e.kind() {
        ErrorKind::UnknownTorrent | ErrorKind::NoMetadata if e.message().is_empty() => {
            EngineError::new(e.kind(), id.to_string())
        }
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_session_can_move_to_its_owning_thread() {
        fn send<T: Send>() {}
        send::<super::Session>();
    }
}
