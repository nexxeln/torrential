//! The port each listen socket asks libtorrent for. libtorrent binds a TCP and a UDP socket on one
//! port per address and moves to another port only when a bind says the port is in use. Windows
//! says a port is refused instead when another program holds it for exclusive use, or when the
//! system reserves it, as Hyper-V does, and libtorrent then listens nowhere on that address.

use std::io;
use std::net::SocketAddr;

use socket2::{Domain, Socket, Type};

/// How many ports the OS offers before a search gives up and leaves the port to libtorrent.
const TRIES: usize = 64;

/// `address`, or `address` on a port the OS chooses where TCP and UDP both bind as libtorrent binds
/// them, when its own port is 0 or either bind finds it taken or refused. Any other failure, such
/// as an address this machine does not have, leaves `address` for libtorrent to report.
pub(crate) fn bindable(address: SocketAddr) -> SocketAddr {
    if address.port() != 0 {
        match both(address) {
            Err(e) if taken(&e) => {}
            _ => return address,
        }
    }
    free(address).unwrap_or(address)
}

/// `address`'s IP on a port the OS offers for TCP where UDP binds too.
fn free(address: SocketAddr) -> Option<SocketAddr> {
    // Held until the search ends, so the OS offers a new port each time.
    let mut refused = Vec::new();
    for _ in 0..TRIES {
        let tcp = bound(Type::STREAM, SocketAddr::new(address.ip(), 0)).ok()?;
        let offered = tcp.local_addr().ok()?.as_socket()?;
        match bound(Type::DGRAM, offered) {
            Ok(_) => return Some(offered),
            Err(e) if taken(&e) => refused.push(tcp),
            Err(_) => return None,
        }
    }
    None
}

fn both(address: SocketAddr) -> io::Result<()> {
    bound(Type::STREAM, address)?;
    bound(Type::DGRAM, address).map(drop)
}

fn taken(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied)
}

/// A socket of `kind` on `address`, with the options libtorrent gives its listen sockets.
fn bound(kind: Type, address: SocketAddr) -> io::Result<Socket> {
    let socket = Socket::new(Domain::for_address(address), kind, None)?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    #[cfg(windows)]
    exclusive(&socket)?;
    #[cfg(unix)]
    if kind == Type::STREAM {
        socket.set_reuse_address(true)?;
    }
    socket.bind(&address.into())?;
    Ok(socket)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn exclusive(socket: &Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SO_EXCLUSIVEADDRUSE, SOCKET, SOL_SOCKET, setsockopt};
    let on = 1i32.to_ne_bytes();
    // The socket is open, and `on` outlives the call.
    if unsafe { setsockopt(SOCKET(socket.as_raw_socket() as usize), SOL_SOCKET, SO_EXCLUSIVEADDRUSE, Some(&on)) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
