use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::path::{Path, PathBuf};

use crate::alert::{Alert, SessionStats, Transport};
use crate::config::{Config, Dht, UploadPolicy};
use crate::error::{EngineError, ErrorKind, NativeError};
use crate::identity::{InfoHash, InfoHashV1, InfoHashV2};
use crate::torrent::{
    AddTorrent, FileEntry, FileIndex, FilePriority, FileProgress, FileSelection, Progress, ResumeData, TorrentSource,
    TorrentState, TorrentStatus,
};

#[cxx::bridge(namespace = "torrential")]
pub(crate) mod ffi {
    enum FailureKind {
        UnknownTorrent,
        NoMetadata,
        InvalidInput,
        Native,
    }

    struct Hashes {
        v1: [u8; 20],
        v2: [u8; 32],
        has_v1: bool,
        has_v2: bool,
    }

    struct ErrorRecord {
        category: String,
        code: i32,
        message: String,
    }

    struct SessionSettings {
        listen_interfaces: String,
        dht: bool,
        dht_bootstrap_nodes: String,
        lsd: bool,
        upnp: bool,
        natpmp: bool,
        download_rate_limit: i32,
        upload_rate_limit: i32,
        upload_off: bool,
        connections_limit: i32,
        multiple_connections_per_ip: bool,
        peer_fingerprint: String,
        user_agent: String,
        state: Vec<u8>,
    }

    enum SourceKind {
        Magnet,
        Metainfo,
        Resume,
    }

    struct Endpoint {
        address: String,
        port: u16,
    }

    struct AddParams {
        source: SourceKind,
        magnet: String,
        bytes: Vec<u8>,
        save_path: String,
        all_files: bool,
        selected: Vec<u32>,
        paused: bool,
        trackers: Vec<String>,
        web_seeds: Vec<String>,
        peers: Vec<Endpoint>,
    }

    struct AddResult {
        hashes: Hashes,
        existed: bool,
    }

    struct FileRecord {
        index: u32,
        path: String,
        size: u64,
    }

    struct ParsedTorrent {
        hashes: Hashes,
        name: String,
        has_metadata: bool,
        piece_length: u32,
        files: Vec<FileRecord>,
        web_seeds: Vec<String>,
    }

    struct PriorityRecord {
        index: u32,
        priority: u8,
    }

    enum State {
        CheckingFiles,
        DownloadingMetadata,
        Downloading,
        Finished,
        Seeding,
        CheckingResumeData,
    }

    struct PieceCounts {
        selected: u32,
        verified: u32,
    }

    struct MissingCounts {
        missing: u32,
        unavailable: u32,
    }

    struct FileProgressRecord {
        index: u32,
        size: u64,
        priority: u8,
        verified: u64,
    }

    struct StatusRecord {
        hashes: Hashes,
        name: String,
        save_path: String,
        state: State,
        paused: bool,
        error: ErrorRecord,
        has_metadata: bool,
        download_rate: i64,
        upload_rate: i64,
        total_payload_download: i64,
        total_payload_upload: i64,
        peers: i32,
        seeds: i32,
        files: Vec<FileProgressRecord>,
    }

    enum AlertKind {
        MetadataReceived,
        MetadataFailed,
        TorrentFinished,
        TorrentPaused,
        TorrentResumed,
        TorrentChecked,
        StateChanged,
        ResumeDataSaved,
        ResumeDataFailed,
        ResumeDataUnchanged,
        TorrentError,
        FileError,
        HashFailed,
        TorrentRemoved,
        FilePrioritiesApplied,
        ListenSucceeded,
        ListenFailed,
        DhtBootstrapped,
        SessionStats,
    }

    enum Transport {
        Tcp,
        TcpSsl,
        Utp,
        UtpSsl,
        Socks5,
        Socks5Ssl,
        Http,
        HttpSsl,
        I2p,
        Rtc,
    }

    struct AlertRecord {
        kind: AlertKind,
        hashes: Hashes,
        from: State,
        to: State,
        error: ErrorRecord,
        path: String,
        operation: String,
        piece: i32,
        address: String,
        scope_id: u32,
        port: u16,
        transport: Transport,
        interface_name: String,
        bytes: Vec<u8>,
        stats: StatsRecord,
    }

    struct StatsRecord {
        sent_payload_bytes: i64,
        received_payload_bytes: i64,
        sent_bytes: i64,
        received_bytes: i64,
        dht_nodes: i64,
        peers_connected: i64,
    }

