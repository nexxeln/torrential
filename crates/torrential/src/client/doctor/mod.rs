//! `torrential doctor`: checks the sandbox, disk, state, engine, config and sources.

mod probe;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use torrential_core::Profile;

use super::rows::ranked_for;
use super::{connect, emit, launch};
use crate::env::Env;
use crate::ffmpeg::Tools;
use crate::ffmpeg::fetch::{self, Fetch, Ffmpeg, Found};
use crate::output::format;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};
use crate::paths::Paths;
use crate::skill::{self, Installed, State};

pub fn run(env: &Env, mode: Mode, network: bool) -> u8 {
    use crate::disk::{Disk, RealDisk, reserve};
    use torrential_core::units::format_size;
    let check = |check: Checked, status, detail: String| Check { check, status, detail };
    let mut checks = Vec::new();
    let mut fix = None;
    match launch::sandbox_signal(env, launch::send_udp) {
        Some(signal) => {
            let error = launch::sandbox_error(launch::Blocked::Network { signal: signal.clone() }, env);
            fix = error.hint.clone();
            checks.push(check(Checked::Sandbox, CheckStatus::Fail, signal));
        }
        None => checks.push(check(Checked::Sandbox, CheckStatus::Ok, "no sandbox detected".into())),
    }
    let paths = match Paths::from_process(env) {
        Ok(paths) => paths,
        Err(error) => {
            checks.push(check(Checked::State, CheckStatus::Fail, error.message));
            return emit(
                mode,
                &doctor(&checks, fix.clone()),
                &Typed { kind: DocKind::Doctor, body: Checks { checks: &checks } },
            );
        }
    };
    let settings = crate::config::Config::load(&paths, env);
    let downloads = settings.as_ref().map_or_else(|_| paths.downloads.clone(), |s| s.downloads_dir(&paths));
    match RealDisk.space(&downloads) {
        Ok(space) => {
            let low = space.free < reserve(space.total);
            let detail = format!("{} free in {}", format_size(space.free), downloads.display());
            checks.push(check(Checked::Disk, if low { CheckStatus::Warn } else { CheckStatus::Ok }, detail));
        }
        Err(e) => checks.push(check(Checked::Disk, CheckStatus::Fail, format!("{}: {e}", downloads.display()))),
    }
    let state = if paths.state.exists() { crate::paths::check_state_filesystem(&paths.state) } else { Ok(()) };
    match state {
        Ok(()) => checks.push(check(Checked::State, CheckStatus::Ok, paths.state.display().to_string())),
        Err(error) => checks.push(check(Checked::State, CheckStatus::Fail, error.message)),
    }
    match connect::running(&paths, env) {
        Ok(Some(conn)) => checks.push(check(
            Checked::Engine,
            CheckStatus::Ok,
            format!("running {} (pid {})", conn.engine_version, conn.pid),
        )),
        Ok(None) => {
            checks.push(check(Checked::Engine, CheckStatus::Ok, "not running; the next command starts it".into()))
        }
        Err(error) => checks.push(check(Checked::Engine, CheckStatus::Fail, error.message)),
    }
    let version = env.version();
    let shipped = skill::Shipped { text: skill::SKILL, version: &version };
    match skill::Layout::new(skill::Scope::User, env, Path::new("/")).and_then(|l| skill::status(&l, shipped)) {
        Ok(installed) => {
            let (row, skill_fix) = skill_check(&installed);
            checks.push(row);
            fix = fix.or(skill_fix);
        }
        Err(error) => checks.push(check(Checked::Skill, CheckStatus::Fail, error.message)),
    }
    let profile = match (&settings, crate::machine::detect(env)) {
        (Ok(settings), Ok(machine)) => {
            profile_check(&Profile { detected: Some(machine), preferences: settings.preferences, ..Profile::default() })
        }
        (Err(_), _) => check(Checked::Profile, CheckStatus::Skipped, "the config has an error".into()),
        (_, Err(error)) => check(Checked::Profile, CheckStatus::Fail, error.message),
    };
    let fetch_ffmpeg = settings.as_ref().map_or(true, |settings| settings.ffmpeg.fetch);
    checks.extend(probe::checks(&paths, settings, network));
    checks.push(profile);
    checks.push(ffmpeg_check(env, &paths, fetch_ffmpeg));
    emit(mode, &doctor(&checks, fix), &Typed { kind: DocKind::Doctor, body: Checks { checks: &checks } })
}

#[derive(Serialize)]
struct Checks<'a> {
    checks: &'a [Check],
}

pub fn profile_check(profile: &Profile) -> Check {
    Check { check: Checked::Profile, status: CheckStatus::Ok, detail: ranked_for(profile) }
}

