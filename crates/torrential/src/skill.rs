use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use torrential_core::error::Details;
use torrential_core::{Error, ErrorCode};

use crate::env::Env;
use crate::output::format;

pub const SKILL: &str = include_str!("../../../skills/torrential/SKILL.md");
const FILE: &str = "SKILL.md";
const SIDECAR: &str = ".torrential.json";
const LINK: &str = "../../.agents/skills/torrential";

#[derive(Subcommand, Debug)]
pub enum SkillCommand {
    /// Install the skill where coding agents load it
    #[command(
        after_help = "Examples:\n  torrential skill install\n  torrential skill install --agent claude --scope project"
    )]
    Install(InstallArgs),
    /// Show whether each installed copy matches this binary
    Status(TargetArgs),
    /// Print the skill bundled with this binary
    Show,
    /// Remove the copies torrential wrote, and keep any that were edited
    Uninstall(TargetArgs),
    /// Rewrite the copies an older torrential wrote and nobody edited, after torrential update
    #[command(hide = true)]
    Refresh,
}

#[derive(Args, Debug)]
pub struct TargetArgs {
    /// `agents` is the shared .agents/skills copy that Codex, Gemini CLI, Cursor, and others read. `claude` links .claude/skills to it
    #[arg(long, value_enum, default_value_t = AgentArg::All)]
    pub agent: AgentArg,
    /// `user` installs under your home folder, `project` under the current folder
    #[arg(long, value_enum, default_value_t = Scope::User)]
    pub scope: Scope,
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Replace a copy that was edited or that torrential did not write
    #[arg(long)]
    pub force: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum AgentArg {
    Agents,
    Claude,
    All,
}

impl AgentArg {
    pub fn covers(self, target: Target) -> bool {
        matches!((self, target), (Self::All, _) | (Self::Agents, Target::Agents) | (Self::Claude, Target::Claude))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Project,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Agents,
    Claude,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Claude => "claude",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Missing,
    UpToDate,
    Outdated,
    Modified,
    Foreign,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::UpToDate => "up_to_date",
            Self::Outdated => "outdated",
            Self::Modified => "modified",
            Self::Foreign => "foreign",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Shipped<'a> {
    pub text: &'a str,
    pub version: &'a str,
}

#[derive(Clone, PartialEq, Debug, Serialize)]
pub struct Installed {
    pub target: Target,
    pub path: PathBuf,
    pub state: State,
    pub version: Option<String>,
}

#[derive(Clone, PartialEq, Debug, Serialize)]
pub struct Change {
    pub target: Target,
    pub path: PathBuf,
    pub result: &'static str,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
struct Sidecar {
    managed_by: String,
    version: String,
    sha256: String,
}

impl Sidecar {
    fn of(shipped: Shipped) -> Self {
        Self {
            managed_by: "torrential".into(),
            version: shipped.version.into(),
            sha256: sha256(shipped.text.as_bytes()),
        }
    }
}

pub struct Layout {
    pub copy: PathBuf,
    pub link: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Link {
    Absent,
    ToCopy,
    Other,
}

impl Layout {
    pub fn new(scope: Scope, env: &Env, cwd: &Path) -> Result<Self, Error> {
        let root = match scope {
            Scope::User => {
                PathBuf::from(env.var("HOME").ok_or_else(|| Error::new(ErrorCode::Usage, "HOME is not set"))?)
            }
            Scope::Project => cwd.to_path_buf(),
        };
        Ok(Self::under(&root))
    }

    pub fn under(root: &Path) -> Self {
        Self { copy: root.join(".agents/skills/torrential"), link: root.join(".claude/skills/torrential") }
    }

