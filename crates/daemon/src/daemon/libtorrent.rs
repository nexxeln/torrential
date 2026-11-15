use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use torrential_core::{Error, ErrorCode, InfoHash, PieceMap, Slice, TorrentId};
use torrential_engine as lt;

use super::engine_port::{Activity, AddSource, AddTorrent, Engine, EngineEvent, EnginePort, Missing, Stats};
use super::metainfo::{MetaFile, Metainfo};
use crate::fsutil::write_atomic;
use crate::log::log;

const BOOTSTRAP_NODES: [&str; 4] = [
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
];
const LISTEN_PORT_FILE: &str = "listen.port";
const DHT_STATE_FILE: &str = "dht.state";
const STATS_EVERY: Duration = Duration::from_millis(500);
const SAVE_DHT_EVERY: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Network {
    Public,
    Loopback { peers: Vec<SocketAddr> },
}

pub struct Libtorrent {
    session: lt::Session,
    torrents: BTreeMap<TorrentId, Torrent>,
    /// libtorrent forgets removed torrents at once, then confirms after their files close.
    removing: BTreeMap<TorrentId, u32>,
    events: Vec<EngineEvent>,
    dht_state: Option<PathBuf>,
    /// libtorrent reports a bootstrap for each listen socket.
    dht_bootstrapped: bool,
    peers: Vec<SocketAddr>,
    stats_at: Instant,
    dht_saved_at: Instant,
}

#[derive(Default)]
struct Torrent {
    files: Option<Vec<lt::FileIndex>>,
    /// libtorrent counts metadata-only torrents as finished.
    wanted: Option<Vec<bool>>,
    /// Priority changes libtorrent has not confirmed. Until it does, `TorrentFinished` can describe
    /// the previous selection.
    unconfirmed: u32,
    check: Check,
    own_pause_pending: bool,
    failed: bool,
}

/// libtorrent hashes only running torrents.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Check {
    #[default]
    Idle,
    /// libtorrent was checking resume data, which makes it ignore a recheck.
    Queued { hold: bool },
    /// A recheck moves the torrent to `CheckingResumeData`, so a `TorrentChecked` before that move
    /// belongs to an earlier check.
    Hashing { hold: bool, started: bool },
}

impl Libtorrent {
    pub fn start(state: &Path, network: Network) -> Result<Self, Error> {
        let ip = match network {
            Network::Public => Ipv4Addr::UNSPECIFIED,
            Network::Loopback { .. } => Ipv4Addr::LOCALHOST,
        };
        let port = listen_port(&state.join(LISTEN_PORT_FILE), ip.into())?;
        let config = config(&network, port);
        let dht_state = (network == Network::Public).then(|| state.join(DHT_STATE_FILE));
        let peers = match network {
            Network::Public => Vec::new(),
            Network::Loopback { peers } => peers,
        };
        let saved = dht_state.as_deref().and_then(|p| std::fs::read(p).ok()).map(lt::DhtState::from_bytes);
        let session = match lt::Session::new(lt::Config { dht_state: saved.clone(), ..config.clone() }) {
            Err(e) if saved.is_some() && e.kind() == lt::ErrorKind::InvalidInput => {
                log(&format!("starting with an empty DHT table, because the saved one is unreadable: {e}"));
                lt::Session::new(config)
            }
            started => started,
        }
        .map_err(|e| Error::internal(format!("libtorrent did not start: {e}")))?;
        let now = Instant::now();
        Ok(Self {
            session,
            torrents: BTreeMap::new(),
            removing: BTreeMap::new(),
            events: Vec::new(),
            dht_state,
            dht_bootstrapped: false,
            peers,
            stats_at: now,
            dht_saved_at: now,
        })
    }

    fn torrent(&mut self, id: TorrentId) -> Result<&mut Torrent, Error> {
        self.torrents.get_mut(&id).ok_or_else(|| Error::internal(format!("the engine has no torrent {id}")))
    }

    /// The download an alert names. A v2-only magnet is keyed by its truncated v2 hash and learns
    /// its v1 hash with the metadata, so both keys are tried.
    fn download<T>(map: &BTreeMap<TorrentId, T>, hash: InfoHash) -> Option<TorrentId> {
        let v2 = hash.v2().map(|v2| InfoHash::V2(v2).id());
        [Some(hash.id()), v2].into_iter().flatten().find(|id| map.contains_key(id))
    }

