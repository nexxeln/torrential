//! `torrential connect`: pairs this torrential with an engine on another machine as a device, the
//! way a phone pairs, and chooses which engine every command drives.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use torrential_core::{Device, Error, ErrorCode};

use super::{connect, emit};
use crate::cli::ConnectArgs;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};
use torrential_daemon::daemon::devices::COOKIE;
use torrential_daemon::env::Env;
use torrential_daemon::paths::Paths;
use torrential_daemon::wire::{self, Address};

/// How long pairing waits for the engine to connect and answer.
const PAIR_WITHIN: Duration = Duration::from_secs(15);

/// The engines on other machines this torrential paired with, and the one it drives. Each token
/// lets requests in, so only this user can read the file.
#[derive(Default, Serialize, Deserialize)]
pub struct Remotes {
    /// The engine every command drives, or `None` for this machine's.
    using: Option<Address>,
    paired: Vec<Remote>,
}

/// An engine on another machine, and the device token it gave this machine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Remote {
    pub address: Address,
    pub token: String,
    /// This machine's device ID on that engine.
    pub device: String,
}

impl Remotes {
    fn read(paths: &Paths) -> Result<Self, Error> {
        let file = paths.remotes();
        let unreadable = |e: &dyn std::fmt::Display| {
            Error::new(ErrorCode::StateCorrupt, format!("cannot read {}: {e}", file.display()))
                .with_hint("run torrential connect --local, or connect to the engine again")
        };
        match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| unreadable(&e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(unreadable(&e)),
        }
    }

    fn write(&self, paths: &Paths) -> Result<(), Error> {
        let file = paths.remotes();
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| Error::internal(e.to_string()))?;
        std::fs::create_dir_all(&paths.state)
            .and_then(|()| torrential_daemon::fsutil::write_private(&file, &bytes))
            .map_err(|e| Error::new(ErrorCode::StateCorrupt, format!("cannot write {}: {e}", file.display())))
    }

    /// The engine on another machine every command drives, or `None` for this machine's.
    pub fn using(paths: &Paths) -> Result<Option<Remote>, Error> {
        let remotes = Self::read(paths)?;
        let Some(address) = &remotes.using else { return Ok(None) };
        Ok(remotes.paired.into_iter().find(|remote| &remote.address == address))
    }
}

/// Refuses what only this machine's engine can do while this torrential drives another: `why`,
/// and what to do `instead`.
pub fn local_only(paths: &Paths, (why, instead): (&str, &str)) -> Result<(), Error> {
    match Remotes::using(paths)? {
        Some(chosen) => Err(Error::new(
            ErrorCode::LocalOnly,
            format!("{why}, and this torrential drives the engine at {}", chosen.address),
        )
        .with_hint(instead)),
        None => Ok(()),
    }
}

#[derive(Serialize)]
#[serde(tag = "engine", rename_all = "snake_case")]
enum Connected {
    Local,
    Remote { address: Address, device: String, version: String },
}

pub fn run(args: ConnectArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let mut remotes = Remotes::read(&paths)?;
    let Some(typed) = args.address.filter(|_| !args.local) else {
        remotes.using = None;
        remotes.write(&paths)?;
        return Ok(shown(mode, &Connected::Local));
    };
    let (address, linked) = Address::parse(&typed).map_err(|message| {
        Error::new(ErrorCode::Usage, message).with_hint("pass a URL, a host:port, or a machine's name")
    })?;
    if is_this_machine(&address) {
        return Err(Error::new(ErrorCode::Usage, format!("{address} is this machine"))
            .with_hint("run torrential connect --local to use this machine's engine"));
    }
    let saved = remotes.paired.iter().find(|remote| remote.address == address).cloned();
    let (remote, conn) = match (args.code.or(linked), saved) {
        (None, Some(saved)) => match connect::remote(&saved, env) {
            Err(error) if error.code == ErrorCode::NotPaired => paired(&address, &code(&address, mode)?, env)?,
            answered => (saved, answered?),
        },
        (Some(code), _) => paired(&address, &code, env)?,
        (None, None) => paired(&address, &code(&address, mode)?, env)?,
    };
    remotes.paired.retain(|other| other.address != address);
    remotes.paired.push(remote.clone());
    remotes.using = Some(address);
    remotes.write(&paths)?;
    Ok(shown(mode, &Connected::Remote { address: remote.address, device: remote.device, version: conn.engine_version }))
}

