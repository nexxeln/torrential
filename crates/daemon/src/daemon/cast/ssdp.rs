//! Finding media renderers with SSDP: an M-SEARCH sent to the UPnP multicast group from each
//! address this Mac has on the home network, and the answers that come back.

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use url::{Host, Url};

/// Where UPnP devices listen for searches.
pub const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
pub const RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
/// How long a device may wait before it answers, in seconds. A search listens one second more.
const MX: u64 = 2;
/// UPnP 1.1 asks for 2, so a search stays on the home network.
const TTL: u32 = 2;
/// A search is sent twice, since a lost datagram is never sent again.
const SENDS: usize = 2;
/// The most devices a search keeps, and the most from one address, so a device that answers for
/// hundreds can neither fill the engine nor crowd out the TVs.
pub const MOST: usize = 32;
const MOST_FROM_ONE: usize = 4;

/// Where searches go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Find {
    /// The UPnP multicast group, from each address on the home network.
    Multicast,
    /// One address, from 127.0.0.1, for a test's fake renderer.
    At(SocketAddr),
}

/// A device that answered a search: where its description is, and the address it answered from.
/// Every request for the device goes to that address.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Heard {
    pub location: Url,
    pub from: Ipv4Addr,
}

/// The devices that answered, one for each description, in the order they answered.
#[derive(Default, Debug)]
pub struct Answers(Vec<Heard>);

impl Answers {
    /// Keeps `heard`, unless its description is kept already or it would pass a limit.
    fn keep(&mut self, heard: Heard) {
        let from_one = self.0.iter().filter(|kept| kept.from == heard.from).count();
        if self.0.len() < MOST && from_one < MOST_FROM_ONE && !self.0.iter().any(|kept| kept.location == heard.location)
        {
            self.0.push(heard);
        }
    }
}