    fn apply_selection(&mut self, id: TorrentId) -> Result<(), Error> {
        let t = self.torrent(id)?;
        let (Some(files), Some(wanted)) = (&t.files, &t.wanted) else { return Ok(()) };
        let priorities: Vec<_> = files
            .iter()
            .zip(wanted)
            .map(|(&index, &w)| (index, if w { lt::FilePriority::Normal } else { lt::FilePriority::Skip }))
            .collect();
        self.session.set_file_priorities(id, &priorities).map_err(engine_error)?;
        self.torrent(id)?.unconfirmed += 1;
        Ok(())
    }

    fn report_if_finished(&mut self, id: TorrentId) {
        let Some(t) = self.torrents.get(&id) else { return };
        if t.wanted.is_none() || t.unconfirmed > 0 || t.check != Check::Idle || t.failed {
            return;
        }
        if let Ok(status) = self.session.status(id)
            && matches!(status.state, lt::TorrentState::Finished | lt::TorrentState::Seeding)
        {
            self.events.push(EngineEvent::Finished { id });
        }
    }

    fn start_hashing(&mut self, id: TorrentId, hold: bool) -> Result<(), Error> {
        self.session.force_recheck(id).map_err(engine_error)?;
        if hold {
            self.session.resume(id).map_err(engine_error)?;
        }
        self.torrent(id)?.check = Check::Hashing { hold, started: false };
        Ok(())
    }

    fn fail(&mut self, id: TorrentId, error: Error) {
        if let Some(t) = self.torrents.get_mut(&id)
            && !t.failed
        {
            t.failed = true;
            self.events.push(EngineEvent::Failed { id, error });
        }
    }

    fn alert(&mut self, alert: lt::Alert) {
        use lt::Alert as A;
        if let A::TorrentRemoved { torrent } = alert {
            if let Some(id) = Self::download(&self.removing, torrent) {
                self.removed(id);
            }
            return;
        }
        let id = alert.torrent().and_then(|hash| Self::download(&self.torrents, hash));
        match (alert, id) {
            (A::MetadataReceived { .. }, Some(id)) => self.metadata(id),
            (A::MetadataFailed { error, .. }, Some(id)) => {
                log(&format!("{} sent metadata that failed its hash check: {error}", short(id)));
            }
            (A::TorrentFinished { .. }, Some(id)) => self.report_if_finished(id),
            (A::TorrentPaused { .. }, Some(id)) => {
                let t = self.torrents.get_mut(&id).expect("found above");
                if t.own_pause_pending {
                    t.own_pause_pending = false;
                } else {
                    self.report_paused(id);
                }
            }
            (A::TorrentChecked { .. }, Some(id)) => self.checked(id),
            (A::StateChanged { to, .. }, Some(id)) => self.state_changed(id, to),
            (A::ResumeDataSaved { data, .. }, Some(id)) => {
                let (verified, files) = self.verified_bytes(id).ok().unzip();
                let piece_map = piece_map(&self.session, id, &self.torrents[&id]);
                self.events.push(EngineEvent::ResumeData { id, data: data.into_bytes(), verified, piece_map, files });
            }
            (A::ResumeDataFailed { error, .. }, Some(id)) => {
                let error = Error::new(ErrorCode::StorageError, format!("could not save the resume data: {error}"))
                    .with_hint(format!("torrential resume {}", short(id)))
                    .with_next(format!("torrential resume {}", short(id)));
                self.events.push(EngineEvent::Failed { id, error });
            }
            (A::TorrentError { file, error, .. }, Some(id)) => {
                let error = storage_error(id, file.as_deref(), &error);
                self.fail(id, error);
            }
            (A::FileError { file, operation, error, .. }, Some(id)) => {
                log(&format!("{} {operation} failed on {}: {error}", short(id), file.display()));
                let error = storage_error(id, Some(&file), &error);
                self.fail(id, error);
            }
            (A::FilePrioritiesApplied { .. }, Some(id)) => {
                let t = self.torrents.get_mut(&id).expect("found above");
                t.unconfirmed = t.unconfirmed.saturating_sub(1);
                self.report_if_finished(id);
            }
            (A::ListenSucceeded { address, transport }, _) => log(&format!("listening on {address} ({transport})")),
            (A::ListenFailed { interface, port, transport, operation, error }, _) => {
                log(&format!("cannot listen on {interface} port {port} ({transport}): {operation}: {error}"));
            }
            (A::DhtBootstrapped, _) => {
                if !self.dht_bootstrapped {
                    self.dht_bootstrapped = true;
                    log("the DHT is bootstrapped");
                }
                // A torrent added before the bootstrap announced to an empty routing table, and
                // libtorrent would wait 15 minutes to announce it again.
                for &id in self.torrents.keys() {
                    let _ = self.session.announce_to_dht(id);
                }
            }
            (
                A::TorrentResumed { .. }
                | A::TorrentRemoved { .. }
                | A::ResumeDataUnchanged { .. }
                | A::HashFailed { .. },
                _,
            ) => {}
            (_, None) => {}
        }
    }

