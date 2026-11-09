use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use torrential_core::{Error, ErrorCode};

use crate::env::Env;

/// The `player` key of config.yaml.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Choice {
    Iina,
    Mpv,
    Vlc,
    /// Called with the video's URL as its last argument.
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

    /// Starts the player on its own, so it outlives torrential and never holds the terminal.
    pub fn launch(&self, launch: &Launch) -> Result<(), Error> {
        let (program, args) = command(self, launch);
        Command::new(&program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(drop)
            .map_err(|e| {
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
        let home = env.var("HOME").map(|home| Path::new(home).join("Applications/IINA.app"));
        [Some(PathBuf::from("/Applications/IINA.app")), home]
            .into_iter()
            .flatten()
            .map(|app| app.join("Contents/MacOS/iina-cli"))
            .find(|cli| cli.is_file())
            .map(|cli| player(Kind::Iina, cli))
    };
    let mpv = || on_path("mpv", env).map(|mpv| player(Kind::Mpv, mpv));
    let vlc = || {
        let app = Path::new("/Applications/VLC.app/Contents/MacOS/VLC");
        let app = (cfg!(target_os = "macos") && app.is_file()).then(|| app.to_path_buf());
        app.or_else(|| on_path("vlc", env)).map(|vlc| player(Kind::Vlc, vlc))
    };
    match choice {
        Some(Choice::Iina) => iina(),
        Some(Choice::Mpv) => mpv(),
        Some(Choice::Vlc) => vlc(),
        Some(Choice::Program(program)) => Some(player(Kind::Program, program.clone())),
        None if cfg!(target_os = "macos") => iina().or_else(mpv).or_else(vlc),
        None => mpv().or_else(vlc).or_else(|| on_path("xdg-open", env).map(|open| player(Kind::Opener, open))),
    }
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
    use std::os::unix::fs::PermissionsExt;
    env.var("PATH")?
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
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
    args.push(launch.video.to_string());
    (player.program.clone(), args)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO: &str = "http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/0/Night.mp4";
    const ENGLISH: &str = "http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/3/Night.en.srt";
    const FRENCH: &str = "http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/4/Night.fr.srt";

    fn argv(kind: Kind, program: &str, subtitles: Vec<&str>, language: Option<&str>) -> Vec<String> {
        let player = Player { kind, program: program.into() };
        let launch = Launch { video: VIDEO, subtitles, language, title: "Night of the Living Dead (1968)" };
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
    fn other_programs_get_only_the_url() {
        assert_eq!(argv(Kind::Opener, "/usr/bin/xdg-open", vec![ENGLISH], Some("en")), ["/usr/bin/xdg-open", VIDEO]);
        assert_eq!(argv(Kind::Program, "/usr/local/bin/play", vec![ENGLISH], None), ["/usr/local/bin/play", VIDEO]);
    }

    #[test]
    fn config_names_a_player_or_an_absolute_path() {
        let parsed = ["iina", "mpv", "vlc", "/usr/local/bin/play", "celluloid", "bin/play", "IINA"].map(Choice::parse);
        assert_eq!(
            parsed,
            [
                Some(Choice::Iina),
                Some(Choice::Mpv),
                Some(Choice::Vlc),
                Some(Choice::Program("/usr/local/bin/play".into())),
                None,
                None,
                None,
            ]
        );
    }
}