/// What a doctor line checks. Its JSON and its text are the same word.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Checked {
    Sandbox,
    Disk,
    State,
    Engine,
    Skill,
    Config,
    Sources,
    /// One search source, which `--network` asks.
    Source(String),
    Profile,
    Ffmpeg,
}

impl fmt::Display for Checked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sandbox => f.write_str("sandbox"),
            Self::Disk => f.write_str("disk"),
            Self::State => f.write_str("state"),
            Self::Engine => f.write_str("engine"),
            Self::Skill => f.write_str("skill"),
            Self::Config => f.write_str("config"),
            Self::Sources => f.write_str("sources"),
            Self::Source(name) => write!(f, "source {name}"),
            Self::Profile => f.write_str("profile"),
            Self::Ffmpeg => f.write_str("ffmpeg"),
        }
    }
}

impl serde::Serialize for Checked {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Clone, PartialEq, Debug, serde::Serialize)]
pub struct Check {
    pub check: Checked,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skipped,
}

impl CheckStatus {
    fn cell(self) -> Cell {
        match self {
            Self::Ok => Cell::styled("ok", Token::Mint),
            Self::Warn => Cell::styled("warn", Token::Accent),
            Self::Fail => Cell::styled("fail", Token::Error),
            Self::Skipped => Cell::styled("skipped", Token::Muted),
        }
    }
}

pub fn doctor(checks: &[Check], fix: Option<String>) -> Doc {
    let rows = checks
        .iter()
        .map(|c| vec![Cell::new(c.check.to_string()), c.status.cell(), Cell::new(c.detail.clone())])
        .collect();
    let probed = checks.iter().any(|c| matches!(c.check, Checked::Source(_)));
    let problems = checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Warn | CheckStatus::Fail))
        .filter(|c| !(probed && c.check == Checked::Sources))
        .count();
    Doc {
        body: Body::Table { header: vec!["CHECK", "STATUS", "DETAIL"], rows },
        summary: vec![format::count(problems, "problem")],
        next: fix,
    }
}

pub fn skill_check(installed: &[Installed]) -> (Check, Option<String>) {
    let states: Vec<State> = installed.iter().map(|i| i.state).collect();
    let detail = installed.iter().map(|i| format!("{} {}", i.target.name(), i.state.name())).collect::<Vec<_>>();
    let status = if states.iter().all(|s| *s == State::UpToDate) {
        CheckStatus::Ok
    } else if states.iter().all(|s| *s == State::Missing) {
        CheckStatus::Skipped
    } else {
        CheckStatus::Warn
    };
    let fixable = states.iter().all(|s| matches!(s, State::UpToDate | State::Missing | State::Outdated));
    let fix = (fixable && status != CheckStatus::Ok).then(|| "torrential skill install".to_string());
    (Check { check: Checked::Skill, status, detail: detail.join(", ") }, fix)
}

/// The `torrential doctor` line about ffmpeg, which the web player uses to play what the browser
/// cannot read on its own: the one the engine runs, or what it does with none. `fetch` is what
/// config.yaml says about fetching one.
pub fn ffmpeg_check(env: &Env, paths: &Paths, fetch: bool) -> Check {
    let path = env.var("PATH").map(String::from);
    let installed = Arc::new(move || Tools::find(path.as_deref()));
    let found = Ffmpeg::new(installed, fetch::root(&paths.state), Fetch::from_env(env, fetch)).found();
    let converts = "the web player converts what the browser cannot read";
    let reads_itself = "the web player plays only MP4 and WebM files the browser reads itself";
    let (status, detail) = match found {
        Found::Installed(programs) => (CheckStatus::Ok, format!("{}; {converts}", programs.ffmpeg.display())),
        Found::Fetched(programs) => {
            (CheckStatus::Ok, format!("{}, fetched by torrential; {converts}", programs.ffmpeg.display()))
        }
        Found::Missing(Fetch::Build(build)) => (
            CheckStatus::Ok,
            format!(
                "not installed; torrential fetches ffmpeg {} ({}) the first time a file needs converting",
                build.version,
                torrential_core::units::format_size(build.size)
            ),
        ),
        Found::Missing(Fetch::Off(_)) => {
            (CheckStatus::Warn, format!("not installed, and config.yaml turns off fetching it; {reads_itself}"))
        }
        Found::Missing(Fetch::Unavailable) => {
            (CheckStatus::Warn, format!("ffmpeg and ffprobe not found; {reads_itself}"))
        }
    };
    Check { check: Checked::Ffmpeg, status, detail }
}
