mod cli;
mod client;
mod completions;
mod config;
mod daemon;
mod env;
mod machine;
mod output;
mod paths;
mod schema;
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

const SHORT_HELP: &str = "torrential searches torrent indexes and downloads torrents.

  torrential search <words>       find something
  torrential files <ID>           list what is inside
  torrential get <ID>             download it
  torrential wait <ID>            follow it until it is done
  torrential status               list downloads
  torrential tui                  open the terminal UI

Run torrential --help for every command, or torrential schema for JSON.
";

fn main() -> ExitCode {
    CompleteEnv::with_factory(Cli::command).complete();
    let env = Env::from_process();
    let args: Vec<String> = std::env::args().collect();
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) if matches!(e.kind(), clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion) => {
            let _ = e.print();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            let json = args.iter().any(|a| a == "--json");
            let mode = Mode::detect(json, stdout_is_terminal, &env);
            client::print_error(mode, &usage_error(&e, &args));
            return ExitCode::from(ErrorCode::Usage.exit_code());
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
    let notices = !matches!(
        command,
        Command::Tui
            | Command::Update(_)
            | Command::Completions { .. }
            | Command::Daemon { command: DaemonCommand::Run { .. } }
    );
    let exit = client::run(command, mode, &env);
    if notices {
        update::print_notice(mode, &env);
    }
    ExitCode::from(exit)
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