    fn removed(&mut self, id: TorrentId) {
        if let Some(left) = self.removing.get_mut(&id) {
            *left -= 1;
            if *left == 0 {
                self.removing.remove(&id);
            }
        }
        self.events.push(EngineEvent::Removed { id });
    }

    fn metadata(&mut self, id: TorrentId) {
        let torrent_file = match self.session.metainfo(id) {
            Err(e) if e.kind() == lt::ErrorKind::NoMetadata => self.session.info_dictionary(id),
            other => other,
        };
        let torrent_file = torrent_file.map_err(engine_error);
        match torrent_file.and_then(|m| Ok((file_indices(&m), metainfo(m)?))) {
            Ok((files, metainfo)) => {
                self.torrents.get_mut(&id).expect("found by the caller").files = Some(files);
                self.events.push(EngineEvent::Metadata { id, metainfo });
                if let Err(error) = self.apply_selection(id) {
                    self.events.push(EngineEvent::Failed { id, error });
                }
            }
            Err(error) => self.events.push(EngineEvent::Failed { id, error }),
        }
    }

    fn state_changed(&mut self, id: TorrentId, to: lt::TorrentState) {
        use lt::TorrentState as S;
        let t = self.torrents.get_mut(&id).expect("found by the caller");
        match (t.check, to) {
            (Check::Hashing { hold, started: false }, S::CheckingResumeData) => {
                t.check = Check::Hashing { hold, started: true };
            }
            (Check::Queued { hold }, S::CheckingFiles) => {
                t.check = Check::Hashing { hold, started: true };
                if hold && let Err(e) = self.session.resume(id) {
                    self.events.push(EngineEvent::Failed { id, error: engine_error(e) });
                }
            }
            _ => {}
        }
        let activity = match to {
            S::CheckingFiles | S::CheckingResumeData => Some(Activity::Checking),
            S::DownloadingMetadata => Some(Activity::Metadata),
            S::Downloading => Some(Activity::Downloading),
            S::Finished | S::Seeding => None,
        };
        if let Some(activity) = activity {
            self.events.push(EngineEvent::Activity { id, activity });
        }
    }

    fn checked(&mut self, id: TorrentId) {
        match self.torrents[&id].check {
            Check::Idle | Check::Hashing { started: false, .. } => {}
            Check::Queued { hold } => {
                if let Err(error) = self.start_hashing(id, hold) {
                    self.events.push(EngineEvent::Failed { id, error });
                }
            }
            Check::Hashing { hold, started: true } => {
                self.torrents.get_mut(&id).expect("found above").check = Check::Idle;
                let counts = self
                    .session
                    .selected_pieces(id)
                    .map_err(engine_error)
                    .and_then(|pieces| Ok((pieces, self.verified_bytes(id)?)));
                match counts {
                    Ok((pieces, (verified, files))) => {
                        let damaged_pieces = pieces.selected - pieces.verified;
                        let piece_map = piece_map(&self.session, id, &self.torrents[&id]);
                        self.events.push(EngineEvent::Checked { id, damaged_pieces, verified, piece_map, files });
                    }
                    Err(error) => self.events.push(EngineEvent::Failed { id, error }),
                }
                if !hold {
                    self.report_if_finished(id);
                } else if let Err(e) = self.session.pause(id) {
                    self.events.push(EngineEvent::Failed { id, error: engine_error(e) });
                } else {
                    self.torrents.get_mut(&id).expect("found above").own_pause_pending = true;
                }
            }
        }
    }

