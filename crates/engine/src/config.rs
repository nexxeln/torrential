use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;

/// Everything a session is created with. Settings cannot change after
/// `Session::new`.
#[derive(Debug, Clone)]
pub struct Config {
    /// Addresses to accept peers on. Port 0 lets the OS choose. A taken port
    /// is not an error: libtorrent tries the next 10 ports, then lets the OS
    /// choose. `Session::listen_port` reports the port it got.
    pub listen: Vec<SocketAddr>,
    pub dht: Dht,
    /// Find peers on the local network (BEP 14), and ask the router to forward the listen port
    /// with UPnP and NAT-PMP.
    pub local_network: bool,
    pub upload: UploadPolicy,
    pub connections: ConnectionLimits,
    pub client: ClientIdentity,
    /// Routing table and node ids from an earlier `Session::save_state`.
    pub dht_state: Option<DhtState>,
}

impl Config {
    /// Peers on this machine only: 127.0.0.1 on a port the OS chooses, no DHT, nothing on the
    /// local network, and any number of connections from 127.0.0.1.
    pub fn loopback() -> Self {
        Config {
            listen: vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)],
            dht: Dht::Off,
            local_network: false,
            upload: UploadPolicy::Normal,
            connections: ConnectionLimits { multiple_per_ip: true, ..ConnectionLimits::default() },
            client: ClientIdentity::default(),
            dht_state: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dht {
    Off,
    /// `bootstrap_nodes` are `host:port` pairs. libtorrent 2.1.2 ignores
    /// `Config::dht_state` when the list is empty, so `Session::new` rejects
    /// that combination.
    On {
        bootstrap_nodes: Vec<String>,
    },
}

/// How much payload this session sends to peers. The limits apply to every
/// peer, including loopback and LAN peers that libtorrent exempts by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadPolicy {
    Normal,
    /// Bytes per second across all torrents.
    Capped(NonZeroU32),
    /// No payload upload at all. Peers stay choked and every block request is
    /// refused.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Peer connections across all torrents.
    pub max_connections: NonZeroU32,
    /// Accept more than one connection from the same IP address.
    pub multiple_per_ip: bool,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        ConnectionLimits { max_connections: NonZeroU32::new(200).unwrap(), multiple_per_ip: false }
    }
}

/// How the session names itself to peers and HTTP servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// The start of the peer id. libtorrent fills the rest of the 20 bytes
    /// with random characters.
    pub peer_id_prefix: String,
    /// Sent to trackers and web seeds, and in the extension handshake.
    pub user_agent: String,
}

impl Default for ClientIdentity {
    fn default() -> Self {
        let version = env!("CARGO_PKG_VERSION");
        ClientIdentity { peer_id_prefix: azureus_prefix("TO", version), user_agent: format!("torrential/{version}") }
    }
}

fn azureus_prefix(code: &str, version: &str) -> String {
    let digit = |n: u32| match n {
        0..=9 => char::from(b'0' + n as u8),
        _ => char::from(b'A' + n.min(35) as u8 - 10),
    };
    let mut parts = version.split(['.', '-', '+']).map(|p| p.parse::<u32>().unwrap_or(0));
    let digits: String = (0..4).map(|_| digit(parts.next().unwrap_or(0))).collect();
    format!("-{code}{digits}-")
}

/// Saved DHT state, opaque to everything but libtorrent.
#[derive(Clone, PartialEq, Eq)]
pub struct DhtState(Vec<u8>);

impl DhtState {
    /// Accepts bytes read back from storage. `Session::new` rejects them with
    /// `ErrorKind::InvalidInput` if libtorrent cannot decode them.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        DhtState(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl std::fmt::Debug for DhtState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DhtState({} bytes)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::azureus_prefix;

    #[test]
    fn version_components_encode_as_azureus_digits() {
        assert_eq!(azureus_prefix("TO", "0.1.0"), "-TO0100-");
        assert_eq!(azureus_prefix("TO", "1.12.40-beta"), "-TO1CZ0-");
    }
}