    fn link_kind(&self) -> Link {
        match std::fs::symlink_metadata(&self.link) {
            Err(_) => Link::Absent,
            Ok(meta) if !meta.file_type().is_symlink() => Link::Other,
            Ok(_) => {
                let same =
                    |a: &Path, b: &Path| a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b);
                if std::fs::read_link(&self.link).is_ok_and(|t| t == Path::new(LINK)) || same(&self.link, &self.copy) {
                    Link::ToCopy
                } else {
                    Link::Other
                }
            }
        }
    }
}

pub struct Writes {
    pub sandboxed: bool,
    pub rerun: String,
}

pub fn command(verb: &str, args: &TargetArgs, force: bool, cwd: &Path) -> String {
    let mut words = vec!["torrential", "skill", verb];
    match args.agent {
        AgentArg::All => {}
        AgentArg::Agents => words.extend(["--agent", "agents"]),
        AgentArg::Claude => words.extend(["--agent", "claude"]),
    }
    if args.scope == Scope::Project {
        words.extend(["--scope", "project"]);
    }
    if force {
        words.push("--force");
    }
    let command = words.join(" ");
    match args.scope {
        Scope::User => command,
        Scope::Project => format!("cd {} && {command}", format::shell_word(&cwd.display().to_string())),
    }
}

impl Writes {
    pub fn new(env: &Env, rerun: String) -> Self {
        Self { sandboxed: env.sandbox_harness().is_some(), rerun }
    }

    pub fn error(&self, path: &Path, e: io::Error) -> Error {
        let denied = matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::EROFS));
        let message = format!("cannot write {}: {e}", path.display());
        if denied && self.sandboxed {
            Error::new(ErrorCode::SandboxWriteBlocked, message).with_hint(format!(
                "the sandbox blocks this write; ask the user to run it in a terminal: {}",
                self.rerun
            ))
        } else {
            Error::new(ErrorCode::StorageError, message)
                .with_hint(format!("make sure that folder is writable, then run {}", self.rerun))
        }
    }
}

pub fn status(layout: &Layout, shipped: Shipped) -> Result<Vec<Installed>, Error> {
    let (copy, sidecar) = copy_state(&layout.copy, shipped)?;
    let version = sidecar.map(|s| s.version);
    let (link, link_version) = match layout.link_kind() {
        Link::Absent => (State::Missing, None),
        Link::ToCopy => (copy, version.clone()),
        Link::Other => (State::Foreign, None),
    };
    Ok(vec![
        Installed { target: Target::Agents, path: layout.copy.clone(), state: copy, version },
        Installed { target: Target::Claude, path: layout.link.clone(), state: link, version: link_version },
    ])
}

pub fn install(
    layout: &Layout,
    agent: AgentArg,
    force: bool,
    shipped: Shipped,
    writes: &Writes,
) -> Result<Vec<Change>, Error> {
    let (copy, sidecar) = copy_state(&layout.copy, shipped)?;
    let link = layout.link_kind();
    let wants_link = agent.covers(Target::Claude);
    let mut conflicts = Vec::new();
    if matches!(copy, State::Modified | State::Foreign) {
        conflicts.push((layout.copy.clone(), copy));
    }
    if wants_link && link == Link::Other {
        conflicts.push((layout.link.clone(), State::Foreign));
    }
    if !force && !conflicts.is_empty() {
        return Err(conflict(&conflicts, writes));
    }
    let result = write_copy(&layout.copy, copy, sidecar, shipped, writes)?;
    let mut changes = vec![Change { target: Target::Agents, path: layout.copy.clone(), result }];
    if wants_link {
        let result = match link {
            Link::ToCopy => result,
            Link::Absent => {
                write_link(&layout.link, writes)?;
                "installed"
            }
            Link::Other => {
                replace_with_link(&layout.link, writes)?;
                "updated"
            }
        };
        changes.push(Change { target: Target::Claude, path: layout.link.clone(), result });
    }
    Ok(changes)
}

pub fn refresh(layout: &Layout, shipped: Shipped, writes: &Writes) -> Result<(), Error> {
    let (state, sidecar) = copy_state(&layout.copy, shipped)?;
    if state == State::Outdated {
        write_copy(&layout.copy, state, sidecar, shipped, writes)?;
    }
    Ok(())
}

pub fn uninstall(layout: &Layout, agent: AgentArg, shipped: Shipped, writes: &Writes) -> Result<Vec<Change>, Error> {
    let (copy, _) = copy_state(&layout.copy, shipped)?;
    let kept = |state| match state {
        State::Modified => Some("kept_modified"),
        State::Foreign => Some("kept_foreign"),
        _ => None,
    };
    let removes_copy = agent.covers(Target::Agents);
    let link = match layout.link_kind() {
        Link::Absent => "not_installed",
        Link::Other => "kept_foreign",
        Link::ToCopy => match kept(copy) {
            Some(kept) => kept,
            None => {
                std::fs::remove_file(&layout.link).map_err(|e| writes.error(&layout.link, e))?;
                "removed"
            }
        },
    };
    let mut changes = Vec::new();
    if removes_copy {
        let result = match kept(copy) {
            Some(kept) => kept,
            None => {
                for file in [FILE, SIDECAR] {
                    let path = layout.copy.join(file);
                    match std::fs::remove_file(&path) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(writes.error(&path, e)),
                        _ => {}
                    }
                }
                let _ = std::fs::remove_dir(&layout.copy);
                if copy == State::Missing { "not_installed" } else { "removed" }
            }
        };
        changes.push(Change { target: Target::Agents, path: layout.copy.clone(), result });
    }
    changes.push(Change { target: Target::Claude, path: layout.link.clone(), result: link });
    Ok(changes)
}