    fn verified_bytes(&self, id: TorrentId) -> Result<(u64, Vec<u64>), Error> {
        let t = self.torrents.get(&id).ok_or_else(|| Error::internal(format!("the engine has no torrent {id}")))?;
        let status = self.session.status(id).map_err(engine_error)?;
        let all = t.files.as_ref().map(|files| vec![true; files.len()]);
        Ok((sample(&status, t).verified, verified(&status.progress, t.files.as_deref(), all.as_deref())))
    }

    fn stats(&mut self) {
        for (&id, t) in &self.torrents {
            let Ok(status) = self.session.status(id) else { continue };
            if !status.paused {
                let (piece_map, missing) = (piece_map(&self.session, id, t), missing(&self.session, id, t));
                let stats = Stats { piece_map, missing, ..sample(&status, t) };
                self.events.push(EngineEvent::Stats { id, stats });
            }
        }
    }

    fn report_paused(&mut self, id: TorrentId) {
        if let (Some(t), Ok(status)) = (self.torrents.get(&id), self.session.status(id)) {
            let stats = Stats { piece_map: piece_map(&self.session, id, t), ..sample(&status, t) };
            self.events.push(EngineEvent::Stats { id, stats });
        }
        self.events.push(EngineEvent::Paused { id });
    }

    fn save_dht_state(&mut self) {
        self.dht_saved_at = Instant::now();
        let Some(path) = &self.dht_state else { return };
        let saved = self.session.save_state().map_err(|e| e.to_string());
        if let Err(e) = saved.and_then(|state| write_atomic(path, state.as_bytes()).map_err(|e| e.to_string())) {
            log(&format!("cannot save the DHT table to {}: {e}", path.display()));
        }
    }
}

impl EnginePort for Libtorrent {
    fn add(&mut self, torrent: AddTorrent) -> Result<(), Error> {
        let AddTorrent { id, source, save_path, wanted, resume_data, paused } = torrent;
        let fallback = match source {
            AddSource::Magnet(uri) => lt::TorrentSource::Magnet(lt::Magnet::parse(&uri).map_err(engine_error)?),
            AddSource::Metainfo(bytes) => {
                lt::TorrentSource::Metainfo(lt::Metainfo::from_bytes(bytes).map_err(engine_error)?)
            }
        };
        let files = match &fallback {
            lt::TorrentSource::Metainfo(m) => Some(file_indices(m)),
            _ => None,
        };
        let source = match resume_data.map(lt::ResumeData::from_bytes) {
            Some(Ok(resume)) => lt::TorrentSource::Resume(resume),
            Some(Err(e)) => {
                log(&format!("{} has unreadable resume data, so libtorrent hashes its files: {e}", short(id)));
                fallback
            }
            None => fallback,
        };
        let mut add = lt::AddTorrent::new(source, save_path);
        add.files = match (&files, &wanted) {
            (Some(_), Some(wanted)) if wanted.iter().all(|w| *w) => lt::FileSelection::All,
            (Some(files), Some(wanted)) => {
                lt::FileSelection::Only(files.iter().zip(wanted).filter(|(_, w)| **w).map(|(i, _)| *i).collect())
            }
            _ => lt::FileSelection::metadata_only(),
        };
        add.paused = paused;
        add.peers = self.peers.clone();
        if let lt::Added::Existing(_) = self.session.add(add).map_err(engine_error)? {
            return Err(Error::internal(format!("the engine already has torrent {id}")));
        }
        self.torrents.insert(id, Torrent { files, wanted, ..Torrent::default() });
        Ok(())
    }

    fn select(&mut self, id: TorrentId, wanted: &[bool]) -> Result<(), Error> {
        self.torrent(id)?.wanted = Some(wanted.to_vec());
        self.apply_selection(id)
    }

