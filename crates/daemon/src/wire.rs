//! How torrential reaches its engine. Each request is a POST to the engine's HTTP port with the
//! token the engine wrote in its state folder, and the engine answers in frames: the updates the
//! request streams, then its reply. The request is HTTP/1.0, so the frames follow the head as they
//! are, and the answer ends when the engine closes the connection.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
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
        let partial = crate::fsutil::temporary(path);
        let written = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&partial)?;
            #[cfg(unix)]
            file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            #[cfg(windows)]
            {
                // TODO(W2): an ACL that lets only the file's owner read it.
            }
            file.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
            std::fs::rename(&partial, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        written
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
    Frames(BufReader<TcpStream>),
    /// An answer without frames, by its status, such as 401 for a token the engine does not take.
    Status(u16),
}

/// Sends `request` to the engine at `access` and reads the head of the answer, waiting at most
/// `within` to connect and for each read.
pub fn post(access: &Access, client: Client, request: &Request, within: Duration) -> io::Result<Answer> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, access.port));
    let mut stream = TcpStream::connect_timeout(&address, within)?;
    stream.set_read_timeout(Some(within))?;
    stream.set_write_timeout(Some(within))?;
    let body = serde_json::to_vec(request).map_err(io::Error::other)?;
    let version: String = url::form_urlencoded::byte_serialize(client.version.as_bytes()).collect();
    let head = format!(
        "POST {PATH}?version={version}&wire={} HTTP/1.0\r\nHost: {address}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        client.wire,
        access.token,
        body.len()
    );
    stream.write_all(&[head.as_bytes(), &body].concat())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let status = line.split(' ').nth(1).and_then(|code| code.parse().ok());
    let status = status.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("not HTTP: {line:?}")))?;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
    }
    Ok(if status == 200 { Answer::Frames(reader) } else { Answer::Status(status) })
}

/// Sends `request` and reads its reply, which comes in one frame.
pub fn ask(access: &Access, client: Client, request: &Request) -> io::Result<Response> {
    match post(access, client, request, request.within())? {
        Answer::Frames(mut frames) => read_frame(&mut frames)?.ok_or_else(|| io::ErrorKind::UnexpectedEof.into()),
        Answer::Status(status) => Err(io::Error::other(format!("the engine answered {status}"))),
    }
}
