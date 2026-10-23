pub mod connect;
pub mod launch;
mod probe;
pub mod service;
mod setup;
pub mod tui;
mod update;
pub mod wait;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use torrential_core::protocol::{FilesReply, GetRequest, Request, Response};
use torrential_core::{Error, ErrorCode, Preferences, Profile, SearchQuery, Target};

use crate::cli::{Command, DaemonCommand, GetArgs, SearchArgs, Until};
use crate::daemon::files;
use crate::daemon::server::METADATA_WAIT;
use crate::env::Env;
use crate::output::views::{self, ChangeCommand, Check, CheckStatus, DaemonReport, FileMatch};
use crate::output::{self, Mode, Typed, render};
use crate::paths::Paths;
use crate::skill::{self, SkillCommand, Writes};
use connect::Conn;

pub fn run(command: Command, mode: Mode, env: &Env) -> u8 {
    match dispatch(command, mode, env) {
        Ok(exit) => exit,
        Err(error) => {
            print_error(mode, &error);
            error.code.exit_code()
        }
    }
}

pub fn print_error(mode: Mode, error: &Error) {
    let out = output::error(mode, error);
    anstream::print!("{}", out.stdout);
    anstream::eprint!("{}", out.stderr);
}

fn emit(mode: Mode, doc: &output::Doc, payload: &impl Serialize) -> u8 {
    let mut stdout = anstream::stdout().lock();
    let _ = stdout.write_all(render(mode, doc, payload).as_bytes());
    let _ = stdout.flush();
    0
}

fn answer(response: Response) -> Result<Response, Error> {
    match response {
        Response::Error { error } => Err(error),
        other => Ok(other),
    }
}

fn dispatch(command: Command, mode: Mode, env: &Env) -> Result<u8, Error> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let paths = || Paths::from_process(env);
    let engine = || connect::ensure(&paths()?, env);
    match command {
        Command::Search(args) => {
            let preferences = crate::config::preferences(&paths()?)?;
            search(engine()?, args, preferences, mode)
        }
        Command::Files(args) => {
            let target = Target::parse(&args.reference, &cwd)?;
            let request = Request::Files { target, timeout_ms: METADATA_WAIT.as_millis() as u32 };
            match answer(engine()?.call_within(&request, METADATA_WAIT + Duration::from_secs(15))?)? {
                Response::Files { mut files } => {
                    let matched =
                        if args.files.is_empty() { None } else { Some(keep_matching(&mut files, &args.files)?) };
                    let doc = views::files(&files, matched.as_ref());
                    Ok(emit(mode, &doc, &Typed { kind: "files", body: Files { files, matched } }))
                }
                other => Err(connect::unexpected(&other)),
            }
        }
        Command::Get(args) => get(engine()?, args, mode, &cwd, env),
        Command::Status(args) => {
            let refs = job_refs(&args.references, &cwd)?;
            let detail = refs.len() == 1;
            match answer(engine()?.call(&Request::Status { refs })?)? {
                Response::Jobs { jobs } => {
                    let doc = if detail { views::job(&jobs[0]) } else { views::jobs(&jobs) };
                    Ok(emit(mode, &doc, &Response::Jobs { jobs }))
                }
                other => Err(connect::unexpected(&other)),
            }
        }
        Command::Wait(args) => {
            let refs = job_refs(&args.references, &cwd)?;
            let timeout = match (args.timeout, mode) {
                (Some(t), _) => Some(t),
                (None, Mode::Human) => None,
                (None, _) => Some(wait::DEFAULT_TIMEOUT),
            };
            let conn = engine()?;
            match mode {
                Mode::Human => wait::attach(conn, refs, args.until, timeout),
                _ => wait::run(conn, refs, args.until, timeout, &wait::Printer { mode }),
            }
        }
        Command::Pause(args) => {
            changes(engine()?, Request::Pause { refs: job_refs(&args.references, &cwd)? }, ChangeCommand::Pause, mode)
        }
        Command::Resume(args) => {
            let request = Request::Resume { refs: job_refs(&args.references, &cwd)?, all: args.all };
            changes(engine()?, request, ChangeCommand::Resume, mode)
        }
        Command::Rm(args) => {
            changes(engine()?, Request::Remove { refs: job_refs(&args.references, &cwd)? }, ChangeCommand::Remove, mode)
        }
        Command::Verify(args) => {
            let reference = job_refs(std::slice::from_ref(&args.reference), &cwd)?.remove(0);
            changes(engine()?, Request::Verify { reference, repair: args.repair }, ChangeCommand::Verify, mode)
        }
        Command::Setup(args) => setup::run(args, mode, env),
        Command::Doctor(args) => Ok(doctor(env, mode, args.network)),
        Command::Tui => tui::run(env),
        Command::Daemon { command } => daemon(command, env, mode),
        Command::Schema => {
            let schema = crate::schema::build(&<crate::cli::Cli as clap::CommandFactory>::command(), env.version());
            print!("{}", output::json_document(&schema, None));
            Ok(0)
        }
        Command::Skill { command } => skill(command, mode, env, &cwd),
        Command::Update(args) => update::run(args, mode, env),
        Command::Completions { shell } => {
            let setup = crate::completions::setup(shell);
            match mode {
                Mode::Json => {
                    #[derive(Serialize)]
                    struct Payload<'a> {
                        #[serde(rename = "type")]
                        kind: &'static str,
                        shell: &'a str,
                        rc_file: &'a str,
                        line: &'a str,
                    }
                    let payload =
                        Payload { kind: "completions", shell: setup.shell, rc_file: setup.rc_file, line: setup.line };
                    print!("{}", output::json_document(&payload, None));
                }
                _ => print!("{}", crate::completions::script(&setup)),
            }
            Ok(0)
        }
    }
}

