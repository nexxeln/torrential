//! How torrential reaches an engine. Each request is a POST to the engine's HTTP port with a token:
//! the one the engine wrote in its state folder for this user's torrential, or the device token
//! `torrential connect` got by pairing with an engine on another machine. The engine answers in
//! frames: the updates the request streams, then its reply. The request is HTTP/1.0, so the
//! frames follow the head as they are, and the answer ends when the engine closes the connection.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use torrential_core::protocol::{Request, Response, read_frame};

/// Where requests go on the engine's HTTP port.
pub const PATH: &str = "/engine";
/// The type of an answer made of frames.
pub const FRAMES: &str = "application/x-torrential-frames";

/// What the engine writes in its state folder for this user's torrential: where it listens, and
/// the token that lets a request in.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Access {
    /// The engine's HTTP port on 127.0.0.1.
    pub port: u16,
    /// 32 random bytes as 64 hex digits, new each time the engine starts.
    pub token: String,
}

impl Access {
    /// What an engine left at `path`, or `None` when none did.
    pub fn read(path: &Path) -> io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes this to `path`, so that only this user can read it.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        crate::fsutil::write_private(path, &serde_json::to_vec(self).map_err(io::Error::other)?)
    }

    /// Where this machine's engine answers.
    pub fn address(&self) -> Address {
        Address::local(self.port)
    }
}

/// Where an engine answers: `http://127.0.0.1:47919` for this machine's, or another machine's
/// address. An `https` address reaches an engine behind a proxy such as Caddy, which the engine
/// itself never needs.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Address {
    pub tls: bool,
    /// A name, an IPv4 address, or an IPv6 address without brackets.
    pub host: String,
    pub port: u16,
}

impl Address {
    pub fn local(port: u16) -> Self {
        Self { tls: false, host: "127.0.0.1".to_string(), port }
    }

    /// Reads what a person types for an engine on another machine: a URL or a pairing link such
    /// as `http://nas.local:47919/pair#123456`, a `host:port`, or a name alone, such as a Tailscale
    /// machine's, for the engine's own port. A pairing link's code comes back with it.
    pub fn parse(text: &str) -> Result<(Self, Option<String>), String> {
        let text = text.trim();
        let url = if text.contains("://") { text.to_string() } else { format!("http://{text}") };
        let url = url::Url::parse(&url).map_err(|e| format!("{text} is not an address: {e}"))?;
        let tls = match url.scheme() {
            "http" => false,
            "https" => true,
            other => return Err(format!("{text} is not an address: torrential speaks http and https, not {other}")),
        };
        let host = match url.host() {
            Some(url::Host::Domain(name)) => name.to_string(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            None => return Err(format!("{text} is not an address: it names no machine")),
        };
        let port = url.port().unwrap_or(if tls { 443 } else { crate::daemon::http::PORT });
        let code = url.fragment().map(|code| code.trim().to_string()).filter(|code| !code.is_empty());
        Ok((Self { tls, host, port }, code))
    }

    /// `host:port`, as a `Host` header names it.
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("{host}:{}", self.port)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.tls, self.port) {
            (true, 443) => write!(f, "https://{}", self.authority().trim_end_matches(":443")),
            (true, _) => write!(f, "https://{}", self.authority()),
            (false, _) => write!(f, "http://{}", self.authority()),
        }
    }
}

impl From<Address> for String {
    fn from(address: Address) -> Self {
        address.to_string()
    }
}

impl TryFrom<String> for Address {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        Self::parse(&text).map(|(address, _)| address)
    }
}

/// Who sends a request: the torrential release, and the protocol it speaks.
#[derive(Clone, Copy, Debug)]
pub struct Client<'a> {
    pub version: &'a str,
    pub wire: u32,
}

/// How the engine answered a request.
pub enum Answer {
    /// The frames of the reply, for `read_frame`.
    Frames(BufReader<Stream>),
    /// An answer without frames, by its status, such as 401 for a token the engine does not take.
    Status(u16),
}

/// A connection to an engine: plain, or TLS for an `https` address.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    /// How long a read waits, or no limit.
    pub fn set_read_timeout(&self, within: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.set_read_timeout(within),
            Self::Tls(tls) => tls.sock.set_read_timeout(within),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.read(buf),
            Self::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.write(buf),
            Self::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.flush(),
            Self::Tls(tls) => tls.flush(),
        }
    }
}

/// The status and headers of an answer, with lowercase header names.
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

/// What torrential tells an engine it is. A device that pairs is named after its system.
const AGENT: &str = concat!("torrential/", env!("CARGO_PKG_VERSION"));
#[cfg(target_os = "macos")]
const SYSTEM: &str = "Macintosh";
#[cfg(windows)]
const SYSTEM: &str = "Windows";
#[cfg(not(any(target_os = "macos", windows)))]
const SYSTEM: &str = "Linux";

