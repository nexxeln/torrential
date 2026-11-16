//! `torrential engine`: whether the engine runs, stopping it, and its login service.

use serde::Serialize;
use torrential_core::Error;
use torrential_core::Job;
use torrential_core::protocol::Request;

use super::{connect, emit, launch};
use crate::cli::DaemonCommand;
use crate::output::theme::Token;
use crate::output::{self, Body, Cell, Doc, DocKind, Mode, Typed};
use torrential_daemon::env::Env;
use torrential_daemon::paths::Paths;
use torrential_daemon::service;

pub fn run(command: DaemonCommand, env: &Env, mode: Mode) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    match command {
        DaemonCommand::Status => {
            let report = match connect::running(&paths, env)? {
                Some(mut conn) => {
                    let jobs = conn.ask::<Vec<Job>>(&Request::Status { refs: Vec::new() });
                    let downloads = jobs.ok().map(|jobs| jobs.len());
                    DaemonReport {
                        engine: "running",
                        pid: Some(conn.pid),
                        version: Some(conn.engine_version.clone()),
                        address: Some(format!("http://127.0.0.1:{}", conn.http_port)),
                        log: paths.log().display().to_string(),
                        downloads,
                    }
                }
                None => DaemonReport {
                    engine: "stopped",
                    pid: None,
                    version: None,
                    address: None,
                    log: paths.log().display().to_string(),
                    downloads: None,
                },
            };
            Ok(emit(mode, &daemon_status(&report), &Typed { kind: DocKind::Daemon, body: &report }))
        }
        DaemonCommand::Stop => {
            let pid = connect::stop_asked(&paths, env)?;
            let result = if pid.is_some() { Stop::Stopped } else { Stop::AlreadyStopped };
            #[derive(Serialize)]
            struct Stopped {
                result: Stop,
                pid: Option<u32>,
            }
            let mut fields = vec![("result", output::Cell::new(result.as_str()))];
            if let Some(pid) = pid {
                fields.push(("pid", output::Cell::new(pid.to_string())));
            }
            let doc = output::Doc {
                body: output::Body::Fields { title: Vec::new(), fields },
                summary: Vec::new(),
                next: None,
            };
            Ok(emit(mode, &doc, &Typed { kind: DocKind::DaemonStop, body: Stopped { result, pid } }))
        }
        DaemonCommand::Install => {
            if let Some(conn) = connect::running(&paths, env)? {
                connect::stop(conn, connect::Stop::Asked)?;
            }
            service_report(service::install(env, &paths)?, mode)
        }
        DaemonCommand::Uninstall => service_report(service::uninstall(env, &paths)?, mode),
        DaemonCommand::Run { .. } => unreachable!("main runs the engine itself"),
        DaemonCommand::Launch => Ok(launch::launch_stage(env)),
        DaemonCommand::Interrupt { .. } => unreachable!("main sends the interrupt itself"),
    }
}

torrential_core::words! {
    enum Stop {
        Stopped = "stopped",
        AlreadyStopped = "already_stopped",
    }
}

fn service_report(report: service::Report, mode: Mode) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Payload {
        result: service::Change,
        manager: &'static str,
        unit: String,
    }
    let unit = report.unit.display().to_string();
    let fields = vec![
        ("result", output::Cell::new(report.result.as_str())),
        ("manager", output::Cell::new(report.manager.name())),
        ("unit", output::Cell::new(unit.clone())),
    ];
    let next = matches!(report.result, service::Change::Installed | service::Change::AlreadyInstalled)
        .then(|| "torrential engine status".to_string());
    let doc = output::Doc { body: output::Body::Fields { title: Vec::new(), fields }, summary: Vec::new(), next };
    let payload = Payload { result: report.result, manager: report.manager.name(), unit };
    Ok(emit(mode, &doc, &Typed { kind: DocKind::DaemonService, body: payload }))
}

#[derive(Clone, PartialEq, Debug, serde::Serialize)]
pub struct DaemonReport {
    pub engine: &'static str,
    pub pid: Option<u32>,
    pub version: Option<String>,
    /// Where the engine serves the web app and this user's torrential.
    pub address: Option<String>,
    pub log: String,
    pub downloads: Option<usize>,
}

pub fn daemon_status(report: &DaemonReport) -> Doc {
    let running = report.pid.is_some();
    let mut fields = vec![("engine", Cell::styled(report.engine, if running { Token::Mint } else { Token::Muted }))];
    if let Some(pid) = report.pid {
        fields.push(("pid", Cell::new(pid.to_string())));
    }
    if let Some(version) = &report.version {
        fields.push(("version", Cell::new(version.clone())));
    }
    if let Some(downloads) = report.downloads {
        fields.push(("downloads", Cell::new(downloads.to_string())));
    }
    if let Some(address) = &report.address {
        fields.push(("address", Cell::new(address.clone())));
    }
    fields.push(("log", Cell::new(report.log.clone())));
    Doc {
        body: Body::Fields { title: Vec::new(), fields },
        summary: Vec::new(),
        next: Some("torrential status".to_string()),
    }
}
