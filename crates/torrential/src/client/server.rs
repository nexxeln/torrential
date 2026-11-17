//! `torrential server`: the engine as a server on Linux. It starts at boot without a login,
//! listens on the network with nothing paired, and lets paired devices in by any address.

use torrential_core::Error;

use crate::cli::ServerCommand;
use crate::output::Mode;
use torrential_daemon::env::Env;

#[cfg(target_os = "linux")]
pub fn run(command: ServerCommand, env: &Env, mode: Mode) -> Result<u8, Error> {
    let paths = torrential_daemon::paths::Paths::from_process(env)?;
    match command {
        ServerCommand::Install => linux::install(env, &paths, mode),
        ServerCommand::Uninstall => linux::uninstall(env, &paths, mode),
    }
}

/// systemd is what starts a server at boot without a login, so other systems say so.
#[cfg(not(target_os = "linux"))]
pub fn run(_: ServerCommand, _: &Env, _: Mode) -> Result<u8, Error> {
    Err(Error::new(torrential_core::ErrorCode::Usage, "torrential server runs only on Linux")
        .with_hint("on this computer, torrential devices add lets a phone or TV in"))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{IsTerminal, Write};

    use serde::Serialize;
    use torrential_core::protocol::Request;
    use torrential_core::{DeviceList, Error, ErrorCode, PairingOffer};
    use torrential_daemon::config;
    use torrential_daemon::env::Env;
    use torrential_daemon::fsutil::Writes;
    use torrential_daemon::paths::{self, Paths};
    use torrential_daemon::qr::qr;
    use torrential_daemon::service;

    use super::super::{connect, devices, emit};
    use crate::output::{Cell, Doc, DocKind, Mode, Typed};

    torrential_core::words! {
        /// What `server install` and `server uninstall` did.
        enum Change {
            Installed = "installed",
            AlreadyInstalled = "already_installed",
            Uninstalled = "uninstalled",
            NotInstalled = "not_installed",
        }
    }

    #[derive(Serialize)]
    struct Report {
        result: Change,
        /// The systemd user unit that runs the engine.
        unit: String,
        /// Whether systemd keeps this user's services running without a login.
        lingering: bool,
        /// The open pairing offer, after an install.
        offer: Option<PairingOffer>,
    }

    pub fn install(env: &Env, paths: &Paths, mode: Mode) -> Result<u8, Error> {
        let uid = paths::uid();
        let lingered = service::lingering(env, uid);
        if !lingered {
            if mode == Mode::Human {
                eprintln!(
                    "Turning on lingering, so systemd runs the engine without a login. sudo may ask for your password."
                );
            }
            service::linger(env, uid, true, mode == Mode::Human && std::io::stdin().is_terminal())?;
            paths.make_state().and_then(|()| std::fs::write(paths.server_lingers(), b"")).map_err(|e| {
                Error::new(ErrorCode::StorageError, format!("cannot write {}: {e}", paths.state.display()))
            })?;
        }
        let (_, wrote) = config::save_server(paths, true, &Writes::new(env, "torrential server install".into()))?;
        // The engine reads config.yaml as it starts, so the one running now makes way.
        if let Some(conn) = connect::running(paths, env)? {
            connect::stop(conn, connect::Stop::Asked)?;
        }
        let unit = service::install(env, paths)?;
        let mut conn = connect::poll(connect::BACK_WITHIN, || connect::existing(paths, env))?.ok_or_else(|| {
            Error::new(ErrorCode::DaemonUnavailable, "systemd did not start the engine")
                .with_hint(format!("see journalctl --user -u torrential and {}", paths.log().display()))
        })?;
        let list: DeviceList = conn.ask(&Request::AddDevice)?;
        let changed = !lingered || wrote || unit.result == service::Change::Installed;
        let result = if changed { Change::Installed } else { Change::AlreadyInstalled };
        let report =
            Report { result, unit: unit.unit.display().to_string(), lingering: true, offer: list.offer.clone() };
        let mut doc = server_doc(&report);
        if let Some(offer) = &list.offer {
            if mode == Mode::Human {
                let mut stdout = anstream::stdout().lock();
                let _ = stdout.write_all(devices::blocks(&qr(&offer.url)).as_bytes());
            }
            let pairing = devices::offer_doc(offer, list.unreachable.as_deref(), torrential_core::time::now());
            if let crate::output::Body::Fields { fields, .. } = pairing.body
                && let crate::output::Body::Fields { fields: all, .. } = &mut doc.body
            {
                all.extend(fields);
            }
            doc.summary.extend(pairing.summary);
        }
        doc.next = Some("torrential devices".into());
        Ok(emit(mode, &doc, &Typed { kind: DocKind::Server, body: &report }))
    }

    pub fn uninstall(env: &Env, paths: &Paths, mode: Mode) -> Result<u8, Error> {
        let uid = paths::uid();
        let lingers = paths.server_lingers().exists();
        if lingers {
            service::linger(env, uid, false, mode == Mode::Human && std::io::stdin().is_terminal())?;
            let _ = std::fs::remove_file(paths.server_lingers());
        }
        let (_, wrote) = config::save_server(paths, false, &Writes::new(env, "torrential server uninstall".into()))?;
        // The next engine, which the next command or login starts, listens on the network only
        // while a device is paired.
        if wrote && let Some(conn) = connect::running(paths, env)? {
            connect::stop(conn, connect::Stop::Asked)?;
        }
        let home = env.home().ok_or_else(|| Error::new(ErrorCode::Usage, "HOME is not set"))?;
        let unit = service::Manager::Systemd.unit_path(home, paths);
        let result = if lingers || wrote { Change::Uninstalled } else { Change::NotInstalled };
        let report =
            Report { result, unit: unit.display().to_string(), lingering: service::lingering(env, uid), offer: None };
        Ok(emit(mode, &server_doc(&report), &Typed { kind: DocKind::Server, body: &report }))
    }

    fn server_doc(report: &Report) -> Doc {
        let fields = vec![
            ("result", Cell::new(report.result.as_str())),
            ("unit", Cell::new(&report.unit)),
            ("linger", Cell::new(if report.lingering { "on" } else { "off" })),
        ];
        let summary = match report.result {
            Change::Installed | Change::AlreadyInstalled => {
                vec!["the engine starts at boot without a login, and paired devices reach it from anywhere".into()]
            }
            Change::Uninstalled | Change::NotInstalled => vec![
                "the engine starts at login or with the next command, and listens on the network only while a device is paired"
                    .into(),
            ],
        };
        Doc { body: crate::output::Body::Fields { title: Vec::new(), fields }, summary, next: None }
    }
}
