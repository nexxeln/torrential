//! The names a phone or TV on the home network reaches this machine by: its .local name, which
//! Bonjour or Avahi answers for, and its addresses.

use std::net::Ipv4Addr;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Names {
    /// Lowercase, such as `studio-mac.local`. `None` when the machine has no host name.
    pub local: Option<String>,
    /// IPv4 addresses on the home network, the most likely to be reachable first.
    pub lan: Vec<Ipv4Addr>,
}

impl Names {
    pub fn read() -> Self {
        Self { local: local_name(), lan: lan_addresses() }
    }
}

/// The name Bonjour answers for: the Local Hostname in System Settings, Sharing.
#[cfg(target_os = "macos")]
fn local_name() -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/scutil").args(["--get", "LocalHostName"]).output().ok();
    let scutil = output.filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    scutil.and_then(|name| dot_local(&name)).or_else(|| dot_local(&host_name()?))
}

/// Avahi, and Windows itself, answer for the host name with `.local` after it.
#[cfg(not(target_os = "macos"))]
fn local_name() -> Option<String> {
    dot_local(&host_name()?)
}

#[cfg(unix)]
fn host_name() -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// `name.local` from a host name's first label, if that label is one a browser can type.
fn dot_local(name: &str) -> Option<String> {
    let label = name.trim().split('.').next()?.to_ascii_lowercase();
    let valid = !label.is_empty()
        && label.len() <= 63
        && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-');
    valid.then(|| format!("{label}.local"))
}

/// The computer's DNS host name, as Windows answers for it over mDNS.
#[cfg(windows)]
fn host_name() -> Option<String> {
    use windows::Win32::System::SystemInformation::{ComputerNamePhysicalDnsHostname, GetComputerNameExW};
    use windows::core::PWSTR;
    let mut size = 0u32;
    // The first call only says how long the name is.
    let _ = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, None, &mut size) };
    let mut name = vec![0u16; size as usize];
    unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, Some(PWSTR(name.as_mut_ptr())), &mut size) }.ok()?;
    Some(String::from_utf16_lossy(&name[..size as usize]))
}

fn lan_addresses() -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    for ip in up_ipv4() {
        if !ip.is_loopback() && !ip.is_unspecified() && !found.contains(&ip) {
            found.push(ip);
        }
    }
    found.sort_by_key(|ip| rank(*ip));
    found
}

/// The prefixes of the bridges Docker, LXD and libvirt add for their containers and machines,
/// which no phone or TV reaches this machine by.
#[cfg(unix)]
const BRIDGES: [&str; 6] = ["docker", "br-", "veth", "virbr", "lxdbr", "cni"];

/// The IPv4 addresses of the network interfaces that are up, but for container bridges.
#[cfg(unix)]
fn up_ipv4() -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let mut next = list;
    while let Some(entry) = unsafe { next.as_ref() } {
        next = entry.ifa_next;
        let up = entry.ifa_flags & libc::IFF_UP as u32 != 0;
        let Some(address) = (unsafe { entry.ifa_addr.as_ref() }) else { continue };
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
        if !up || i32::from(address.sa_family) != libc::AF_INET || BRIDGES.iter().any(|b| name.starts_with(b)) {
            continue;
        }
        let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        found.push(Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)));
    }
    unsafe { libc::freeifaddrs(list) };
    found
}

/// The IPv4 addresses of the network adapters that are up.
#[cfg(windows)]
fn up_ipv4() -> Vec<Ipv4Addr> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 16 * 1024u32;
    // u64s, so the adapters the call writes there are aligned.
    let mut buffer = vec![0u64; size as usize / 8];
    loop {
        let list = Some(buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>());
        match unsafe { GetAdaptersAddresses(u32::from(AF_INET.0), flags, None, list, &mut size) } {
            code if code == NO_ERROR.0 => break,
            code if code == ERROR_BUFFER_OVERFLOW.0 => buffer.resize(size as usize / 8 + 1, 0),
            _ => return Vec::new(),
        }
    }
    let mut found = Vec::new();
    let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // Each pointer is null or points into `buffer`, which outlives the walk.
    while let Some(entry) = unsafe { adapter.as_ref() } {
        adapter = entry.Next;
        if entry.OperStatus != IfOperStatusUp {
            continue;
        }
        let mut unicast = entry.FirstUnicastAddress;
        while let Some(address) = unsafe { unicast.as_ref() } {
            unicast = address.Next;
            let socket = address.Address.lpSockaddr;
            if unsafe { socket.as_ref() }.is_some_and(|socket| socket.sa_family == AF_INET) {
                let socket = unsafe { &*socket.cast::<SOCKADDR_IN>() };
                found.push(Ipv4Addr::from(u32::from_be(unsafe { socket.sin_addr.S_un.S_addr })));
            }
        }
    }
    found
}

/// Private addresses are the home network. A link-local address is all a machine has without a
/// router, and others are a VPN or a public address.
fn rank(ip: Ipv4Addr) -> u8 {
    if ip.is_private() {
        0
    } else if ip.is_link_local() {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_name_becomes_the_name_bonjour_answers_for() {
        assert_eq!(dot_local("Studio-Mac"), Some("studio-mac.local".into()));
        assert_eq!(dot_local("studio-mac.local\n"), Some("studio-mac.local".into()));
        assert_eq!(dot_local("media.home.arpa"), Some("media.local".into()));
        assert_eq!(dot_local("Living Room Mac"), None, "a name with spaces is not one a browser opens");
        assert_eq!(dot_local(""), None);
        assert_eq!(dot_local("-mac"), None);
    }

    #[test]
    fn the_home_network_comes_before_a_vpn_and_a_bare_link() {
        let mut ips: Vec<Ipv4Addr> =
            ["169.254.3.4", "100.64.0.7", "192.168.1.23"].iter().map(|ip| ip.parse().unwrap()).collect();
        ips.sort_by_key(|ip| rank(*ip));
        assert_eq!(
            ips,
            [
                "192.168.1.23".parse::<Ipv4Addr>().unwrap(),
                "100.64.0.7".parse().unwrap(),
                "169.254.3.4".parse().unwrap()
            ]
        );
    }
}
