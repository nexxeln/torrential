//! Finding media renderers with SSDP: an M-SEARCH sent to the UPnP multicast group from each
//! address this Mac has on the home network, and the answers that come back.

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;

/// Where UPnP devices listen for searches.
pub const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
pub const RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
/// How long a device may wait before it answers, in seconds. A search listens one second more.
const MX: u64 = 2;
/// UPnP 1.1 asks for 2, so a search stays on the home network.
const TTL: u32 = 2;
/// A search is sent twice, since a lost datagram is never sent again.
const SENDS: usize = 2;

/// Where searches go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Find {
    /// The UPnP multicast group, from each address on the home network.
    Multicast,
    /// One address, from 127.0.0.1, for a test's fake renderer.
    At(SocketAddr),
}

/// A device that answered: where its description is, by the USN it gave.
pub type Answers = BTreeMap<String, String>;

/// Searches for media renderers and returns the answers by USN, each with its LOCATION.
pub async fn search(find: Find, sources: &[Ipv4Addr]) -> io::Result<Answers> {
    let (sockets, to) = match find {
        Find::Multicast => {
            let sockets: Vec<UdpSocket> = sources.iter().filter_map(|ip| multicast_socket(*ip).ok()).collect();
            (sockets, SocketAddr::V4(GROUP))
        }
        Find::At(address) => (vec![UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?], address),
    };
    if sockets.is_empty() {
        return Ok(Answers::new());
    }
    let message = request(to);
    for _ in 0..SENDS {
        for socket in &sockets {
            let _ = socket.send_to(message.as_bytes(), to).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut answers = Answers::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(MX + 1);
    let mut tasks = tokio::task::JoinSet::new();
    for socket in sockets {
        tasks.spawn(async move {
            let mut heard = Vec::new();
            let mut buf = vec![0; 8 * 1024];
            while let Ok(Ok((length, _))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
                heard.extend(answer(&String::from_utf8_lossy(&buf[..length])));
            }
            heard
        });
    }
    while let Some(heard) = tasks.join_next().await {
        answers.extend(heard.unwrap_or_default());
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

/// The USN and LOCATION of an answer to a search for media renderers. Anything else is `None`.
fn answer(datagram: &str) -> Option<(String, String)> {
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
    let location = headers.get("location").filter(|l| l.starts_with("http://"))?;
    if headers.get("st").is_some_and(|st| !st.eq_ignore_ascii_case(RENDERER)) {
        return None;
    }
    let usn = headers.get("usn").cloned().unwrap_or_else(|| location.clone());
    Some((usn, location.clone()))
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
    fn an_answer_gives_the_usn_and_where_the_description_is() {
        let lg = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\n\
                  LOCATION: http://192.168.1.40:1876/\r\nSERVER: Linux/4.4 UPnP/1.0 LGE WebOS TV/Version 0.9\r\n\
                  ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
                  USN: uuid:6a3e0b2c::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(
            answer(lg),
            Some((
                "uuid:6a3e0b2c::urn:schemas-upnp-org:device:MediaRenderer:1".into(),
                "http://192.168.1.40:1876/".into()
            ))
        );
        let bare = "HTTP/1.1 200 OK\nlocation: http://10.0.0.9/desc.xml\n\n";
        assert_eq!(answer(bare), Some(("http://10.0.0.9/desc.xml".into(), "http://10.0.0.9/desc.xml".into())));
        assert_eq!(answer("NOTIFY * HTTP/1.1\r\nLOCATION: http://10.0.0.9/\r\n\r\n"), None, "not an answer");
        let router = "HTTP/1.1 200 OK\r\nLOCATION: http://10.0.0.1/\r\nST: upnp:rootdevice\r\n\r\n";
        assert_eq!(answer(router), None, "a device that is not a renderer");
        assert_eq!(answer("HTTP/1.1 200 OK\r\nLOCATION: https://10.0.0.9/\r\n\r\n"), None);
    }
}