/// Sends `method target` with `headers` and `body` to the engine at `address`, waiting at most
/// `within` to connect and for each read and write, and reads the head of the answer. The rest of
/// the answer follows in the reader.
pub fn exchange(
    address: &Address,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    within: Duration,
) -> io::Result<(Head, BufReader<Stream>)> {
    let mut stream = open(address, within)?;
    let mut head =
        format!("{method} {target} HTTP/1.0\r\nHost: {}\r\nUser-Agent: {AGENT} ({SYSTEM})\r\n", address.authority());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(&[head.as_bytes(), body].concat())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let status = line.split(' ').nth(1).and_then(|code| code.parse().ok());
    let status = status.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("not HTTP: {line:?}")))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Ok((Head { status, headers }, reader))
}

/// A connection to `address`, trying each of its addresses in turn.
fn open(address: &Address, within: Duration) -> io::Result<Stream> {
    let mut failed = io::Error::new(io::ErrorKind::NotFound, format!("{} names no address", address.host));
    for ip in (address.host.as_str(), address.port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&ip, within) {
            Ok(tcp) => {
                tcp.set_read_timeout(Some(within))?;
                tcp.set_write_timeout(Some(within))?;
                tcp.set_nodelay(true)?;
                if !address.tls {
                    return Ok(Stream::Plain(tcp));
                }
                let name = rustls::pki_types::ServerName::try_from(address.host.clone())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                let tls = rustls::ClientConnection::new(tls()?, name).map_err(io::Error::other)?;
                return Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(tls, tcp))));
            }
            Err(e) => failed = e,
        }
    }
    Err(failed)
}

/// TLS that trusts what the system trusts.
fn tls() -> io::Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        use rustls_platform_verifier::BuilderVerifierExt;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(config.with_no_client_auth()))
    });
    config.clone().map_err(io::Error::other)
}

/// Sends `request` to the engine at `address` with `token`, and reads the head of the answer,
/// waiting at most `within` to connect and for each read.
pub fn post(address: &Address, token: &str, client: Client, request: &Request, within: Duration) -> io::Result<Answer> {
    let body = serde_json::to_vec(request).map_err(io::Error::other)?;
    let version: String = url::form_urlencoded::byte_serialize(client.version.as_bytes()).collect();
    let target = format!("{PATH}?version={version}&wire={}", client.wire);
    let bearer = format!("Bearer {token}");
    let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
    let (head, reader) = exchange(address, "POST", &target, &headers, &body, within)?;
    Ok(if head.status == 200 { Answer::Frames(reader) } else { Answer::Status(head.status) })
}

/// Sends `request` to this machine's engine and reads its reply, which comes in one frame.
pub fn ask(access: &Access, client: Client, request: &Request) -> io::Result<Response> {
    match post(&access.address(), &access.token, client, request, request.within())? {
        Answer::Frames(mut frames) => read_frame(&mut frames)?.ok_or_else(|| io::ErrorKind::UnexpectedEof.into()),
        Answer::Status(status) => Err(io::Error::other(format!("the engine answered {status}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_read_from_a_pairing_link_a_url_a_host_and_port_or_a_name() {
        let read = |text: &str| Address::parse(text).map(|(address, code)| (address.to_string(), code));
        let cases = [
            ("http://nas.local:47919/pair#123456", "http://nas.local:47919", Some("123456")),
            ("nas.local:47919", "http://nas.local:47919", None),
            ("  NAS.local  ", "http://nas.local:47919", None),
            ("my-server", "http://my-server:47919", None),
            ("my-server.tail1234.ts.net", "http://my-server.tail1234.ts.net:47919", None),
            ("100.101.102.103:8080", "http://100.101.102.103:8080", None),
            ("[fd7a:115c:a1e0::1]:47919", "http://[fd7a:115c:a1e0::1]:47919", None),
            ("https://tv.example.com", "https://tv.example.com", None),
            ("https://tv.example.com:8443/", "https://tv.example.com:8443", None),
        ];
        for (text, address, code) in cases {
            assert_eq!(read(text), Ok((address.to_string(), code.map(String::from))), "{text}");
        }
        for wrong in ["ftp://nas.local", "http://", "nas.local:99999"] {
            assert!(read(wrong).is_err(), "{wrong}");
        }
        let (tailnet, _) = Address::parse("my-server.tail1234.ts.net").unwrap();
        assert_eq!(serde_json::to_string(&tailnet).unwrap(), r#""http://my-server.tail1234.ts.net:47919""#);
        assert_eq!(serde_json::from_str::<Address>(r#""http://my-server.tail1234.ts.net:47919""#).unwrap(), tailnet);
        assert_eq!(Address::local(47919).authority(), "127.0.0.1:47919");
    }

    #[cfg(windows)]
    #[test]
    fn only_the_token_files_owner_can_open_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine-token");
        let written = Access { port: 47919, token: "ab".repeat(32) };
        written.write(&path).unwrap();
        let listed = std::process::Command::new("icacls").arg(&path).output().unwrap();
        let listed = String::from_utf8_lossy(&listed.stdout);
        // icacls prints one access entry a line, the first after the file's path.
        let entries: Vec<&str> = listed.lines().filter(|line| line.contains(":(")).collect();
        assert_eq!(entries.len(), 1, "{listed}");
        assert!(entries[0].ends_with("OWNER RIGHTS:(F)"), "{listed}");
        assert_eq!(Access::read(&path).unwrap(), Some(written));
    }
}