fn search(mut conn: Conn, args: SearchArgs, preferences: Preferences, mode: Mode) -> Result<u8, Error> {
    let query = SearchQuery {
        text: args.words.join(" "),
        sources: args.source,
        category: args.category.map(Into::into),
        min_seeds: args.min_seeds,
        max_size: args.max_size,
        sort: args.sort.into(),
        limit: args.limit,
        timeout_ms: args.timeout.as_millis().min(u128::from(u32::MAX)) as u32,
        adult: args.adult,
        fresh: args.fresh,
        profile: Profile { preferences, ..Profile::default() },
    };
    let within = args.timeout + Duration::from_secs(10);
    match answer(conn.call_within(&Request::Search { query, stream: false }, within)?)? {
        Response::Search { result } => {
            let doc = views::search(&result, crate::daemon::owner::now());
            Ok(emit(mode, &doc, &Response::Search { result }))
        }
        other => Err(connect::unexpected(&other)),
    }
}

fn get(mut conn: Conn, args: GetArgs, mode: Mode, cwd: &Path, env: &Env) -> Result<u8, Error> {
    let target = Target::parse(&args.reference, cwd)?;
    if !args.files.is_empty() {
        crate::daemon::files::compile(&args.files)?;
    }
    let dir = args.dir.map(|d| absolute(&d, cwd, env));
    let dry_run = args.dry_run;
    let setup_max_size = crate::config::preferences(&Paths::from_process(env)?)?.max_size;
    let request = Request::Get(GetRequest {
        target,
        dir: dir.clone(),
        files: args.files,
        max_size: args.max_size,
        setup_max_size,
        dry_run,
    });
    let within = if dry_run { METADATA_WAIT + Duration::from_secs(15) } else { connect::DEFAULT_REPLY_WITHIN };
    let reply = match answer(conn.call_within(&request, within)?)? {
        Response::Get { reply } => reply,
        other => return Err(connect::unexpected(&other)),
    };
    let exit = emit(mode, &views::get(&reply, dir.as_deref()), &Response::Get { reply: reply.clone() });
    let attachable = mode == Mode::Human && !dry_run && !reply.job.status.is_stopped();
    if attachable {
        eprintln!("Ctrl-C stops watching. The download keeps going.");
        return wait::attach(
            connect::ensure(&Paths::from_process(env)?, env)?,
            vec![reply.job.id.to_hex()],
            Until::Done,
            None,
        );
    }
    Ok(exit)
}

