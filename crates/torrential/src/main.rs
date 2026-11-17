mod cli;
mod client;
mod completions;
mod output;
mod schema;
mod skill;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use torrential_core::{Error, ErrorCode};

use cli::{Cli, Command, DaemonCommand};
use output::Mode;
use output::theme::{Token, paint};
use torrential_daemon::env::Env;
use torrential_daemon::paths::Paths;
use torrential_daemon::update::Audience;
use torrential_daemon::{daemon, service};

const SHORT_HELP: &str = "torrential searches torrent indexes and downloads torrents.

  torrential search <words>       search for torrents
  torrential titles <words>       find a film or series
  torrential play <ID>            play a film, an episode, or a torrent's video
  torrential files <ID>           list a torrent's files
  torrential get <ID>             start a download
  torrential wait <ID>            follow a download until it finishes
  torrential status               list downloads
  torrential tui                  open the terminal UI

Run torrential --help for every command, or torrential schema for JSON.
";

fn main() -> ExitCode {
    let env = Env::from_process();
    CompleteEnv::with_factory(Cli::command).complete();
    let args: Vec<String> = std::env::args().collect();
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        // The version an engine reports, which an update reads from the installed binary.
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayVersion => {
            println!("torrential {}", env.version());
            return ExitCode::SUCCESS;
        }
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {
            let _ = e.print();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            let json = args.iter().any(|a| a == "--json");
            let mode = Mode::detect(json, stdout_is_terminal, &env);
            client::print_error(mode, &usage_error(&e, &args));
            return ExitCode::from(torrential_daemon::exit::code(ErrorCode::Usage));
        }
    };
    let Some(command) = cli.command else {
        if !cli.json && stdout_is_terminal && std::io::stdin().is_terminal() {
            return ExitCode::from(client::run(Command::Tui, Mode::Human, &env));
        }
        print!("{SHORT_HELP}");
        return ExitCode::SUCCESS;
    };
    if let Command::Daemon { command: DaemonCommand::Launch } = command {
        return ExitCode::from(client::launch::launch_stage(&env));
    }
    if let Command::Daemon { command: DaemonCommand::Interrupt { pid } } = command {
        return ExitCode::from(interrupt(pid));
    }
    let mode = Mode::detect(cli.json, stdout_is_terminal, &env);
    if let Command::Daemon { command: DaemonCommand::Run { service, http_port } } = command {
        return ExitCode::from(engine(service, http_port, mode, &env));
    }
    let notices = !matches!(command, Command::Tui | Command::Update(_) | Command::Completions { .. });
    let exit = client::run(command, mode, &env);
    if notices {
        print_notices(mode, &env);
    }
    ExitCode::from(exit)
}

/// `torrential daemon interrupt`. On Windows this process attaches to the engine's console, so it
/// is the only work it does.
fn interrupt(pid: u32) -> u8 {
    #[cfg(windows)]
    let sent = torrential_daemon::process::send_interrupt(pid).is_ok();
    #[cfg(unix)]
    let sent = {
        torrential_daemon::process::interrupt(pid);
        true
    };
    if sent { torrential_daemon::exit::SUCCESS } else { torrential_daemon::exit::code(ErrorCode::DaemonUnavailable) }
}

/// `torrential engine run`: the engine in this process until it stops.
fn engine(service: bool, http_port: Option<u16>, mode: Mode, env: &Env) -> u8 {
    match Paths::from_process(env) {
        Ok(_) => daemon::run(env, service, http_port),
        Err(error) => {
            client::print_error(mode, &error);
            torrential_daemon::exit::code(error.code)
        }
    }
}

fn usage_error(e: &clap::Error, args: &[String]) -> Error {
    let rendered = e.render().to_string();
    let message = rendered.lines().next().unwrap_or("invalid arguments").trim_start_matches("error: ").to_string();
    let subcommand = args.iter().skip(1).find(|a| !a.starts_with('-'));
    let known = subcommand.is_some_and(|s| Cli::command().find_subcommand(s).is_some());
    let hint = match subcommand {
        Some(s) if known => format!("torrential {s} --help"),
        _ => "torrential --help".to_string(),
    };
    Error::new(ErrorCode::Usage, message).with_hint(hint.clone()).with_next(hint)
}

/// Who reads the notices after a command: no one when it prints JSON, an agent when one runs
/// torrential, and a person at a terminal.
fn audience(mode: Mode, env: &Env, stderr_is_terminal: bool) -> Option<Audience> {
    match mode {
        Mode::Json => None,
        _ if env.agent().is_some() => Some(Audience::Agent),
        _ if stderr_is_terminal => Some(Audience::Person),
        _ => None,
    }
}

/// Prints, after a command, that the engine it used does not run this torrential, that the
/// engine added the login service, and that an update is out, each once.
fn print_notices(mode: Mode, env: &Env) {
    let Some(audience) = audience(mode, env, std::io::stderr().is_terminal()) else { return };
    let Ok(paths) = Paths::from_process(env) else { return };
    let lines = [
        client::connect::notice(audience),
        service::notice(&paths, audience),
        client::update::notice(&paths, env, audience, torrential_core::time::now()),
    ];
    for line in lines.into_iter().flatten() {
        match audience {
            Audience::Person => anstream::eprintln!("{}", paint(Token::Muted, &line)),
            Audience::Agent => eprintln!("{line}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_has_no_notices_and_agents_get_theirs_without_a_terminal() {
        let env = |pairs: &[(&str, &str)]| Env::from_pairs(pairs.iter().copied());
        assert_eq!(audience(Mode::Json, &env(&[("CLAUDECODE", "1")]), true), None);
        assert_eq!(audience(Mode::Plain, &env(&[("CLAUDECODE", "1")]), false), Some(Audience::Agent));
        assert_eq!(audience(Mode::Human, &env(&[]), true), Some(Audience::Person));
        assert_eq!(audience(Mode::Plain, &env(&[]), false), None);
    }
}