    fn pause(&mut self, id: TorrentId) -> Result<(), Error> {
        let paused = self.session.status(id).map_err(engine_error)?.paused;
        let t = self.torrent(id)?;
        if let Check::Hashing { started, .. } = t.check {
            t.check = Check::Hashing { hold: false, started };
        }
        match (paused, t.own_pause_pending) {
            (true, true) => t.own_pause_pending = false,
            (true, false) => self.report_paused(id),
            (false, _) => self.session.pause(id).map_err(engine_error)?,
        }
        Ok(())
    }

    fn resume(&mut self, id: TorrentId) -> Result<(), Error> {
        let t = self.torrent(id)?;
        t.check = match t.check {
            Check::Queued { .. } => Check::Queued { hold: false },
            Check::Hashing { started, .. } => Check::Hashing { hold: false, started },
            Check::Idle => Check::Idle,
        };
        self.session.resume(id).map_err(engine_error)?;
        self.report_if_finished(id);
        Ok(())
    }

    fn recheck(&mut self, id: TorrentId) -> Result<(), Error> {
        let status = self.session.status(id).map_err(engine_error)?;
        let hold = status.paused;
        match status.state {
            lt::TorrentState::CheckingResumeData => self.torrent(id)?.check = Check::Queued { hold },
            // libtorrent is already hashing every piece and ignores a recheck.
            lt::TorrentState::CheckingFiles if !hold => {
                self.torrent(id)?.check = Check::Hashing { hold, started: true };
            }
            _ => self.start_hashing(id, hold)?,
        }
        Ok(())
    }

    fn save_resume_data(&mut self, id: TorrentId) -> Result<(), Error> {
        self.session.save_resume_data(id).map_err(engine_error)
    }

    fn save_resume_data_if_progressed(&mut self, id: TorrentId) -> Result<(), Error> {
        self.session.save_resume_data_if_progressed(id).map_err(engine_error)
    }

    fn remove(&mut self, id: TorrentId) -> Result<(), Error> {
        let removing = self.session.remove(id);
        self.torrents.remove(&id);
        match removing {
            Ok(()) => *self.removing.entry(id).or_default() += 1,
            Err(e) if e.kind() == lt::ErrorKind::UnknownTorrent => self.events.push(EngineEvent::Removed { id }),
            Err(e) => return Err(engine_error(e)),
        }
        Ok(())
    }

    fn file_progress(&mut self, id: TorrentId) -> Result<Vec<u64>, Error> {
        Ok(self.verified_bytes(id)?.1)
    }

    fn want_range(&mut self, id: TorrentId, file: u32, offset: u64, length: u64) -> Result<u64, Error> {
        let index = self.torrents.get(&id).and_then(|t| t.files.as_ref()?.get(file as usize).copied());
        let index = index.ok_or_else(|| Error::internal(format!("the engine has no file {file} in torrent {id}")))?;
        self.session.want_range(id, index, offset, length).map_err(engine_error)
    }

    fn written(&mut self, id: TorrentId, file: u32, offset: u64, length: u64, runs: u32) -> Result<Vec<Slice>, Error> {
        let index = self.torrents.get(&id).and_then(|t| t.files.as_ref()?.get(file as usize).copied());
        let index = index.ok_or_else(|| Error::internal(format!("the engine has no file {file} in torrent {id}")))?;
        let counted = self.session.written_pieces(id, index, offset, length, runs).map_err(engine_error)?;
        Ok(counted.into_iter().map(|run| Slice::of(run.written, run.pieces)).collect())
    }

    fn stream(&mut self, id: TorrentId) -> Result<(), Error> {
        self.session.download_in_order(id).map_err(engine_error)
    }

    fn limit(&mut self, id: TorrentId, rate: Option<NonZeroU32>) -> Result<(), Error> {
        self.session.set_download_limit(id, rate).map_err(engine_error)
    }

    fn add_web_seeds(&mut self, id: TorrentId, seeds: &[String]) {
        if !self.torrents.contains_key(&id) {
            return;
        }
        for seed in seeds {
            if let Err(e) = self.session.add_web_seed(id, seed) {
                log(&format!("{} cannot add the web seed {seed}: {e}", short(id)));
            }
        }
        log(&format!("{} also downloads from {}", short(id), seeds.join(", ")));
    }
}