impl IntoIterator for Answers {
    type Item = Heard;
    type IntoIter = std::vec::IntoIter<Heard>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Searches for media renderers from `sources`, this machine's addresses on the home network.
pub async fn search(find: Find, sources: &[Ipv4Addr]) -> io::Result<Answers> {
    let (sockets, to) = match find {
        Find::Multicast => {
            let sockets: Vec<UdpSocket> = sources.iter().filter_map(|ip| multicast_socket(*ip).ok()).collect();
            (sockets, SocketAddr::V4(GROUP))
        }
        Find::At(address) => (vec![UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?], address),
    };
    if sockets.is_empty() {
        return Ok(Answers::default());
    }
    let message = request(to);
    for _ in 0..SENDS {
        for socket in &sockets {
            let _ = socket.send_to(message.as_bytes(), to).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut answers = Answers::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(MX + 1);
    let link_local = sources.iter().any(Ipv4Addr::is_link_local);
    let mut tasks = tokio::task::JoinSet::new();
    for socket in sockets {
        tasks.spawn(async move {
            let mut heard = Answers::default();
            let mut buf = vec![0; 8 * 1024];
            while let Ok(Ok((length, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
                let SocketAddr::V4(from) = from else { continue };
                if neighbour(*from.ip(), link_local)
                    && let Some(answer) = answer(&String::from_utf8_lossy(&buf[..length]), *from.ip())
                {
                    heard.keep(answer);
                }
            }
            heard
        });
    }
    while let Some(heard) = tasks.join_next().await {
        for answer in heard.unwrap_or_default() {
            answers.keep(answer);
        }
    }
    Ok(answers)
}

/// A UDP socket that sends multicast out of the interface that holds `ip`, so a Mac on Wi-Fi and
/// a VPN at once still searches the home network.
fn multicast_socket(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    socket.set_multicast_if_v4(&ip)?;
    socket.set_multicast_ttl_v4(TTL)?;
    socket.bind(&SocketAddr::from((ip, 0)).into())?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

fn request(to: SocketAddr) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {to}\r\nMAN: \"ssdp:discover\"\r\nMX: {MX}\r\nST: {RENDERER}\r\n\
         USER-AGENT: UPnP/1.1 torrential/{}\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Whether a device at `ip` can be one on the home network: not this machine's loopback, and
/// link-local only when this machine has a link-local address too.
fn neighbour(ip: Ipv4Addr, link_local: bool) -> bool {
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast())
        && (link_local || !ip.is_link_local())
}

/// Whether `url` is plain HTTP to `ip`, the one place a device's requests may go.
pub fn at(url: &Url, ip: Ipv4Addr) -> bool {
    url.scheme() == "http" && url.host() == Some(Host::Ipv4(ip))
}

/// Where the description is of an answer from `from` to a search for media renderers. An answer
/// whose LOCATION is on another address is `None`, as is anything else.
fn answer(datagram: &str, from: Ipv4Addr) -> Option<Heard> {
    let mut lines = datagram.split("\r\n").flat_map(|line| line.split('\n'));
    let status = lines.next()?;
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        return None;
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let location = Url::parse(headers.get("location")?).ok().filter(|location| at(location, from))?;
    if headers.get("st").is_some_and(|st| !st.eq_ignore_ascii_case(RENDERER)) {
        return None;
    }
    Some(Heard { location, from })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_asks_for_media_renderers_with_the_headers_upnp_names() {
        let text = request(SocketAddr::V4(GROUP));
        assert!(text.starts_with(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\
             ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSER-AGENT: UPnP/1.1 torrential/"
        ));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn an_answer_gives_where_the_description_is() {
        let lg = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\n\
                  LOCATION: http://192.168.1.40:1876/\r\nSERVER: Linux/4.4 UPnP/1.0 LGE WebOS TV/Version 0.9\r\n\
                  ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
                  USN: uuid:6a3e0b2c::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        let tv = Ipv4Addr::new(192, 168, 1, 40);
        let heard = |location: &str, from| Heard { location: Url::parse(location).unwrap(), from };
        assert_eq!(answer(lg, tv), Some(heard("http://192.168.1.40:1876/", tv)));
        let nas = Ipv4Addr::new(10, 0, 0, 9);
        let bare = "HTTP/1.1 200 OK\nlocation: http://10.0.0.9/desc.xml\n\n";
        assert_eq!(answer(bare, nas), Some(heard("http://10.0.0.9/desc.xml", nas)));
        assert_eq!(answer("NOTIFY * HTTP/1.1\r\nLOCATION: http://10.0.0.9/\r\n\r\n", nas), None, "not an answer");
        let router = "HTTP/1.1 200 OK\r\nLOCATION: http://10.0.0.1/\r\nST: upnp:rootdevice\r\n\r\n";
        assert_eq!(answer(router, Ipv4Addr::new(10, 0, 0, 1)), None, "a device that is not a renderer");
        assert_eq!(answer("HTTP/1.1 200 OK\r\nLOCATION: https://10.0.0.9/\r\n\r\n", nas), None);
        assert_eq!(answer(lg, Ipv4Addr::new(192, 168, 1, 66)), None, "a description on another address");
        let loopback = "HTTP/1.1 200 OK\r\nLOCATION: http://127.0.0.1:47919/\r\n\r\n";
        assert_eq!(answer(loopback, nas), None);
        let named = "HTTP/1.1 200 OK\r\nLOCATION: http://localhost:47919/\r\n\r\n";
        assert_eq!(answer(named, nas), None, "a name could lead anywhere");
    }

    #[test]
    fn a_search_keeps_each_description_once_and_few_from_one_address() {
        let mut answers = Answers::default();
        for host in 1..=20u8 {
            for n in 0..10 {
                let from = Ipv4Addr::new(192, 168, 1, host);
                let location = Url::parse(&format!("http://{from}:1876/{n}.xml")).unwrap();
                answers.keep(Heard { location: location.clone(), from });
                answers.keep(Heard { location, from });
            }
        }
        let kept: Vec<String> = answers.into_iter().map(|heard| heard.location.to_string()).collect();
        assert_eq!(kept.len(), MOST);
        assert_eq!(
            kept[..5],
            [
                "http://192.168.1.1:1876/0.xml",
                "http://192.168.1.1:1876/1.xml",
                "http://192.168.1.1:1876/2.xml",
                "http://192.168.1.1:1876/3.xml",
                "http://192.168.1.2:1876/0.xml",
            ]
        );
        assert_eq!(kept.last().map(String::as_str), Some("http://192.168.1.8:1876/3.xml"));
    }

    #[test]
    fn answers_count_only_from_devices_that_can_be_on_the_home_network() {
        assert!(neighbour(Ipv4Addr::new(192, 168, 1, 40), false));
        assert!(!neighbour(Ipv4Addr::LOCALHOST, false));
        assert!(!neighbour(Ipv4Addr::new(127, 0, 0, 2), true));
        assert!(!neighbour(Ipv4Addr::UNSPECIFIED, true));
        assert!(!neighbour(Ipv4Addr::BROADCAST, true));
        assert!(!neighbour(Ipv4Addr::new(169, 254, 7, 7), false), "link-local, when this machine has a router");
        assert!(neighbour(Ipv4Addr::new(169, 254, 7, 7), true), "link-local, when this machine has none either");
    }
}