fn absolute(path: &Path, cwd: &Path, env: &Env) -> PathBuf {
    let expanded = match (path.strip_prefix("~"), env.var("HOME")) {
        (Ok(rest), Some(home)) => Path::new(home).join(rest),
        _ => path.to_path_buf(),
    };
    cwd.join(expanded)
}

fn job_refs(references: &[String], cwd: &Path) -> Result<Vec<String>, Error> {
    references
        .iter()
        .map(|reference| match Target::parse(reference, cwd)? {
            Target::Prefix(prefix) => Ok(prefix),
            Target::InfoHash(_) => Ok(reference.trim().to_string()),
            Target::Magnet(magnet) => Ok(magnet),
            Target::TorrentUrl(_) => Err(not_a_download("a .torrent URL")),
            Target::TorrentFile(_) => Err(not_a_download("a .torrent file")),
        })
        .collect()
}

fn not_a_download(kind: &str) -> Error {
    Error::new(ErrorCode::InvalidRef, format!("this command takes a download ID, not {kind}"))
        .with_hint("run torrential status to list download IDs")
}

fn changes(mut conn: Conn, request: Request, command: ChangeCommand, mode: Mode) -> Result<u8, Error> {
    let within = match command {
        ChangeCommand::Pause => Duration::from_secs(20),
        _ => connect::DEFAULT_REPLY_WITHIN,
    };
    match answer(conn.call_within(&request, within)?)? {
        Response::Changed { changes } => {
            let doc = views::changes(command, &changes);
            Ok(emit(mode, &doc, &Response::Changed { changes }))
        }
        other => Err(connect::unexpected(&other)),
    }
}

fn daemon(command: DaemonCommand, env: &Env, mode: Mode) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    match command {
        DaemonCommand::Status => {
            let report = match connect::existing(&paths, env)? {
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
            Ok(emit(mode, &views::daemon_status(&report), &Typed { kind: "daemon", body: &report }))
        }
        DaemonCommand::Stop => {
            let (result, pid) = match connect::existing(&paths, env)? {
                Some(conn) => ("stopped", Some(connect::stop(conn)?)),
                None => ("already_stopped", None),
            };
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
            if let Some(conn) = connect::existing(&paths, env)? {
                connect::stop(conn)?;
            }
            service_report(service::install(env, &paths.log())?, mode)
        }
        DaemonCommand::Uninstall => service_report(service::uninstall(env)?, mode),
        DaemonCommand::Run { service } => Ok(crate::daemon::run(env, service)),
        DaemonCommand::Launch => Ok(launch::launch_stage(env)),
    }
}

