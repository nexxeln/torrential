use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use torrential_core::{Error, ErrorCode};

use crate::env::{Env, hooks};

/// The `player` key of config.yaml.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Choice {
    Iina,
    Mpv,
    Vlc,
    /// Called with the video's URL as its last argument. One named as a known player also gets
    /// that player's flag for where to start.
    Program(PathBuf),
}

impl Choice {
    pub fn parse(value: &str) -> Option<Choice> {
        match value {
            "iina" => Some(Choice::Iina),
            "mpv" => Some(Choice::Mpv),
            "vlc" => Some(Choice::Vlc),
            path if Path::new(path).is_absolute() => Some(Choice::Program(path.into())),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Iina,
    Mpv,
    Vlc,
    /// `xdg-open`, which hands the URL to the desktop's default player.
    Opener,
    Program,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Player {
    kind: Kind,
    program: PathBuf,
}

/// What to play, as `PlayReply` serves it.
pub struct Launch<'a> {
    pub video: &'a str,
    pub subtitles: Vec<&'a str>,
    /// An ISO 639 code for the subtitles to show first.
    pub language: Option<&'a str>,
    pub title: &'a str,
    /// Where to start, as a person resumes a video. Players get whole seconds, since VLC may read
    /// a fraction in the system's locale.
    pub start: Option<Duration>,
    /// Where the player opens a socket that says where it is, for a player that opens one.
    pub socket: Option<&'a Path>,
}

impl Player {
    pub fn name(&self) -> String {
        match self.kind {
            Kind::Iina => "IINA".into(),
            Kind::Mpv => "mpv".into(),
            Kind::Vlc => "VLC".into(),
            Kind::Opener => "the default player".into(),
            Kind::Program => self.program.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        }
    }

    /// Whether the player opens a socket that says where it is: mpv and IINA do.
    pub fn says_where(&self) -> bool {
        socket_flag(self.acts_as(), Path::new("")).is_some()
    }

    /// The known player this one takes the flags of. A program named as a known player is that
    /// player.
    fn acts_as(&self) -> Kind {
        match self.kind {
            Kind::Program => named(&self.program).unwrap_or(Kind::Program),
            kind => kind,
        }
    }

    /// Starts the player on its own, so it outlives torrential and never holds the terminal.
    pub fn launch(&self, launch: &Launch) -> Result<(), Error> {
        let (program, args) = command(self, launch);
        let mut command = Command::new(&program);
        command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        on_its_own(&mut command);
        command.spawn().map(drop).map_err(|e| {
            Error::new(ErrorCode::Usage, format!("cannot start {}: {e}", program.display()))
                .with_hint("set player: in config.yaml to iina, mpv, vlc, or the absolute path of a player")
        })
    }
}

/// The configured player, or the first one installed: IINA, mpv, then VLC on macOS, and mpv, VLC,
/// then the desktop's default elsewhere.
pub fn find(choice: Option<&Choice>, env: &Env) -> Option<Player> {
    let player = |kind, program| Player { kind, program };
    let iina = || {
        let home = env.home().map(|home| home.join("Applications/IINA.app"));
        [Some(PathBuf::from("/Applications/IINA.app")), home]
            .into_iter()
            .flatten()
            .map(|app| app.join("Contents/MacOS/iina-cli"))
            .find(|cli| cli.is_file())
            .map(|cli| player(Kind::Iina, cli))
    };
    let mpv = || on_path("mpv", env).or_else(|| installed(env, &["mpv/mpv.exe"])).map(|mpv| player(Kind::Mpv, mpv));
    let vlc = || {
        let app = Path::new("/Applications/VLC.app/Contents/MacOS/VLC");
        let app = (cfg!(target_os = "macos") && app.is_file()).then(|| app.to_path_buf());
        app.or_else(|| on_path("vlc", env))
            .or_else(|| installed(env, &["VideoLAN/VLC/vlc.exe"]))
            .map(|vlc| player(Kind::Vlc, vlc))
    };
    match choice {
        Some(Choice::Iina) => iina(),
        Some(Choice::Mpv) => mpv(),
        Some(Choice::Vlc) => vlc(),
        Some(Choice::Program(program)) => Some(player(Kind::Program, program.clone())),
        None if cfg!(target_os = "macos") => iina().or_else(mpv).or_else(vlc),
        // Windows has no program that hands a stream to the default player.
        None if cfg!(windows) => mpv().or_else(vlc),
        None => mpv().or_else(vlc).or_else(|| on_path("xdg-open", env).map(|open| player(Kind::Opener, open))),
    }
}

/// The first of `programs` that the Windows installers put under Program Files, or under the
/// person's own programs folder.
fn installed(env: &Env, programs: &[&str]) -> Option<PathBuf> {
    let folders = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|var| env.var(var).map(PathBuf::from))
        .chain(env.var("LOCALAPPDATA").map(|local| Path::new(local).join("Programs")));
    folders.flat_map(|folder| programs.iter().map(move |program| folder.join(program))).find(|path| path.is_file())
}

/// Detaches `command` from torrential, so the player outlives it and a Ctrl-C in its terminal.
#[cfg(unix)]
fn on_its_own(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn on_its_own(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    crate::process::keep_standard_handles();
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Opens `url`, or a folder, in the default app. A test names a program in `OPENER` to run in its
/// place.
pub fn open_url(url: &str, env: &Env) -> Result<(), Error> {
    let program = match env.test().and_then(|test| test.path(hooks::OPENER)) {
        Some(program) => program.to_path_buf(),
        None if cfg!(target_os = "macos") => PathBuf::from("/usr/bin/open"),
        None if cfg!(windows) => PathBuf::from("explorer.exe"),
        None => PathBuf::from("xdg-open"),
    };
    Command::new(&program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| {
            Error::new(ErrorCode::NotFound, format!("cannot open a browser with {}: {e}", program.display()))
                .with_hint(format!("open {url} in your browser"))
        })
}

/// The error for no player, which names the URL so any player can open it.
pub fn missing(choice: Option<&Choice>, url: &str, config: &Path) -> Error {
    let message = match choice {
        Some(Choice::Iina) => "config.yaml sets player: iina, but IINA is not installed".to_string(),
        Some(Choice::Mpv) => "config.yaml sets player: mpv, but mpv is not on PATH".to_string(),
        Some(Choice::Vlc) => "config.yaml sets player: vlc, but VLC is not installed".to_string(),
        Some(Choice::Program(_)) | None => "no media player found (looked for IINA, mpv, and VLC)".to_string(),
    };
    Error::new(ErrorCode::NotFound, message).with_hint(format!(
        "open {url} in any player, or set player: in {} to the player's absolute path",
        config.display()
    ))
}

fn on_path(name: &str, env: &Env) -> Option<PathBuf> {
    let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(env.var("PATH")?).map(|dir| dir.join(&name)).find(|path| runnable(path))
}

#[cfg(unix)]
fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn runnable(path: &Path) -> bool {
    path.is_file()
}

/// The program and arguments that open `launch` in `player`.
fn command(player: &Player, launch: &Launch) -> (PathBuf, Vec<String>) {
    let mut args = Vec::new();
    match player.kind {
        Kind::Iina => {
            args.push("--no-stdin".to_string());
            // IINA sets each --mpv- option as a property, so a repeated one replaces the one before.
            // sub-files takes the whole list, split at `:` unless a backslash escapes it.
            if !launch.subtitles.is_empty() {
                let escaped: Vec<String> = launch.subtitles.iter().map(|url| url.replace(':', "\\:")).collect();
                args.push(format!("--mpv-sub-files={}", escaped.join(":")));
            }
            args.extend(launch.language.map(|code| format!("--mpv-slang={code}")));
            args.push(format!("--mpv-force-media-title={}", launch.title));
        }
        Kind::Mpv => {
            args.extend(launch.subtitles.iter().map(|url| format!("--sub-file={url}")));
            args.extend(launch.language.map(|code| format!("--slang={code}")));
            args.push(format!("--force-media-title={}", launch.title));
        }
        Kind::Vlc => {
            // --sub-file reads its value as a local path, while each input slave may be a URL.
            if !launch.subtitles.is_empty() {
                args.push(format!("--input-slave={}", launch.subtitles.join("#")));
            }
            args.extend(launch.language.map(|code| format!("--sub-language={code}")));
            args.push(format!("--meta-title={}", launch.title));
        }
        Kind::Opener | Kind::Program => {}
    }
    let acts_as = player.acts_as();
    args.extend(launch.start.and_then(|at| start_flag(acts_as, at)));
    args.extend(launch.socket.and_then(|path| socket_flag(acts_as, path)));
    args.push(launch.video.to_string());
    (player.program.clone(), args)
}

/// The known player a program's file name names, in any case: `mpv`, `iina` or its `iina-cli`,
/// or `vlc`. An app's program inside its bundle is named as the app.
fn named(program: &Path) -> Option<Kind> {
    let name = program.file_name()?.to_str()?.to_ascii_lowercase();
    match name.strip_suffix(".exe").unwrap_or(&name) {
        "mpv" => Some(Kind::Mpv),
        "iina" | "iina-cli" => Some(Kind::Iina),
        "vlc" => Some(Kind::Vlc),
        _ => None,
    }
}

/// The argument that starts a player of `kind` at `at`, for the players that have one.
fn start_flag(kind: Kind, at: Duration) -> Option<String> {
    let seconds = at.as_secs();
    match kind {
        Kind::Iina => Some(format!("--mpv-start={seconds}")),
        Kind::Mpv => Some(format!("--start={seconds}")),
        Kind::Vlc => Some(format!("--start-time={seconds}")),
        Kind::Opener | Kind::Program => None,
    }
}

/// The argument that has a player of `kind` open a socket at `path` that says where it is, for
/// the players that have one.
fn socket_flag(kind: Kind, path: &Path) -> Option<String> {
    match kind {
        Kind::Iina => Some(format!("--mpv-input-ipc-server={}", path.display())),
        Kind::Mpv => Some(format!("--input-ipc-server={}", path.display())),
        Kind::Vlc | Kind::Opener | Kind::Program => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO: &str = "http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/0/Night.mp4";
    const ENGLISH: &str = "http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/3/Night.en.srt";
    const FRENCH: &str = "http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/4/Night.fr.srt";

    fn argv(kind: Kind, program: &str, subtitles: Vec<&str>, language: Option<&str>) -> Vec<String> {
        let player = Player { kind, program: program.into() };
        let launch = Launch {
            video: VIDEO,
            subtitles,
            language,
            title: "Night of the Living Dead (1968)",
            start: None,
            socket: None,
        };
        let (program, args) = command(&player, &launch);
        [program.display().to_string()].into_iter().chain(args).collect()
    }

    #[test]
    fn iina_gets_every_subtitle_in_one_escaped_list() {
        assert_eq!(
            argv(Kind::Iina, "/Applications/IINA.app/Contents/MacOS/iina-cli", vec![ENGLISH, FRENCH], Some("en")),
            [
                "/Applications/IINA.app/Contents/MacOS/iina-cli",
                "--no-stdin",
                "--mpv-sub-files=http\\://127.0.0.1\\:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/3/Night.en.srt:\
                 http\\://127.0.0.1\\:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/4/Night.fr.srt",
                "--mpv-slang=en",
                "--mpv-force-media-title=Night of the Living Dead (1968)",
                VIDEO,
            ]
        );
        assert_eq!(
            argv(Kind::Iina, "/Applications/IINA.app/Contents/MacOS/iina-cli", vec![], None),
            [
                "/Applications/IINA.app/Contents/MacOS/iina-cli",
                "--no-stdin",
                "--mpv-force-media-title=Night of the Living Dead (1968)",
                VIDEO,
            ]
        );
    }

    #[test]
    fn mpv_gets_one_sub_file_per_subtitle() {
        assert_eq!(
            argv(Kind::Mpv, "/opt/homebrew/bin/mpv", vec![ENGLISH, FRENCH], Some("en")),
            [
                "/opt/homebrew/bin/mpv",
                &format!("--sub-file={ENGLISH}"),
                &format!("--sub-file={FRENCH}"),
                "--slang=en",
                "--force-media-title=Night of the Living Dead (1968)",
                VIDEO,
            ]
        );
    }

    #[test]
    fn vlc_gets_subtitles_as_input_slaves() {
        assert_eq!(
            argv(Kind::Vlc, "/Applications/VLC.app/Contents/MacOS/VLC", vec![ENGLISH, FRENCH], Some("fr")),
            [
                "/Applications/VLC.app/Contents/MacOS/VLC",
                &format!("--input-slave={ENGLISH}#{FRENCH}"),
                "--sub-language=fr",
                "--meta-title=Night of the Living Dead (1968)",
                VIDEO,
            ]
        );
    }

    #[test]
    fn each_known_player_starts_at_the_whole_second_a_person_stopped() {
        let started = |kind, program: &str| {
            let player = Player { kind, program: program.into() };
            let start = Some(Duration::from_secs_f64(200.9));
            let launch =
                Launch { video: VIDEO, subtitles: vec![], language: None, title: "Night", start, socket: None };
            command(&player, &launch).1
        };
        assert_eq!(
            [
                started(Kind::Iina, "/Applications/IINA.app/Contents/MacOS/iina-cli"),
                started(Kind::Mpv, "/opt/homebrew/bin/mpv"),
                started(Kind::Vlc, "/Applications/VLC.app/Contents/MacOS/VLC"),
                started(Kind::Opener, "/usr/bin/xdg-open"),
                started(Kind::Program, "/usr/local/bin/play"),
            ],
            [
                vec!["--no-stdin", "--mpv-force-media-title=Night", "--mpv-start=200", VIDEO],
                vec!["--force-media-title=Night", "--start=200", VIDEO],
                vec!["--meta-title=Night", "--start-time=200", VIDEO],
                vec![VIDEO],
                vec![VIDEO],
            ]
        );
    }

    #[test]
    fn a_program_given_as_a_path_starts_where_a_person_stopped_and_says_where_it_is_as_the_known_player_it_is_named() {
        let started = |program: &str| {
            let player = Player { kind: Kind::Program, program: program.into() };
            let start = Some(Duration::from_secs_f64(200.9));
            let socket = Some(Path::new("/tmp/th/run/player-1.sock"));
            let launch =
                Launch { video: VIDEO, subtitles: vec![ENGLISH], language: Some("en"), title: "Night", start, socket };
            (player.says_where(), command(&player, &launch).1)
        };
        let args = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        let mpv = (true, args(&["--start=200", "--input-ipc-server=/tmp/th/run/player-1.sock", VIDEO]));
        let iina = (true, args(&["--mpv-start=200", "--mpv-input-ipc-server=/tmp/th/run/player-1.sock", VIDEO]));
        let vlc = (false, args(&["--start-time=200", VIDEO]));
        let other = (false, args(&[VIDEO]));
        let programs = [
            "/opt/homebrew/bin/mpv",
            "/usr/local/bin/MPV",
            "/usr/local/bin/iina",
            "/Applications/IINA.app/Contents/MacOS/iina-cli",
            "/Applications/IINA.app/Contents/MacOS/IINA",
            "/usr/bin/vlc",
            "/Applications/VLC.app/Contents/MacOS/VLC",
            "/usr/local/bin/mpv-wrapper",
            "/usr/local/bin/play",
        ];
        assert_eq!(
            programs.map(started),
            [mpv.clone(), mpv, iina.clone(), iina.clone(), iina, vlc.clone(), vlc, other.clone(), other]
        );
    }

    #[test]
    fn other_programs_get_only_the_url() {
        assert_eq!(argv(Kind::Opener, "/usr/bin/xdg-open", vec![ENGLISH], Some("en")), ["/usr/bin/xdg-open", VIDEO]);
        assert_eq!(argv(Kind::Program, "/usr/local/bin/play", vec![ENGLISH], None), ["/usr/local/bin/play", VIDEO]);
    }

    #[test]
    fn config_names_a_player_or_an_absolute_path() {
        let absolute = if cfg!(windows) { r"C:\Tools\play.exe" } else { "/usr/local/bin/play" };
        let parsed = ["iina", "mpv", "vlc", absolute, "celluloid", "bin/play", "IINA"].map(Choice::parse);
        assert_eq!(
            parsed,
            [
                Some(Choice::Iina),
                Some(Choice::Mpv),
                Some(Choice::Vlc),
                Some(Choice::Program(absolute.into())),
                None,
                None,
                None,
            ]
        );
    }
}