/// Pairs with the engine at `address` and checks that it answers this torrential's requests.
fn paired(address: &Address, code: &str, env: &Env) -> Result<(Remote, connect::Conn), Error> {
    let remote = pair(address, code)?;
    let conn = connect::remote(&remote, env)?;
    Ok((remote, conn))
}

/// Whether `address` names this machine by its loopback address, where the engine counts every
/// request as this machine's own.
fn is_this_machine(address: &Address) -> bool {
    address.host == "localhost" || address.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The pairing code, which the person types in a terminal.
fn code(address: &Address, mode: Mode) -> Result<String, Error> {
    let hint = format!("run torrential devices add on that machine, then torrential connect {address} --code <code>");
    let needed = || {
        Error::new(ErrorCode::NotPaired, format!("pairing with the engine at {address} needs a code"))
            .with_hint(hint.clone())
    };
    if mode != Mode::Human || !std::io::stdin().is_terminal() {
        return Err(needed());
    }
    eprint!("Run torrential devices add on the machine at {address}, then type the code it shows: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| Error::internal(e.to_string()))?;
    let code = line.trim().to_string();
    if code.is_empty() { Err(needed()) } else { Ok(code) }
}

/// Pairs as a phone does: posts the code to the engine's pairing route, which answers with the
/// device it added and its token in a cookie.
fn pair(address: &Address, code: &str) -> Result<Remote, Error> {
    let body = serde_json::json!({ "code": code }).to_string();
    let headers = [("Content-Type", "application/json")];
    let unreachable = |e: std::io::Error| connect::Engine::Remote(address.clone()).lost(e);
    let (head, mut reader) =
        wire::exchange(address, "POST", "/api/pair", &headers, body.as_bytes(), PAIR_WITHIN).map_err(unreachable)?;
    let mut answer = Vec::new();
    reader.read_to_end(&mut answer).map_err(unreachable)?;
    #[derive(Deserialize)]
    struct Paired {
        device: Device,
    }
    #[derive(Deserialize)]
    struct Failed {
        error: Error,
    }
    let token = head.header("set-cookie").and_then(|cookie| {
        let pair = cookie.split(';').next()?.trim();
        Some(pair.strip_prefix(COOKIE)?.strip_prefix('=')?.to_string())
    });
    match (head.status, token, serde_json::from_slice::<Paired>(&answer)) {
        (200, Some(token), Ok(Paired { device })) => Ok(Remote { address: address.clone(), token, device: device.id }),
        _ => Err(match serde_json::from_slice::<Failed>(&answer) {
            Ok(Failed { error }) => error,
            Err(_) => {
                let text = String::from_utf8_lossy(&answer);
                let why = text.lines().next().filter(|line| !line.is_empty()).unwrap_or("no reason given");
                Error::new(
                    ErrorCode::NotPaired,
                    format!("the engine at {address} refused to pair with HTTP {}: {why}", head.status),
                )
            }
        }),
    }
}

fn shown(mode: Mode, connected: &Connected) -> u8 {
    let doc = match connected {
        Connected::Local => Doc {
            body: Body::Fields {
                title: Vec::new(),
                fields: vec![("engine", Cell::styled("this machine's", Token::Mint))],
            },
            summary: vec!["every command drives the engine on this machine again".to_string()],
            next: Some("torrential engine status".to_string()),
        },
        Connected::Remote { address, device, version } => Doc {
            body: Body::Fields {
                title: Vec::new(),
                fields: vec![
                    ("engine", Cell::styled(address.to_string(), Token::Accent)),
                    ("version", Cell::new(version)),
                    ("device", Cell::new(device)),
                ],
            },
            summary: vec!["every command drives that engine now; torrential connect --local switches back".to_string()],
            next: Some("torrential status".to_string()),
        },
    };
    emit(mode, &doc, &Typed { kind: DocKind::Connect, body: connected })
}