fn skill(command: SkillCommand, mode: Mode, env: &Env, cwd: &Path) -> Result<u8, Error> {
    let version = env.version();
    let shipped = skill::Shipped { text: skill::SKILL, version: &version };
    #[derive(Serialize)]
    struct Changes<'a> {
        scope: skill::Scope,
        changes: &'a [skill::Change],
    }
    match command {
        SkillCommand::Show => {
            #[derive(Serialize)]
            struct Text<'a> {
                version: &'a str,
                text: &'a str,
            }
            match mode {
                Mode::Json => {
                    let body = Text { version: &version, text: skill::SKILL };
                    print!("{}", output::json_document(&Typed { kind: "skill", body }, None));
                }
                _ => print!("{}", skill::SKILL),
            }
            Ok(0)
        }
        SkillCommand::Status(args) => {
            #[derive(Serialize)]
            struct Status<'a> {
                version: &'a str,
                scope: skill::Scope,
                targets: &'a [skill::Installed],
            }
            let layout = skill::Layout::new(args.scope, env, cwd)?;
            let installed: Vec<skill::Installed> =
                skill::status(&layout, shipped)?.into_iter().filter(|i| args.agent.covers(i.target)).collect();
            let install = skill::command("install", &args, false, cwd);
            let doc = views::skill_status(&installed, &version, &install);
            let body = Status { version: &version, scope: args.scope, targets: &installed };
            Ok(emit(mode, &doc, &Typed { kind: "skill_status", body }))
        }
        SkillCommand::Install(args) => {
            let layout = skill::Layout::new(args.target.scope, env, cwd)?;
            let writes = Writes::new(env, skill::command("install", &args.target, args.force, cwd));
            let changes = skill::install(&layout, args.target.agent, args.force, shipped, &writes)?;
            let body = Changes { scope: args.target.scope, changes: &changes };
            Ok(emit(mode, &views::skill_changes(&changes), &Typed { kind: "skill_install", body }))
        }
        SkillCommand::Refresh => {
            let writes = Writes::new(env, "torrential skill install".into());
            for scope in [skill::Scope::User, skill::Scope::Project] {
                skill::refresh(&skill::Layout::new(scope, env, cwd)?, shipped, &writes)?;
            }
            Ok(0)
        }
        SkillCommand::Uninstall(args) => {
            let layout = skill::Layout::new(args.scope, env, cwd)?;
            let writes = Writes::new(env, skill::command("uninstall", &args, false, cwd));
            let changes = skill::uninstall(&layout, args.agent, shipped, &writes)?;
            let body = Changes { scope: args.scope, changes: &changes };
            Ok(emit(mode, &views::skill_changes(&changes), &Typed { kind: "skill_uninstall", body }))
        }
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
        matches!(report.result, "installed" | "already_installed").then(|| "torrential daemon status".to_string());
    let doc = output::Doc { body: output::Body::Fields { title: Vec::new(), fields }, summary: Vec::new(), next };
    let payload = Payload { result: report.result, manager: report.manager.name(), unit };
    Ok(emit(mode, &doc, &Typed { kind: "daemon_service", body: payload }))
}

fn doctor(env: &Env, mode: Mode, network: bool) -> u8 {
    use crate::daemon::admission::{Disk, RealDisk, reserve};
    use torrential_core::units::format_size;
    let check = |check: &str, status, detail: String| Check { check: check.into(), status, detail };
    let mut checks = Vec::new();
    let mut fix = None;
    match launch::sandbox_signal(env, launch::send_udp) {
        Some(signal) => {
            let error = launch::sandbox_error(launch::Blocked::Network { signal: signal.clone() }, env);
            fix = error.hint.clone();
            checks.push(check("sandbox", CheckStatus::Fail, signal));
        }
        None => checks.push(check("sandbox", CheckStatus::Ok, "no sandbox signal".into())),
    }
    let paths = match Paths::from_process(env) {
        Ok(paths) => paths,
        Err(error) => {
            checks.push(check("state", CheckStatus::Fail, error.message));
            return emit(
                mode,
                &views::doctor(&checks, fix.clone()),
                &Typed { kind: "doctor", body: Checks { checks: &checks } },
            );
        }
    };
    let settings = crate::config::Config::load(&paths, env);
    let downloads = settings.as_ref().map_or_else(|_| paths.downloads.clone(), |s| s.downloads_dir(&paths));
    match RealDisk.space(&downloads) {
        Ok(space) => {
            let low = space.free < reserve(space.total);
            let detail = format!("{} free in {}", format_size(space.free), downloads.display());
            checks.push(check("disk", if low { CheckStatus::Warn } else { CheckStatus::Ok }, detail));
        }
        Err(e) => checks.push(check("disk", CheckStatus::Fail, format!("{}: {e}", downloads.display()))),
    }
    let state = if paths.state.exists() { crate::paths::check_state_filesystem(&paths.state) } else { Ok(()) };
    match state {
        Ok(()) => checks.push(check("state", CheckStatus::Ok, paths.state.display().to_string())),
        Err(error) => checks.push(check("state", CheckStatus::Fail, error.message)),
    }
    match connect::existing(&paths, env) {
        Ok(Some(conn)) => {
            checks.push(check("engine", CheckStatus::Ok, format!("running {} (pid {})", conn.engine_version, conn.pid)))
        }
        Ok(None) => checks.push(check("engine", CheckStatus::Ok, "not running; the next command starts it".into())),
        Err(error) => checks.push(check("engine", CheckStatus::Fail, error.message)),
    }
    let version = env.version();
    let shipped = skill::Shipped { text: skill::SKILL, version: &version };
    match skill::Layout::new(skill::Scope::User, env, Path::new("/")).and_then(|l| skill::status(&l, shipped)) {
        Ok(installed) => {
            let (row, skill_fix) = views::skill_check(&installed);
            checks.push(row);
            fix = fix.or(skill_fix);
        }
        Err(error) => checks.push(check("skill", CheckStatus::Fail, error.message)),
    }
    let profile = match (&settings, crate::machine::detect(env)) {
        (Ok(settings), Ok(machine)) => {
            views::profile_check(&Profile { detected: Some(machine), preferences: settings.preferences, room: None })
        }
        (Err(_), _) => check("profile", CheckStatus::Skipped, "the config has an error".into()),
        (_, Err(error)) => check("profile", CheckStatus::Fail, error.message),
    };
    checks.extend(probe::checks(&paths, settings, network));
    checks.push(profile);
    emit(mode, &views::doctor(&checks, fix), &Typed { kind: "doctor", body: Checks { checks: &checks } })
}

