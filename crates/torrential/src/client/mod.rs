mod changes;
pub mod connect;
mod devices;
mod doctor;
mod engine;
mod files;
mod get;
pub mod launch;
pub mod play;
pub mod rows;
mod search;
mod setup;
mod skill;
mod status;
mod titles;
pub mod tui;
mod tv;
pub mod update;
pub mod wait;
mod web;

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use torrential_core::protocol::{FilesReply, JobAction, Request, Response};
use torrential_core::{Error, ErrorCode, Job, Target};

use crate::cli::Command;
use crate::env::Env;
use crate::output::{self, DocKind, Mode, Typed, render};
use crate::paths::Paths;
use connect::Conn;
use torrential_core::protocol::METADATA_WAIT;

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

fn dispatch(command: Command, mode: Mode, env: &Env) -> Result<u8, Error> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let paths = || Paths::from_process(env);
    let engine = || connect::ensure(&paths()?, env);
    match command {
        Command::Search(args) => search::run(engine()?, args, mode),
        Command::Titles(args) => titles::titles(engine()?, &args.words, mode),
        Command::Title(args) => {
            let video = titles::video(&args.id, None)?;
            titles::title(engine()?, video, mode)
        }
        Command::Sources(args) => {
            let video = titles::video(&args.video, args.episode.as_deref())?;
            titles::sources(engine()?, video, mode)
        }
        Command::Files(args) => {
            let mut files = file_list(&mut engine()?, Target::parse(&args.reference, &cwd)?)?;
            let matched =
                if args.files.is_empty() { None } else { Some(files::keep_matching(&mut files, &args.files)?) };
            let doc = files::files(&files, matched.as_ref());
            Ok(emit(mode, &doc, &Typed { kind: DocKind::Files, body: files::Files { files, matched } }))
        }
        Command::Get(args) => match Target::parse(&args.reference, &cwd)? {
            Target::Title(_) => get::title(engine, args, mode, env),
            target => get::run(engine()?, target, args, mode, &cwd, env),
        },
        Command::Play(args) => play::run(args, mode, env, &cwd),
        Command::Web(args) => web::run(&engine()?, args, mode, env),
        Command::Devices { command } => devices::run(engine()?, command, mode),
        Command::Tv { command } => tv::run(engine()?, command, mode, &cwd),
        Command::Status(args) => {
            let refs = job_refs(&args.references, &cwd)?;
            let detail = refs.len() == 1;
            let jobs: Vec<Job> = engine()?.ask(&Request::Status { refs })?;
            let doc = if detail { status::job(&jobs[0]) } else { status::jobs(&jobs) };
            Ok(emit(mode, &doc, &Response::Jobs { jobs }))
        }
        Command::Wait(args) => {
            let refs = job_refs(&args.references, &cwd)?;
            let timeout = match (args.timeout, mode) {
                (Some(t), _) => Some(t),
                (None, Mode::Human) => None,
                (None, _) => Some(wait::DEFAULT_TIMEOUT),
            };
            let following = (engine()?, wait::Engine { paths: paths()?, env: env.clone() });
            match mode {
                Mode::Human => wait::attach(following, refs, args.until, timeout),
                _ => wait::run(following, refs, args.until, timeout, &wait::Printer { mode }),
            }
        }
        Command::Pause(args) => {
            changes::run(engine()?, Request::Pause { refs: job_refs(&args.references, &cwd)? }, JobAction::Pause, mode)
        }
        Command::Resume(args) => {
            let request = Request::Resume { refs: job_refs(&args.references, &cwd)?, all: args.all };
            changes::run(engine()?, request, JobAction::Resume, mode)
        }
        Command::Rm(args) => {
            let request = Request::Remove { refs: job_refs(&args.references, &cwd)?, delete: args.delete };
            changes::run(engine()?, request, JobAction::Remove, mode)
        }
        Command::Verify(args) => {
            let reference = job_refs(std::slice::from_ref(&args.reference), &cwd)?.remove(0);
            changes::run(engine()?, Request::Verify { reference, repair: args.repair }, JobAction::Verify, mode)
        }
        Command::Setup(args) => setup::run(args, mode, env),
        Command::Doctor(args) => Ok(doctor::run(env, mode, args.network)),
        Command::Tui => tui::run(env),
        Command::Daemon { command } => engine::run(command, env, mode),
        Command::Schema => {
            let schema = crate::schema::build(&<crate::cli::Cli as clap::CommandFactory>::command(), env.version());
            print!("{}", output::json_document(&schema, None));
            Ok(0)
        }
        Command::Skill { command } => skill::run(command, mode, env, &cwd),
        Command::Update(args) => update::run(args, mode, env),
        Command::Completions { shell } => {
            let setup = crate::completions::setup(shell);
            match mode {
                Mode::Json => {
                    #[derive(Serialize)]
                    struct Payload<'a> {
                        #[serde(rename = "type")]
                        kind: DocKind,
                        shell: &'a str,
                        rc_file: &'a str,
                        line: &'a str,
                    }
                    let payload = Payload {
                        kind: DocKind::Completions,
                        shell: setup.shell,
                        rc_file: setup.rc_file,
                        line: setup.line,
                    };
                    print!("{}", output::json_document(&payload, None));
                }
                _ => print!("{}", crate::completions::script(&setup)),
            }
            Ok(0)
        }
    }
}

/// A torrent's files, fetching its metadata if needed, as `torrential files` lists them.
pub fn file_list(conn: &mut Conn, target: Target) -> Result<FilesReply, Error> {
    conn.ask(&Request::Files { target, timeout_ms: METADATA_WAIT.as_millis() as u32 })
}

pub fn absolute(path: &Path, cwd: &Path, env: &Env) -> PathBuf {
    cwd.join(env.expand_home(path))
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
            Target::Title(video) => Err(torrential_core::id::not_a_torrent(&video, "this command")),
        })
        .collect()
}

fn not_a_download(kind: &str) -> Error {
    Error::new(ErrorCode::InvalidRef, format!("this command takes a download ID, not {kind}"))
        .with_hint("run torrential status to list download IDs")
        .with_next("torrential status")
}

#[cfg(test)]
mod tests;
