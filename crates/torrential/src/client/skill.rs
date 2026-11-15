//! `torrential skill`: the skill coding agents load.

use std::path::Path;

use serde::Serialize;
use torrential_core::Error;

use super::emit;
use crate::env::Env;
use crate::fsutil::Writes;
use crate::output;
use crate::output::format::NONE;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, Mode, Typed};
use crate::skill::{self, Change as SkillChange, Installed, SkillCommand, State};

pub fn run(command: SkillCommand, mode: Mode, env: &Env, cwd: &Path) -> Result<u8, Error> {
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
            let doc = skill_status(&installed, &version, &install);
            let body = Status { version: &version, scope: args.scope, targets: &installed };
            Ok(emit(mode, &doc, &Typed { kind: "skill_status", body }))
        }
        SkillCommand::Install(args) => {
            let layout = skill::Layout::new(args.target.scope, env, cwd)?;
            let writes = Writes::new(env, skill::command("install", &args.target, args.force, cwd));
            let changes = skill::install(&layout, args.target.agent, args.force, shipped, &writes)?;
            let body = Changes { scope: args.target.scope, changes: &changes };
            Ok(emit(mode, &skill_changes(&changes), &Typed { kind: "skill_install", body }))
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
            Ok(emit(mode, &skill_changes(&changes), &Typed { kind: "skill_uninstall", body }))
        }
    }
}

fn skill_state_cell(state: State) -> Cell {
    let token = match state {
        State::UpToDate => Token::Mint,
        State::Missing => Token::Muted,
        State::Outdated | State::Modified | State::Foreign => Token::Accent,
    };
    Cell::styled(state.name(), token)
}

pub fn skill_status(installed: &[Installed], version: &str, install: &str) -> Doc {
    let rows = installed
        .iter()
        .map(|i| {
            vec![
                Cell::styled(i.target.name(), Token::Accent),
                skill_state_cell(i.state),
                Cell::new(i.version.as_deref().unwrap_or(NONE)),
                Cell::new(i.path.display().to_string()),
            ]
        })
        .collect();
    let mut summary = vec![format!("this binary includes the skill from torrential {version}")];
    for i in installed {
        match i.state {
            State::Modified => summary.push(format!("{} was edited after torrential wrote it", i.target.name())),
            State::Foreign => summary.push(format!("{} was not written by torrential", i.target.name())),
            _ => {}
        }
    }
    let kept = installed.iter().any(|i| matches!(i.state, State::Modified | State::Foreign));
    if kept {
        summary.push(format!("to replace them: {install} --force"));
    }
    let stale = installed.iter().any(|i| matches!(i.state, State::Missing | State::Outdated));
    let next = (stale && !kept).then(|| install.to_string());
    Doc { body: Body::Table { header: vec!["TARGET", "STATE", "VERSION", "PATH"], rows }, summary, next }
}

pub fn skill_changes(changes: &[SkillChange]) -> Doc {
    let rows = changes
        .iter()
        .map(|c| {
            vec![
                Cell::styled(c.target.name(), Token::Accent),
                Cell::styled(c.result, Token::Mint),
                Cell::new(c.path.display().to_string()),
            ]
        })
        .collect();
    Doc { body: Body::Table { header: vec!["TARGET", "RESULT", "PATH"], rows }, summary: Vec::new(), next: None }
}