    unsafe extern "C++" {
        include!("torrential-engine/cpp/shim.h");

        type Session;

        fn new_session(settings: &SessionSettings) -> Result<UniquePtr<Session>>;
        fn add_torrent(self: Pin<&mut Session>, params: &AddParams) -> Result<AddResult>;
        fn pause(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn resume(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn set_file_priorities(self: Pin<&mut Session>, id: &[u8], priorities: &[PriorityRecord]) -> Result<()>;
        fn force_recheck(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn announce_to_dht(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn save_resume_data(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn save_resume_data_if_progressed(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn remove(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn connect_peer(self: Pin<&mut Session>, id: &[u8], address: &str, port: u16) -> Result<()>;
        fn add_web_seed(self: Pin<&mut Session>, id: &[u8], url: &str) -> Result<()>;
        fn metainfo(self: &Session, id: &[u8]) -> Result<Vec<u8>>;
        fn info_dictionary(self: &Session, id: &[u8]) -> Result<Vec<u8>>;
        fn files(self: &Session, id: &[u8]) -> Result<Vec<FileRecord>>;
        fn status(self: &Session, id: &[u8]) -> Result<StatusRecord>;
        fn piece_slices(self: &Session, id: &[u8], slices: u32) -> Result<Vec<PieceCounts>>;
        fn missing_pieces(self: &Session, id: &[u8]) -> Result<MissingCounts>;
        fn want_range(self: Pin<&mut Session>, id: &[u8], file: u32, offset: u64, length: u64) -> Result<bool>;
        fn download_in_order(self: Pin<&mut Session>, id: &[u8]) -> Result<()>;
        fn post_session_stats(self: Pin<&mut Session>) -> Result<()>;
        fn save_state(self: &Session) -> Result<Vec<u8>>;
        fn listen_port(self: &Session) -> Result<u16>;
        fn wait_alerts(self: Pin<&mut Session>, timeout_ms: u32) -> Result<Vec<AlertRecord>>;

        fn parse_magnet(uri: &str) -> Result<ParsedTorrent>;
        fn parse_metainfo(bytes: &[u8]) -> Result<ParsedTorrent>;
        fn parse_resume_data(bytes: &[u8]) -> Result<ParsedTorrent>;
        fn create_torrent(path: &str, piece_size: u32, hybrid: bool) -> Result<Vec<u8>>;
        fn libtorrent_version() -> Result<String>;
    }
}

// SAFETY: lt::session has no thread affinity. Its calls are marshalled to
// libtorrent's network thread, and wait_for_alert and pop_alerts lock the
// alert queue, so any one thread at a time may own it. The shim writes its own
// fields only in the constructor.
unsafe impl Send for ffi::Session {}

/// cxx carries only an exception's message, so the shim's trycatch writes the
/// `FailureKind` discriminant in front of it.
pub(crate) fn error(e: cxx::Exception) -> EngineError {
    let what = e.what();
    let Some((repr, message)) = what.split_once(':').and_then(|(kind, message)| Some((kind.parse().ok()?, message)))
    else {
        return EngineError::new(ErrorKind::Native, what);
    };
    let kind = match (ffi::FailureKind { repr }) {
        ffi::FailureKind::UnknownTorrent => ErrorKind::UnknownTorrent,
        ffi::FailureKind::NoMetadata => ErrorKind::NoMetadata,
        ffi::FailureKind::InvalidInput => ErrorKind::InvalidInput,
        _ => ErrorKind::Native,
    };
    EngineError::new(kind, message)
}

pub(crate) fn info_hash(h: &ffi::Hashes) -> Result<InfoHash, EngineError> {
    match (h.has_v1, h.has_v2) {
        (true, false) => Ok(InfoHash::V1(InfoHashV1::from_bytes(h.v1))),
        (false, true) => Ok(InfoHash::V2(InfoHashV2::from_bytes(h.v2))),
        (true, true) => Ok(InfoHash::Hybrid { v1: InfoHashV1::from_bytes(h.v1), v2: InfoHashV2::from_bytes(h.v2) }),
        (false, false) => Err(EngineError::new(ErrorKind::Native, "record has no info hash")),
    }
}

fn native_error(e: ffi::ErrorRecord) -> Option<NativeError> {
    (e.code != 0).then_some(NativeError { category: e.category, code: e.code, message: e.message })
}

fn required_error(e: ffi::ErrorRecord) -> NativeError {
    native_error(e).unwrap_or_else(|| NativeError {
        category: String::new(),
        code: 0,
        message: "libtorrent reported a failure without an error code".to_owned(),
    })
}

fn state(s: ffi::State) -> Result<TorrentState, EngineError> {
    Ok(match s {
        ffi::State::CheckingFiles => TorrentState::CheckingFiles,
        ffi::State::DownloadingMetadata => TorrentState::DownloadingMetadata,
        ffi::State::Downloading => TorrentState::Downloading,
        ffi::State::Finished => TorrentState::Finished,
        ffi::State::Seeding => TorrentState::Seeding,
        ffi::State::CheckingResumeData => TorrentState::CheckingResumeData,
        _ => return Err(EngineError::new(ErrorKind::Native, "unknown torrent state")),
    })
}

fn transport(t: ffi::Transport) -> Result<Transport, EngineError> {
    Ok(match t {
        ffi::Transport::Tcp => Transport::Tcp,
        ffi::Transport::TcpSsl => Transport::TcpSsl,
        ffi::Transport::Utp => Transport::Utp,
        ffi::Transport::UtpSsl => Transport::UtpSsl,
        ffi::Transport::Socks5 => Transport::Socks5,
        ffi::Transport::Socks5Ssl => Transport::Socks5Ssl,
        ffi::Transport::Http => Transport::Http,
        ffi::Transport::HttpSsl => Transport::HttpSsl,
        ffi::Transport::I2p => Transport::I2p,
        ffi::Transport::Rtc => Transport::Rtc,
        _ => return Err(EngineError::new(ErrorKind::Native, "unknown transport")),
    })
}

pub(crate) fn priority(p: FilePriority) -> u8 {
    match p {
        FilePriority::Skip => 0,
        FilePriority::Low => 1,
        FilePriority::Normal => 4,
        FilePriority::High => 7,
    }
}

fn priority_from(p: u8) -> FilePriority {
    match p {
        0 => FilePriority::Skip,
        1..=3 => FilePriority::Low,
        4 => FilePriority::Normal,
        _ => FilePriority::High,
    }
}

pub(crate) fn path_str(path: &Path) -> Result<&str, EngineError> {
    path.to_str()
        .ok_or_else(|| EngineError::new(ErrorKind::InvalidInput, format!("path is not UTF-8: {}", path.display())))
}

pub(crate) fn settings(config: &Config) -> ffi::SessionSettings {
    let listen_interfaces = config.listen.iter().map(SocketAddr::to_string).collect::<Vec<_>>().join(",");
    let (dht, dht_bootstrap_nodes) = match &config.dht {
        Dht::Off => (false, String::new()),
        Dht::On { bootstrap_nodes } => (true, bootstrap_nodes.join(",")),
    };
    let (upload_rate_limit, upload_off) = match config.upload {
        UploadPolicy::Normal => (0, false),
        UploadPolicy::Capped(rate) => (setting_int(rate.get()), false),
        UploadPolicy::Off => (0, true),
    };
    ffi::SessionSettings {
        listen_interfaces,
        dht,
        dht_bootstrap_nodes,
        lsd: config.lsd,
        upnp: config.upnp,
        natpmp: config.natpmp,
        download_rate_limit: config.download_limit.map_or(0, |r| setting_int(r.get())),
        upload_rate_limit,
        upload_off,
        connections_limit: setting_int(config.connections.max_connections.get()),
        multiple_connections_per_ip: config.connections.multiple_per_ip,
        peer_fingerprint: config.client.peer_id_prefix.clone(),
        user_agent: config.client.user_agent.clone(),
        state: config.dht_state.as_ref().map_or_else(Vec::new, |s| s.as_bytes().to_vec()),
    }
}

fn setting_int(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

pub(crate) fn add_params(add: &AddTorrent) -> Result<ffi::AddParams, EngineError> {
    let (source, magnet, bytes) = match &add.source {
        TorrentSource::Magnet(m) => (ffi::SourceKind::Magnet, m.as_str().to_owned(), Vec::new()),
        TorrentSource::Metainfo(m) => (ffi::SourceKind::Metainfo, String::new(), m.as_bytes().to_vec()),
        TorrentSource::Resume(r) => (ffi::SourceKind::Resume, String::new(), r.as_bytes().to_vec()),
    };
    let (all_files, selected) = match &add.files {
        FileSelection::All => (true, Vec::new()),
        FileSelection::Only(indices) => (false, indices.iter().map(|i| i.get()).collect()),
    };
    Ok(ffi::AddParams {
        source,
        magnet,
        bytes,
        save_path: path_str(&add.save_path)?.to_owned(),
        all_files,
        selected,
        paused: add.paused,
        trackers: add.trackers.clone(),
        web_seeds: add.web_seeds.clone(),
        peers: add.peers.iter().map(endpoint).collect(),
    })
}

pub(crate) fn endpoint(peer: &SocketAddr) -> ffi::Endpoint {
    ffi::Endpoint { address: peer.ip().to_string(), port: peer.port() }
}

pub(crate) fn file_entries(records: Vec<ffi::FileRecord>) -> Vec<FileEntry> {
    records
        .into_iter()
        .map(|r| FileEntry { index: FileIndex::new(r.index), path: PathBuf::from(r.path), size: r.size })
        .collect()
}

pub(crate) fn status(r: ffi::StatusRecord) -> Result<TorrentStatus, EngineError> {
    let progress = if r.has_metadata {
        Progress::Known(
            r.files
                .into_iter()
                .map(|f| FileProgress {
                    index: FileIndex::new(f.index),
                    size: f.size,
                    priority: priority_from(f.priority),
                    verified: f.verified,
                })
                .collect(),
        )
    } else {
        Progress::AwaitingMetadata
    };
    Ok(TorrentStatus {
        info_hash: info_hash(&r.hashes)?,
        name: r.name,
        save_path: PathBuf::from(r.save_path),
        state: state(r.state)?,
        paused: r.paused,
        error: native_error(r.error),
        download_rate: non_negative(r.download_rate),
        upload_rate: non_negative(r.upload_rate),
        downloaded_payload: non_negative(r.total_payload_download),
        uploaded_payload: non_negative(r.total_payload_upload),
        peers: non_negative(r.peers.into()),
        seeds: non_negative(r.seeds.into()),
        progress,
    })
}

fn non_negative<T: TryFrom<i64> + Default>(v: i64) -> T {
    T::try_from(v).unwrap_or_default()
}

pub(crate) fn alert(r: ffi::AlertRecord) -> Result<Alert, EngineError> {
    use ffi::AlertKind as K;
    let torrent = || info_hash(&r.hashes);
    Ok(match r.kind {
        K::MetadataReceived => Alert::MetadataReceived { torrent: torrent()? },
        K::MetadataFailed => Alert::MetadataFailed { torrent: torrent()?, error: required_error(r.error) },
        K::TorrentFinished => Alert::TorrentFinished { torrent: torrent()? },
        K::TorrentPaused => Alert::TorrentPaused { torrent: torrent()? },
        K::TorrentResumed => Alert::TorrentResumed { torrent: torrent()? },
        K::TorrentChecked => Alert::TorrentChecked { torrent: torrent()? },
        K::StateChanged => Alert::StateChanged { torrent: torrent()?, from: state(r.from)?, to: state(r.to)? },
        K::ResumeDataSaved => {
            let torrent = torrent()?;
            Alert::ResumeDataSaved { torrent, data: ResumeData::new(r.bytes, torrent) }
        }
        K::ResumeDataFailed => Alert::ResumeDataFailed { torrent: torrent()?, error: required_error(r.error) },
        K::ResumeDataUnchanged => Alert::ResumeDataUnchanged { torrent: torrent()? },
        K::TorrentError => Alert::TorrentError {
            torrent: torrent()?,
            file: (!r.path.is_empty()).then(|| PathBuf::from(&r.path)),
            error: required_error(r.error),
        },
        K::FileError => Alert::FileError {
            torrent: torrent()?,
            file: PathBuf::from(&r.path),
            operation: r.operation,
            error: required_error(r.error),
        },
        K::HashFailed => Alert::HashFailed {
            torrent: torrent()?,
            piece: u32::try_from(r.piece).map_err(|_| EngineError::new(ErrorKind::Native, "negative piece index"))?,
        },
        K::TorrentRemoved => Alert::TorrentRemoved { torrent: torrent()? },
        K::FilePrioritiesApplied => Alert::FilePrioritiesApplied { torrent: torrent()? },
        K::ListenSucceeded => Alert::ListenSucceeded {
            address: match ip(&r.address)? {
                IpAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(v6, r.port, 0, r.scope_id)),
                v4 => SocketAddr::new(v4, r.port),
            },
            transport: transport(r.transport)?,
        },
        K::ListenFailed => Alert::ListenFailed {
            interface: r.interface_name,
            port: r.port,
            transport: transport(r.transport)?,
            operation: r.operation,
            error: required_error(r.error),
        },
        K::DhtBootstrapped => Alert::DhtBootstrapped,
        K::SessionStats => Alert::SessionStats(session_stats(&r.stats)),
        _ => return Err(EngineError::new(ErrorKind::Native, "unknown alert kind")),
    })
}

fn ip(address: &str) -> Result<IpAddr, EngineError> {
    address.parse().map_err(|_| {
        EngineError::new(ErrorKind::Native, format!("libtorrent reported an unparsable address {address:?}"))
    })
}

fn session_stats(s: &ffi::StatsRecord) -> SessionStats {
    SessionStats {
        sent_payload_bytes: non_negative(s.sent_payload_bytes),
        received_payload_bytes: non_negative(s.received_payload_bytes),
        sent_bytes: non_negative(s.sent_bytes),
        received_bytes: non_negative(s.received_bytes),
        dht_nodes: non_negative(s.dht_nodes),
        peers_connected: non_negative(s.peers_connected),
    }
}
