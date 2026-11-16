//! The Unix socket that torrential 0.5 reached its engine through, kept only so a 0.5 torrential
//! can update itself to this release. It is deleted in 0.7.
//!
//! Before 0.5 installs a release, it starts the release's engine in a scratch home, asks it at this
//! socket for a hello and a status, and then asks it to stop. This engine refuses every request as
//! one from an older torrential, which 0.5 takes as an engine that runs, and stops when asked. The
//! other way, a command from this release asks a 0.5 engine that the update left running to make
//! way, as a newer 0.5 client did.
// Remove in 0.7

use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use torrential_core::protocol::{decode_frame, encode_frame, frame_len, read_frame};
use torrential_core::{Error, ErrorCode};

use super::ctx::OwnerHandle;
use super::owner::Command;
use crate::log::log;
use crate::paths::{self, Paths};

/// The protocol version a 0.5.2 client says it speaks.
const WIRE: u32 = 10;
/// How long a 0.5 engine has to answer the hello.
const HELLO_WITHIN: Duration = Duration::from_secs(5);
/// How long a 0.5 engine has to save every download and answer the stop.
const STOP_WITHIN: Duration = Duration::from_secs(20);

/// The requests of 0.5's protocol that the bridge sends or tells apart.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "snake_case", tag = "op")]
enum Request {
    Hello {
        client_version: String,
        wire: u32,
    },
    /// Without `asked`, a newer client's request that the engine make way for its version, which a
    /// 0.5 engine turns down while a film plays.
    Shutdown {
        #[serde(default)]
        asked: bool,
    },
    #[serde(other)]
    Other,
}

/// The replies of 0.5's protocol that the bridge sends or reads.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "snake_case", tag = "type")]
enum Response {
    Hello { pid: u32 },
    Ok,
    Error { error: Error },
}

fn socket(paths: &Paths) -> PathBuf {
    paths.runtime.join("engine.sock")
}

/// Answers 0.5 torrentials at their socket until the engine stops. An engine that cannot listen
/// there runs anyway, and logs why.
pub(crate) async fn answer(paths: Paths, owner: OwnerHandle, version: String) {
    let path = socket(&paths);
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => Bound { listener, path },
        Err(e) => {
            log(&format!("cannot listen on {} for torrential 0.5: {e}", path.display()));
            return;
        }
    };
    let _ = std::fs::set_permissions(&listener.path, std::fs::Permissions::from_mode(0o600));
    let refusal = Error::new(
        ErrorCode::DaemonVersion,
        format!("the engine runs the newer torrential {version}, so restart this torrential"),
    );
    let uid = paths::uid();
    while let Ok((stream, _)) = listener.listener.accept().await {
        if stream.peer_cred().is_ok_and(|cred| cred.uid() == uid) {
            tokio::spawn(connection(stream, owner.clone(), refusal.clone()));
        }
    }
}

/// The listener, whose socket file goes when the engine stops.
struct Bound {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for Bound {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn connection(mut stream: UnixStream, owner: OwnerHandle, refusal: Error) -> io::Result<()> {
    while let Some(request) = read(&mut stream).await? {
        let response = match request {
            Request::Shutdown { .. } => {
                let (reply, done) = oneshot::channel();
                owner.tell(Command::Shutdown { reply });
                let _ = done.await;
                Response::Ok
            }
            _ => Response::Error { error: refusal.clone() },
        };
        stream.write_all(&encode(&response)?).await?;
    }
    Ok(())
}

/// Reads one frame as 0.5's `wire::read_async` does, or `None` when the client hung up.
async fn read(stream: &mut UnixStream) -> io::Result<Option<Request>> {
    let mut header = [0; 4];
    if stream.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut header[1..]).await?;
    let mut body = vec![0; frame_len(header)?];
    stream.read_exact(&mut body).await?;
    decode_frame(&body).map(Some)
}

fn encode(message: &impl Serialize) -> io::Result<Vec<u8>> {
    encode_frame(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Asks a 0.5 engine that still runs after the update to this release to stop for `version`, and
/// returns its process ID. `None` when no 0.5 engine answers at its socket. The engine turns the
/// request down while a film plays, and the error says when it switches.
pub fn make_way(paths: &Paths, version: &str) -> Result<Option<u32>, Error> {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket(paths)) else { return Ok(None) };
    let hello = Request::Hello { client_version: version.to_string(), wire: WIRE };
    let Ok(Some(Response::Hello { pid })) = ask(&mut stream, &hello, HELLO_WITHIN) else { return Ok(None) };
    match ask(&mut stream, &Request::Shutdown { asked: false }, STOP_WITHIN) {
        Ok(Some(Response::Error { error })) => Err(error),
        _ => Ok(Some(pid)),
    }
}

fn ask(
    stream: &mut std::os::unix::net::UnixStream,
    request: &Request,
    within: Duration,
) -> io::Result<Option<Response>> {
    stream.set_read_timeout(Some(within))?;
    stream.write_all(&encode(request)?)?;
    read_frame(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames torrential 0.5.2 sends and expects, byte for byte.
    #[test]
    fn frames_match_what_torrential_0_5_sends_and_reads() {
        let frame = |json: &str| [&(json.len() as u32).to_be_bytes()[..], json.as_bytes()].concat();
        let decoded = |json: &str| decode_frame::<Request>(json.as_bytes()).unwrap();
        let refusal = Response::Error { error: Error::new(ErrorCode::DaemonVersion, "newer") };

        assert_eq!(
            decoded(r#"{"op":"hello","client_version":"0.5.2","wire":10}"#),
            Request::Hello { client_version: "0.5.2".into(), wire: 10 }
        );
        assert_eq!(decoded(r#"{"op":"status","refs":[]}"#), Request::Other);
        assert_eq!(decoded(r#"{"op":"shutdown","asked":true}"#), Request::Shutdown { asked: true });
        assert_eq!(decoded(r#"{"op":"shutdown"}"#), Request::Shutdown { asked: false });
        assert_eq!(
            encode(&refusal).unwrap(),
            frame(r#"{"type":"error","error":{"code":"daemon_version","message":"newer"}}"#)
        );
        assert_eq!(encode(&Response::Ok).unwrap(), frame(r#"{"type":"ok"}"#));
        assert_eq!(encode(&Request::Shutdown { asked: false }).unwrap(), frame(r#"{"op":"shutdown","asked":false}"#));
        assert_eq!(
            decode_frame::<Response>(
                br#"{"type":"hello","engine_version":"0.5.2","wire":10,"pid":42,"http_port":47919}"#
            )
            .unwrap(),
            Response::Hello { pid: 42 }
        );
    }
}
