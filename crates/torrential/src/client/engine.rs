//! `torrential engine`: whether the engine runs, stopping it, and its login service.

use serde::Serialize;
use torrential_core::Error;
use torrential_core::protocol::{Request, Response};

use super::{connect, emit, launch};
use crate::cli::DaemonCommand;
use crate::env::Env;
use crate::output::theme::Token;
use crate::output::{self, Body, Cell, Doc, Mode, Typed};
use crate::paths::Paths;
use crate::service;

pub fn run(command: DaemonCommand, env: &Env, mode: Mode) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    match command {
        DaemonCommand::Status => {
            let report = match connect::running(&paths, env)? {
                Some(mut conn) => {
                    let downloads = match conn.call(&Request::Status { refs: Vec::new() }) {
                        Ok(Response::Jobs { jobs }) => Some(jobs.len()),
                        _ => None,
                    };
                    DaemonReport {
                        engine: "running",
                        pid: Some(conn.pid),
                        version: Some(conn.engine_version.clone()),
                        socket: paths.socket().display().to_string(),
                        log: paths.log().display().to_string(),
                        downloads,
                    }
                }
                None => DaemonReport {
                    engine: "stopped",
                    pid: None,
                    version: None,
                    socket: paths.socket().display().to_string(),
                    log: paths.log().display().to_string(),
                    downloads: None,
                },
            };
            Ok(emit(mode, &daemon_status(&report), &Typed { kind: "daemon", body: &report }))
        }
        DaemonCommand::Stop => {
            let pid = connect::stop_asked(&paths, env)?;
            let result = if pid.is_some() { "stopped" } else { "already_stopped" };
            #[derive(Serialize)]
            struct Stopped {
                result: &'static str,
                pid: Option<u32>,
            }
            let mut fields = vec![("result", output::Cell::new(result))];
            if let Some(pid) = pid {
                fields.push(("pid", output::Cell::new(pid.to_string())));
            }
            let doc = output::Doc {
                body: output::Body::Fields { title: Vec::new(), fields },
                summary: Vec::new(),
                next: None,
            };
            Ok(emit(mode, &doc, &Typed { kind: "daemon_stop", body: Stopped { result, pid } }))
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
    }
}

fn service_report(report: service::Report, mode: Mode) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Payload {
        result: &'static str,
        manager: &'static str,
        unit: String,
    }
    let unit = report.unit.display().to_string();
    let fields = vec![
        ("result", output::Cell::new(report.result)),
        ("manager", output::Cell::new(report.manager.name())),
        ("unit", output::Cell::new(unit.clone())),
    ];
    let next =
        matches!(report.result, "installed" | "already_installed").then(|| "torrential engine status".to_string());
    let doc = output::Doc { body: output::Body::Fields { title: Vec::new(), fields }, summary: Vec::new(), next };
    let payload = Payload { result: report.result, manager: report.manager.name(), unit };
    Ok(emit(mode, &doc, &Typed { kind: "daemon_service", body: payload }))
}

#[derive(Clone, PartialEq, Debug, serde::Serialize)]
pub struct DaemonReport {
    pub engine: &'static str,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub socket: String,
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
    fields.push(("socket", Cell::new(report.socket.clone())));
    fields.push(("log", Cell::new(report.log.clone())));
    Doc {
        body: Body::Fields { title: Vec::new(), fields },
        summary: Vec::new(),
        next: Some("torrential status".to_string()),
    }
}
