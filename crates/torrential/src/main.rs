mod cli;
mod client;
mod completions;
mod config;
mod daemon;
mod disk;
mod env;
mod exit;
mod ffmpeg;
mod fsutil;
mod log;
mod machine;
mod output;
mod paths;
mod playback;
mod player;
mod qr;
mod schema;
mod selection;
mod service;
mod skill;
mod update;
mod wire;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use torrential_core::{Error, ErrorCode};

use cli::{Cli, Command, DaemonCommand};
use env::Env;
use output::Mode;
use output::theme::{Token, paint};
use paths::Paths;
use update::Audience;

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
            return ExitCode::from(crate::exit::code(ErrorCode::Usage));
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

/// `torrential engine run`: the engine in this process until it stops.
fn engine(service: bool, http_port: Option<u16>, mode: Mode, env: &Env) -> u8 {
    match Paths::from_process(env) {
        Ok(_) => daemon::run(env, service, http_port),
        Err(error) => {
            client::print_error(mode, &error);
            crate::exit::code(error.code)
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

/// Prints, after a command, that the engine added the login service and that an update is
/// out, each once.
fn print_notices(mode: Mode, env: &Env) {
    let Some(audience) = Audience::of(mode, env, std::io::stderr().is_terminal()) else { return };
    let Ok(paths) = Paths::from_process(env) else { return };
    let lines = [
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