impl Engine for Libtorrent {
    fn port(&mut self) -> &mut dyn EnginePort {
        self
    }

    fn notify(&mut self, wake: Box<dyn Fn() + Send + Sync>) -> bool {
        self.session.notify(wake);
        true
    }

    fn poll(&mut self, max_wait: Duration) -> Vec<EngineEvent> {
        let wait = if self.events.is_empty() { max_wait } else { Duration::ZERO };
        match self.session.wait_alerts(wait) {
            Ok(alerts) => alerts.into_iter().for_each(|alert| self.alert(alert)),
            Err(e) => log(&format!("cannot read libtorrent alerts: {e}")),
        }
        if self.stats_at.elapsed() >= STATS_EVERY {
            self.stats_at = Instant::now();
            self.stats();
        }
        if self.dht_saved_at.elapsed() >= SAVE_DHT_EVERY {
            self.save_dht_state();
        }
        std::mem::take(&mut self.events)
    }
}

impl Drop for Libtorrent {
    fn drop(&mut self) {
        self.save_dht_state();
    }
}

pub fn parse(bytes: &[u8]) -> Result<Metainfo, Error> {
    let metainfo = lt::Metainfo::from_bytes(bytes.to_vec())
        .map_err(|e| Error::new(ErrorCode::InvalidTorrent, format!("not a torrent file: {}", e.message())))?;
    self::metainfo(metainfo)
}

fn config(network: &Network, port: u16) -> lt::Config {
    let off = lt::Config {
        listen: vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)],
        upload: lt::UploadPolicy::Off,
        ..lt::Config::loopback()
    };
    match network {
        Network::Loopback { .. } => off,
        Network::Public => lt::Config {
            listen: vec![
                SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port),
                SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port),
            ],
            dht: lt::Dht::On { bootstrap_nodes: BOOTSTRAP_NODES.map(String::from).to_vec() },
            local_network: true,
            connections: lt::ConnectionLimits::default(),
            ..off
        },
    }
}

fn listen_port(path: &Path, ip: IpAddr) -> Result<u16, Error> {
    let saved = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<u16>().ok()).filter(|p| *p != 0);
    if let Some(port) = saved {
        return Ok(port);
    }
    let unavailable = |e: std::io::Error| Error::internal(format!("cannot choose a listen port: {e}"));
    let port = std::net::TcpListener::bind((ip, 0)).and_then(|l| l.local_addr()).map_err(unavailable)?.port();
    write_atomic(path, format!("{port}\n").as_bytes())
        .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot write {}: {e}", path.display())))?;
    Ok(port)
}

fn metainfo(m: lt::Metainfo) -> Result<Metainfo, Error> {
    let name = m.name().to_string();
    let single_file = matches!(m.files(), [only] if only.path == Path::new(&name));
    let mut files = Vec::with_capacity(m.files().len());
    for f in m.files() {
        let path =
            if single_file { Some(name.as_str()) } else { f.path.strip_prefix(&name).ok().and_then(Path::to_str) };
        let path = path.ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidTorrent,
                format!("libtorrent placed {} outside the torrent's folder", f.path.display()),
            )
        })?;
        files.push(MetaFile { path: path.to_string(), size: f.size });
    }
    let web_seeds = m.web_seeds().to_vec();
    Ok(Metainfo::new(m.info_hash(), name, files, single_file, m.into_bytes())?.with_web_seeds(web_seeds))
}

fn file_indices(m: &lt::Metainfo) -> Vec<lt::FileIndex> {
    m.files().iter().map(|f| f.index).collect()
}

fn verified(progress: &lt::Progress, files: Option<&[lt::FileIndex]>, wanted: Option<&[bool]>) -> Vec<u64> {
    let (lt::Progress::Known(known), Some(files), Some(wanted)) = (progress, files, wanted) else { return Vec::new() };
    let by_index: BTreeMap<lt::FileIndex, u64> = known.iter().map(|f| (f.index, f.verified)).collect();
    files.iter().zip(wanted).map(|(i, w)| if *w { by_index.get(i).copied().unwrap_or(0) } else { 0 }).collect()
}