fn copy_state(dir: &Path, shipped: Shipped) -> Result<(State, Option<Sidecar>), Error> {
    let sidecar = std::fs::read(dir.join(SIDECAR))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Sidecar>(&bytes).ok())
        .filter(|s| s.managed_by == "torrential");
    let path = dir.join(FILE);
    let text = match std::fs::read(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((State::Missing, sidecar)),
        Err(e) => {
            return Err(Error::new(ErrorCode::StorageError, format!("cannot read {}: {e}", path.display()))
                .with_hint("fix the permissions of that file"));
        }
    };
    let state = match &sidecar {
        None => State::Foreign,
        Some(_) if text == shipped.text.as_bytes() => State::UpToDate,
        Some(s) if s.sha256 == sha256(&text) => State::Outdated,
        Some(_) => State::Modified,
    };
    Ok((state, sidecar))
}

fn write_copy(
    dir: &Path,
    state: State,
    sidecar: Option<Sidecar>,
    shipped: Shipped,
    writes: &Writes,
) -> Result<&'static str, Error> {
    let wanted = Sidecar::of(shipped);
    let record = serde_json::to_vec_pretty(&wanted).expect("a sidecar serializes");
    let write = |path: PathBuf, bytes: &[u8]| write_atomic(&path, bytes).map_err(|e| writes.error(&path, e));
    std::fs::create_dir_all(dir).map_err(|e| writes.error(dir, e))?;
    if sidecar.is_none() {
        write(dir.join(SIDECAR), &record)?;
    }
    if state != State::UpToDate {
        write(dir.join(FILE), shipped.text.as_bytes())?;
    }
    if sidecar.is_some_and(|s| s != wanted) {
        write(dir.join(SIDECAR), &record)?;
    }
    Ok(match state {
        State::Missing => "installed",
        State::UpToDate => "up_to_date",
        State::Outdated | State::Modified | State::Foreign => "updated",
    })
}

fn write_link(link: &Path, writes: &Writes) -> Result<(), Error> {
    let parent = link.parent().expect("the link lives in a skills folder");
    std::fs::create_dir_all(parent).map_err(|e| writes.error(parent, e))?;
    let partial = temporary(link);
    let _ = std::fs::remove_file(&partial);
    std::os::unix::fs::symlink(LINK, &partial)
        .and_then(|()| std::fs::rename(&partial, link))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })
        .map_err(|e| writes.error(link, e))
}