#[derive(Serialize)]
struct Checks<'a> {
    checks: &'a [Check],
}

#[derive(Serialize)]
pub struct Files {
    pub files: FilesReply,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<FileMatch>,
}

pub fn keep_matching(reply: &mut FilesReply, patterns: &[String]) -> Result<FileMatch, Error> {
    let set = files::compile(patterns)?;
    let wanted = reply.files.iter().map(|f| set.is_match(&f.path)).collect();
    let wanted = files::some_match(wanted, patterns, reply.name.as_deref().unwrap_or(&reply.id), &reply.id)?;
    let total_files = reply.files.len();
    let mut wanted = wanted.into_iter();
    reply.files.retain(|_| wanted.next().unwrap_or(false));
    Ok(FileMatch { patterns: patterns.to_vec(), total_files, size: reply.files.iter().map(|f| f.size).sum() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::TorrentFile;

    fn reply() -> FilesReply {
        let file = |index, path: &str, size| TorrentFile {
            index,
            path: path.into(),
            size,
            selected: false,
            verified: 0,
            risk: None,
        };
        FilesReply {
            id: "6bc1af".to_string() + &"0".repeat(34),
            name: Some("film".into()),
            total_size: 600,
            files: vec![file(0, "Film.mp4", 400), file(1, "extras/Trailer.MP4", 150), file(2, "notes.txt", 50)],
            metadata_source: "torrent_file".into(),
            elapsed_ms: 1,
        }
    }

    fn keep(patterns: &[&str]) -> Result<(Vec<u32>, FileMatch), Error> {
        let mut reply = reply();
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        let matched = keep_matching(&mut reply, &patterns)?;
        Ok((reply.files.iter().map(|f| f.index).collect(), matched))
    }

    #[test]
    fn globs_keep_the_files_get_would_pick() {
        let (kept, matched) = keep(&["*.mp4"]).unwrap();
        assert_eq!(kept, [0, 1]);
        assert_eq!((matched.total_files, matched.size), (3, 550));
        assert_eq!(keep(&["NOTES.TXT", "film.mp4"]).unwrap().0, [0, 2]);
        assert_eq!(keep(&["extras/Trailer.MP4"]).unwrap().0, [1]);
    }

    #[test]
    fn globs_that_match_nothing_fail_like_get() {
        let err = keep(&["*.mkv"]).unwrap_err();
        assert_eq!(err.code, ErrorCode::NoSelection);
        assert_eq!(err.message, "no file in film matches \"*.mkv\"");
        assert_eq!(err.hint.as_deref(), Some("run torrential files 6bc1af to see the paths"));
        assert_eq!(keep(&["["]).unwrap_err().code, ErrorCode::Usage);
    }
}