fn sample(status: &lt::TorrentStatus, t: &Torrent) -> Stats {
    Stats {
        verified: verified(&status.progress, t.files.as_deref(), t.wanted.as_deref()).iter().sum(),
        piece_map: None,
        missing: None,
        down_rate: status.download_rate,
        up_rate: status.upload_rate,
        peers: status.peers,
        seeds: status.seeds,
    }
}

fn piece_map(session: &lt::Session, id: TorrentId, t: &Torrent) -> Option<PieceMap> {
    if t.files.is_none() || t.wanted.is_none() {
        return None;
    }
    let slices = session.piece_slices(id, PieceMap::SLICES as u32).ok()?;
    let slices: [lt::SelectedPieces; PieceMap::SLICES] = slices.try_into().ok()?;
    Some(PieceMap::new(slices.map(|s| Slice::of(s.verified, s.selected))))
}

fn missing(session: &lt::Session, id: TorrentId, t: &Torrent) -> Option<Missing> {
    if t.files.is_none() || t.wanted.is_none() {
        return None;
    }
    let m = session.missing_pieces(id).ok()?;
    Some(Missing { pieces: m.missing, unavailable: m.unavailable })
}

fn storage_error(id: TorrentId, file: Option<&Path>, error: &lt::NativeError) -> Error {
    let errno = matches!(error.category.as_str(), "system" | "generic");
    let place = file.map(|f| format!(" ({})", f.display())).unwrap_or_default();
    if errno && [libc::ENOSPC, libc::EDQUOT].contains(&error.code) {
        Error::new(ErrorCode::DiskFull, format!("the disk is full{place}"))
            .with_hint(format!("free some space, then run torrential resume {}", short(id)))
    } else {
        Error::new(ErrorCode::StorageError, format!("{}{place}", error.message))
            .with_hint(format!("check the download folder, then run torrential resume {}", short(id)))
    }
}

fn engine_error(e: lt::EngineError) -> Error {
    match e.kind() {
        lt::ErrorKind::InvalidInput => Error::new(ErrorCode::InvalidTorrent, e.message().to_string()),
        _ => Error::internal(e.to_string()),
    }
}

fn short(id: TorrentId) -> String {
    id.to_hex()[..6].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_disk_is_disk_full_and_other_failures_are_storage_errors() {
        let id = TorrentId::from_bytes([0xab; 20]);
        let native = |code, message: &str| lt::NativeError { category: "system".into(), code, message: message.into() };
        let full =
            storage_error(id, Some(Path::new("/dl/set/a.bin")), &native(libc::ENOSPC, "No space left on device"));
        assert_eq!((full.code, full.message.as_str()), (ErrorCode::DiskFull, "the disk is full (/dl/set/a.bin)"));
        assert_eq!(full.hint.as_deref(), Some("free some space, then run torrential resume ababab"));
        let denied = storage_error(id, None, &native(libc::EACCES, "Permission denied"));
        assert_eq!((denied.code, denied.message.as_str()), (ErrorCode::StorageError, "Permission denied"));
    }

    #[test]
    fn torrent_files_list_paths_inside_the_torrents_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("set/docs")).unwrap();
        std::fs::write(dir.path().join("set/a.bin"), [1; 20_000]).unwrap();
        std::fs::write(dir.path().join("set/docs/b.txt"), [2; 5_000]).unwrap();
        std::fs::write(dir.path().join("one.iso"), [3; 30_000]).unwrap();
        let piece = lt::PieceSize::new(16 * 1024).unwrap();
        let torrent = |path: &str| {
            let created = lt::create_torrent(&dir.path().join(path), piece, lt::TorrentFormat::V1).unwrap();
            parse(created.as_bytes()).unwrap()
        };
        let file = |path: &str, size| MetaFile { path: path.into(), size };

        let set = torrent("set");
        assert_eq!((set.name.as_str(), set.single_file), ("set", false));
        assert_eq!(set.files, [file("a.bin", 20_000), file("docs/b.txt", 5_000)]);
        let iso = torrent("one.iso");
        assert_eq!((iso.name.as_str(), iso.single_file), ("one.iso", true));
        assert_eq!(iso.files, [file("one.iso", 30_000)]);
        assert_eq!(parse(b"d4:infoi1ee").unwrap_err().code, ErrorCode::InvalidTorrent);
    }
}