fn replace_with_link(link: &Path, writes: &Writes) -> Result<(), Error> {
    if link.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        let entries: Vec<PathBuf> = std::fs::read_dir(link)
            .map_err(|e| writes.error(link, e))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        let only_skill = entries.iter().all(|p| p.file_name().is_some_and(|n| n == FILE || n == SIDECAR));
        if !only_skill {
            return Err(Error::new(
                ErrorCode::PathConflict,
                format!("{} contains files torrential did not write, so it was not replaced", link.display()),
            )
            .with_hint(format!("move it aside, then run {}", writes.rerun)));
        }
        for path in &entries {
            std::fs::remove_file(path).map_err(|e| writes.error(path, e))?;
        }
        std::fs::remove_dir(link).map_err(|e| writes.error(link, e))?;
    }
    write_link(link, writes)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let kept = std::fs::metadata(path).ok().map(|m| m.permissions());
    let partial = temporary(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    if let Some(permissions) = &kept {
        options.mode(permissions.mode());
    }
    options
        .open(&partial)
        .and_then(|mut file| {
            // The umask may have narrowed the mode `open` asked for.
            if let Some(permissions) = kept {
                file.set_permissions(permissions)?;
            }
            file.write_all(bytes)
        })
        .and_then(|()| std::fs::rename(&partial, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })
}

fn temporary(path: &Path) -> PathBuf {
    let name = path.file_name().expect("written paths end in a name").to_string_lossy();
    path.with_file_name(format!(".{name}.{}.partial", std::process::id()))
}

