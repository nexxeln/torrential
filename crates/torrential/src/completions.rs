use clap_complete::engine::CompletionCandidate;
use torrential_core::protocol::{Request, Response};

use crate::cli::Shell;
use crate::client::connect;
use crate::client::rows::short_ids;
use crate::env::Env;
use crate::paths::Paths;

pub fn complete_download_ids() -> Vec<CompletionCandidate> {
    let env = Env::from_process();
    let Ok(paths) = Paths::from_process(&env) else { return Vec::new() };
    let Ok(Some(mut conn)) = connect::existing(&paths, &env) else { return Vec::new() };
    let Ok(Response::Jobs { jobs }) = conn.call(&Request::Status { refs: Vec::new() }) else { return Vec::new() };
    let ids = short_ids(&jobs.iter().map(|j| j.id).collect::<Vec<_>>());
    jobs.iter()
        .zip(ids)
        .map(|(job, id)| {
            let help = job.name.as_deref().map(torrential_core::text::sanitize).unwrap_or_default();
            CompletionCandidate::new(id).help(Some(help.into()))
        })
        .collect()
}

pub struct Setup {
    pub shell: &'static str,
    pub rc_file: &'static str,
    pub line: &'static str,
}

pub fn setup(shell: Shell) -> Setup {
    match shell {
        Shell::Bash => Setup { shell: "bash", rc_file: "~/.bashrc", line: "source <(COMPLETE=bash torrential)" },
        Shell::Zsh => Setup { shell: "zsh", rc_file: "~/.zshrc", line: "source <(COMPLETE=zsh torrential)" },
        Shell::Fish => {
            Setup { shell: "fish", rc_file: "~/.config/fish/config.fish", line: "COMPLETE=fish torrential | source" }
        }
        Shell::Elvish => Setup {
            shell: "elvish",
            rc_file: "~/.config/elvish/rc.elv",
            line: "eval (E:COMPLETE=elvish torrential | slurp)",
        },
        Shell::Powershell => Setup {
            shell: "powershell",
            rc_file: "$PROFILE",
            line: "$env:COMPLETE = \"powershell\"; torrential | Out-String | Invoke-Expression; Remove-Item Env:\\COMPLETE",
        },
    }
}

pub fn script(setup: &Setup) -> String {
    format!("# add this line to {}\n{}\n", setup.rc_file, setup.line)
}
