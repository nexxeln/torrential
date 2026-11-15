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

/// Avahi answers for the host name with `.local` after it.
#[cfg(not(target_os = "macos"))]
fn local_name() -> Option<String> {
    dot_local(&host_name()?)
}

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

fn lan_addresses() -> Vec<Ipv4Addr> {
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
        if !up || i32::from(address.sa_family) != libc::AF_INET {
            continue;
        }
        let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        if !ip.is_loopback() && !ip.is_unspecified() && !found.contains(&ip) {
            found.push(ip);
        }
    }
    unsafe { libc::freeifaddrs(list) };
    found.sort_by_key(|ip| rank(*ip));
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

    #[test]
    fn this_machine_has_a_name_and_reads_its_addresses() {
        let names = Names::read();
        assert!(names.local.as_deref().is_none_or(|name| name.ends_with(".local")), "{names:?}");
        assert!(names.lan.iter().all(|ip| !ip.is_loopback()), "{names:?}");
    }
}