fn conflict(conflicts: &[(PathBuf, State)], writes: &Writes) -> Error {
    let message = match conflicts {
        [(_, State::Modified)] => "the installed skill was edited after torrential wrote it".to_string(),
        [_] => "the installed skill was not written by torrential".to_string(),
        _ => format!("{} were edited or not written by torrential", format::count(conflicts.len(), "installed skill")),
    };
    Error::new(ErrorCode::PathConflict, message)
        .with_details(Details::Paths(conflicts.iter().map(|(p, _)| p.display().to_string()).collect()))
        .with_hint(format!(
            "keep {it}, or replace {it} with {} --force",
            writes.rerun,
            it = if conflicts.len() == 1 { "it" } else { "them" }
        ))
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: Shipped = Shipped { text: "---\nname: torrential\n---\nold\n", version: "0.0.9" };
    const NOW: Shipped = Shipped { text: SKILL, version: "0.1.0" };

    struct Home {
        dir: tempfile::TempDir,
        layout: Layout,
    }

    impl Home {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let layout = Layout::under(dir.path());
            Self { dir, layout }
        }

        fn states(&self) -> Vec<(&'static str, &'static str)> {
            status(&self.layout, NOW).unwrap().iter().map(|i| (i.target.name(), i.state.name())).collect()
        }

        fn install(&self, shipped: Shipped, force: bool) -> Result<Vec<(&'static str, &'static str)>, Error> {
            let changes = install(&self.layout, AgentArg::All, force, shipped, &writes(false))?;
            Ok(changes.iter().map(|c| (c.target.name(), c.result)).collect())
        }

        fn uninstall(&self, agent: AgentArg) -> Vec<(&'static str, &'static str)> {
            let changes = uninstall(&self.layout, agent, NOW, &writes(false)).unwrap();
            changes.iter().map(|c| (c.target.name(), c.result)).collect()
        }

        fn skill(&self) -> String {
            std::fs::read_to_string(self.layout.copy.join(FILE)).unwrap()
        }

        fn tree(&self) -> Vec<(PathBuf, String)> {
            let mut out = Vec::new();
            let mut pending = vec![self.dir.path().to_path_buf()];
            while let Some(dir) = pending.pop() {
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    let meta = path.symlink_metadata().unwrap();
                    let what = if meta.file_type().is_symlink() {
                        format!("-> {}", std::fs::read_link(&path).unwrap().display())
                    } else if meta.is_dir() {
                        pending.push(path.clone());
                        "dir".to_string()
                    } else {
                        std::fs::read_to_string(&path).unwrap()
                    };
                    out.push((path, what));
                }
            }
            out.sort();
            out
        }
    }

    fn writes(sandboxed: bool) -> Writes {
        Writes { sandboxed, rerun: "torrential skill install".into() }
    }

    const BOTH_UP_TO_DATE: [(&str, &str); 2] = [("agents", "up_to_date"), ("claude", "up_to_date")];

    #[test]
    fn install_writes_the_copy_and_links_claude_to_it_and_twice_changes_nothing() {
        let home = Home::new();
        assert_eq!(home.states(), [("agents", "missing"), ("claude", "missing")]);
        assert_eq!(home.install(NOW, false).unwrap(), [("agents", "installed"), ("claude", "installed")]);
        assert_eq!(home.states(), BOTH_UP_TO_DATE);
        assert_eq!(home.skill(), SKILL);
        assert_eq!(std::fs::read_link(&home.layout.link).unwrap(), Path::new("../../.agents/skills/torrential"));
        assert_eq!(home.layout.link.canonicalize().unwrap(), home.layout.copy.canonicalize().unwrap());
        let sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.layout.copy.join(".torrential.json")).unwrap()).unwrap();
        assert_eq!(sidecar["version"], "0.1.0");
        assert_eq!(sidecar["sha256"], sha256(SKILL.as_bytes()));

        let before = home.tree();
        assert_eq!(home.install(NOW, false).unwrap(), BOTH_UP_TO_DATE);
        assert_eq!(home.tree(), before);
    }

    #[test]
    fn an_older_copy_is_outdated_until_install_updates_it() {
        let home = Home::new();
        home.install(OLD, false).unwrap();
        assert_eq!(home.states(), [("agents", "outdated"), ("claude", "outdated")]);
        assert_eq!(home.install(NOW, false).unwrap(), [("agents", "updated"), ("claude", "updated")]);
        assert_eq!(home.states(), BOTH_UP_TO_DATE);
        assert_eq!(home.skill(), SKILL);
    }

    #[test]
    fn an_edited_copy_is_modified_and_kept_unless_forced() {
        let home = Home::new();
        home.install(NOW, false).unwrap();
        std::fs::write(home.layout.copy.join(FILE), "my notes").unwrap();
        assert_eq!(home.states(), [("agents", "modified"), ("claude", "modified")]);
        let err = home.install(NOW, false).unwrap_err();
        assert_eq!(err.code, ErrorCode::PathConflict);
        assert_eq!(err.message, "the installed skill was edited after torrential wrote it");
        assert_eq!(err.details, Some(Details::Paths(vec![home.layout.copy.display().to_string()])));
        assert_eq!(err.hint.as_deref(), Some("keep it, or replace it with torrential skill install --force"));
        assert_eq!(home.skill(), "my notes");
        assert_eq!(home.uninstall(AgentArg::All), [("agents", "kept_modified"), ("claude", "kept_modified")]);
        assert_eq!(home.skill(), "my notes");
        assert_eq!(home.install(NOW, true).unwrap(), [("agents", "updated"), ("claude", "updated")]);
        assert_eq!(home.states(), BOTH_UP_TO_DATE);
    }

    #[test]
    fn copies_torrential_did_not_write_are_foreign_and_kept_unless_forced() {
        let home = Home::new();
        std::fs::create_dir_all(&home.layout.copy).unwrap();
        std::fs::write(home.layout.copy.join(FILE), "someone else's skill").unwrap();
        std::fs::create_dir_all(&home.layout.link).unwrap();
        std::fs::write(home.layout.link.join(FILE), SKILL).unwrap();
        assert_eq!(home.states(), [("agents", "foreign"), ("claude", "foreign")]);
        let err = home.install(NOW, false).unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            (ErrorCode::PathConflict, "2 installed skills were edited or not written by torrential")
        );
        assert_eq!(home.uninstall(AgentArg::All), [("agents", "kept_foreign"), ("claude", "kept_foreign")]);
        assert_eq!(home.install(NOW, true).unwrap(), [("agents", "updated"), ("claude", "updated")]);
        assert_eq!(home.states(), BOTH_UP_TO_DATE);
    }

    #[test]
    fn force_keeps_a_claude_folder_that_holds_other_files() {
        let home = Home::new();
        std::fs::create_dir_all(home.layout.link.join("references")).unwrap();
        std::fs::write(home.layout.link.join(FILE), "mine").unwrap();
        let err = home.install(NOW, true).unwrap_err();
        assert_eq!(err.code, ErrorCode::PathConflict);
        assert_eq!(std::fs::read_to_string(home.layout.link.join(FILE)).unwrap(), "mine");
    }

    #[test]
    fn uninstall_removes_only_what_it_wrote_and_twice_finds_nothing() {
        let home = Home::new();
        home.install(NOW, false).unwrap();
        std::fs::write(home.layout.copy.join("notes.md"), "kept").unwrap();
        assert_eq!(home.uninstall(AgentArg::All), [("agents", "removed"), ("claude", "removed")]);
        assert_eq!(home.states(), [("agents", "missing"), ("claude", "missing")]);
        assert_eq!(std::fs::read_to_string(home.layout.copy.join("notes.md")).unwrap(), "kept");
        assert_eq!(home.uninstall(AgentArg::All), [("agents", "not_installed"), ("claude", "not_installed")]);
    }

    #[test]
    fn uninstalling_claude_keeps_the_shared_copy() {
        let home = Home::new();
        home.install(NOW, false).unwrap();
        assert_eq!(home.uninstall(AgentArg::Claude), [("claude", "removed")]);
        assert_eq!(home.states(), [("agents", "up_to_date"), ("claude", "missing")]);
    }

    #[test]
    fn a_blocked_write_in_a_sandbox_names_the_command_for_the_human() {
        use std::os::unix::fs::PermissionsExt;
        if crate::paths::uid() == 0 {
            return;
        }
        let home = Home::new();
        let locked = |mode| std::fs::set_permissions(home.dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        locked(0o555);
        let sandboxed = install(&home.layout, AgentArg::All, false, NOW, &writes(true)).unwrap_err();
        let outside = install(&home.layout, AgentArg::All, false, NOW, &writes(false)).unwrap_err();
        locked(0o755);
        assert_eq!(sandboxed.code, ErrorCode::SandboxWriteBlocked);
        assert_eq!(
            sandboxed.hint.as_deref(),
            Some("the sandbox blocks this write; ask the user to run it in a terminal: torrential skill install")
        );
        assert_eq!(outside.code, ErrorCode::StorageError);
        assert_eq!(home.states(), [("agents", "missing"), ("claude", "missing")]);
    }

    #[test]
    fn a_crash_before_the_first_skill_file_lands_reads_as_missing() {
        let home = Home::new();
        std::fs::create_dir_all(&home.layout.copy).unwrap();
        std::fs::write(home.layout.copy.join(SIDECAR), serde_json::to_vec(&Sidecar::of(NOW)).unwrap()).unwrap();
        assert_eq!(home.states(), [("agents", "missing"), ("claude", "missing")]);
        assert_eq!(home.install(NOW, false).unwrap(), [("agents", "installed"), ("claude", "installed")]);
    }

    #[test]
    fn project_scope_installs_under_the_current_folder() {
        let project = tempfile::tempdir().unwrap();
        let env = Env::from_pairs([("HOME", "/nonexistent")]);
        let layout = Layout::new(Scope::Project, &env, project.path()).unwrap();
        assert_eq!(layout.copy, project.path().join(".agents/skills/torrential"));
        let args = TargetArgs { agent: AgentArg::Claude, scope: Scope::Project };
        let rerun = command("install", &args, true, Path::new("/src/my repo"));
        assert_eq!(rerun, "cd '/src/my repo' && torrential skill install --agent claude --scope project --force");
    }
}
